//! TrueType fonts (stage 6).
//!
//! In a TrueType font, the `glyf` table (the drawing of each character)
//! weighs half the file and compresses badly: each glyph mixes its header,
//! its flags and its X and Y coordinates. Here, these elements are stored in
//! **separate streams** (all flags together, all X coordinates together…),
//! without re-encoding anything: the bytes are those of the file, only their
//! order changes. Two things are recomputed instead of being stored: the
//! bounding box of each simple glyph (when it is exact) and the `loca` table
//! (position of each glyph).
//!
//! Exactness is checked by the writer, which rebuilds the file and compares
//! it with the original (see rebuild::prepare).
//!
//! Transformed form (see SPEC.md):
//! ```text
//! varint version (1)
//! varint position of loca, varint length of loca, u8 long loca (0/1)
//! varint position of glyf, varint length of glyf
//! varint length of the rest, then the rest: the file without loca or glyf
//! then 12 streams, each preceded by its length (varint):
//!   kinds, paddings, contours, boxes, end points, instructions,
//!   flags, X, Y, composite lengths, composites, padding bytes
//! ```

use anyhow::{bail, ensure, Result};

use crate::format::{put_varint, Cursor};

const VERSION: u64 = 1;

/// Empty glyph (same position as the next one in loca).
const EMPTY: u8 = 0;
/// Simple glyph, bounding box recomputed.
const SIMPLE: u8 = 1;
/// Simple glyph, bounding box stored.
const SIMPLE_BOX: u8 = 2;
/// Composite glyph (made of other glyphs): stored as is.
const COMPOSITE: u8 = 3;

/// Point flags (OpenType specification, glyf table).
const REPEAT: u8 = 0x08;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const X_SAME: u8 = 0x10;
const Y_SAME: u8 = 0x20;

#[derive(Default)]
struct Streams {
    kinds: Vec<u8>,
    pads: Vec<u8>,
    contours: Vec<u8>,
    boxes: Vec<u8>,
    ends: Vec<u8>,
    instr: Vec<u8>,
    flags: Vec<u8>,
    xs: Vec<u8>,
    ys: Vec<u8>,
    comp_lens: Vec<u8>,
    comps: Vec<u8>,
    pad_bytes: Vec<u8>,
}

impl Streams {
    fn all(&self) -> [&Vec<u8>; 12] {
        [
            &self.kinds,
            &self.pads,
            &self.contours,
            &self.boxes,
            &self.ends,
            &self.instr,
            &self.flags,
            &self.xs,
            &self.ys,
            &self.comp_lens,
            &self.comps,
            &self.pad_bytes,
        ]
    }
}

fn be16(d: &[u8], o: usize) -> Option<u16> {
    d.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn be32(d: &[u8], o: usize) -> Option<u32> {
    d.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// A TrueType font (quadratic contours, glyf table)?
pub fn is_truetype(d: &[u8]) -> bool {
    d.starts_with(&[0, 1, 0, 0]) || d.starts_with(b"true")
}

/// Position and length of a table according to the directory.
fn table(d: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let n = usize::from(be16(d, 4)?);
    (0..n).find_map(|i| {
        let e = 12 + 16 * i;
        (d.get(e..e + 4)? == tag).then(|| Some((be32(d, e + 8)? as usize, be32(d, e + 12)? as usize)))?
    })
}

/// Flags of a simple glyph with `npts` points, read from `p`: returns the
/// flags (one per point) and the position after them.
fn read_flags(g: &[u8], mut p: usize, npts: usize) -> Option<(Vec<u8>, usize)> {
    let mut fl = Vec::with_capacity(npts);
    while fl.len() < npts {
        let f = *g.get(p)?;
        p += 1;
        fl.push(f);
        if f & REPEAT != 0 {
            let r = *g.get(p)?;
            p += 1;
            fl.extend(std::iter::repeat(f).take(usize::from(r)));
        }
    }
    fl.truncate(npts);
    Some((fl, p))
}

/// Coordinates of one axis: bytes read from `p` and absolute values.
fn read_coords(g: &[u8], mut p: usize, fl: &[u8], short: u8, same: u8) -> Option<(Vec<i32>, usize)> {
    let mut v = 0i32;
    let mut out = Vec::with_capacity(fl.len());
    for &f in fl {
        if f & short != 0 {
            let d = i32::from(*g.get(p)?);
            p += 1;
            v += if f & same != 0 { d } else { -d };
        } else if f & same == 0 {
            v += i32::from(be16(g, p)? as i16);
            p += 2;
        }
        out.push(v);
    }
    Some((out, p))
}

fn bbox(xs: &[i32], ys: &[i32]) -> Option<[u8; 8]> {
    let fit = |v: i32| i16::try_from(v).ok().map(i16::to_be_bytes);
    let (x0, x1) = (fit(*xs.iter().min()?)?, fit(*xs.iter().max()?)?);
    let (y0, y1) = (fit(*ys.iter().min()?)?, fit(*ys.iter().max()?)?);
    let mut b = [0; 8];
    b[..2].copy_from_slice(&x0);
    b[2..4].copy_from_slice(&y0);
    b[4..6].copy_from_slice(&x1);
    b[6..].copy_from_slice(&y1);
    Some(b)
}

/// Transformed form of a TrueType font; None if the font has no glyf table
/// or if its structure is out of the ordinary.
pub fn encode(d: &[u8]) -> Option<Vec<u8>> {
    if !is_truetype(d) {
        return None;
    }
    let (go, gl) = table(d, b"glyf")?;
    let (lo, ll) = table(d, b"loca")?;
    let (ho, _) = table(d, b"head")?;
    let long = match be16(d, ho + 50)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    // Tables inside the file and non-overlapping.
    if go.checked_add(gl)? > d.len() || lo.checked_add(ll)? > d.len() || (lo < go + gl && go < lo + ll) {
        return None;
    }
    let size = if long { 4 } else { 2 };
    if ll % size != 0 || ll < 2 * size {
        return None;
    }
    let loca: Vec<usize> = (0..ll / size)
        .map(|i| if long { be32(d, lo + 4 * i).unwrap() as usize } else { 2 * usize::from(be16(d, lo + 2 * i).unwrap()) })
        .collect();
    if loca[0] != 0 || loca.windows(2).any(|w| w[1] < w[0]) || *loca.last()? > gl {
        return None;
    }
    let g = &d[go..go + gl];
    let mut s = Streams::default();
    for w in loca.windows(2) {
        let (a, b) = (w[0], w[1]);
        let glyph = &g[a..b];
        if glyph.is_empty() {
            s.kinds.push(EMPTY);
            continue;
        }
        if glyph.len() < 10 {
            return None;
        }
        let nc = be16(glyph, 0)? as i16;
        s.contours.extend_from_slice(&glyph[..2]);
        if nc < 0 {
            s.kinds.push(COMPOSITE);
            put_varint(&mut s.comp_lens, (glyph.len() - 2) as u64);
            s.comps.extend_from_slice(&glyph[2..]);
            continue;
        }
        let nc = nc as usize;
        let mut p = 10;
        let ends = glyph.get(p..p + 2 * nc)?;
        p += 2 * nc;
        let npts = if nc == 0 { 0 } else { usize::from(be16(ends, 2 * nc - 2)?) + 1 };
        let il = usize::from(be16(glyph, p)?);
        let instr = glyph.get(p..p + 2 + il)?;
        p += 2 + il;
        let (fl, q) = read_flags(glyph, p, npts)?;
        let flags = &glyph[p..q];
        let (xs, qx) = read_coords(glyph, q, &fl, X_SHORT, X_SAME)?;
        let (ys, qy) = read_coords(glyph, qx, &fl, Y_SHORT, Y_SAME)?;
        if bbox(&xs, &ys).as_ref().map(|b| &b[..]) == Some(&glyph[2..10]) {
            s.kinds.push(SIMPLE);
        } else {
            s.kinds.push(SIMPLE_BOX);
            s.boxes.extend_from_slice(&glyph[2..10]);
        }
        s.ends.extend_from_slice(ends);
        s.instr.extend_from_slice(instr);
        s.flags.extend_from_slice(flags);
        s.xs.extend_from_slice(&glyph[q..qx]);
        s.ys.extend_from_slice(&glyph[qx..qy]);
        put_varint(&mut s.pads, (glyph.len() - qy) as u64);
        s.pad_bytes.extend_from_slice(&glyph[qy..]);
    }
    // End of glyf after the last glyph.
    s.pad_bytes.extend_from_slice(&g[*loca.last()?..]);

    let mut out = Vec::with_capacity(d.len());
    put_varint(&mut out, VERSION);
    for v in [lo, ll, usize::from(long), go, gl] {
        put_varint(&mut out, v as u64);
    }
    let (first, second) = if lo < go { ((lo, ll), (go, gl)) } else { ((go, gl), (lo, ll)) };
    let rest = [&d[..first.0], &d[first.0 + first.1..second.0], &d[second.0 + second.1..]];
    put_varint(&mut out, rest.iter().map(|r| r.len() as u64).sum());
    rest.iter().for_each(|r| out.extend_from_slice(r));
    for st in s.all() {
        put_varint(&mut out, st.len() as u64);
        out.extend_from_slice(st);
    }
    Some(out)
}

/// Rebuilds the original font from its transformed form, never exceeding
/// `max` bytes.
pub fn decode(t: &[u8], max: u64) -> Result<Vec<u8>> {
    let mut c = Cursor::new(t);
    ensure!(c.varint()? == VERSION, "police : version inconnue");
    let mut v = [0usize; 5];
    for x in &mut v {
        *x = usize::try_from(c.varint()?)?;
    }
    let [lo, ll, long, go, gl] = v;
    ensure!(long <= 1, "police invalide");
    let total = ll.checked_add(gl).and_then(|n| n.checked_add(usize::try_from(c.varint().ok()?).ok()?));
    let total = total.ok_or_else(|| anyhow::anyhow!("police invalide"))?;
    ensure!(total as u64 <= max, "fichier reconstruit trop long");
    let rest_len = total - ll - gl;
    let rest = c.bytes(rest_len)?;
    let mut st: Vec<Cursor> = Vec::with_capacity(12);
    for _ in 0..12 {
        let n = usize::try_from(c.varint()?)?;
        st.push(Cursor::new(c.bytes(n)?));
    }
    ensure!(c.remaining() == 0, "police : données en trop");
    let [kinds, pads, contours, boxes, ends, instr, flags, xs, ys, comp_lens, comps, pad_bytes] = &mut st[..] else {
        unreachable!()
    };

    let size = if long == 1 { 4 } else { 2 };
    ensure!(ll % size == 0 && ll >= 2 * size, "police invalide");
    let n = ll / size - 1;
    let mut g = Vec::with_capacity(gl);
    let mut loca = Vec::with_capacity(ll);
    let put_loca = |loca: &mut Vec<u8>, off: usize| -> Result<()> {
        if long == 1 {
            loca.extend_from_slice(&u32::try_from(off)?.to_be_bytes());
        } else {
            ensure!(off % 2 == 0, "police invalide");
            loca.extend_from_slice(&u16::try_from(off / 2)?.to_be_bytes());
        }
        Ok(())
    };
    for _ in 0..n {
        put_loca(&mut loca, g.len())?;
        ensure!(g.len() <= gl, "police invalide");
        let kind = kinds.u8()?;
        match kind {
            EMPTY => continue,
            COMPOSITE => {
                g.extend_from_slice(contours.bytes(2)?);
                let len = usize::try_from(comp_lens.varint()?)?;
                ensure!(len <= gl, "police invalide");
                g.extend_from_slice(comps.bytes(len)?);
                continue;
            }
            SIMPLE | SIMPLE_BOX => {}
            _ => bail!("police : genre de glyphe inconnu"),
        }
        let ncb = contours.bytes(2)?;
        let nc = i16::from_be_bytes([ncb[0], ncb[1]]);
        ensure!(nc >= 0, "police invalide");
        let nc = nc as usize;
        let e = ends.bytes(2 * nc)?;
        let npts = if nc == 0 { 0 } else { usize::from(be16(e, 2 * nc - 2).unwrap()) + 1 };
        let ilb = instr.bytes(2)?;
        let il = usize::from(u16::from_be_bytes([ilb[0], ilb[1]]));
        let ins = instr.bytes(il)?;
        // Flags: read from their stream, with the same rule as the encoder.
        let mut fl = Vec::with_capacity(npts);
        let mut fbytes = Vec::new();
        while fl.len() < npts {
            let f = flags.u8()?;
            fbytes.push(f);
            fl.push(f);
            if f & REPEAT != 0 {
                let r = flags.u8()?;
                fbytes.push(r);
                fl.extend(std::iter::repeat(f).take(usize::from(r)));
            }
        }
        fl.truncate(npts);
        let axis = |cur: &mut Cursor, short: u8, same: u8| -> Result<(Vec<u8>, Vec<i32>)> {
            let (mut bytes, mut vals, mut v) = (Vec::new(), Vec::with_capacity(fl.len()), 0i32);
            for &f in &fl {
                if f & short != 0 {
                    let d = cur.u8()?;
                    bytes.push(d);
                    v += if f & same != 0 { i32::from(d) } else { -i32::from(d) };
                } else if f & same == 0 {
                    let b = cur.bytes(2)?;
                    bytes.extend_from_slice(b);
                    v += i32::from(i16::from_be_bytes([b[0], b[1]]));
                }
                vals.push(v);
            }
            Ok((bytes, vals))
        };
        let (xb, xv) = axis(xs, X_SHORT, X_SAME)?;
        let (yb, yv) = axis(ys, Y_SHORT, Y_SAME)?;
        g.extend_from_slice(ncb);
        if kind == SIMPLE {
            g.extend_from_slice(&bbox(&xv, &yv).ok_or_else(|| anyhow::anyhow!("police invalide"))?);
        } else {
            g.extend_from_slice(boxes.bytes(8)?);
        }
        g.extend_from_slice(e);
        g.extend_from_slice(&ilb);
        g.extend_from_slice(ins);
        g.extend_from_slice(&fbytes);
        g.extend_from_slice(&xb);
        g.extend_from_slice(&yb);
        let pad = usize::try_from(pads.varint()?)?;
        g.extend_from_slice(pad_bytes.bytes(pad)?);
        ensure!(g.len() <= gl, "police invalide");
    }
    put_loca(&mut loca, g.len())?;
    g.extend_from_slice(pad_bytes.bytes(gl.checked_sub(g.len()).ok_or_else(|| anyhow::anyhow!("police invalide"))?)?);
    ensure!(st.iter().all(|s| s.remaining() == 0), "police : données en trop");
    ensure!(loca.len() == ll && g.len() == gl, "police invalide");

    // The rest, with loca and glyf put back in place.
    let (first, second) = if lo < go { ((lo, &loca), (go, &g)) } else { ((go, &g), (lo, &loca)) };
    let mut out = Vec::with_capacity(total);
    let mut r = rest;
    let take = |r: &mut &[u8], n: usize| -> Result<Vec<u8>> {
        ensure!(n <= r.len(), "police invalide");
        let (a, b) = r.split_at(n);
        *r = b;
        Ok(a.to_vec())
    };
    out.extend(take(&mut r, first.0)?);
    out.extend_from_slice(first.1);
    let gap = second.0.checked_sub(out.len()).ok_or_else(|| anyhow::anyhow!("police invalide"))?;
    out.extend(take(&mut r, gap)?);
    out.extend_from_slice(second.1);
    out.extend_from_slice(r);
    ensure!(out.len() == total, "police invalide");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small font: head, short loca, glyf with an empty glyph, a square, a glyph
    /// with a wrong box and a composite, then some other table.
    fn police() -> Vec<u8> {
        let mut carre = Vec::new();
        carre.extend_from_slice(&1i16.to_be_bytes());
        for v in [10i16, 0, 500, 700] {
            carre.extend_from_slice(&v.to_be_bytes());
        }
        carre.extend_from_slice(&3u16.to_be_bytes()); // 4 points
        carre.extend_from_slice(&[0, 2, 0xAB, 0xCD]); // 2 bytes of instructions
        carre.extend_from_slice(&[0x01, 0x09, 2]); // flags: 1, then 0x09 three times
        for x in [10i16, 490, 0, -490] {
            carre.extend_from_slice(&x.to_be_bytes());
        }
        for y in [0i16, 0, 700, 0] {
            carre.extend_from_slice(&y.to_be_bytes());
        }
        carre.push(0); // padding
        let mut faux = carre.clone();
        faux[2] = 0x7F; // box that does not match the points
        let compose = [0xFF, 0xFF, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x22, 0, 1, 5, 5];
        let glyphs: Vec<&[u8]> = vec![&[], &carre, &faux, &compose];
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        for gl in &glyphs {
            loca.extend_from_slice(&((glyf.len() / 2) as u16).to_be_bytes());
            glyf.extend_from_slice(gl);
        }
        loca.extend_from_slice(&((glyf.len() / 2) as u16).to_be_bytes());
        glyf.extend_from_slice(&[0, 0]); // end of table after the last glyph
        let mut head = vec![0u8; 54];
        head[50..52].copy_from_slice(&0u16.to_be_bytes());
        let tables: [(&[u8; 4], Vec<u8>); 4] = [(b"glyf", glyf), (b"head", head), (b"loca", loca), (b"name", b"CN Technology".to_vec())];
        let mut d = vec![0, 1, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0];
        let mut off = 12 + 16 * tables.len();
        let mut corps = Vec::new();
        for (tag, t) in &tables {
            d.extend_from_slice(*tag);
            d.extend_from_slice(&[0; 4]);
            d.extend_from_slice(&(off as u32).to_be_bytes());
            d.extend_from_slice(&(t.len() as u32).to_be_bytes());
            corps.extend_from_slice(t);
            while corps.len() % 4 != 0 {
                corps.push(0);
            }
            off = 12 + 16 * tables.len() + corps.len();
        }
        d.extend(corps);
        d
    }

    #[test]
    fn aller_retour() {
        let d = police();
        let t = encode(&d).unwrap();
        assert_eq!(decode(&t, d.len() as u64).unwrap(), d);
        assert!(decode(&t, d.len() as u64 - 1).is_err());
    }

    #[test]
    fn forme_abimee_sans_plantage() {
        let d = police();
        let t = encode(&d).unwrap();
        for i in 0..t.len() {
            for v in [0u8, 1, 0x7F, 0xFF] {
                let mut x = t.clone();
                x[i] = v;
                let _ = decode(&x, 1 << 20);
            }
        }
        for n in 0..t.len() {
            let _ = decode(&t[..n], 1 << 20);
        }
    }
}
