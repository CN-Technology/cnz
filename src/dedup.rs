//! Deduplication: store only once what repeats.
//!
//! 1. **Identical whole files**: the dispatcher already has the hash of
//!    small files (read in full) and of the start of large ones; only large
//!    files with the same size and the same start as another one are read
//!    again. A copy reuses the original's extents: it costs nothing more.
//! 2. **Chunks of large files** (normal stream, 1 MB and more): each file
//!    is cut according to its contents (FastCDC, ~64 KB); a chunk already
//!    seen refers to the place where it is stored. Photos and videos are not
//!    concerned: their duplicates are whole files, and reading them again in
//!    full would be costly for nothing.
//!
//! Decisions rely on a full BLAKE3 (32 bytes); the index then keeps only 16
//! bytes per chunk, for verification.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use rayon::prelude::*;

use crate::classify::Digest;
use crate::progress::Progress;

/// Size from which a file is cut into chunks.
pub const CHUNK_MIN_FILE: u64 = 1 << 20;
const CHUNK_MIN: u32 = 16 << 10;
const CHUNK_AVG: u32 = 64 << 10;
const CHUNK_MAX: u32 = 256 << 10;

/// Chunk of a large file.
#[derive(Clone, Copy, Debug)]
pub struct Chunk {
    pub offset: u64,
    pub len: u64,
    pub hash: [u8; 32],
}

#[derive(Default)]
pub struct Found {
    /// Duplicate file → its original.
    pub copies: Vec<(usize, usize)>,
    /// Large files cut into chunks.
    pub chunks: HashMap<usize, Vec<Chunk>>,
}

/// Looks for identical files among `normal` and `compressed` (storage order:
/// the first one met is the original), removes them from these lists, then
/// cuts the large files of the normal stream.
/// `files[i]`: path, size and hash taken by the dispatcher for entry i
/// (None: not a file).
pub fn find(
    files: &[Option<(PathBuf, u64, Option<Digest>)>],
    normal: &mut Vec<usize>,
    compressed: &mut Vec<usize>,
    progress: &Progress,
) -> Result<Found> {
    let mut found = Found::default();
    let size = |i: usize| files[i].as_ref().map_or(0, |f| f.1);

    // 1. Whole files. Comparison key: size and dispatcher hash. For a small
    // file, read in full, it is enough; for a large one, it is the hash of the
    // start: only those with the same size and the same start as another one
    // are read again in full.
    let ordered: Vec<usize> = normal.iter().chain(compressed.iter()).copied().collect();
    let key = |i: usize| match &files[i] {
        Some((_, len, Some(d))) if *len > 0 => Some((*len, *d)),
        _ => None,
    };
    let mut count: HashMap<(u64, Digest), u32> = HashMap::new();
    for k in ordered.iter().filter_map(|&i| key(i)) {
        *count.entry(k).or_default() += 1;
    }
    let candidates: Vec<usize> = ordered.iter().copied().filter(|&i| key(i).is_some_and(|k| count[&k] > 1)).collect();
    let to_reread: Vec<usize> = candidates.iter().copied().filter(|&i| !key(i).unwrap().1.whole).collect();
    let to_read: u64 = to_reread.iter().map(|&i| size(i)).sum::<u64>()
        + normal.iter().map(|&i| size(i)).filter(|&s| s >= CHUNK_MIN_FILE).sum::<u64>();
    progress.start("analyse", to_read);

    let full: HashMap<usize, Option<[u8; 32]>> = to_reread
        .par_iter()
        .map(|&i| {
            let (path, len, _) = files[i].as_ref().unwrap();
            Ok((i, hash_file(path, *len, progress)?))
        })
        .collect::<Result<_>>()?;
    let mut original: HashMap<(u64, [u8; 32]), usize> = HashMap::new();
    let mut is_copy = vec![false; files.len()];
    // `candidates` follows the storage order: the first one is the original.
    for &i in &candidates {
        let (len, digest) = key(i).unwrap();
        let hash = if digest.whole { digest.hash } else {
            // Unreadable file: it will be reported when it is read for real.
            let Some(h) = full[&i] else { continue };
            h
        };
        match original.get(&(len, hash)) {
            Some(&o) => {
                found.copies.push((i, o));
                is_copy[i] = true;
            }
            None => {
                original.insert((len, hash), i);
            }
        }
    }
    normal.retain(|&i| !is_copy[i]);
    compressed.retain(|&i| !is_copy[i]);

    // 2. Chunks of the large files of the normal stream.
    let big: Vec<usize> = normal.iter().copied().filter(|&i| size(i) >= CHUNK_MIN_FILE).collect();
    let chunked: Vec<Option<Vec<Chunk>>> = big
        .par_iter()
        .map(|&i| {
            let (path, len, _) = files[i].as_ref().unwrap();
            chunk_file(path, *len, progress)
        })
        .collect::<Result<_>>()?;
    for (i, c) in big.into_iter().zip(chunked) {
        if let Some(c) = c {
            found.chunks.insert(i, c);
        }
    }
    Ok(found)
}

/// Hash of a whole file; None if it is unreadable or changed size.
fn hash_file(path: &PathBuf, len: u64, progress: &Progress) -> Result<Option<[u8; 32]>> {
    let Ok(mut f) = File::open(path) else { return Ok(None) };
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; (len as usize).min(1 << 20)];
    let mut total = 0u64;
    loop {
        progress.check()?;
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        hasher.update(&buf[..n]);
        total += n as u64;
        progress.advance(n as u64);
    }
    Ok((total == len).then(|| *hasher.finalize().as_bytes()))
}

/// Cuts a file into chunks according to its contents; None if it is
/// unreadable or changed size.
fn chunk_file(path: &PathBuf, len: u64, progress: &Progress) -> Result<Option<Vec<Chunk>>> {
    let Ok(f) = File::open(path) else { return Ok(None) };
    let mut chunks = Vec::new();
    let mut total = 0u64;
    for c in fastcdc::v2020::StreamCDC::new(f, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
        progress.check()?;
        let Ok(c) = c else { return Ok(None) };
        chunks.push(Chunk { offset: c.offset, len: c.length as u64, hash: *blake3::hash(&c.data).as_bytes() });
        total += c.length as u64;
        progress.advance(c.length as u64);
    }
    Ok((total == len).then_some(chunks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn doublons_et_morceaux() {
        let dir = std::env::temp_dir().join(format!("cnz-dedup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut x = 7u64;
        let gros: Vec<u8> = (0..3 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        // b = a; c = start of a + something else; d = same size as a, different contents.
        let mut c = gros[..2 << 20].to_vec();
        c.extend_from_slice(&[1u8; 1 << 20]);
        let mut d = gros.clone();
        d[0] ^= 1;
        let paths: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(|n| dir.join(n)).collect();
        for (p, data) in paths.iter().zip([&gros, &gros, &c, &d]) {
            fs::write(p, data).unwrap();
        }
        let files: Vec<Option<(PathBuf, u64, Option<Digest>)>> = paths
            .iter()
            .map(|p| {
                let len = fs::metadata(p).unwrap().len();
                Some((p.clone(), len, crate::classify::profile(p, len).digest))
            })
            .collect();
        let (mut normal, mut compressed) = (vec![0, 1, 2, 3], vec![]);
        let found = find(&files, &mut normal, &mut compressed, &Progress::default()).unwrap();
        assert_eq!(found.copies, vec![(1, 0)]);
        assert_eq!(normal, vec![0, 2, 3]);
        // The start of c is cut like the start of a.
        let ha: Vec<_> = found.chunks[&0].iter().map(|c| c.hash).collect();
        let shared = found.chunks[&2].iter().filter(|c| ha.contains(&c.hash)).map(|c| c.len).sum::<u64>();
        assert!(shared > 3 << 19, "{shared}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
