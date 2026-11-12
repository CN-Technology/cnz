//! Uncompressed WAV sound (stage 6).
//!
//! The samples of a recording vary little from one instant to the next: each
//! sample is replaced by its difference from a prediction made from the
//! previous samples of the same channel (order 1: the previous one; order 2:
//! the extension of the two previous ones). The small differences are
//! "zigzag" coded (0, -1, 1, -2… → 0, 1, 2, 3…) then stored by byte planes
//! (all low bytes, then all high bytes): zstd compresses them much better
//! than the original sound.
//!
//! All computations are done modulo 2^bits: the transform is exact whatever
//! the samples (unsigned 8-bit included).
//!
//! Transformed form (see SPEC.md):
//! ```text
//! varint version (1)
//! varint position of the samples, varint length of the samples
//! varint channels, u8 bytes per sample (1 to 4), u8 order (1 or 2)
//! varint header length, then the header (bytes before the samples)
//! then the byte planes of the differences, then the end of the file
//! ```

use anyhow::{ensure, Result};

use crate::format::{put_varint, Cursor};

const VERSION: u64 = 1;
/// Beyond this, prediction makes no sense (and the format does not allow it).
const MAX_CHANNELS: usize = 64;

/// Position of the samples, their length (whole frames), channels and bytes
/// per sample; None if this is not an ordinary PCM WAV.
fn layout(d: &[u8]) -> Option<(usize, usize, usize, usize)> {
    if d.len() < 12 || &d[..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        return None;
    }
    let le16 = |o: usize| d.get(o..o + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])));
    let le32 = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let mut p = 12;
    let mut fmt = None;
    while p + 8 <= d.len() {
        let size = le32(p + 4)?;
        let body = p + 8;
        match &d[p..p + 4] {
            b"fmt " => {
                // Integer PCM (1), or extended format (0xFFFE) whose subformat is PCM.
                let tag = le16(body)?;
                let pcm = tag == 1 || (tag == 0xFFFE && size >= 26 && le16(body + 24)? == 1);
                let (ch, bits) = (le16(body + 2)?, le16(body + 14)?);
                if !pcm || ch == 0 || ch > MAX_CHANNELS || bits == 0 || bits > 32 {
                    return None;
                }
                fmt = Some((ch, bits.div_ceil(8)));
            }
            b"data" => {
                let (ch, width) = fmt?;
                let frame = ch * width;
                let len = size.min(d.len() - body);
                return Some((body, len - len % frame, ch, width));
            }
            _ => {}
        }
        p = body.checked_add(size)?.checked_add(size & 1)?;
    }
    None
}

fn sample(b: &[u8]) -> u32 {
    b.iter().rev().fold(0u32, |v, &x| (v << 8) | u32::from(x))
}

/// Interprets a `bits`-bit value as signed.
fn signed(v: u32, bits: u32) -> i64 {
    let s = 64 - bits;
    ((u64::from(v) << s) as i64) >> s
}

fn predict(order: u8, a: i64, b: i64) -> i64 {
    if order == 1 {
        a
    } else {
        2 * a - b
    }
}

/// Differences (zigzag) of all samples, in file order.
fn residuals(s: &[u8], ch: usize, width: usize, order: u8) -> Vec<u32> {
    let bits = 8 * width as u32;
    let mask = if bits == 32 { u32::MAX } else { (1u32 << bits) - 1 };
    let mut hist = vec![(0i64, 0i64); ch];
    s.chunks_exact(width)
        .enumerate()
        .map(|(k, b)| {
            let x = sample(b);
            let h = &mut hist[k % ch];
            let p = predict(order, h.0, h.1) as u32 & mask;
            let r = signed(x.wrapping_sub(p) & mask, bits);
            *h = (signed(x, bits), h.0);
            (((r << 1) ^ (r >> 63)) as u32) & mask
        })
        .collect()
}

/// Transformed form of a WAV file; None if it is not integer PCM.
pub fn encode(d: &[u8]) -> Option<Vec<u8>> {
    let (start, len, ch, width) = layout(d)?;
    if len == 0 {
        return None;
    }
    let s = &d[start..start + len];
    // Order chosen from the sum of the differences (zigzag ≈ 2 × |difference|).
    let cost = |o: u8| residuals(s, ch, width, o).iter().map(|&z| u64::from(z)).sum::<u64>();
    let order = if cost(2) < cost(1) { 2 } else { 1 };
    let res = residuals(s, ch, width, order);

    let mut out = Vec::with_capacity(d.len() + 32);
    put_varint(&mut out, VERSION);
    for v in [start, len, ch] {
        put_varint(&mut out, v as u64);
    }
    out.extend_from_slice(&[width as u8, order]);
    put_varint(&mut out, start as u64);
    out.extend_from_slice(&d[..start]);
    for plane in 0..width {
        out.extend(res.iter().map(|&z| (z >> (8 * plane)) as u8));
    }
    out.extend_from_slice(&d[start + len..]);
    Some(out)
}

/// Rebuilds the original WAV file, without exceeding `max` bytes.
pub fn decode(t: &[u8], max: u64) -> Result<Vec<u8>> {
    let mut c = Cursor::new(t);
    ensure!(c.varint()? == VERSION, "son : version inconnue");
    let start = usize::try_from(c.varint()?)?;
    let len = usize::try_from(c.varint()?)?;
    let ch = usize::try_from(c.varint()?)?;
    let (width, order) = (usize::from(c.u8()?), c.u8()?);
    ensure!((1..=MAX_CHANNELS).contains(&ch) && (1..=4).contains(&width) && (1..=2).contains(&order), "son invalide");
    ensure!(len % (ch * width) == 0, "son invalide");
    ensure!(usize::try_from(c.varint()?)? == start, "son invalide");
    let head = c.bytes(start)?;
    let planes = c.bytes(len)?;
    let tail = c.bytes(c.remaining())?;
    let total = start as u64 + len as u64 + tail.len() as u64;
    ensure!(total <= max, "fichier reconstruit trop long");

    let n = len / width;
    let bits = 8 * width as u32;
    let mask = if bits == 32 { u32::MAX } else { (1u32 << bits) - 1 };
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(head);
    let mut hist = vec![(0i64, 0i64); ch];
    for k in 0..n {
        let z = (0..width).fold(0u32, |v, p| v | (u32::from(planes[p * n + k]) << (8 * p)));
        let r = i64::from(z >> 1) ^ -i64::from(z & 1);
        let h = &mut hist[k % ch];
        let p = predict(order, h.0, h.1) as u32 & mask;
        let x = p.wrapping_add(r as u32) & mask;
        *h = (signed(x, bits), h.0);
        out.extend_from_slice(&x.to_le_bytes()[..width]);
    }
    out.extend_from_slice(tail);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(ch: u16, bits: u16, data: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut d = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        d.extend_from_slice(&16u32.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&ch.to_le_bytes());
        d.extend_from_slice(&44100u32.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&(ch * bits / 8).to_le_bytes());
        d.extend_from_slice(&bits.to_le_bytes());
        d.extend_from_slice(b"data");
        d.extend_from_slice(&(data.len() as u32).to_le_bytes());
        d.extend_from_slice(data);
        d.extend_from_slice(extra);
        d
    }

    #[test]
    fn aller_retour() {
        // 16-bit stereo sine wave, extremes included, and an incomplete frame.
        let mut s: Vec<u8> = (0..20_000)
            .flat_map(|i| {
                let v = (f64::from(i) * 0.05).sin() * 32767.0;
                let w = if i % 997 == 0 { i16::MIN } else { v as i16 };
                [w.to_le_bytes(), (-w).to_le_bytes()].concat()
            })
            .collect();
        s.push(7);
        for (ch, bits) in [(2, 16), (1, 8), (3, 24), (1, 32), (4, 16)] {
            let d = wav(ch, bits, &s, b"LIST\x04\0\0\0abcd");
            let t = encode(&d).unwrap();
            assert_eq!(decode(&t, d.len() as u64).unwrap(), d, "{ch} canaux, {bits} bits");
            assert!(decode(&t, d.len() as u64 - 1).is_err());
        }
        // The predicted sound compresses better.
        let d = wav(2, 16, &s, b"");
        let t = encode(&d).unwrap();
        assert!(zstd::bulk::compress(&t, 19).unwrap().len() < zstd::bulk::compress(&d, 19).unwrap().len() / 2);
    }

    #[test]
    fn pas_un_wav() {
        assert!(encode(b"RIFF\0\0\0\0WEBPVP8 ").is_none());
        let flottant = {
            let mut d = wav(1, 32, &[0; 64], b"");
            d[20] = 3;
            d
        };
        assert!(encode(&flottant).is_none());
    }

    #[test]
    fn forme_abimee_sans_plantage() {
        let s: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let t = encode(&wav(2, 24, &s, b"x")).unwrap();
        for i in 0..t.len() {
            for v in [0u8, 1, 3, 0x7F, 0xFF] {
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
