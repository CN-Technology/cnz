//! Exact Deflate reader and rewriter (stage 4b, step 1).
//!
//! A Deflate stream (RFC 1951) is decoded into **tokens**: literals,
//! repetitions (length, distance) and end-of-block markers, with everything
//! that describes the blocks (type, Huffman tables as they are written).
//! Rewriting rebuilds the stream **bit for bit**: everything a program may
//! choose freely is recorded (padding bits, coding of length 258, coding of
//! the tables).
//!
//! This is the basis for rebuilding streams written by programs other than
//! zlib: their tokens can be rebuilt from the decompressed data, recording
//! only the differences.

use anyhow::{bail, ensure, Result};

/// A token of a compressed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Literal(u8),
    /// Repetition: `len` bytes (3 to 258) taken `dist` bytes back.
    Match { len: u16, dist: u16 },
}

/// Huffman table of a dynamic block, as it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicHeader {
    /// Number of literal/length codes (257 to 286) and of distance codes (1 to 32).
    pub hlit: u16,
    pub hdist: u8,
    /// Code lengths of the code-length code, in writing order (4 to 19 values).
    pub cl_lens: Vec<u8>,
    /// Written sequence of code-length symbols (0 to 18) and their extra bits.
    pub cl_symbols: Vec<(u8, u8)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// Stored block: the padding bits up to the byte boundary (often zero, not
    /// always), the LEN and NLEN bytes as they are, then the data.
    Stored { pad: u8, pad_bits: u8, len: u16, nlen: u16, data: Vec<u8> },
    Fixed,
    Dynamic(DynamicHeader),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub last: bool,
    pub kind: BlockKind,
    /// Tokens (empty for a stored block).
    pub tokens: Vec<Token>,
    /// Positions (in `tokens`) of the lengths 258 written with code 284 and 31
    /// extra bits, instead of the usual code 285.
    pub long_258: Vec<u32>,
}

/// A decoded stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub blocks: Vec<Block>,
    /// Remaining bits of the last byte after the last block.
    pub tail: u8,
    pub tail_bits: u8,
    /// Bytes read.
    pub len: usize,
}

// ---------------------------------------------------------------- bits

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0, nbits: 0 }
    }

    fn need(&mut self, n: u32) -> Result<()> {
        while self.nbits < n {
            let Some(&b) = self.data.get(self.pos) else { bail!("flux Deflate tronqué") };
            self.bit |= u32::from(b) << self.nbits;
            self.pos += 1;
            self.nbits += 8;
        }
        Ok(())
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.bit & ((1u32 << n) - 1);
        self.bit >>= n;
        self.nbits -= n;
        Ok(v)
    }

    /// Remaining bits of the current byte (value, count).
    fn rest_of_byte(&mut self) -> (u8, u8) {
        let n = self.nbits % 8;
        let v = (self.bit & ((1 << n) - 1)) as u8;
        self.bit >>= n;
        self.nbits -= n;
        (v, n as u8)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.bits(8)? as u8)
    }
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bit: u64,
    nbits: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        self.bit |= u64::from(v) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.bit as u8);
            self.bit >>= 8;
            self.nbits -= 8;
        }
    }

    /// Huffman code: written from the most significant bit to the least significant one.
    fn code(&mut self, c: Code) {
        self.put(u32::from(c.rev), u32::from(c.len));
    }

    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.bit as u8);
        }
        self.out
    }
}

// ---------------------------------------------------------------- Huffman

/// Canonical code of a symbol, bits already reversed for writing.
#[derive(Clone, Copy, Default, Debug)]
struct Code {
    rev: u16,
    len: u8,
}

/// Canonical codes from the lengths (RFC 1951, 3.2.2).
fn codes(lens: &[u8]) -> Vec<Code> {
    let mut count = [0u16; 16];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u16; 16];
    let mut code = 0u16;
    for b in 1..16 {
        code = (code + count[b - 1]) << 1;
        next[b] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                return Code::default();
            }
            let c = next[l as usize];
            next[l as usize] += 1;
            Code { rev: c.reverse_bits() >> (16 - l), len: l }
        })
        .collect()
}

/// Decoder: table indexed by the next 15 bits (reversed).
struct Decoder {
    /// For each `max`-bit pattern: (symbol, code length), length 0 = invalid.
    table: Vec<(u16, u8)>,
    max: u32,
}

impl Decoder {
    fn new(lens: &[u8]) -> Result<Self> {
        let max = u32::from(*lens.iter().max().unwrap_or(&0)).max(1);
        ensure!(max <= 15, "code de Huffman trop long");
        // Check: the code must not be over-subscribed.
        let mut left: i32 = 1;
        let mut count = [0i32; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        for c in &count[1..] {
            left = left * 2 - c;
            ensure!(left >= 0, "code de Huffman invalide");
        }
        let mut table = vec![(0u16, 0u8); 1 << max];
        for (sym, c) in codes(lens).iter().enumerate() {
            if c.len == 0 {
                continue;
            }
            let step = 1usize << c.len;
            let mut i = c.rev as usize;
            while i < table.len() {
                table[i] = (sym as u16, c.len);
                i += step;
            }
        }
        Ok(Self { table, max })
    }

    fn decode(&self, r: &mut BitReader) -> Result<u16> {
        // Near the end of the stream, fewer than `max` bits may remain.
        let avail = {
            let _ = r.need(self.max);
            r.nbits.min(self.max)
        };
        let idx = (r.bit & ((1 << self.max) - 1)) as usize;
        let (sym, len) = self.table[idx];
        ensure!(len != 0 && u32::from(len) <= avail, "code de Huffman inconnu");
        r.bit >>= len;
        r.nbits -= u32::from(len);
        Ok(sym)
    }
}

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// Writing order of the code-length code lengths.
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn fixed_lens() -> (Vec<u8>, Vec<u8>) {
    let mut lit = vec![8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    (lit, vec![5u8; 30])
}

/// Code and extra bits of a length (code 285 for 258).
fn len_code(len: u16) -> (usize, u16) {
    let i = LEN_BASE.partition_point(|&b| b <= len) - 1;
    (i, len - LEN_BASE[i])
}

fn dist_code(dist: u16) -> (usize, u16) {
    let i = DIST_BASE.partition_point(|&b| b <= dist) - 1;
    (i, dist - DIST_BASE[i])
}

// ---------------------------------------------------------------- reading

/// Decodes a raw Deflate stream. `max_out` bounds the decompressed size.
pub fn decode(data: &[u8], max_out: usize) -> Result<Stream> {
    let mut r = BitReader::new(data);
    let mut blocks = Vec::new();
    let mut out_len = 0usize;
    loop {
        let last = r.bits(1)? == 1;
        let btype = r.bits(2)?;
        let block = match btype {
            0 => {
                let (pad, pad_bits) = r.rest_of_byte();
                let len = u16::from_le_bytes([r.byte()?, r.byte()?]);
                let nlen = u16::from_le_bytes([r.byte()?, r.byte()?]);
                ensure!(len == !nlen, "bloc stocké invalide");
                let mut data = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    data.push(r.byte()?);
                }
                out_len += len as usize;
                Block { last, kind: BlockKind::Stored { pad, pad_bits, len, nlen, data }, tokens: Vec::new(), long_258: Vec::new() }
            }
            1 => {
                let (lit, dist) = fixed_lens();
                let (tokens, long_258) = read_tokens(&mut r, &Decoder::new(&lit)?, &Decoder::new(&dist)?, &mut out_len, max_out)?;
                Block { last, kind: BlockKind::Fixed, tokens, long_258 }
            }
            2 => {
                let (header, lit, dist) = read_dynamic_header(&mut r)?;
                let (tokens, long_258) = read_tokens(&mut r, &Decoder::new(&lit)?, &Decoder::new(&dist)?, &mut out_len, max_out)?;
                Block { last, kind: BlockKind::Dynamic(header), tokens, long_258 }
            }
            _ => bail!("type de bloc Deflate invalide"),
        };
        ensure!(out_len <= max_out, "flux Deflate trop long");
        blocks.push(block);
        if last {
            break;
        }
    }
    let (tail, tail_bits) = r.rest_of_byte();
    // Bytes loaded ahead for decoding are not part of the stream.
    let len = r.pos - (r.nbits / 8) as usize;
    Ok(Stream { blocks, tail, tail_bits, len })
}

fn read_dynamic_header(r: &mut BitReader) -> Result<(DynamicHeader, Vec<u8>, Vec<u8>)> {
    let hlit = r.bits(5)? as u16 + 257;
    let hdist = r.bits(5)? as u8 + 1;
    let hclen = r.bits(4)? as usize + 4;
    ensure!(hlit <= 286 && hdist <= 30, "table de Huffman invalide");
    let mut cl_lens = Vec::with_capacity(hclen);
    let mut cl = [0u8; 19];
    for &i in &CL_ORDER[..hclen] {
        let l = r.bits(3)? as u8;
        cl_lens.push(l);
        cl[i] = l;
    }
    let cl_dec = Decoder::new(&cl)?;
    let total = hlit as usize + hdist as usize;
    let mut lens = Vec::with_capacity(total);
    let mut cl_symbols = Vec::new();
    while lens.len() < total {
        let sym = cl_dec.decode(r)? as u8;
        let (extra, rep, val) = match sym {
            0..=15 => (0, 1, sym),
            16 => {
                let Some(&prev) = lens.last() else { bail!("table de Huffman invalide") };
                let e = r.bits(2)? as u8;
                (e, 3 + e as usize, prev)
            }
            17 => {
                let e = r.bits(3)? as u8;
                (e, 3 + e as usize, 0)
            }
            _ => {
                let e = r.bits(7)? as u8;
                (e, 11 + e as usize, 0)
            }
        };
        ensure!(lens.len() + rep <= total, "table de Huffman invalide");
        lens.extend(std::iter::repeat(val).take(rep));
        cl_symbols.push((sym, extra));
    }
    let lit = lens[..hlit as usize].to_vec();
    let dist = lens[hlit as usize..].to_vec();
    ensure!(lit[256] != 0, "table sans fin de bloc");
    Ok((DynamicHeader { hlit, hdist, cl_lens, cl_symbols }, lit, dist))
}

fn read_tokens(
    r: &mut BitReader,
    lit: &Decoder,
    dist: &Decoder,
    out_len: &mut usize,
    max_out: usize,
) -> Result<(Vec<Token>, Vec<u32>)> {
    let mut tokens = Vec::new();
    let mut long_258 = Vec::new();
    loop {
        let sym = lit.decode(r)?;
        match sym {
            0..=255 => {
                tokens.push(Token::Literal(sym as u8));
                *out_len += 1;
            }
            256 => return Ok((tokens, long_258)),
            257..=285 => {
                let i = (sym - 257) as usize;
                let len = LEN_BASE[i] + r.bits(u32::from(LEN_EXTRA[i]))? as u16;
                if i == 27 && len == 258 {
                    long_258.push(tokens.len() as u32);
                }
                let d = dist.decode(r)? as usize;
                ensure!(d < 30, "distance invalide");
                let dv = DIST_BASE[d] + r.bits(u32::from(DIST_EXTRA[d]))? as u16;
                ensure!(dv as usize <= *out_len, "distance avant le début du flux");
                tokens.push(Token::Match { len, dist: dv });
                *out_len += len as usize;
            }
            _ => bail!("symbole de longueur invalide"),
        }
        ensure!(*out_len <= max_out, "flux Deflate trop long");
    }
}

// ---------------------------------------------------------------- writing

/// Rewrites a decoded stream, bit for bit.
pub fn encode(s: &Stream) -> Result<Vec<u8>> {
    let mut w = BitWriter::default();
    for b in &s.blocks {
        w.put(u32::from(b.last), 1);
        match &b.kind {
            BlockKind::Stored { pad, pad_bits, len, nlen, data } => {
                w.put(0, 2);
                ensure!((w.nbits % 8) as u8 == (8 - *pad_bits) % 8, "bourrage incohérent");
                w.put(u32::from(*pad), u32::from(*pad_bits));
                for b in len.to_le_bytes().iter().chain(&nlen.to_le_bytes()).chain(data) {
                    w.put(u32::from(*b), 8);
                }
            }
            BlockKind::Fixed => {
                w.put(1, 2);
                let (lit, dist) = fixed_lens();
                write_tokens(&mut w, b, &codes(&lit), &codes(&dist))?;
            }
            BlockKind::Dynamic(h) => {
                w.put(2, 2);
                let (lit, dist) = write_dynamic_header(&mut w, h)?;
                write_tokens(&mut w, b, &codes(&lit), &codes(&dist))?;
            }
        }
    }
    w.put(u32::from(s.tail), u32::from(s.tail_bits));
    ensure!(w.nbits == 0, "fin de flux incohérente");
    Ok(w.finish())
}

fn write_dynamic_header(w: &mut BitWriter, h: &DynamicHeader) -> Result<(Vec<u8>, Vec<u8>)> {
    w.put(u32::from(h.hlit - 257), 5);
    w.put(u32::from(h.hdist - 1), 5);
    w.put(h.cl_lens.len() as u32 - 4, 4);
    let mut cl = [0u8; 19];
    for (k, &l) in h.cl_lens.iter().enumerate() {
        w.put(u32::from(l), 3);
        cl[CL_ORDER[k]] = l;
    }
    let cl_codes = codes(&cl);
    let mut lens = Vec::new();
    for &(sym, extra) in &h.cl_symbols {
        w.code(cl_codes[sym as usize]);
        match sym {
            0..=15 => lens.push(sym),
            16 => {
                w.put(u32::from(extra), 2);
                let prev = *lens.last().unwrap_or(&0);
                lens.extend(std::iter::repeat(prev).take(3 + extra as usize));
            }
            17 => {
                w.put(u32::from(extra), 3);
                lens.extend(std::iter::repeat(0).take(3 + extra as usize));
            }
            _ => {
                w.put(u32::from(extra), 7);
                lens.extend(std::iter::repeat(0).take(11 + extra as usize));
            }
        }
    }
    let hlit = h.hlit as usize;
    ensure!(lens.len() == hlit + h.hdist as usize, "table incohérente");
    Ok((lens[..hlit].to_vec(), lens[hlit..].to_vec()))
}

fn write_tokens(w: &mut BitWriter, b: &Block, lit: &[Code], dist: &[Code]) -> Result<()> {
    // A symbol missing from the table cannot be written.
    let get = |table: &[Code], i: usize| match table.get(i) {
        Some(&c) if c.len > 0 => Ok(c),
        _ => bail!("symbole absent de la table de Huffman"),
    };
    let mut long = b.long_258.iter().peekable();
    for (k, t) in b.tokens.iter().enumerate() {
        match *t {
            Token::Literal(c) => w.code(get(lit, c as usize)?),
            Token::Match { len, dist: d } => {
                ensure!((3..=258).contains(&len) && d >= 1 && d <= 32768, "répétition invalide");
                let (mut i, mut extra) = len_code(len);
                if long.peek() == Some(&&(k as u32)) {
                    long.next();
                    ensure!(len == 258, "longueur 258 attendue");
                    (i, extra) = (27, 31);
                }
                w.code(get(lit, 257 + i)?);
                w.put(u32::from(extra), u32::from(LEN_EXTRA[i]));
                let (j, e) = dist_code(d);
                w.code(get(dist, j)?);
                w.put(u32::from(e), u32::from(DIST_EXTRA[j]));
            }
        }
    }
    ensure!(long.next().is_none(), "longueur 258 hors des jetons");
    w.code(get(lit, 256)?);
    Ok(())
}

/// Decompressed data from the tokens.
pub fn inflate(s: &Stream) -> Vec<u8> {
    let mut out = Vec::new();
    for b in &s.blocks {
        if let BlockKind::Stored { data, .. } = &b.kind {
            out.extend_from_slice(data);
        }
        for t in &b.tokens {
            match *t {
                Token::Literal(c) => out.push(c),
                Token::Match { len, dist } => {
                    let start = out.len() - dist as usize;
                    for k in 0..len as usize {
                        out.push(out[start + k]);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texte() -> Vec<u8> {
        (0..5000).map(|i| format!("ligne {} : bonjour {}\n", i, i % 17)).collect::<String>().into_bytes()
    }

    #[test]
    fn aller_retour_exact_zlib() {
        let raw = texte();
        for level in [1, 6, 9] {
            let p = crate::rebuild::Params { level, mem: 8, wbits: 15, chunk: 0 };
            let mut z = Vec::new();
            crate::rebuild::deflate_with(&raw, p, &mut z).unwrap();
            let s = decode(&z, usize::MAX).unwrap();
            assert_eq!(s.len, z.len());
            assert_eq!(inflate(&s), raw);
            assert_eq!(encode(&s).unwrap(), z);
        }
    }

    #[test]
    fn bloc_stocke_et_bourrage() {
        // Final stored block with non-zero padding bits (0b10101).
        let mut z = vec![0b1010_1001u8];
        z.extend_from_slice(&[3, 0, !3, 0xFF, b'a', b'b', b'c']);
        let s = decode(&z, usize::MAX).unwrap();
        assert_eq!(inflate(&s), b"abc");
        assert_eq!(encode(&s).unwrap(), z);
    }

    #[test]
    fn longueur_258_codee_en_284() {
        // Fixed block: 'a', then 258 bytes at distance 1 coded as 284 + 31.
        let s = Stream {
            blocks: vec![Block {
                last: true,
                kind: BlockKind::Fixed,
                tokens: vec![Token::Literal(b'a'), Token::Match { len: 258, dist: 1 }],
                long_258: vec![1],
            }],
            // 3 + 8 + 8 + 5 + 5 + 7 = 36 bits: 4 bits to finish the byte.
            tail: 0,
            tail_bits: 4,
            len: 5,
        };
        let z = encode(&s).unwrap();
        assert_eq!(decode(&z, usize::MAX).unwrap(), s);
        assert_eq!(inflate(&s), vec![b'a'; 259]);
    }

    #[test]
    fn flux_abimes_sans_plantage() {
        let raw = texte();
        let mut z = Vec::new();
        crate::rebuild::deflate_with(&raw, crate::rebuild::Params { level: 6, mem: 8, wbits: 15, chunk: 0 }, &mut z).unwrap();
        for i in 0..z.len().min(400) {
            let mut y = z.clone();
            y[i] ^= 0x24;
            if let Ok(s) = decode(&y, 1 << 20) {
                assert_eq!(encode(&s).unwrap(), y[..s.len]);
            }
        }
    }
}
