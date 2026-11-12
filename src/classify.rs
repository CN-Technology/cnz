//! The dispatcher: recognise each file by its contents (not by its
//! extension) to store it next to similar ones.
//!
//! - the first bytes give the type (known signatures: JPEG, ZIP…);
//! - otherwise, we check whether it is text;
//! - otherwise, a quick compression test on the start of the file tells
//!   whether anything is left to gain.
//!
//! For text, a similarity fingerprint (MinHash over lines) puts side by side
//! files that share many lines even when they have different names.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Number of bytes read at the start of each file to recognise it.
pub const HEAD_LEN: usize = 64 << 10;
/// Maximum size read for the similarity fingerprint of a text.
pub const SIMILARITY_LEN: usize = 256 << 10;

/// Family of a file. The order of the variants is the storage order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// Text (code, HTML, CSV…).
    Text,
    /// Document mixing text and compressed data (PDF).
    Document,
    /// Program (ELF, Windows, macOS).
    Executable,
    /// Other binary data (databases, raw images…).
    Binary,
    /// Already compressed content (photos, videos, music, archives, web
    /// fonts…): zstd gains almost nothing on it.
    Compressed,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Text => "texte",
            Family::Document => "document",
            Family::Executable => "programme",
            Family::Binary => "binaire",
            Family::Compressed => "déjà compressé",
        }
    }
}

/// Recognises the family of a file from its first bytes.
pub fn detect(head: &[u8]) -> Family {
    if let Some(f) = by_signature(head) {
        return f;
    }
    if is_text(head) {
        return Family::Text;
    }
    if head.len() >= 4096 && !compressible(head) {
        return Family::Compressed;
    }
    Family::Binary
}

/// Signatures ("magic numbers") of common formats.
fn by_signature(h: &[u8]) -> Option<Family> {
    use Family::*;
    let starts = |s: &[u8]| h.starts_with(s);
    let at = |pos: usize, s: &[u8]| h.get(pos..pos + s.len()) == Some(s);
    let compressed: &[&[u8]] = &[
        b"\xFF\xD8\xFF",                      // JPEG
        b"\x89PNG\r\n\x1a\n",                 // PNG
        b"GIF87a",                            // GIF
        b"GIF89a",                            //
        b"PK\x03\x04",                        // ZIP, Office, OpenDocument, JAR, APK, EPUB
        b"PK\x05\x06",                        // empty ZIP
        b"\x1F\x8B",                          // gzip
        b"BZh",                               // bzip2
        b"\xFD7zXZ\x00",                      // xz
        b"7z\xBC\xAF\x27\x1C",                // 7-Zip
        b"\x28\xB5\x2F\xFD",                  // zstd
        b"Rar!\x1A\x07",                      // RAR
        b"\x04\x22\x4D\x18",                  // LZ4
        b"\x89CNZ\r\n\x1a\n",                 // CNz
        b"wOFF",                              // WOFF font
        b"wOF2",                              // WOFF2 font
        b"\x1A\x45\xDF\xA3",                  // Matroska, WebM
        b"OggS",                              // Ogg
        b"fLaC",                              // FLAC
        b"ID3",                               // MP3
        b"\x00\x00\x00\x0CjP  \r\n\x87\n",    // JPEG 2000
        b"\xFF\x0A",                          // JPEG XL
        b"\x00\x00\x00\x0CJXL \r\n\x87\n",    // JPEG XL (container)
    ];
    if compressed.iter().any(|s| starts(s)) {
        return Some(Compressed);
    }
    // RIFF: WebP and AVI are compressed, WAV is not.
    if starts(b"RIFF") && (at(8, b"WEBP") || at(8, b"AVI ")) {
        return Some(Compressed);
    }
    // MP4, MOV, HEIC, AVIF, 3GP: "ftyp" box at the start.
    if at(4, b"ftyp") {
        return Some(Compressed);
    }
    // MP3 frame without a tag.
    if h.len() >= 2 && h[0] == 0xFF && matches!(h[1], 0xFB | 0xF3 | 0xF2) {
        return Some(Compressed);
    }
    if starts(b"%PDF-") {
        return Some(Document);
    }
    let executable: &[&[u8]] = &[
        b"\x7FELF",
        b"MZ",
        b"\xFE\xED\xFA\xCE",
        b"\xFE\xED\xFA\xCF",
        b"\xCE\xFA\xED\xFE",
        b"\xCF\xFA\xED\xFE",
        b"\0asm", // WebAssembly
    ];
    if executable.iter().any(|s| starts(s)) {
        return Some(Executable);
    }
    None
}

/// Text: no null byte, valid UTF-8 (the last character may be cut by the
/// read limit), few control characters.
fn is_text(h: &[u8]) -> bool {
    if h.is_empty() || h.contains(&0) {
        return false;
    }
    let valid = match std::str::from_utf8(h) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && h.len() - e.valid_up_to() < 4,
    };
    if !valid {
        // Latin-1 or Windows-1252 text: tolerated if there are almost no control
        // characters.
        let control = h.iter().filter(|&&c| c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r' | 0x0C)).count();
        return control * 100 < h.len();
    }
    let control = h.iter().filter(|&&c| c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B)).count();
    control * 100 < h.len()
}

/// Quick compression test: does the content gain at least 3 %?
pub fn compressible(sample: &[u8]) -> bool {
    match zstd::bulk::compress(sample, 1) {
        Ok(c) => (c.len() as u64) * 100 < (sample.len() as u64) * 97,
        Err(_) => true,
    }
}

/// Number of values in the similarity fingerprint.
pub const SIGNATURE_LEN: usize = 4;

/// Similarity fingerprint of a text (MinHash): for each of `SIGNATURE_LEN`
/// hash functions, the smallest value obtained over the lines of the file.
/// Two files that share many lines are likely to have the same values. Lines
/// that are too short (" }", empty lines) do not count: every file has them.
pub fn signature(text: &[u8]) -> [u64; SIGNATURE_LEN] {
    let mut sig = [u64::MAX; SIGNATURE_LEN];
    for line in text.split(|&c| c == b'\n') {
        let line = line.trim_ascii();
        if line.len() < 8 {
            continue;
        }
        let h = xxh(line);
        for (k, s) in sig.iter_mut().enumerate() {
            // Hash functions derived by mixing (odd multiplication).
            let v = mix(h ^ SEEDS[k]);
            *s = (*s).min(v);
        }
    }
    sig
}

const SEEDS: [u64; SIGNATURE_LEN] = [0x9E37_79B9_7F4A_7C15, 0xC2B2_AE3D_27D4_EB4F, 0x1656_67B1_9E37_79F9, 0x27D4_EB2F_1656_67C5];

fn xxh(data: &[u8]) -> u64 {
    // 64-bit FNV-1a, enough for lines.
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01B3);
    }
    h
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

/// Profile of a file: its family and, for a text, its fingerprint.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub family: Family,
    pub signature: [u64; SIGNATURE_LEN],
    /// BLAKE3 hash of what was read (None: unreadable file). For a file read in
    /// full, this is the hash of the file: duplicate detection does not need to
    /// read it again.
    pub digest: Option<Digest>,
}

/// Hash of the start of a file, or of the whole file if `whole`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Digest {
    pub hash: [u8; 32],
    pub whole: bool,
}

/// Reads the start of the file and establishes its profile. An unreadable
/// file is stored as binary: the error will be reported when it is read for
/// real.
pub fn profile(path: &Path, size: u64) -> Profile {
    let mut buf = Vec::new();
    let want = (size as usize).min(SIMILARITY_LEN);
    let read = File::open(path).and_then(|f| f.take(want as u64).read_to_end(&mut buf));
    if read.is_err() {
        return Profile { family: Family::Binary, signature: [u64::MAX; SIGNATURE_LEN], digest: None };
    }
    let mut family = detect(&buf[..buf.len().min(HEAD_LEN)]);
    if family == Family::Compressed && size >= PROBE_MIN {
        // A compressed format is not always compressed well (lightly compressed
        // PNG, Git packs…): test in the middle of the file, far from headers and
        // thumbnails. If there is something left to gain, the file follows the
        // normal stream.
        if let Some(sample) = sample_middle(path, size) {
            if compressible(&sample) {
                family = Family::Binary;
            }
        }
    }
    let signature = if family == Family::Text { signature(&buf) } else { [u64::MAX; SIGNATURE_LEN] };
    let digest = Some(Digest { hash: *blake3::hash(&buf).as_bytes(), whole: buf.len() as u64 == size });
    Profile { family, signature, digest }
}

/// Groups similar files: two files with at least one matching fingerprint
/// value go into the same group. Groups keep the order of their first
/// member, and each group the order of its members: a file that resembles no
/// other does not move.
/// Returns the new order, as positions in `signatures`.
pub fn group_similar(signatures: &[[u64; SIGNATURE_LEN]]) -> Vec<usize> {
    fn root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..signatures.len()).collect();
    for k in 0..SIGNATURE_LEN {
        let mut seen = std::collections::HashMap::new();
        for (pos, sig) in signatures.iter().enumerate() {
            if sig[k] == u64::MAX {
                continue;
            }
            if let Some(&other) = seen.get(&sig[k]) {
                let (a, b) = (root(&mut parent, pos), root(&mut parent, other));
                parent[a.max(b)] = a.min(b);
            } else {
                seen.insert(sig[k], pos);
            }
        }
    }
    let mut keyed: Vec<(usize, usize)> = (0..signatures.len()).map(|pos| (root(&mut parent, pos), pos)).collect();
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, pos)| pos).collect()
}

/// Below this size, the middle test is not worth it.
const PROBE_MIN: u64 = 16 << 10;
const PROBE_LEN: u64 = 64 << 10;

fn sample_middle(path: &Path, size: u64) -> Option<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let len = PROBE_LEN.min(size);
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start((size - len) / 2)).ok()?;
    let mut buf = Vec::with_capacity(len as usize);
    f.take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        assert_eq!(detect(b"\xFF\xD8\xFF\xE0\x00\x10JFIF"), Family::Compressed);
        assert_eq!(detect(b"PK\x03\x04\x14\x00"), Family::Compressed);
        assert_eq!(detect(b"\x00\x00\x00\x20ftypisom"), Family::Compressed);
        assert_eq!(detect(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Family::Compressed);
        assert_eq!(detect(b"%PDF-1.7\n"), Family::Document);
        assert_eq!(detect(b"\x7FELF\x02\x01\x01"), Family::Executable);
        assert_eq!(detect(b"<?php\necho 'bonjour';\n"), Family::Text);
        assert_eq!(detect("café crème\n".as_bytes()), Family::Text);
        // Latin-1: "café" without UTF-8.
        assert_eq!(detect(b"caf\xE9 cr\xE8me\n"), Family::Text);
        assert_eq!(detect(b"SQLite format 3\x00\x10\x00"), Family::Binary);
    }

    #[test]
    fn contenu_aleatoire_reconnu_comme_compresse() {
        // Pseudo-random sequence: incompressible, no signature.
        let mut x = 0x1234_5678u64;
        let data: Vec<u8> = (0..HEAD_LEN)
            .map(|_| {
                x = mix(x.wrapping_add(1));
                x as u8
            })
            .collect();
        assert_eq!(detect(&data), Family::Compressed);
        // Zeros: highly compressible binary.
        assert_eq!(detect(&vec![0u8; HEAD_LEN]), Family::Binary);
    }

    #[test]
    fn ressemblance() {
        let a = b"use App\\Models\\User;\nuse Illuminate\\Http\\Request;\nclass UserController extends Controller\n";
        let b = b"use App\\Models\\User;\nuse Illuminate\\Http\\Request;\nclass UserController extends Controller\n}\n";
        let c = b"body { margin: 0 auto; }\n.titre { color: #333333; }\n";
        assert_eq!(signature(a), signature(b));
        assert_ne!(signature(a), signature(c));
    }

    #[test]
    fn groupes() {
        let rien = [u64::MAX; SIGNATURE_LEN];
        let (x, y) = ([1, 2, 3, 4], [9, 9, 9, 9]);
        // 0 and 2 are similar (one common value), 3 too through 2: a single group.
        let sigs = [x, y, [1, 7, 7, 7], [8, 8, 8, 7], rien, rien];
        assert_eq!(group_similar(&sigs), vec![0, 2, 3, 1, 4, 5]);
    }
}
