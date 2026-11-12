//! x86, x86-64 and ARM64 programs (stage 6): "BCJ" filter.
//!
//! In machine code, a call gives its target as a **distance** from the
//! instruction: the same call to the same function is written differently
//! at each place. The filter replaces this distance with the target
//! **address** (distance + position), identical everywhere: zstd finds many
//! more repetitions in it. Only code sections are filtered (according to the
//! ELF, Windows or macOS headers).
//!
//! - x86: `E8` calls and `E9` jumps of less than 16 MiB (high byte 00 or
//!   FF), like xz's x86 filter. After each E8 or E9 byte, its 4 distance
//!   bytes (converted or not) are skipped: windows never overlap, and the
//!   converted address keeps a high byte of 00 or FF, so decoding takes the
//!   same decisions at the same places.
//! - ARM64: aligned 4-byte instructions; `BL` calls (26 bits) and `ADRP`
//!   pages of less than 512 MiB (±2^17 pages), like xz's ARM64 filter. The
//!   converted address of an ADRP stays within the same range.
//!
//! Transformed form (see SPEC.md):
//! ```text
//! varint version (1)
//! u8     architecture: 1 = x86 / x86-64, 2 = ARM64
//! varint number of zones N, then N times: varint start, varint length
//! then the file, with filtered zones
//! ```

use anyhow::{bail, ensure, Result};

use crate::format::{put_varint, Cursor};

const VERSION: u64 = 1;
/// Beyond this, it is no longer an ordinary program.
const MAX_ZONES: usize = 256;

/// Instruction set of a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86,
    Arm64,
}

impl Arch {
    fn code(self) -> u8 {
        match self {
            Arch::X86 => 1,
            Arch::Arm64 => 2,
        }
    }
}

fn le16(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
}

fn le32(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

fn le64(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 8).and_then(|b| usize::try_from(u64::from_le_bytes(b.try_into().unwrap())).ok())
}

/// Instruction set and code sections of a program (ELF, Windows or macOS),
/// sorted and non-overlapping; None for another architecture or format.
pub fn code_zones(d: &[u8]) -> Option<(Arch, Vec<(usize, usize)>)> {
    let (arch, mut z) = if d.starts_with(b"\x7FELF") {
        elf(d)?
    } else if d.starts_with(b"MZ") {
        pe(d)?
    } else if d.starts_with(b"\xCF\xFA\xED\xFE") {
        macho(d)?
    } else {
        return None;
    };
    z.retain(|&(a, n)| n >= 16 && a.checked_add(n).is_some_and(|e| e <= d.len()));
    z.sort_unstable();
    z.dedup();
    if z.is_empty() || z.len() > MAX_ZONES || z.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return None;
    }
    Some((arch, z))
}

/// Little-endian ELF, x86 (3), x86-64 (62) or ARM64 (183): executable
/// PROGBITS sections.
fn elf(d: &[u8]) -> Option<(Arch, Vec<(usize, usize)>)> {
    const SHT_PROGBITS: usize = 1;
    const SHF_EXECINSTR: usize = 4;
    if d.get(5) != Some(&1) {
        return None;
    }
    let is64 = match d.get(4) {
        Some(1) => false,
        Some(2) => true,
        _ => return None,
    };
    let arch = match le16(d, 18)? {
        3 | 62 => Arch::X86,
        183 if is64 => Arch::Arm64,
        _ => return None,
    };
    let (shoff, entsize, num) = if is64 { (le64(d, 0x28)?, le16(d, 0x3A)?, le16(d, 0x3C)?) } else { (le32(d, 0x20)?, le16(d, 0x2E)?, le16(d, 0x30)?) };
    if num > 4096 || entsize < if is64 { 64 } else { 40 } {
        return None;
    }
    let mut z = Vec::new();
    for i in 0..num {
        let e = shoff.checked_add(i * entsize)?;
        let (typ, flags, off, size) = if is64 {
            (le32(d, e + 4)?, le64(d, e + 8)?, le64(d, e + 24)?, le64(d, e + 32)?)
        } else {
            (le32(d, e + 4)?, le32(d, e + 8)?, le32(d, e + 16)?, le32(d, e + 20)?)
        };
        if typ == SHT_PROGBITS && flags & SHF_EXECINSTR != 0 {
            z.push((off, size));
        }
    }
    Some((arch, z))
}

/// Windows program (PE), x86 (0x14C), x86-64 (0x8664) or ARM64 (0xAA64):
/// code sections.
fn pe(d: &[u8]) -> Option<(Arch, Vec<(usize, usize)>)> {
    const CNT_CODE: usize = 0x20;
    const MEM_EXECUTE: usize = 0x2000_0000;
    let pe = le32(d, 0x3C)?;
    if d.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let arch = match le16(d, pe + 4)? {
        0x14C | 0x8664 => Arch::X86,
        0xAA64 => Arch::Arm64,
        _ => return None,
    };
    let (num, opt) = (le16(d, pe + 6)?, le16(d, pe + 20)?);
    let first = pe + 24 + opt;
    let mut z = Vec::new();
    for i in 0..num {
        let s = first + 40 * i;
        let (size, off, flags) = (le32(d, s + 16)?, le32(d, s + 20)?, le32(d, s + 36)?);
        if flags & (CNT_CODE | MEM_EXECUTE) != 0 {
            z.push((off, size));
        }
    }
    Some((arch, z))
}

/// 64-bit macOS program (Mach-O), x86-64 or ARM64: instruction sections of
/// the segments.
fn macho(d: &[u8]) -> Option<(Arch, Vec<(usize, usize)>)> {
    const LC_SEGMENT_64: usize = 0x19;
    const INSTRUCTIONS: usize = 0x8000_0400;
    let arch = match le32(d, 4)? {
        0x0100_0007 => Arch::X86,
        0x0100_000C => Arch::Arm64,
        _ => return None,
    };
    let ncmds = le32(d, 16)?;
    if ncmds > 4096 {
        return None;
    }
    let mut z = Vec::new();
    let mut p = 32usize;
    for _ in 0..ncmds {
        let (cmd, size) = (le32(d, p)?, le32(d, p + 4)?);
        if size < 8 {
            return None;
        }
        if cmd == LC_SEGMENT_64 {
            let nsects = le32(d, p + 64)?;
            for k in 0..nsects.min(4096) {
                let s = p + 72 + 80 * k;
                let (len, off, flags) = (le64(d, s + 40)?, le32(d, s + 48)?, le32(d, s + 64)?);
                if flags & INSTRUCTIONS != 0 && off != 0 {
                    z.push((off, len));
                }
            }
        }
        p = p.checked_add(size)?;
    }
    Some((arch, z))
}

/// x86: filters a zone in place; `encode`: distances → addresses.
fn filter_x86(d: &mut [u8], start: usize, len: usize, encode: bool) {
    let end = start + len;
    let mut i = start;
    while i + 5 <= end {
        if d[i] & 0xFE == 0xE8 {
            if d[i + 4] == 0 || d[i + 4] == 0xFF {
                let v = i32::from_le_bytes(d[i + 1..i + 5].try_into().unwrap());
                let here = (i + 5) as i32;
                let w = if encode { v.wrapping_add(here) } else { v.wrapping_sub(here) };
                // Result brought back to 25 signed bits: high byte 00 or FF.
                let w = (w << 7) >> 7;
                d[i + 1..i + 5].copy_from_slice(&w.to_le_bytes());
            }
            // Always 5 bytes further: a conversion never touches a byte on which a
            // previous decision was based.
            i += 5;
        } else {
            i += 1;
        }
    }
}

/// ARM64: filters a zone in place (instructions aligned on 4 bytes from the
/// start of the zone); `encode`: distances → addresses.
fn filter_arm64(d: &mut [u8], start: usize, len: usize, encode: bool) {
    let end = start + len;
    let mut i = start;
    while i + 4 <= end {
        let ins = u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
        let pc = i as u32;
        let out = if ins >> 26 == 0x25 {
            // BL: distance in 4-byte words over 26 bits.
            let pc = if encode { pc >> 2 } else { (pc >> 2).wrapping_neg() };
            Some(0x9400_0000 | (ins.wrapping_add(pc) & 0x03FF_FFFF))
        } else if ins & 0x9F00_0000 == 0x9000_0000 {
            // ADRP: distance in 4 KiB pages, converted if |distance| < 2^17.
            let src = ((ins >> 29) & 3) | ((ins >> 3) & 0x001F_FFFC);
            if src.wrapping_add(0x0002_0000) & 0x001C_0000 != 0 {
                None
            } else {
                let pc = if encode { pc >> 12 } else { (pc >> 12).wrapping_neg() };
                let dest = src.wrapping_add(pc);
                Some(
                    (ins & 0x9000_001F)
                        | ((dest & 3) << 29)
                        | ((dest & 0x0003_FFFC) << 3)
                        | (0u32.wrapping_sub(dest & 0x0002_0000) & 0x00E0_0000),
                )
            }
        } else {
            None
        };
        if let Some(v) = out {
            d[i..i + 4].copy_from_slice(&v.to_le_bytes());
        }
        i += 4;
    }
}

fn filter(arch: Arch, d: &mut [u8], start: usize, len: usize, encode: bool) {
    match arch {
        Arch::X86 => filter_x86(d, start, len, encode),
        Arch::Arm64 => filter_arm64(d, start, len, encode),
    }
}

/// Transformed form of a program; None if it is not one.
pub fn encode(d: &[u8]) -> Option<Vec<u8>> {
    let (arch, zones) = code_zones(d)?;
    let mut out = Vec::with_capacity(d.len() + 16 + 8 * zones.len());
    put_varint(&mut out, VERSION);
    out.push(arch.code());
    put_varint(&mut out, zones.len() as u64);
    for &(a, n) in &zones {
        put_varint(&mut out, a as u64);
        put_varint(&mut out, n as u64);
    }
    let base = out.len();
    out.extend_from_slice(d);
    for &(a, n) in &zones {
        filter(arch, &mut out[base..], a, n, true);
    }
    Some(out)
}

/// Rebuilds the original program, without exceeding `max` bytes.
pub fn decode(t: &[u8], max: u64) -> Result<Vec<u8>> {
    let mut c = Cursor::new(t);
    ensure!(c.varint()? == VERSION, "programme : version inconnue");
    let arch = match c.u8()? {
        1 => Arch::X86,
        2 => Arch::Arm64,
        _ => bail!("programme : architecture inconnue"),
    };
    let n = usize::try_from(c.varint()?)?;
    ensure!(n <= MAX_ZONES, "programme invalide");
    let mut zones = Vec::with_capacity(n);
    for _ in 0..n {
        zones.push((usize::try_from(c.varint()?)?, usize::try_from(c.varint()?)?));
    }
    let mut out = c.bytes(c.remaining())?.to_vec();
    ensure!(out.len() as u64 <= max, "fichier reconstruit trop long");
    for &(a, n) in &zones {
        ensure!(a.checked_add(n).is_some_and(|e| e <= out.len()), "programme invalide");
        filter(arch, &mut out, a, n, false);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal x86-64 ELF: one code section full of calls, one data section.
    fn programme() -> Vec<u8> {
        let mut code = Vec::new();
        for i in 0..2000i32 {
            code.push(if i % 3 == 0 { 0xE9 } else { 0xE8 });
            // Always the same target: distance decreasing by 8 bytes per call.
            code.extend_from_slice(&(300 - i * 8).to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0xE5]);
        }
        code.extend_from_slice(&[0xE8, 1, 2, 3, 0x42, 0xE8, 0xFF]); // not a call; zone cut
        let data = vec![0xE8u8; 64];
        let mut d = vec![0u8; 64];
        d[..4].copy_from_slice(b"\x7FELF");
        d[4] = 2;
        d[5] = 1;
        d[18..20].copy_from_slice(&62u16.to_le_bytes());
        let code_off = d.len();
        d.extend_from_slice(&code);
        let data_off = d.len();
        d.extend_from_slice(&data);
        let shoff = d.len();
        d[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        d[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
        d[0x3C..0x3E].copy_from_slice(&3u16.to_le_bytes());
        for (typ, flags, off, size) in [(0u32, 0u64, 0usize, 0usize), (1, 6, code_off, code.len()), (1, 2, data_off, data.len())] {
            let mut e = vec![0u8; 64];
            e[4..8].copy_from_slice(&typ.to_le_bytes());
            e[8..16].copy_from_slice(&flags.to_le_bytes());
            e[24..32].copy_from_slice(&(off as u64).to_le_bytes());
            e[32..40].copy_from_slice(&(size as u64).to_le_bytes());
            d.extend(e);
        }
        d
    }

    #[test]
    fn aller_retour() {
        let d = programme();
        assert_eq!(code_zones(&d).unwrap(), (Arch::X86, vec![(64, 16_007)]));
        let t = encode(&d).unwrap();
        assert_ne!(&t[t.len() - d.len()..], &d[..]);
        assert_eq!(decode(&t, d.len() as u64).unwrap(), d);
        assert!(decode(&t, d.len() as u64 - 1).is_err());
        // All calls target the same address: much more compressible.
        assert!(zstd::bulk::compress(&t, 19).unwrap().len() * 2 < zstd::bulk::compress(&d, 19).unwrap().len());
    }

    #[test]
    fn filtre_exact_pour_toute_valeur() {
        let mut rng = 0x1234_5678u64;
        let mut d: Vec<u8> = (0..100_000)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                [0xE8, 0xE9, 0, 0xFF, 0x94, 0x90, 0x97, rng as u8][(rng >> 32) as usize % 8]
            })
            .collect();
        let orig = d.clone();
        for arch in [Arch::X86, Arch::Arm64] {
            filter(arch, &mut d, 3, 99_990, true);
            assert_ne!(d, orig);
            filter(arch, &mut d, 3, 99_990, false);
            assert_eq!(d, orig);
        }
    }

    #[test]
    fn forme_abimee_sans_plantage() {
        let t = encode(&programme()).unwrap();
        for i in 0..40 {
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
