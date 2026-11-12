//! Archive index: the list of files, folders and links, and for each file
//! the place where its data lies (its "extents").
//!
//! The index is a journal: each compression or addition writes a **layer**
//! that holds only its novelties (blocks, entries, removed paths) and a link
//! to the previous layer. Each layer is a **version** of the archive; the
//! state of a version is obtained by applying the layers in order, from the
//! first one up to it.

use std::collections::HashSet;

use anyhow::{bail, Result};

use crate::format::{put_varint, Cursor};

const INDEX_VERSION: u64 = 2;

/// Length of a chunk hash: the first 16 bytes of BLAKE3. 128 bits are far
/// enough to detect corruption, and the hash weighed 80 % of the compressed
/// index (it does not compress).
pub const HASH_LEN: usize = 16;

/// Hash of a file chunk.
pub fn hash(data: &[u8]) -> [u8; HASH_LEN] {
    blake3::hash(data).as_bytes()[..HASH_LEN].try_into().unwrap()
}

/// Chunk of a file stored in a data block, with its hash: an error is
/// located to the nearest chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct Extent {
    pub block: u64,
    pub offset: u64,
    pub len: u64,
    pub hash: [u8; HASH_LEN],
}

/// File stored in transformed form (Deflate rebuilding, see rebuild.rs): its
/// extents describe the transformed form, `size` stays the size of the
/// original file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rebuilt {
    /// Size of the transformed form (sum of the extents).
    pub stored: u64,
    /// Hash of the original file, checked after rebuilding.
    pub hash: [u8; HASH_LEN],
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    File { size: u64, extents: Vec<Extent>, rebuilt: Option<Rebuilt> },
    Dir,
    Symlink { target: Vec<u8> },
}

impl Kind {
    /// Number of bytes stored in the blocks for this file.
    pub fn stored_len(&self) -> u64 {
        match self {
            Kind::File { rebuilt: Some(r), .. } => r.stored,
            Kind::File { size, .. } => *size,
            _ => 0,
        }
    }
}

/// Optional "tag + value" field (see meta.rs).
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub tag: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Relative path, as bytes, separated by "/".
    pub path: Vec<u8>,
    pub mode: u32,
    pub mtime_s: i64,
    pub mtime_ns: u32,
    pub kind: Kind,
    pub fields: Vec<Field>,
}

/// Complete state of a version: all entries and all blocks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Index {
    pub entries: Vec<Entry>,
    /// Position of each data block in the archive.
    pub blocks: Vec<u64>,
}

/// An index layer, as it is written in the archive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layer {
    /// Version number (1 for the first one).
    pub number: u64,
    /// Write date, in seconds since 1970.
    pub created: u64,
    /// Previous layer: position of the main index and of its copy.
    pub previous: Option<(u64, u64)>,
    /// Number of the first block added by this layer.
    pub first_block: u64,
    /// Position of the blocks added by this layer.
    pub blocks: Vec<u64>,
    /// Paths of the previous version removed (replaced) by this layer.
    pub removed: Vec<Vec<u8>>,
    /// Entries added by this layer.
    pub entries: Vec<Entry>,
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_varint(o, b.len() as u64);
    o.extend_from_slice(b);
}

fn get_bytes(c: &mut Cursor) -> Result<Vec<u8>> {
    let n = c.varint()? as usize;
    Ok(c.bytes(n)?.to_vec())
}

fn encode_entry(o: &mut Vec<u8>, e: &Entry) {
    o.push(match e.kind {
        Kind::File { rebuilt: None, .. } => 0,
        Kind::File { rebuilt: Some(_), .. } => 3,
        Kind::Dir => 1,
        Kind::Symlink { .. } => 2,
    });
    put_bytes(o, &e.path);
    put_varint(o, u64::from(e.mode));
    put_varint(o, zigzag(e.mtime_s));
    put_varint(o, u64::from(e.mtime_ns));
    match &e.kind {
        Kind::File { size, extents, rebuilt } => {
            put_varint(o, *size);
            if let Some(r) = rebuilt {
                o.extend_from_slice(&r.hash);
                put_varint(o, r.stored);
            }
            put_varint(o, extents.len() as u64);
            for x in extents {
                put_varint(o, x.block);
                put_varint(o, x.offset);
                put_varint(o, x.len);
                o.extend_from_slice(&x.hash);
            }
        }
        Kind::Dir => {}
        Kind::Symlink { target } => put_bytes(o, target),
    }
    put_varint(o, e.fields.len() as u64);
    for f in &e.fields {
        put_varint(o, f.tag);
        put_bytes(o, &f.data);
    }
}

fn decode_entry(c: &mut Cursor) -> Result<Entry> {
    let tag = c.u8()?;
    let path = get_bytes(c)?;
    let mode = c.varint()? as u32;
    let mtime_s = unzigzag(c.varint()?);
    let mtime_ns = c.varint()? as u32;
    let kind = match tag {
        0 | 3 => {
            let size = c.varint()?;
            let rebuilt = if tag == 3 {
                let hash: [u8; HASH_LEN] = c.bytes(HASH_LEN)?.try_into().unwrap();
                let stored = c.varint()?;
                if stored > crate::rebuild::MAX_STORED {
                    bail!("forme transformée trop grande");
                }
                Some(Rebuilt { stored, hash })
            } else {
                None
            };
            let n = c.varint()? as usize;
            // Capacity bounded by the remaining size: a booby-trapped index cannot make
            // gigabytes of memory be reserved.
            let mut extents = Vec::with_capacity(n.min(c.remaining()));
            let mut total = 0u64;
            for _ in 0..n {
                let (block, offset, len) = (c.varint()?, c.varint()?, c.varint()?);
                let hash: [u8; HASH_LEN] = c.bytes(HASH_LEN)?.try_into().unwrap();
                total = total.checked_add(len).ok_or_else(|| anyhow::anyhow!("taille incohérente"))?;
                extents.push(Extent { block, offset, len, hash });
            }
            if total != rebuilt.map_or(size, |r| r.stored) {
                bail!("taille de fichier incohérente dans l'index");
            }
            Kind::File { size, extents, rebuilt }
        }
        1 => Kind::Dir,
        2 => Kind::Symlink { target: get_bytes(c)? },
        v => bail!("type d'entrée inconnu ({v})"),
    };
    let nfields = c.varint()? as usize;
    let mut fields = Vec::with_capacity(nfields.min(c.remaining()));
    for _ in 0..nfields {
        let tag = c.varint()?;
        fields.push(Field { tag, data: get_bytes(c)? });
    }
    Ok(Entry { path, mode, mtime_s, mtime_ns, kind, fields })
}

impl Layer {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        put_varint(&mut o, INDEX_VERSION);
        put_varint(&mut o, self.number);
        put_varint(&mut o, self.created);
        let (prev, prev_backup) = self.previous.unwrap_or((0, 0));
        put_varint(&mut o, prev);
        put_varint(&mut o, prev_backup);
        put_varint(&mut o, self.first_block);
        put_varint(&mut o, self.blocks.len() as u64);
        for &b in &self.blocks {
            put_varint(&mut o, b);
        }
        put_varint(&mut o, self.removed.len() as u64);
        for p in &self.removed {
            put_bytes(&mut o, p);
        }
        put_varint(&mut o, self.entries.len() as u64);
        for e in &self.entries {
            encode_entry(&mut o, e);
        }
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(buf);
        let version = c.varint()?;
        if version != INDEX_VERSION {
            bail!("version d'index inconnue ({version})");
        }
        let number = c.varint()?;
        let created = c.varint()?;
        let (prev, prev_backup) = (c.varint()?, c.varint()?);
        let previous = (prev != 0).then_some((prev, prev_backup));
        if number == 0 || (number == 1) != previous.is_none() {
            bail!("numéro de version incohérent");
        }
        let first_block = c.varint()?;
        let nblocks = c.varint()? as usize;
        let mut blocks = Vec::with_capacity(nblocks.min(c.remaining()));
        for _ in 0..nblocks {
            blocks.push(c.varint()?);
        }
        let nremoved = c.varint()? as usize;
        let mut removed = Vec::with_capacity(nremoved.min(c.remaining()));
        for _ in 0..nremoved {
            removed.push(get_bytes(&mut c)?);
        }
        let nentries = c.varint()? as usize;
        let mut entries = Vec::with_capacity(nentries.min(c.remaining()));
        for _ in 0..nentries {
            entries.push(decode_entry(&mut c)?);
        }
        Ok(Self { number, created, previous, first_block, blocks, removed, entries })
    }
}

/// Applies the layers in order and returns the resulting state.
pub fn merge(layers: &[Layer]) -> Result<Index> {
    let mut blocks: Vec<u64> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    for layer in layers {
        if layer.first_block != blocks.len() as u64 {
            bail!("chaîne d'index incohérente (version {})", layer.number);
        }
        blocks.extend(&layer.blocks);
        if !layer.removed.is_empty() {
            let removed: HashSet<&[u8]> = layer.removed.iter().map(Vec::as_slice).collect();
            entries.retain(|e| !removed.contains(e.path.as_slice()));
        }
        entries.extend(layer.entries.iter().cloned());
    }
    for e in &entries {
        if let Kind::File { extents, .. } = &e.kind {
            if extents.iter().any(|x| x.block >= blocks.len() as u64) {
                bail!("extent vers un bloc inexistant");
            }
        }
    }
    Ok(Index { entries, blocks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, block: u64) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            mode: 0o644,
            mtime_s: -3,
            mtime_ns: 0,
            kind: Kind::File {
                size: 10,
                extents: vec![Extent { block, offset: 0, len: 10, hash: [7; HASH_LEN] }],
                rebuilt: None,
            },
            fields: vec![],
        }
    }

    #[test]
    fn couche_aller_retour() {
        let layer = Layer {
            number: 2,
            created: 1_760_000_000,
            previous: Some((5000, 6000)),
            first_block: 1,
            blocks: vec![7000],
            removed: vec![b"docs/a.txt".to_vec()],
            entries: vec![
                Entry {
                    path: b"docs".to_vec(),
                    mode: 0o755,
                    mtime_s: 1_760_000_000,
                    mtime_ns: 5,
                    kind: Kind::Dir,
                    fields: vec![Field { tag: 3, data: b"nicolas".to_vec() }],
                },
                file("docs/a.txt", 1),
                Entry {
                    path: b"docs/b.pdf".to_vec(),
                    mode: 0o644,
                    mtime_s: 1,
                    mtime_ns: 0,
                    kind: Kind::File {
                        size: 5000,
                        extents: vec![Extent { block: 1, offset: 10, len: 9000, hash: [8; HASH_LEN] }],
                        rebuilt: Some(Rebuilt { stored: 9000, hash: [9; HASH_LEN] }),
                    },
                    fields: vec![],
                },
                Entry {
                    path: b"lien".to_vec(),
                    mode: 0o777,
                    mtime_s: 0,
                    mtime_ns: 0,
                    kind: Kind::Symlink { target: b"docs".to_vec() },
                    // Unknown tag from a future version: kept as is.
                    fields: vec![Field { tag: 999, data: vec![1, 2, 3] }],
                },
            ],
        };
        assert_eq!(Layer::decode(&layer.encode()).unwrap(), layer);
    }

    #[test]
    fn couche_tronquee_refusee() {
        let layer = Layer { number: 1, first_block: 0, blocks: vec![16], ..Default::default() };
        let buf = layer.encode();
        assert!(Layer::decode(&buf[..buf.len() - 1]).is_err());
    }

    #[test]
    fn versions_successives() {
        let v1 = Layer { number: 1, blocks: vec![16], entries: vec![file("a", 0), file("b", 0)], ..Default::default() };
        let v2 = Layer {
            number: 2,
            previous: Some((100, 200)),
            first_block: 1,
            blocks: vec![300],
            removed: vec![b"a".to_vec()],
            entries: vec![file("a", 1), file("c", 1)],
            ..Default::default()
        };
        let old = merge(&[v1.clone()]).unwrap();
        assert_eq!(old.entries.len(), 2);
        let new = merge(&[v1, v2]).unwrap();
        let paths: Vec<&[u8]> = new.entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, [&b"b"[..], b"a", b"c"]);
        assert_eq!(new.blocks, [16, 300]);
    }
}
