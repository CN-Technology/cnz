//! Image model for PNGs (stage 7, first in-house model).
//!
//! The decompressed contents of a PNG are made of rows, each preceded by its
//! **filter** (none, left, up, average, Paeth). The filter of each row is
//! recorded, the real pixels are recovered, and they are coded with the
//! arithmetic coder (arith.rs): each value is predicted from its neighbours
//! (JPEG-LS median predictor: left, up, diagonal), and only the difference
//! is coded, with a context that measures the local "activity" (flat or
//! detailed area). In colour, green is coded first; red and blue benefit
//! from the error made on green. Palette images code the colour index,
//! first checking whether it equals the left or upper one. When decoding,
//! the same filters are applied again: the decompressed contents become
//! identical again, byte for byte.
//!
//! Supported: 8 bits per value, non-interlaced, grey, colour, grey or colour
//! with transparency, palette.

use anyhow::{bail, ensure, Result};

use crate::arith::{Coder, Decoder, Encoder, Prob};

/// What we need to know about the image (IHDR header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// PNG colour type: 0 grey, 2 colour, 3 palette, 4 grey + alpha,
    /// 6 colour + alpha.
    pub color: u8,
}

impl Image {
    /// Reads the IHDR header at the start of a PNG; None if the image is not
    /// supported.
    pub fn from_png(d: &[u8]) -> Option<Image> {
        let ihdr = d.get(8..33)?;
        if &ihdr[4..8] != b"IHDR" || u32::from_be_bytes(ihdr[0..4].try_into().ok()?) != 13 {
            return None;
        }
        let width = u32::from_be_bytes(ihdr[8..12].try_into().ok()?);
        let height = u32::from_be_bytes(ihdr[12..16].try_into().ok()?);
        let (depth, color, interlace) = (ihdr[16], ihdr[17], ihdr[20]);
        let img = Image { width, height, color };
        (depth == 8 && interlace == 0 && matches!(color, 0 | 2 | 3 | 4 | 6) && width > 0 && height > 0).then_some(img)
    }

    pub fn channels(self) -> usize {
        match self.color {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4,
        }
    }

    fn row_bytes(self) -> usize {
        self.width as usize * self.channels()
    }

    /// Expected size of the decompressed contents: each row and its filter.
    pub fn raw_len(self) -> Option<usize> {
        (self.row_bytes() + 1).checked_mul(self.height as usize)
    }

    /// Compact header, for the transformed form.
    pub fn encode(self) -> [u8; 9] {
        let mut o = [0u8; 9];
        o[..4].copy_from_slice(&self.width.to_le_bytes());
        o[4..8].copy_from_slice(&self.height.to_le_bytes());
        o[8] = self.color;
        o
    }

    pub fn decode(b: &[u8]) -> Result<Image> {
        ensure!(b.len() == 9 && matches!(b[8], 0 | 2 | 3 | 4 | 6), "image PNG invalide");
        let img = Image {
            width: u32::from_le_bytes(b[..4].try_into().unwrap()),
            height: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            color: b[8],
        };
        ensure!(img.width > 0 && img.height > 0, "image PNG invalide");
        Ok(img)
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Removes the filter of a row (filtered `row` → pixels), `prev`: previous
/// row already unfiltered, `bpp`: bytes per pixel.
fn unfilter(f: u8, row: &mut [u8], prev: &[u8], bpp: usize) -> Result<()> {
    match f {
        0 => {}
        1 => {
            for i in bpp..row.len() {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        2 => {
            for i in 0..row.len() {
                row[i] = row[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..row.len() {
                let left = if i >= bpp { u16::from(row[i - bpp]) } else { 0 };
                row[i] = row[i].wrapping_add(((left + u16::from(prev[i])) / 2) as u8);
            }
        }
        4 => {
            for i in 0..row.len() {
                let (a, c) = if i >= bpp { (row[i - bpp], prev[i - bpp]) } else { (0, 0) };
                row[i] = row[i].wrapping_add(paeth(a, prev[i], c));
            }
        }
        _ => bail!("filtre PNG inconnu"),
    }
    Ok(())
}

/// Applies the filter of a row again (pixels → filtered row).
fn refilter(f: u8, pix: &[u8], prev: &[u8], bpp: usize, out: &mut Vec<u8>) {
    for i in 0..pix.len() {
        let a = if i >= bpp { pix[i - bpp] } else { 0 };
        let c = if i >= bpp { prev[i - bpp] } else { 0 };
        let b = prev[i];
        out.push(match f {
            0 => pix[i],
            1 => pix[i].wrapping_sub(a),
            2 => pix[i].wrapping_sub(b),
            3 => pix[i].wrapping_sub(((u16::from(a) + u16::from(b)) / 2) as u8),
            _ => pix[i].wrapping_sub(paeth(a, b, c)),
        });
    }
}

fn bitlen(v: u32) -> usize {
    (32 - v.leading_zeros()) as usize
}

const ACT: usize = 12;
/// Equality patterns between neighbours (left = up, up = diagonal, left =
/// diagonal, up = up-right): 16 cases.
const EQ: usize = 16;

/// Model probabilities.
struct Model {
    filter: [[Prob; 8]; 8],
    zero: Vec<Prob>,
    sign: Vec<Prob>,
    exp: Vec<[Prob; 8]>,
    mant: Vec<[Prob; 8]>,
    raw: Prob,
    /// Palette: "same as left?", "same as above?", then the index.
    same_w: Vec<Prob>,
    same_n: Vec<Prob>,
    index: Vec<[Prob; 256]>,
}

impl Model {
    fn new() -> Self {
        let p = Prob::NEW;
        Self {
            filter: [[p; 8]; 8],
            zero: vec![p; 3 * ACT * EQ],
            sign: vec![p; 3 * ACT * EQ],
            exp: vec![[p; 8]; 3 * ACT * EQ],
            mant: vec![[p; 8]; 3 * 9],
            raw: p,
            same_w: vec![p; 4],
            same_n: vec![p; 4],
            index: vec![[p; 256]; 257],
        }
    }
}

/// Codes (or decodes) a signed difference from −128 to 127; `class`: 0 green
/// or grey, 1 red or blue, 2 transparency; `act`: activity context.
fn code_residual<C: Coder>(c: &mut C, m: &mut Model, class: usize, act: usize, eq: usize, r: i32) -> i32 {
    let ctx = (class * ACT + act) * EQ + eq;
    if !c.code(&mut m.zero[ctx], r != 0) {
        return 0;
    }
    let neg = c.code(&mut m.sign[ctx], r < 0);
    let mag = r.unsigned_abs();
    // Size of |r| (1 to 8 bits) in unary, then the bits below the most significant bit.
    let e_real = bitlen(mag);
    let mut e = 1;
    while e < 8 && c.code(&mut m.exp[ctx][e - 1], e_real > e) {
        e += 1;
    }
    let mut v = 1u32;
    for i in (0..e - 1).rev() {
        let p = if i == e - 2 { &mut m.mant[class * 9 + e][i] } else { &mut m.raw };
        v = (v << 1) | u32::from(c.code(p, (mag >> i) & 1 == 1));
    }
    if neg {
        -(v as i32)
    } else {
        v as i32
    }
}

/// The shared traversal: `raw` (filtered) is read when coding, filled when decoding.
fn run<C: Coder>(c: &mut C, img: Image, raw: &mut [u8], decoding: bool) -> Result<()> {
    let (rb, ch) = (img.row_bytes(), img.channels());
    ensure!(Some(raw.len()) == img.raw_len(), "taille de l'image incohérente");
    let mut m = Model::new();
    let mut prev = vec![0u8; rb];
    let mut cur = vec![0u8; rb];
    let mut prev_filter = 0usize;
    for y in 0..img.height as usize {
        let line = &mut raw[y * (rb + 1)..(y + 1) * (rb + 1)];
        // Filter of the row (0 to 4), on 3 bits, context: previous filter.
        let real = usize::from(line[0]);
        if !decoding {
            ensure!(real <= 4, "filtre PNG inconnu");
        }
        let mut node = 1;
        for i in (0..3).rev() {
            let bit = c.code(&mut m.filter[prev_filter][node], (real >> i) & 1 == 1);
            node = node * 2 + usize::from(bit);
        }
        let f = node - 8;
        ensure!(f <= 4, "filtre PNG invalide");
        prev_filter = f;
        // Pixels of the row (real ones when coding, to be decoded otherwise).
        if !decoding {
            cur.copy_from_slice(&line[1..]);
            unfilter(f as u8, &mut cur, &prev, ch)?;
        }
        for x in 0..img.width as usize {
            let base = x * ch;
            let order: &[usize] = match ch {
                3 => &[1, 0, 2],
                4 => &[1, 0, 2, 3],
                2 => &[0, 1],
                _ => &[0],
            };
            let mut g_err = 0i32;
            for &k in order {
                let i = base + k;
                let w = if x > 0 { i32::from(cur[i - ch]) } else { 0 };
                let n = i32::from(prev[i]);
                let nw = if x > 0 { i32::from(prev[i - ch]) } else { 0 };
                let ne = if x + 1 < img.width as usize { i32::from(prev[i + ch]) } else { n };
                if img.color == 3 {
                    // Palette: same as left? same as above? otherwise the index.
                    let v = i32::from(cur[i]);
                    let sctx = usize::from(w == n) * 2 + usize::from(nw == n);
                    if x > 0 && c.code(&mut m.same_w[sctx], v == w) {
                        cur[i] = w as u8;
                        continue;
                    }
                    if y > 0 && n != w && c.code(&mut m.same_n[sctx], v == n) {
                        cur[i] = n as u8;
                        continue;
                    }
                    let mut node = 1usize;
                    // One tree per left colour (256 = no neighbour).
                    let tree = &mut m.index[if x > 0 { w as usize } else { 256 }];
                    for b in (0..8).rev() {
                        let bit = c.code(&mut tree[node], (v >> b) & 1 == 1);
                        node = node * 2 + usize::from(bit);
                    }
                    cur[i] = (node - 256) as u8;
                    continue;
                }
                // Median predictor (JPEG-LS).
                let pred = if nw >= w.max(n) {
                    w.min(n)
                } else if nw <= w.min(n) {
                    w.max(n)
                } else {
                    w + n - nw
                };
                let class = match (ch, k) {
                    (3 | 4, 0 | 2) => 1,
                    (4, 3) | (2, 1) => 2,
                    _ => 0,
                };
                // Red and blue: the error made on green is added.
                let pred = if class == 1 { (pred + g_err).clamp(0, 255) } else { pred };
                let act = bitlen(((w - nw).abs() + (n - nw).abs() + (n - ne).abs()) as u32).min(ACT - 1);
                let eq = usize::from(w == n) | usize::from(n == nw) << 1 | usize::from(w == nw) << 2 | usize::from(n == ne) << 3;
                let real = i32::from(cur[i]);
                let r = (real - pred + 128).rem_euclid(256) - 128;
                let r = code_residual(c, &mut m, class, act, eq, r);
                let v = (pred + r).rem_euclid(256);
                if class == 0 && (ch == 3 || ch == 4) {
                    g_err = v - pred;
                }
                cur[i] = v as u8;
            }
        }
        if decoding {
            let mut out = Vec::with_capacity(rb + 1);
            out.push(f as u8);
            refilter(f as u8, &cur, &prev, ch, &mut out);
            line.copy_from_slice(&out);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    Ok(())
}

/// Codes the decompressed contents (filtered rows) of an image.
pub fn encode(img: Image, raw: &[u8]) -> Result<Vec<u8>> {
    let mut e = Encoder::new();
    let mut copy = raw.to_vec();
    run(&mut e, img, &mut copy, false)?;
    Ok(e.finish())
}

/// Gives back the decompressed contents, identical.
pub fn decode(img: Image, data: &[u8]) -> Result<Vec<u8>> {
    let len = img.raw_len().ok_or_else(|| anyhow::anyhow!("image trop grande"))?;
    // Each row costs at least a few bits: a bound against an image announced
    // with an absurd size.
    ensure!(len <= data.len().saturating_mul(4096).saturating_add(1 << 20), "image trop grande pour ses données");
    let mut raw = vec![0u8; len];
    let mut d = Decoder::new(data);
    run(&mut d, img, &mut raw, true)?;
    ensure!(d.overrun() <= 5, "données d'image tronquées");
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(color: u8, w: u32, h: u32) -> (Image, Vec<u8>) {
        let img = Image { width: w, height: h, color };
        let rb = img.row_bytes();
        let mut raw = Vec::new();
        let mut x = 99u32;
        for y in 0..h as usize {
            let f = (y % 5) as u8;
            raw.push(f);
            for i in 0..rb {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                raw.push(((i * 3 + y * 5) as u32 + (x >> 28)) as u8);
            }
        }
        (img, raw)
    }

    #[test]
    fn aller_retour_toutes_couleurs() {
        for color in [0, 2, 3, 4, 6] {
            let (img, raw) = image(color, 37, 23);
            let c = encode(img, &raw).unwrap();
            assert_eq!(decode(img, &c).unwrap(), raw, "type de couleur {color}");
        }
    }

    #[test]
    fn filtres_inverses() {
        let prev: Vec<u8> = (0..30).map(|i| (i * 7) as u8).collect();
        let pix: Vec<u8> = (0..30).map(|i| (i * 13 + 5) as u8).collect();
        for f in 0..5 {
            let mut out = Vec::new();
            refilter(f, &pix, &prev, 3, &mut out);
            unfilter(f, &mut out, &prev, 3).unwrap();
            assert_eq!(out, pix);
        }
    }
}
