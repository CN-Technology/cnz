//! Binary structures of the .cnz format: header, blocks, file footer and
//! variable-length integers. The full description is in SPEC.md.

use std::fs::File;

use anyhow::{bail, Result};

/// Start-of-file signature. The 0x89 and the \r\n / \x1a\n detect transfers
/// that damage files (same idea as PNG).
pub const MAGIC: [u8; 8] = *b"\x89CNZ\r\n\x1a\n";
pub const VERSION_MAJOR: u8 = 1;
pub const VERSION_MINOR: u8 = 0;
pub const HEADER_LEN: usize = 16;
/// Header flag: the archive is encrypted.
pub const FLAG_ENCRYPTED: u16 = 1;

pub const BLOCK_MAGIC: [u8; 4] = *b"CNZB";
pub const BLOCK_HEADER_LEN: usize = 32;
/// Maximum decompressed size of a block (protects against booby-trapped archives).
pub const MAX_BLOCK_RAW: u64 = 1 << 30;

pub const FOOTER_MAGIC: [u8; 4] = *b"CNZE";
pub const FOOTER_END: [u8; 4] = *b"CNZ!";
pub const FOOTER_LEN: usize = 48;

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub fn encode_header(flags: u16) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(&MAGIC);
    h[8] = VERSION_MAJOR;
    h[9] = VERSION_MINOR;
    h[10..12].copy_from_slice(&flags.to_le_bytes());
    let crc = crc32c::crc32c(&h[..12]);
    h[12..].copy_from_slice(&crc.to_le_bytes());
    h
}

/// Checks the header and returns its flags.
pub fn check_header(h: &[u8]) -> Result<u16> {
    if h.len() < HEADER_LEN || h[..8] != MAGIC {
        bail!("ce fichier n'est pas une archive .cnz");
    }
    if crc32c::crc32c(&h[..12]) != u32_at(h, 12) {
        bail!("en-tête d'archive endommagé");
    }
    if h[8] != VERSION_MAJOR {
        bail!("archive .cnz version {}.{} : non prise en charge par cette version de CNz", h[8], h[9]);
    }
    let flags = u16::from_le_bytes([h[10], h[11]]);
    if flags & !FLAG_ENCRYPTED != 0 {
        bail!("cette archive utilise des fonctions inconnues de cette version de CNz");
    }
    Ok(flags)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockKind {
    Data = 0,
    Index = 1,
    /// Encryption parameters (in clear).
    Params = 2,
    /// Recovery data table.
    Recovery = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Store = 0,
    Zstd = 1,
    /// zstd with built-in dictionary no. 1 (see dict.rs).
    ZstdDict = 2,
    /// CNz context-mixing engine, "maximum" mode (see cm.rs).
    Cm = 3,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockHeader {
    pub kind: BlockKind,
    pub codec: Codec,
    pub raw_len: u64,
    pub comp_len: u64,
    pub payload_crc: u32,
}

impl BlockHeader {
    pub fn encode(&self) -> [u8; BLOCK_HEADER_LEN] {
        let mut h = [0u8; BLOCK_HEADER_LEN];
        h[..4].copy_from_slice(&BLOCK_MAGIC);
        h[4] = self.kind as u8;
        h[5] = self.codec as u8;
        h[8..16].copy_from_slice(&self.raw_len.to_le_bytes());
        h[16..24].copy_from_slice(&self.comp_len.to_le_bytes());
        h[24..28].copy_from_slice(&self.payload_crc.to_le_bytes());
        let crc = crc32c::crc32c(&h[..28]);
        h[28..].copy_from_slice(&crc.to_le_bytes());
        h
    }

    pub fn decode(h: &[u8]) -> Result<Self> {
        if h.len() < BLOCK_HEADER_LEN || h[..4] != BLOCK_MAGIC {
            bail!("signature de bloc absente");
        }
        if crc32c::crc32c(&h[..28]) != u32_at(h, 28) {
            bail!("en-tête de bloc endommagé");
        }
        let kind = match h[4] {
            0 => BlockKind::Data,
            1 => BlockKind::Index,
            2 => BlockKind::Params,
            3 => BlockKind::Recovery,
            v => bail!("type de bloc inconnu ({v})"),
        };
        let codec = match h[5] {
            0 => Codec::Store,
            1 => Codec::Zstd,
            2 => Codec::ZstdDict,
            3 => Codec::Cm,
            v => bail!("méthode de compression inconnue ({v})"),
        };
        let raw_len = u64_at(h, 8);
        if raw_len > MAX_BLOCK_RAW {
            bail!("bloc trop grand ({raw_len} octets)");
        }
        Ok(Self { kind, codec, raw_len, comp_len: u64_at(h, 16), payload_crc: u32_at(h, 24) })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Footer {
    pub index_offset: u64,
    pub backup_index_offset: u64,
    /// Position of the recovery table, 0 if the archive has none.
    pub recovery_offset: u64,
    pub archive_len: u64,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut f = [0u8; FOOTER_LEN];
        f[..4].copy_from_slice(&FOOTER_MAGIC);
        f[8..16].copy_from_slice(&self.index_offset.to_le_bytes());
        f[16..24].copy_from_slice(&self.backup_index_offset.to_le_bytes());
        f[24..32].copy_from_slice(&self.recovery_offset.to_le_bytes());
        f[32..40].copy_from_slice(&self.archive_len.to_le_bytes());
        let crc = crc32c::crc32c(&f[..40]);
        f[40..44].copy_from_slice(&crc.to_le_bytes());
        f[44..].copy_from_slice(&FOOTER_END);
        f
    }

    pub fn decode(f: &[u8]) -> Result<Self> {
        if f.len() < FOOTER_LEN || f[..4] != FOOTER_MAGIC || f[44..48] != FOOTER_END {
            bail!("signature du pied absente");
        }
        if crc32c::crc32c(&f[..40]) != u32_at(f, 40) {
            bail!("pied d'archive endommagé");
        }
        Ok(Self {
            index_offset: u64_at(f, 8),
            backup_index_offset: u64_at(f, 16),
            recovery_offset: u64_at(f, 24),
            archive_len: u64_at(f, 32),
        })
    }
}

/// Positioned read, without moving a shared cursor: several cores can read
/// the same file at the same time.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            match std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], offset + done as u64)? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                n => done += n,
            }
        }
        Ok(())
    }
}

/// Reads a block (header + stored payload) and checks its CRCs.
pub fn read_block_at(file: &File, file_len: u64, offset: u64) -> Result<(BlockHeader, Vec<u8>)> {
    if offset.checked_add(BLOCK_HEADER_LEN as u64).map_or(true, |end| end > file_len) {
        bail!("bloc hors de l'archive");
    }
    let mut h = [0u8; BLOCK_HEADER_LEN];
    read_exact_at(file, &mut h, offset)?;
    let header = BlockHeader::decode(&h)?;
    if header.comp_len > file_len - offset - BLOCK_HEADER_LEN as u64 {
        bail!("bloc tronqué");
    }
    let mut payload = vec![0u8; header.comp_len as usize];
    read_exact_at(file, &mut payload, offset + BLOCK_HEADER_LEN as u64)?;
    if crc32c::crc32c(&payload) != header.payload_crc {
        bail!("données du bloc endommagées");
    }
    Ok((header, payload))
}

/// Writes a variable-length integer (LEB128): 7 bits per byte, the high bit
/// says more bytes follow. Small numbers take 1 byte.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Sequential reading of a buffer, with overflow checks.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift > 63 {
                bail!("entier variable trop long");
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            bail!("index tronqué");
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_aller_retour() {
        for v in [0, 1, 127, 128, 300, 1 << 35, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(Cursor::new(&buf).varint().unwrap(), v);
        }
    }

    #[test]
    fn en_tete_de_bloc_aller_retour() {
        let h = BlockHeader { kind: BlockKind::Data, codec: Codec::Zstd, raw_len: 42, comp_len: 7, payload_crc: 99 };
        let d = BlockHeader::decode(&h.encode()).unwrap();
        assert_eq!((d.kind, d.codec, d.raw_len, d.comp_len, d.payload_crc), (h.kind, h.codec, 42, 7, 99));
    }

    #[test]
    fn en_tete_de_bloc_abime_refuse() {
        let h = BlockHeader { kind: BlockKind::Data, codec: Codec::Store, raw_len: 1, comp_len: 1, payload_crc: 0 };
        let mut b = h.encode();
        b[10] ^= 1;
        assert!(BlockHeader::decode(&b).is_err());
    }
}
