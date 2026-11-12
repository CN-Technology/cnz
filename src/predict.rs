//! Deflate predictor and corrections (stage 4b, step 2).
//!
//! For a Deflate stream written by a program other than zlib, recompressing
//! cannot repeat the same choices. We then keep the decompressed contents
//! and a list of **corrections**: an in-house match finder walks through
//! the data and guesses, token by token, what the original program wrote;
//! only the places where it is wrong are recorded.
//!
//! Prediction at a position: the longest repetition (the closest at equal
//! length) among the previous positions with the same start (3-byte hash
//! chains, 32 KB back, at most 4096 candidates); in "lazy" mode, a literal
//! if the next position offers a longer one. It only depends on data already
//! known: extraction replays exactly the same predictions.
//!
//! For each token, a **kind**:
//! - 0: the prediction is right;
//! - 1: a literal, where a repetition was expected;
//! - 2: a repetition (length difference from the best one, rank of the
//!   right candidate among those that are long enough);
//! - 3: a repetition out of the finder's reach (length and distance).
//!
//! The block structure (types, Huffman tables as written, paddings) is
//! recorded as is; the rewriter of deflate.rs then rebuilds the stream bit
//! for bit.

use anyhow::{bail, ensure, Result};

use crate::deflate::{Block, BlockKind, DynamicHeader, Stream, Token};
use crate::format::{put_varint, Cursor};

const WINDOW: usize = 32768;
const MAX_CHAIN: usize = 4096;
const MAX_MATCH: usize = 258;
const NONE: u32 = u32::MAX;
const VERSION: u64 = 1;

/// 3-byte hash chains: for each position, the previous position with the
/// same hash.
struct Chains {
    head: Vec<u32>,
    prev: Vec<u32>,
    /// Candidates examined (to bound the writer's work).
    work: std::cell::Cell<u64>,
}

impl Chains {
    fn new(n: usize) -> Self {
        Self { head: vec![NONE; 1 << 16], prev: vec![NONE; n], work: std::cell::Cell::new(0) }
    }

    fn hash(d: &[u8], i: usize) -> usize {
        let v = u32::from(d[i]) << 16 | u32::from(d[i + 1]) << 8 | u32::from(d[i + 2]);
        (v.wrapping_mul(0x9E37_79B1) >> 16) as usize
    }

    fn insert(&mut self, d: &[u8], i: usize) {
        if i + 3 <= d.len() {
            let h = Self::hash(d, i);
            self.prev[i] = self.head[h];
            self.head[h] = i as u32;
        }
    }

    /// Removes `i` if it is at the head of its chain (provisional insertion).
    fn remove(&mut self, d: &[u8], i: usize) {
        if i + 3 <= d.len() {
            let h = Self::hash(d, i);
            if self.head[h] == i as u32 {
                self.head[h] = self.prev[i];
                self.prev[i] = NONE;
            }
        }
    }

    /// Candidate positions for `pos`, from closest to farthest (at most 32 KB,
    /// at most 4096 candidates).
    fn chain<'a>(&'a self, d: &[u8], pos: usize) -> impl Iterator<Item = usize> + 'a {
        let mut c = if pos + 3 <= d.len() { self.head[Self::hash(d, pos)] } else { NONE };
        let mut hops = 0;
        std::iter::from_fn(move || {
            if c == NONE || hops >= MAX_CHAIN {
                return None;
            }
            let cu = c as usize;
            if cu >= pos || pos - cu > WINDOW {
                return None;
            }
            hops += 1;
            self.work.set(self.work.get() + 1);
            // Positions always decreasing: a chain cannot loop.
            let next = self.prev[cu];
            c = if next != NONE && next < c { next } else { NONE };
            Some(cu)
        })
    }

    /// Best repetition at `pos`: the longest, the closest at equal length; stop
    /// as soon as the maximum length is reached.
    fn best(&self, d: &[u8], pos: usize) -> (u16, u16) {
        let max = d.len().saturating_sub(pos).min(MAX_MATCH);
        let (mut len, mut dist) = (2usize, 0usize);
        for cu in self.chain(d, pos) {
            // The byte that would exceed the best length must match (a zlib trick):
            // otherwise this candidate cannot do better.
            if d[cu + len] != d[pos + len] || d[cu] != d[pos] {
                continue;
            }
            let mut l = 0;
            while l < max && d[cu + l] == d[pos + l] {
                l += 1;
            }
            if l > len {
                (len, dist) = (l, pos - cu);
                if len == max {
                    break;
                }
            }
        }
        if dist == 0 {
            (0, 0)
        } else {
            (len as u16, dist as u16)
        }
    }

    /// Does the candidate share at least `len` bytes with `pos`?
    fn at_least(d: &[u8], cu: usize, pos: usize, len: usize) -> bool {
        d[cu + len - 1] == d[pos + len - 1] && d[cu..cu + len] == d[pos..pos + len]
    }

    /// Rank of `dist` among the candidates of at least `len` bytes (None: out
    /// of reach).
    fn rank(&self, d: &[u8], pos: usize, len: u16, dist: u16) -> Option<usize> {
        let len = len as usize;
        if pos + len > d.len() {
            return None;
        }
        let mut seen = 0;
        for cu in self.chain(d, pos) {
            if Self::at_least(d, cu, pos, len) {
                if pos - cu == dist as usize {
                    return Some(seen);
                }
                seen += 1;
            }
        }
        None
    }

    /// Distance of the rank-`r` candidate among those of at least `len` bytes.
    fn nth(&self, d: &[u8], pos: usize, len: u16, r: usize) -> Option<u16> {
        let len = len as usize;
        if pos + len > d.len() {
            return None;
        }
        self.chain(d, pos).filter(|&cu| Self::at_least(d, cu, pos, len)).nth(r).map(|cu| (pos - cu) as u16)
    }
}

/// Prediction mode (corrections mode byte): bit 0 = lazy; bits 1 and 2 =
/// rule for 3-byte repetitions (0: taken, 1: ignored beyond 4096 bytes back,
/// like zlib, 2: always ignored).
const MODES: u8 = 6;
const FAR_3: u16 = 4096;

fn mode_valid(m: u8) -> bool {
    m < MODES
}

/// Forecast of a mode, from the best repetition (`best`, `dist`) and the
/// best one at the next position (`next`, useful in lazy mode).
fn choose(mode: u8, best: u16, dist: u16, next: impl FnOnce() -> u16) -> (u16, u16) {
    if best < 3 {
        return (0, 0);
    }
    let rule3 = mode >> 1;
    if best == 3 && (rule3 == 2 || (rule3 == 1 && dist > FAR_3)) {
        return (0, 0);
    }
    if mode & 1 == 1 && (best as usize) < MAX_MATCH && next() > best {
        return (0, 0);
    }
    (best, dist)
}

/// What the predictor expects at `pos`: (length, distance), length 0 = literal.
/// Also returns the best length found (reference for the differences).
fn predict_at(d: &[u8], ch: &mut Chains, pos: usize, mode: u8) -> ((u16, u16), u16) {
    let (best, dist) = ch.best(d, pos);
    let pred = choose(mode, best, dist, || {
        ch.insert(d, pos);
        let (next, _) = ch.best(d, pos + 1);
        ch.remove(d, pos);
        next
    });
    (pred, best)
}

/// Corrections of a stream (decompressed data `raw`, actual tokens `s`), and
/// the number of correctly guessed tokens: all variants are computed in a
/// single pass (they only differ by the forecast), the best one is kept.
pub fn diff(raw: &[u8], s: &Stream) -> Result<(Vec<u8>, usize)> {
    let all = diff_core(raw, s, [true; MODES as usize])?;
    Ok(all.into_iter().flatten().max_by_key(|v| v.1).unwrap())
}

/// Corrections with a given prediction mode.
pub fn diff_mode(raw: &[u8], s: &Stream, mode: u8) -> Result<(Vec<u8>, usize)> {
    ensure!(mode_valid(mode), "mode inconnu");
    let mut want = [false; MODES as usize];
    want[mode as usize] = true;
    Ok(diff_core(raw, s, want)?.into_iter().flatten().next().unwrap())
}

/// Corrections of one variant: kinds, details, correctly guessed tokens.
#[derive(Default)]
struct Variant {
    kinds: Vec<u8>,
    details: Vec<u8>,
    exact: usize,
}

impl Variant {
    /// Records the actual token `t` against the prediction `pred`; `rank` gives
    /// (on demand) the rank of the true distance.
    fn note(&mut self, t: Token, pred: (u16, u16), best: u16, rank: &mut dyn FnMut(u16, u16) -> Option<usize>) {
        let kind = match t {
            Token::Literal(_) => u8::from(pred.0 != 0),
            Token::Match { len, dist } if pred == (len, dist) => 0,
            Token::Match { len, dist } => match (len <= best).then(|| rank(len, dist)).flatten() {
                Some(r) => {
                    put_varint(&mut self.details, u64::from(best - len));
                    put_varint(&mut self.details, r as u64);
                    2
                }
                None => {
                    put_varint(&mut self.details, u64::from(len));
                    put_varint(&mut self.details, u64::from(dist));
                    3
                }
            },
        };
        self.exact += usize::from(kind == 0);
        self.kinds.push(kind);
    }
}

/// Safeguard check every CHECK_EVERY tokens.
const CHECK_EVERY: u64 = 1 << 16;
/// Maximum candidates examined per byte, on average.
const MAX_WORK_PER_BYTE: u64 = 32;

fn diff_core(raw: &[u8], s: &Stream, want: [bool; MODES as usize]) -> Result<Vec<Option<(Vec<u8>, usize)>>> {
    let mut tokens_seen = 0u64;
    let mut structure = Vec::new();
    let mut v: Vec<Variant> = (0..MODES).map(|_| Variant::default()).collect();
    let mut ch = Chains::new(raw.len());
    let mut pos = 0usize;
    put_varint(&mut structure, s.blocks.len() as u64);
    for b in &s.blocks {
        put_block(&mut structure, b);
        if let BlockKind::Stored { data, .. } = &b.kind {
            ensure!(raw.get(pos..pos + data.len()) == Some(&data[..]), "données incohérentes");
            for i in pos..pos + data.len() {
                ch.insert(raw, i);
            }
            pos += data.len();
            continue;
        }
        for &t in &b.tokens {
            // Writer safeguards (no effect on the format): give up if the predictor
            // guesses badly (corrections would weigh more than the gain) or if the
            // search costs too much (very repetitive images). The stream is then stored
            // as is.
            tokens_seen += 1;
            if tokens_seen % CHECK_EVERY == 0 {
                let best_exact = v.iter().map(|x| x.exact).max().unwrap_or(0) as u64;
                ensure!(best_exact * 10 >= tokens_seen * 6, "prédiction trop mauvaise");
                ensure!(ch.work.get() <= MAX_WORK_PER_BYTE * pos as u64 + (1 << 20), "prédiction trop coûteuse");
            }
            let step = match t {
                Token::Literal(c) => {
                    ensure!(raw.get(pos) == Some(&c), "données incohérentes");
                    1
                }
                Token::Match { len, .. } => len as usize,
            };
            // The best repetition, then (only if a lazy mode needs it) the best one at
            // the next position, once.
            let (best, dist) = ch.best(raw, pos);
            let mut next = None;
            let preds: Vec<Option<(u16, u16)>> = (0..MODES)
                .map(|m| {
                    want[m as usize].then(|| {
                        choose(m, best, dist, || {
                            *next.get_or_insert_with(|| {
                                ch.insert(raw, pos);
                                let (n, _) = ch.best(raw, pos + 1);
                                ch.remove(raw, pos);
                                n
                            })
                        })
                    })
                })
                .collect();
            let mut cached = None;
            let mut rank = |len: u16, dist: u16| *cached.get_or_insert_with(|| ch.rank(raw, pos, len, dist));
            for (k, pred) in preds.into_iter().enumerate() {
                if let Some(pred) = pred {
                    v[k].note(t, pred, best, &mut rank);
                }
            }
            for i in pos..pos + step {
                ch.insert(raw, i);
            }
            pos += step;
        }
    }
    ensure!(pos == raw.len(), "données incohérentes");
    let out = |k: usize, v: &Variant| {
        let mut out = Vec::with_capacity(structure.len() + v.kinds.len() + v.details.len() + 16);
        put_varint(&mut out, VERSION);
        out.push(k as u8);
        out.extend_from_slice(&[s.tail, s.tail_bits]);
        for part in [&structure, &v.kinds, &v.details] {
            put_varint(&mut out, part.len() as u64);
        }
        out.extend_from_slice(&structure);
        out.extend_from_slice(&v.kinds);
        out.extend_from_slice(&v.details);
        (out, v.exact)
    };
    Ok((0..MODES as usize).map(|k| want[k].then(|| out(k, &v[k]))).collect())
}

/// Replays the predictions and corrections: gives back the decoded stream.
pub fn replay(raw: &[u8], corrections: &[u8]) -> Result<Stream> {
    let mut c = Cursor::new(corrections);
    ensure!(c.varint()? == VERSION, "corrections de version inconnue");
    let mode = c.u8()?;
    ensure!(mode_valid(mode), "corrections invalides");
    let (tail, tail_bits) = (c.u8()?, c.u8()?);
    ensure!(tail_bits < 8 && u32::from(tail) >> tail_bits == 0, "corrections invalides");
    let (ls, lk, ld) = (c.varint()? as usize, c.varint()? as usize, c.varint()? as usize);
    let mut structure = Cursor::new(c.bytes(ls)?);
    let mut kinds = c.bytes(lk)?.iter();
    let mut details = Cursor::new(c.bytes(ld)?);
    ensure!(c.remaining() == 0, "corrections invalides");

    let mut ch = Chains::new(raw.len());
    let mut pos = 0usize;
    let n = structure.varint()? as usize;
    let mut blocks = Vec::with_capacity(n.min(structure.remaining()));
    for k in 0..n {
        let (mut block, count) = get_block(&mut structure, k + 1 == n)?;
        if let BlockKind::Stored { len, data, .. } = &mut block.kind {
            let part = raw.get(pos..pos + *len as usize).ok_or_else(|| anyhow::anyhow!("corrections invalides"))?;
            data.extend_from_slice(part);
            for i in pos..pos + part.len() {
                ch.insert(raw, i);
            }
            pos += part.len();
            blocks.push(block);
            continue;
        }
        block.tokens.reserve(count.min(raw.len() - pos + 1));
        for _ in 0..count {
            ensure!(pos < raw.len(), "corrections invalides");
            let kind = *kinds.next().ok_or_else(|| anyhow::anyhow!("corrections invalides"))?;
            let (pred, best) = predict_at(raw, &mut ch, pos, mode);
            let token = match kind {
                0 if pred.0 == 0 => Token::Literal(raw[pos]),
                0 => Token::Match { len: pred.0, dist: pred.1 },
                1 => Token::Literal(raw[pos]),
                2 => {
                    let delta = details.varint()?;
                    let r = details.varint()? as usize;
                    let len = u64::from(best).checked_sub(delta).filter(|&l| l >= 3).ok_or_else(|| anyhow::anyhow!("corrections invalides"))? as u16;
                    let dist = ch.nth(raw, pos, len, r).ok_or_else(|| anyhow::anyhow!("corrections invalides"))?;
                    Token::Match { len, dist }
                }
                3 => {
                    let len = details.varint()?;
                    let dist = details.varint()?;
                    ensure!((3..=258).contains(&len) && (1..=WINDOW as u64).contains(&dist), "corrections invalides");
                    ensure!(dist as usize <= pos && pos + len as usize <= raw.len(), "corrections invalides");
                    // The repetition must match the data.
                    let (l, d) = (len as usize, dist as usize);
                    ensure!((0..l).all(|i| raw[pos + i] == raw[pos - d + i]), "corrections invalides");
                    Token::Match { len: len as u16, dist: dist as u16 }
                }
                _ => bail!("corrections invalides"),
            };
            let step = match token {
                Token::Literal(_) => 1,
                Token::Match { len, .. } => len as usize,
            };
            ensure!(pos + step <= raw.len(), "corrections invalides");
            for i in pos..pos + step {
                ch.insert(raw, i);
            }
            pos += step;
            block.tokens.push(token);
        }
        blocks.push(block);
    }
    ensure!(pos == raw.len() && kinds.next().is_none() && details.remaining() == 0, "corrections invalides");
    ensure!(structure.remaining() == 0, "corrections invalides");
    Ok(Stream { blocks, tail, tail_bits, len: 0 })
}

/// Structure of a block: kind, number of tokens, lengths 258 as 284, then
/// depending on the kind the padding (stored) or the table (dynamic).
fn put_block(o: &mut Vec<u8>, b: &Block) {
    match &b.kind {
        BlockKind::Stored { pad, pad_bits, len, nlen, .. } => {
            o.push(0);
            o.extend_from_slice(&[*pad, *pad_bits]);
            o.extend_from_slice(&len.to_le_bytes());
            o.extend_from_slice(&nlen.to_le_bytes());
            return;
        }
        BlockKind::Fixed => o.push(1),
        BlockKind::Dynamic(_) => o.push(2),
    }
    put_varint(o, b.tokens.len() as u64);
    put_varint(o, b.long_258.len() as u64);
    let mut last = 0u32;
    for &k in &b.long_258 {
        put_varint(o, u64::from(k - last));
        last = k;
    }
    if let BlockKind::Dynamic(h) = &b.kind {
        put_varint(o, u64::from(h.hlit));
        o.push(h.hdist);
        put_varint(o, h.cl_lens.len() as u64);
        o.extend_from_slice(&h.cl_lens);
        put_varint(o, h.cl_symbols.len() as u64);
        for &(sym, extra) in &h.cl_symbols {
            o.push(sym);
            if sym >= 16 {
                o.push(extra);
            }
        }
    }
}

fn get_block(c: &mut Cursor, last: bool) -> Result<(Block, usize)> {
    let genre = c.u8()?;
    if genre == 0 {
        let (pad, pad_bits) = (c.u8()?, c.u8()?);
        ensure!(pad_bits < 8 && u32::from(pad) >> pad_bits == 0, "corrections invalides");
        let b = c.bytes(4)?;
        let (len, nlen) = (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]]));
        let kind = BlockKind::Stored { pad, pad_bits, len, nlen, data: Vec::new() };
        return Ok((Block { last, kind, tokens: Vec::new(), long_258: Vec::new() }, 0));
    }
    let count = c.varint()? as usize;
    let n258 = c.varint()? as usize;
    let mut long_258 = Vec::with_capacity(n258.min(c.remaining()));
    let mut k = 0u64;
    for i in 0..n258 {
        let step = c.varint()?;
        ensure!(i == 0 || step > 0, "corrections invalides");
        k = k.checked_add(step).filter(|&k| k < count as u64).ok_or_else(|| anyhow::anyhow!("corrections invalides"))?;
        long_258.push(k as u32);
    }
    let kind = match genre {
        1 => BlockKind::Fixed,
        2 => {
            let hlit = u16::try_from(c.varint()?)?;
            let hdist = c.u8()?;
            let ncl = c.varint()? as usize;
            ensure!((257..=286).contains(&hlit) && (1..=30).contains(&hdist) && (4..=19).contains(&ncl), "corrections invalides");
            let cl_lens = c.bytes(ncl)?.to_vec();
            ensure!(cl_lens.iter().all(|&l| l <= 7), "corrections invalides");
            let nsym = c.varint()? as usize;
            let mut cl_symbols = Vec::with_capacity(nsym.min(c.remaining()));
            for _ in 0..nsym {
                let sym = c.u8()?;
                ensure!(sym <= 18, "corrections invalides");
                let extra = if sym >= 16 { c.u8()? } else { 0 };
                let max = [3u8, 7, 127][usize::from(sym.saturating_sub(16))];
                ensure!(sym < 16 || extra <= max, "corrections invalides");
                cl_symbols.push((sym, extra));
            }
            BlockKind::Dynamic(DynamicHeader { hlit, hdist, cl_lens, cl_symbols })
        }
        _ => bail!("corrections invalides"),
    };
    Ok((Block { last, kind, tokens: Vec::new(), long_258 }, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texte() -> Vec<u8> {
        (0..4000).map(|i| format!("<w:p><w:r><w:t>Ligne {} : {}</w:t></w:r></w:p>\n", i, i * 13 % 97)).collect::<String>().into_bytes()
    }

    /// Stream made "like another program": closest repetitions instead of the
    /// longest, lengths 3 ignored.
    fn flux_maison(raw: &[u8]) -> Stream {
        let mut tokens = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            let mut found = None;
            for dist in 1..=pos.min(4096) {
                let mut l = 0;
                while l < 258 && pos + l < raw.len() && raw[pos + l] == raw[pos - dist + l] {
                    l += 1;
                }
                if l >= 4 {
                    found = Some((l, dist));
                    break;
                }
            }
            match found {
                Some((l, d)) => {
                    tokens.push(Token::Match { len: l as u16, dist: d as u16 });
                    pos += l;
                }
                None => {
                    tokens.push(Token::Literal(raw[pos]));
                    pos += 1;
                }
            }
        }
        let mut s = Stream {
            blocks: vec![Block { last: true, kind: BlockKind::Fixed, tokens, long_258: vec![] }],
            tail: 0,
            tail_bits: 0,
            len: 0,
        };
        // End bits to complete the byte.
        for n in 0..8 {
            s.tail_bits = n;
            if crate::deflate::encode(&s).is_ok() {
                break;
            }
        }
        s
    }

    #[test]
    fn rejoue_un_flux_d_un_autre_programme() {
        let raw = texte();
        let s = flux_maison(&raw);
        let z = crate::deflate::encode(&s).unwrap();
        let s = crate::deflate::decode(&z, usize::MAX).unwrap();
        let (corr, exact) = diff(&raw, &s).unwrap();
        assert!(exact > 0);
        let back = replay(&raw, &corr).unwrap();
        assert_eq!(crate::deflate::encode(&back).unwrap(), z);
    }

    #[test]
    fn rejoue_un_flux_zlib() {
        let raw = texte();
        let mut z = Vec::new();
        crate::rebuild::deflate_with(&raw, crate::rebuild::Params { level: 9, mem: 8, wbits: 15, chunk: 0 }, &mut z).unwrap();
        let s = crate::deflate::decode(&z, usize::MAX).unwrap();
        let (corr, _) = diff(&raw, &s).unwrap();
        assert_eq!(crate::deflate::encode(&replay(&raw, &corr).unwrap()).unwrap(), z);
    }

    #[test]
    fn corrections_abimees_sans_plantage() {
        let raw = texte();
        let s = flux_maison(&raw);
        let (corr, _) = diff(&raw, &s).unwrap();
        for i in 0..corr.len().min(300) {
            let mut c = corr.clone();
            c[i] ^= 0x41;
            if let Ok(st) = replay(&raw, &c) {
                let _ = crate::deflate::encode(&st);
            }
        }
        assert!(replay(&raw, &corr[..corr.len() - 1]).is_err());
    }
}
