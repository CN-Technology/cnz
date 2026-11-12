//! Deflate rebuilding (stage 4).
//!
//! A PDF, an Office document, a ZIP or a .gz file contains Deflate-compressed
//! streams, on which zstd gains almost nothing. When a stream can be
//! recreated **byte for byte** by recompressing its contents with zlib (the
//! same level and memory as the program that wrote it), the decompressed
//! contents can be stored instead, which zstd compresses much better, and
//! the stream is rebuilt at extraction.
//!
//! The file is then stored in a **transformed form**: a recipe ("so many
//! bytes as they are, then a Deflate stream of so many decompressed bytes,
//! level 6, memory 8, window 15…"), followed by the data of each segment.
//! Only the Deflate core of the streams is redone by zlib: the surrounding
//! headers and checksums (zlib, gzip) are copied as they are.
//!
//! Safety: the writer performs the whole rebuild and compares it with the
//! original file before choosing the transformed form; the reader checks the
//! hash of the rebuilt file. At the slightest difference, the file is stored
//! as is.
//!
//! The result depends on zlib's exact algorithm: CNz ships its own zlib
//! (pinned version, see the `zlib_figee` test).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, ensure, Result};
use rayon::prelude::*;

use crate::format::{put_varint, Cursor};
use crate::index::{self, HASH_LEN};
use crate::pngmodel::{self, Image};

/// Maximum size of a file to analyse: the file, its transformed form and the
/// rebuilt file are in memory at the same time.
pub const MAX_FILE: u64 = 64 << 20;
/// Maximum size of a transformed form (the reader refuses beyond it).
pub const MAX_STORED: u64 = 1 << 30;
const VERSION: u64 = 1;
/// zlib version whose algorithm is the reference for rebuilding.
pub const ZLIB_VERSION: &str = "1.3.2";
/// After this many streams without any success, the file is given up: its
/// program does not use zlib.
const GIVE_UP: usize = 4;
/// Streams too small to be worth a recipe line.
const MIN_STREAM: usize = 32;

/// A segment of the transformed form.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    /// Bytes copied as they are.
    Copy { len: u64 },
    /// Raw Deflate stream redone by zlib from `raw_len` bytes.
    Deflate { raw_len: u64, params: Params },
    /// Deflate stream from another program (stage 4b): `raw_len` bytes of data
    /// and `corr_len` bytes of predictor corrections (predict.rs), which give
    /// back the tokens; the rewriter (deflate.rs) rebuilds the stream.
    Predicted { raw_len: u64, corr_len: u64 },
    /// Recompressed JPEG photo (stage 5): the whole file, `len` bytes (headers,
    /// end, exceptions and re-coded coefficients, see jpeg_payload).
    Jpeg { len: u64 },
    /// PNG image: the whole file, `len` bytes (IDAT chunk split and transformed
    /// form of the "glued" file, see png_recipe).
    Png { len: u64 },
    /// Deflate stream of PNG pixels redone by zlib, pixels coded by the image
    /// model (pngmodel.rs): `coded_len` bytes.
    ImageDeflate { raw_len: u64, params: Params, image: Image, coded_len: u64 },
    /// Deflate stream of PNG pixels redone by the predictor, pixels coded by the
    /// image model: `coded_len` bytes, then `corr_len` of corrections.
    ImagePredicted { raw_len: u64, corr_len: u64, image: Image, coded_len: u64 },
    /// Embedded file, stored uncompressed inside another one (photo or PNG in an
    /// Office document): its own transformed form, `len` bytes, which gives back
    /// `size` bytes.
    Embedded { len: u64, size: u64 },
    /// Deflate stream redone by zlib whose contents (photo or PNG) are described
    /// by their own transformed form (`len` bytes): the same image thus has the
    /// same form, whether stored as is or compressed in the document.
    DeflateNested { raw_len: u64, params: Params, len: u64 },
    /// Same for a stream redone by the predictor: `len` bytes of transformed
    /// form of the contents, then `corr_len` of corrections.
    PredictedNested { raw_len: u64, corr_len: u64, len: u64 },
    /// TrueType font (stage 6): the whole file, `len` bytes (glyphs stored in
    /// separate streams, see font.rs).
    Font { len: u64 },
    /// WAV sound (stage 6): the whole file, `len` bytes (differences from the
    /// prediction, see audio.rs).
    Audio { len: u64 },
    /// x86 program (stage 6): the whole file, `len` bytes (BCJ filter, see
    /// bcj.rs).
    Code { len: u64 },
}

/// zlib settings that reproduce a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Level, from 1 to 9.
    pub level: u8,
    /// Memory (memLevel), from 1 to 9.
    pub mem: u8,
    /// Window size in bits, from 9 to 15.
    pub wbits: u8,
    /// Compression in slices (0: in one go). Each slice of this size is
    /// compressed separately, with the last 2^wbits bytes of the previous one as
    /// a dictionary, and ended by Z_SYNC_FLUSH; this is how LibreOffice
    /// compresses large files, on several cores.
    pub chunk: u32,
}

/// Possible slices: from 4 KB to 16 MB.
const CHUNK_RANGE: std::ops::RangeInclusive<u32> = 4096..=(16 << 20);

impl Params {
    fn valid(self) -> bool {
        (1..=9).contains(&self.level)
            && (1..=9).contains(&self.mem)
            && (9..=15).contains(&self.wbits)
            && (self.chunk == 0 || CHUNK_RANGE.contains(&self.chunk))
    }
}

/// Recipe of a file: its segments and their place in the original file.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    /// Segment, position and length in the original file.
    pub segments: Vec<(Segment, usize, usize)>,
    /// Corrections of each predicted stream, in order, compressed (zstd 1): kept
    /// since the analysis so the predictor does not have to run again.
    pub corrections: Vec<Vec<u8>>,
    /// Recompressed photo (Jpeg segment): kept since the analysis; the writer
    /// puts it aside in a temporary file (a photo weighs a lot).
    pub jpeg: Option<Vec<u8>>,
    /// PNG image: split of the IDAT chunks and recipe of the glued file.
    pub png: Option<Box<(PngLayout, Recipe)>>,
    /// Recipes of the embedded files (Embedded segments), in order.
    pub embedded: Vec<Recipe>,
}

/// Split of a PNG: position of the first IDAT chunk, data length of each
/// chunk, size of the glued file.
#[derive(Clone, Debug, PartialEq)]
pub struct PngLayout {
    first: usize,
    lens: Vec<u32>,
    virtual_len: usize,
}

/// File that gains from being stored in transformed form.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub recipe: Recipe,
    /// Size of the transformed form.
    pub stored: u64,
    /// Hash of the original file.
    pub hash: [u8; HASH_LEN],
}

/// Analyses a file: returns its recipe if at least one stream can be
/// reproduced, and if the complete rebuild gives back exactly the file.
/// `photos`: also recompress JPEG photos.
pub fn prepare(path: &Path, size: u64, photos: bool) -> Option<Prepared> {
    // A zlib other than ours (built with the system zlib) would produce other
    // bytes: nothing is transformed.
    if size > MAX_FILE || size < 64 {
        return None;
    }
    let mut f = File::open(path).ok()?;
    // The first bytes are enough to rule out formats without streams to redo.
    let mut head = [0u8; 5];
    f.read_exact(&mut head).ok()?;
    let deflate = head.starts_with(b"%PDF-")
        || head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"\x1F\x8B\x08")
        || head.starts_with(b"\x89PNG\r")
        // PHP archive (PHAR): starts with its PHP stub.
        || head.starts_with(b"#!")
        || head.starts_with(b"<?");
    let photo = photos && head.starts_with(&[0xFF, 0xD8, 0xFF]);
    // Fonts, WAV sound, programs: transformed as a whole (stage 6).
    let whole = crate::font::is_truetype(&head)
        || head.starts_with(b"RIFF")
        || head.starts_with(b"\x7FELF")
        || head.starts_with(b"MZ");
    if !photo && !whole && !(deflate && zlib::version() == ZLIB_VERSION) {
        return None;
    }
    let mut data = head.to_vec();
    data.reserve(size as usize);
    f.take(size - 5).read_to_end(&mut data).ok()?;
    if data.len() as u64 != size {
        return None;
    }
    let recipe = analyze(&data)?;
    let stored = transform(&data, &recipe).ok()?;
    // Full verification, with the reader's own code.
    if rebuild(&stored, size).ok()? != data {
        return None;
    }
    Some(Prepared { recipe, stored: stored.len() as u64, hash: index::hash(&data) })
}

/// Looks for the Deflate streams of the file according to its format, and
/// the zlib settings that reproduce them.
pub fn analyze(data: &[u8]) -> Option<Recipe> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return jpeg_recipe(data);
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png_recipe(data);
    }
    // Whole-file transforms (stage 6).
    let whole = if crate::font::is_truetype(data) {
        crate::font::encode(data).map(|t| Segment::Font { len: t.len() as u64 })
    } else if data.starts_with(b"RIFF") {
        crate::audio::encode(data).map(|t| Segment::Audio { len: t.len() as u64 })
    } else if data.starts_with(b"\x7FELF") || data.starts_with(b"MZ") {
        crate::bcj::encode(data).map(|t| Segment::Code { len: t.len() as u64 })
    } else {
        None
    };
    if let Some(seg) = whole {
        let seg = (seg, 0, data.len());
        return Some(Recipe { segments: vec![seg], corrections: Vec::new(), jpeg: None, png: None, embedded: Vec::new() });
    }
    let candidates = if data.starts_with(b"%PDF-") {
        // Deflate streams, and photos stored as they are (DCTDecode).
        let r = analyze_streams(data, pdf_streams(data), None);
        return with_inside(data, r, pdf_photos(data));
    } else if data.starts_with(b"PK\x03\x04") {
        // Deflate streams, and photos and PNGs stored as they are (Office documents).
        let r = analyze_streams(data, zip_streams(data), None);
        return with_inside(data, r, zip_stored(data));
    } else if data.starts_with(b"\x1F\x8B\x08") {
        gzip_streams(data)
    } else if data.starts_with(b"#!") || data.starts_with(b"<?") {
        phar_streams(data)
    } else {
        return None;
    };
    if candidates.is_empty() {
        return None;
    }
    analyze_streams(data, candidates, None)
}

/// Adds to a recipe the embedded files (photos, PNGs) stored as they are at
/// the positions `regions`.
fn with_inside(data: &[u8], r: Option<Recipe>, regions: Vec<(usize, usize)>) -> Option<Recipe> {
    let inside: Vec<(usize, usize, Recipe)> = regions
        .into_iter()
        .filter_map(|(start, len)| {
            let part = &data[start..start + len];
            let isnt_nested = |r: &Recipe| r.segments.iter().all(|s| !has_inner(s.0));
            analyze(part).filter(isnt_nested).map(|r| (start, len, r))
        })
        .collect();
    if inside.is_empty() {
        return r;
    }
    let base = r.unwrap_or_else(|| Recipe {
        segments: vec![(Segment::Copy { len: data.len() as u64 }, 0, data.len())],
        corrections: Vec::new(),
        jpeg: None,
        png: None,
        embedded: Vec::new(),
    });
    merge_embedded(data, base, inside)
}

/// JPEG photos stored as they are inside a PDF (DCTDecode streams): from the
/// start of the stream up to the `endstream` keyword, line ending excluded.
fn pdf_photos(d: &[u8]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut i = 0;
    while let Some(p) = memfind(&d[i..], b"stream") {
        let mut s = i + p + 6;
        i = s;
        if d.get(s) == Some(&b'\r') {
            s += 1;
        }
        if d.get(s) != Some(&b'\n') {
            continue;
        }
        s += 1;
        if d.get(s..s + 3) != Some(&[0xFF, 0xD8, 0xFF]) {
            continue;
        }
        let Some(e) = memfind(&d[s..], b"endstream").map(|e| s + e) else { break };
        let mut end = e;
        while end > s && matches!(d[end - 1], b'\r' | b'\n') {
            end -= 1;
        }
        if end - s >= 64 {
            v.push((s, end - s));
        }
        i = e;
    }
    v
}

/// Files stored without compression (method 0) in a ZIP that are JPEG
/// photos or PNGs: position and length.
fn zip_stored(d: &[u8]) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let tail = d.len().saturating_sub(65_557);
    let Some(end) = d[tail..].windows(4).rposition(|w| w == b"PK\x05\x06").map(|p| p + tail) else { return v };
    let le16 = |o: usize| d.get(o..o + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])));
    let le32 = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let (Some(n), Some(mut pos)) = (le16(end + 10), le32(end + 16)) else { return v };
    for _ in 0..n {
        if d.get(pos..pos + 4) != Some(b"PK\x01\x02") {
            break;
        }
        let (Some(method), Some(size), Some(local), Some(a), Some(b), Some(c)) =
            (le16(pos + 10), le32(pos + 20), le32(pos + 42), le16(pos + 28), le16(pos + 30), le16(pos + 32))
        else {
            break;
        };
        pos += 46 + a + b + c;
        if method != 0 || size < 64 || d.get(local..local + 4) != Some(b"PK\x03\x04") {
            continue;
        }
        let (Some(name), Some(extra)) = (le16(local + 26), le16(local + 28)) else { continue };
        let start = local + 30 + name + extra;
        let Some(part) = d.get(start..start + size) else { continue };
        if part.starts_with(&[0xFF, 0xD8, 0xFF]) || part.starts_with(b"\x89PNG\r\n\x1a\n") {
            v.push((start, size));
        }
    }
    v.sort_unstable();
    v.dedup_by_key(|x| x.0);
    v
}

/// Inserts the embedded files into a recipe: they replace parts of its copy
/// segments (they overlap no Deflate stream).
fn merge_embedded(data: &[u8], base: Recipe, inside: Vec<(usize, usize, Recipe)>) -> Option<Recipe> {
    let Recipe { segments, corrections, jpeg, png, embedded: base_inner } = base;
    // Inner recipes follow the order of the segments that have one.
    let mut base_inner = base_inner.into_iter();
    let mut embedded = Vec::new();
    let mut out = Vec::new();
    let mut inside = inside.into_iter().peekable();
    for (seg, start, len) in segments {
        let Segment::Copy { .. } = seg else {
            if has_inner(seg) {
                embedded.push(base_inner.next()?);
            }
            out.push((seg, start, len));
            continue;
        };
        let end = start + len;
        let mut pos = start;
        while let Some(&(s, l, _)) = inside.peek() {
            if s < pos || s + l > end {
                // Outside this copy segment (or overlapping): left alone.
                if s + l <= pos {
                    inside.next();
                    continue;
                }
                break;
            }
            let (s, l, r) = inside.next().unwrap();
            let t = transform(&data[s..s + l], &r).ok()?;
            // Only if the transformed form is smaller on its own (recompressed photo,
            // PNG coded by the image model): pixels left raw for zstd would be much
            // bigger, and copies of the same image in several documents would no
            // longer land in the same block.
            if t.len() >= l {
                continue;
            }
            if s > pos {
                out.push((Segment::Copy { len: (s - pos) as u64 }, pos, s - pos));
            }
            out.push((Segment::Embedded { len: t.len() as u64, size: l as u64 }, s, l));
            embedded.push(r);
            pos = s + l;
        }
        if end > pos {
            out.push((Segment::Copy { len: (end - pos) as u64 }, pos, end - pos));
        }
    }
    Some(Recipe { segments: out, corrections, jpeg, png, embedded })
}

/// Segments that carry an inner recipe (embedded file, contents of a stream
/// described by its own transformed form).
fn has_inner(s: Segment) -> bool {
    matches!(s, Segment::Embedded { .. } | Segment::DeflateNested { .. } | Segment::PredictedNested { .. })
}

/// Recipe from the candidate streams. `pixels`: image data, transformed only
/// if the estimate shows a gain (decompressed pixels sometimes compress
/// worse than the original Deflate).
/// `pixels`: PNG pixel streams, with the image if the model supports it.
fn analyze_streams(data: &[u8], candidates: Vec<Candidate>, pixels: Option<Option<Image>>) -> Option<Recipe> {
    let mut found: Vec<(usize, usize, Segment, Option<Recipe>)> = Vec::new();
    let mut corrections = Vec::new();
    let mut last: Option<Params> = None;
    let (mut tried, mut zlib_found) = (0, false);
    for Candidate { start, wbits, levels } in candidates {
        if found.last().is_some_and(|f| start < f.0 + f.1) {
            continue;
        }
        let Some((raw, used)) = inflate_raw(&data[start..]) else { continue };
        if used < MIN_STREAM {
            continue;
        }
        let original = &data[start..start + used];
        tried += 1;
        // Contents that are themselves a photo or a PNG (LibreOffice document):
        // described by their own transformed form, if it is smaller.
        let nested = if pixels.is_none() && (raw.starts_with(&[0xFF, 0xD8, 0xFF]) || raw.starts_with(b"\x89PNG\r\n\x1a\n")) {
            analyze(&raw).and_then(|r| {
                let t = transform(&raw, &r).ok()?;
                (t.len() < raw.len()).then_some((r, t.len() as u64))
            })
        } else {
            None
        };
        // After a few streams without success, the program does not use zlib: it
        // is no longer tried and we go straight to the predictor.
        let try_zlib = zlib_found || tried <= GIVE_UP;
        // Pixels: coded by the image model if it does clearly better than zstd
        // (photographic images), otherwise stored as they are. Transformed only if
        // it pays off.
        let mut model: Option<(Image, u64)> = None;
        let mut raw_cost = None;
        if let Some(img) = pixels {
            let zs = zstd::bulk::compress(&raw, 6).ok()?.len();
            let coded = img
                .filter(|i| i.raw_len() == Some(raw.len()))
                .and_then(|i| pngmodel::encode(i, &raw).ok().map(|c| (i, c.len())));
            let cost = match coded {
                Some((i, n)) if (n as f64) < zs as f64 * 0.9 => {
                    model = Some((i, n as u64));
                    n
                }
                _ => zs,
            };
            if cost as f64 >= original.len() as f64 * 0.95 {
                continue;
            }
            raw_cost = Some(cost);
        }
        if let Some(p) = try_zlib.then(|| find_params(&raw, original, wbits, levels, last)).flatten() {
            let raw_len = raw.len() as u64;
            let (seg, inner) = match (model, nested) {
                (Some((image, coded_len)), _) => (Segment::ImageDeflate { raw_len, params: p, image, coded_len }, None),
                (None, Some((r, len))) => (Segment::DeflateNested { raw_len, params: p, len }, Some(r)),
                (None, None) => (Segment::Deflate { raw_len, params: p }, None),
            };
            found.push((start, used, seg, inner));
            last = Some(p);
            zlib_found = true;
        } else if let Some((seg, corr)) = predicted(&raw, original, raw_cost) {
            let (seg, inner) = match (seg, model, nested) {
                (Segment::Predicted { raw_len, corr_len }, Some((image, coded_len)), _) => {
                    (Segment::ImagePredicted { raw_len, corr_len, image, coded_len }, None)
                }
                (Segment::Predicted { raw_len, corr_len }, None, Some((r, len))) => {
                    (Segment::PredictedNested { raw_len, corr_len, len }, Some(r))
                }
                (seg, _, _) => (seg, None),
            };
            found.push((start, used, seg, inner));
            corrections.push(zstd::bulk::compress(&corr, 1).ok()?);
        }
    }
    if found.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    let mut embedded = Vec::new();
    let mut pos = 0;
    for (start, used, seg, inner) in found {
        if start > pos {
            segments.push((Segment::Copy { len: (start - pos) as u64 }, pos, start - pos));
        }
        segments.push((seg, start, used));
        embedded.extend(inner);
        pos = start + used;
    }
    if pos < data.len() {
        segments.push((Segment::Copy { len: (data.len() - pos) as u64 }, pos, data.len() - pos));
    }
    Some(Recipe { segments, corrections, jpeg: None, png: None, embedded })
}

/// Maximum data size of a predicted stream: the tokens are in memory during
/// the rebuild.
const MAX_PREDICTED: usize = 256 << 20;

/// Stream from another program: predictor and corrections, if the stream is
/// rewritten exactly and if storing data + corrections pays off.
/// `raw_cost`: estimated cost of the data (pixels: image model or zstd).
fn predicted(raw: &[u8], original: &[u8], raw_cost: Option<usize>) -> Option<(Segment, Vec<u8>)> {
    if raw.len() > MAX_PREDICTED {
        return None;
    }
    let s = crate::deflate::decode(original, MAX_PREDICTED).ok()?;
    if s.len != original.len() {
        return None;
    }
    let (corr, _) = crate::predict::diff(raw, &s).ok()?;
    // Text (Office XML, text of PDF pages): always worth it, because zstd
    // benefits from similarities between neighbouring files in the block (a
    // single stream does not show it). Otherwise (images, fonts): estimate on
    // the stream alone.
    if let Some(cost) = raw_cost {
        let corr_cost = zstd::bulk::compress(&corr, 3).ok()?.len();
        if (cost + corr_cost) as f64 >= original.len() as f64 * 0.95 {
            return None;
        }
    } else {
        let text = crate::classify::detect(&raw[..raw.len().min(crate::classify::HEAD_LEN)]) == crate::classify::Family::Text;
        if !text && !worth_it(raw, &corr, original.len()) {
            return None;
        }
    }
    Some((Segment::Predicted { raw_len: raw.len() as u64, corr_len: corr.len() as u64 }, corr))
}

/// JPEG photo: decoded down to the coefficients, rewritten identically, then
/// coefficients re-coded (jpegmodel.rs). Segment data:
///
/// ```text
/// varint version (1)
/// varint length of the headers H (up to the scan included), varint
///        length of the end F (end-of-image marker included), varint
///        length Z, then Z bytes: headers and end concatenated, compressed
///        by zstd (EXIF and thumbnails compress well)
/// varint number of data segments, then for each: u8 fill bits, u8 number
///        of those bits
/// varint number of blocks with superfluous "16 zeros", then for each:
///        varint gap from the previous block number, u8 number of codes,
///        u8 end of block
/// the rest: the re-coded coefficients
/// ```
fn jpeg_recipe(data: &[u8]) -> Option<Recipe> {
    // The exactness of the rewrite is checked at the end by prepare(), which
    // rebuilds the whole photo and compares it.
    let j = crate::jpeg::decode(data).ok()?;
    let coded = crate::jpegmodel::encode(&j).ok()?;
    if j.progressive {
        return jpeg_progressive_recipe(data, &j, coded);
    }
    let sides = zstd::bulk::compress(&[&j.header[..], &j.trailer[..]].concat(), 19).ok()?;
    let mut p = Vec::with_capacity(coded.len() + sides.len() + 64);
    put_varint(&mut p, 1);
    put_varint(&mut p, j.header.len() as u64);
    put_varint(&mut p, j.trailer.len() as u64);
    put_varint(&mut p, sides.len() as u64);
    p.extend_from_slice(&sides);
    put_varint(&mut p, j.pads.len() as u64);
    for &(v, n) in &j.pads {
        p.extend_from_slice(&[v, n]);
    }
    put_varint(&mut p, j.extra_zrl.len() as u64);
    let mut last = 0u32;
    for &(b, z, e) in &j.extra_zrl {
        put_varint(&mut p, u64::from(b - last));
        p.extend_from_slice(&[z, u8::from(e)]);
        last = b;
    }
    p.extend_from_slice(&coded);
    // Worth it only if the recompressed photo is smaller.
    if p.len() >= data.len() {
        return None;
    }
    let seg = (Segment::Jpeg { len: p.len() as u64 }, 0, data.len());
    Some(Recipe { segments: vec![seg], corrections: Vec::new(), jpeg: Some(p), png: None, embedded: Vec::new() })
}

/// Progressive photo (version 2 of the segment data):
///
/// ```text
/// varint version (2)
/// varint length of the headers H, varint length of the end F, varint
///        number of scans N, then (N − 1) × varint length of the bytes
///        preceding each scan after the first one
/// varint length Z, then Z bytes: headers, end and those bytes concatenated,
///        compressed by zstd
/// N ×    varint number of segments, then for each u8 fill bits, u8 their
///        count; varint number of runs of empty blocks, then the length of
///        each one (varint)
/// the rest: the re-coded coefficients
/// ```
fn jpeg_progressive_recipe(data: &[u8], j: &crate::jpeg::Jpeg, coded: Vec<u8>) -> Option<Recipe> {
    let mut j = j.clone();
    crate::jpegprog::mark_standard(&mut j).ok()?;
    let mut all = [&j.header[..], &j.trailer[..]].concat();
    for sc in &j.scans[1..] {
        all.extend_from_slice(&sc.prefix);
    }
    let sides = zstd::bulk::compress(&all, 19).ok()?;
    let mut p = Vec::with_capacity(coded.len() + sides.len() + 256);
    put_varint(&mut p, 2);
    put_varint(&mut p, j.header.len() as u64);
    put_varint(&mut p, j.trailer.len() as u64);
    put_varint(&mut p, j.scans.len() as u64);
    for sc in &j.scans[1..] {
        put_varint(&mut p, sc.prefix.len() as u64);
    }
    put_varint(&mut p, sides.len() as u64);
    p.extend_from_slice(&sides);
    // Description of the scans, compressed (repetitive fills).
    let mut info = Vec::new();
    for sc in &j.scans {
        put_varint(&mut info, sc.pads.len() as u64);
        for &(v, n) in &sc.pads {
            info.extend_from_slice(&[v, n]);
        }
        info.push(u8::from(sc.standard));
        if !sc.standard {
            put_varint(&mut info, sc.eobruns.len() as u64);
            for &r in &sc.eobruns {
                put_varint(&mut info, u64::from(r));
            }
        }
    }
    let packed = zstd::bulk::compress(&info, 19).ok()?;
    put_varint(&mut p, info.len() as u64);
    put_varint(&mut p, packed.len() as u64);
    p.extend_from_slice(&packed);
    p.extend_from_slice(&coded);
    if p.len() >= data.len() {
        return None;
    }
    let seg = (Segment::Jpeg { len: p.len() as u64 }, 0, data.len());
    Some(Recipe { segments: vec![seg], corrections: Vec::new(), jpeg: Some(p), png: None, embedded: Vec::new() })
}

fn jpeg_progressive_rebuild(c: &mut Cursor, size: u64) -> Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("photo recompressée invalide");
    let hl = usize::try_from(c.varint()?)?;
    let tl = usize::try_from(c.varint()?)?;
    let n = usize::try_from(c.varint()?)?;
    ensure!((1..=1000).contains(&n) && n <= c.remaining() + 1, bad());
    let mut lens = Vec::with_capacity(n - 1);
    for _ in 1..n {
        lens.push(usize::try_from(c.varint()?)?);
    }
    let total = lens.iter().try_fold(hl.checked_add(tl).ok_or_else(bad)?, |a, &l| a.checked_add(l)).ok_or_else(bad)?;
    ensure!(total as u64 <= size, bad());
    let zl = usize::try_from(c.varint()?)?;
    let all = zstd::bulk::decompress(c.bytes(zl)?, total)?;
    ensure!(all.len() == total, bad());
    let (header, rest) = all.split_at(hl);
    let (trailer, mut prefixes) = rest.split_at(tl);
    let info_len = usize::try_from(c.varint()?)?;
    let packed_len = usize::try_from(c.varint()?)?;
    ensure!(info_len as u64 <= size.saturating_mul(8).saturating_add(1 << 16), bad());
    let info = zstd::bulk::decompress(c.bytes(packed_len)?, info_len)?;
    ensure!(info.len() == info_len, bad());
    let coded = c.bytes(c.remaining())?;
    let mut c = Cursor::new(&info);
    let mut scans = Vec::with_capacity(n);
    for i in 0..n {
        let mut sc = crate::jpegprog::Scan::default();
        if i > 0 {
            let (a, b) = prefixes.split_at(lens[i - 1]);
            sc.prefix = a.to_vec();
            prefixes = b;
        }
        let np = usize::try_from(c.varint()?)?;
        ensure!(np <= c.remaining() / 2, bad());
        for _ in 0..np {
            sc.pads.push((c.u8()?, c.u8()?));
        }
        sc.standard = match c.u8()? {
            0 => false,
            1 => true,
            _ => bail!("photo recompressée invalide"),
        };
        if !sc.standard {
            let nr = usize::try_from(c.varint()?)?;
            ensure!(nr <= c.remaining(), bad());
            for _ in 0..nr {
                sc.eobruns.push(u32::try_from(c.varint()?).map_err(|_| bad())?);
            }
        }
        scans.push(sc);
    }
    ensure!(c.remaining() == 0, bad());
    let mut j = crate::jpeg::parse_header(header)?;
    ensure!(j.header.len() == header.len() && j.progressive, bad());
    j.trailer = trailer.to_vec();
    j.scans = scans;
    crate::jpegmodel::decode(&mut j, coded, size.saturating_mul(4))?;
    crate::jpeg::encode(&j)
}

/// Rebuilds a photo from its segment data, without exceeding `size` bytes.
fn jpeg_rebuild(p: &[u8], size: u64) -> Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("photo recompressée invalide");
    let mut c = Cursor::new(p);
    match c.varint()? {
        1 => {}
        2 => return jpeg_progressive_rebuild(&mut c, size),
        _ => bail!("photo recompressée de version inconnue"),
    }
    let hl = usize::try_from(c.varint()?)?;
    let tl = usize::try_from(c.varint()?)?;
    ensure!((hl as u64).saturating_add(tl as u64) <= size, bad());
    let zl = usize::try_from(c.varint()?)?;
    let sides = zstd::bulk::decompress(c.bytes(zl)?, hl + tl)?;
    ensure!(sides.len() == hl + tl, bad());
    let (header, trailer) = sides.split_at(hl);
    let trailer = trailer.to_vec();
    let np = usize::try_from(c.varint()?)?;
    ensure!(np <= c.remaining() / 2, bad());
    let mut pads = Vec::with_capacity(np);
    for _ in 0..np {
        pads.push((c.u8()?, c.u8()?));
    }
    let ne = usize::try_from(c.varint()?)?;
    ensure!(ne <= c.remaining() / 3, bad());
    let mut extra = Vec::with_capacity(ne);
    let mut b = 0u64;
    for i in 0..ne {
        let step = c.varint()?;
        ensure!(i == 0 || step > 0, bad());
        b = b.checked_add(step).filter(|&b| b <= u64::from(u32::MAX)).ok_or_else(bad)?;
        let (z, e) = (c.u8()?, c.u8()?);
        ensure!(e <= 1 && z <= 4, bad());
        extra.push((b as u32, z, e == 1));
    }
    let coded = c.bytes(c.remaining())?;
    let mut j = crate::jpeg::parse_header(header)?;
    ensure!(j.header.len() == header.len() && !j.progressive, bad());
    (j.trailer, j.pads, j.extra_zrl) = (trailer, pads, extra);
    // Each block of a JPEG costs at least 2 bits: a header announcing more
    // blocks than the file can hold is refused (memory).
    crate::jpegmodel::decode(&mut j, coded, size.saturating_mul(4))?;
    crate::jpeg::encode(&j)
}

/// Decompressed data (and corrections) clearly smaller than the original
/// stream once passed through zstd 3?
fn worth_it(raw: &[u8], corr: &[u8], original: usize) -> bool {
    let z = |d: &[u8]| if d.is_empty() { Some(0) } else { zstd::bulk::compress(d, 3).ok().map(|c| c.len()) };
    match (z(raw), z(corr)) {
        (Some(a), Some(b)) => ((a + b) as f64) < original as f64 * 0.95,
        _ => false,
    }
}

/// PNG image: the IDAT chunks put end to end form a single zlib stream. A
/// "glued" file is made (IDAT headers and CRCs removed) and analysed like
/// any other; at extraction, the stream is cut again at the same lengths and
/// the CRCs are recomputed.
fn png_recipe(d: &[u8]) -> Option<Recipe> {
    let mut pos = 8;
    let (mut first, mut lens, mut idat) = (None, Vec::new(), Vec::new());
    let mut after = None;
    while pos + 12 <= d.len() {
        let len = u32::from_be_bytes(d[pos..pos + 4].try_into().unwrap()) as usize;
        let end = pos.checked_add(12 + len).filter(|&e| e <= d.len())?;
        if &d[pos + 4..pos + 8] == b"IDAT" {
            // The IDATs must be consecutive.
            if after.is_some() {
                return None;
            }
            first.get_or_insert(pos);
            lens.push(len as u32);
            idat.extend_from_slice(&d[pos + 8..pos + 8 + len]);
        } else if first.is_some() && after.is_none() {
            after = Some(pos);
        }
        pos = end;
    }
    let first = first?;
    let after = after.unwrap_or(pos);
    let mut virt = Vec::with_capacity(d.len());
    virt.extend_from_slice(&d[..first]);
    virt.extend_from_slice(&idat);
    virt.extend_from_slice(&d[after..]);
    let (&cmf, &flg) = (idat.first()?, idat.get(1)?);
    if cmf & 0x0F != 8 || cmf >> 4 > 7 || flg & 0x20 != 0 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return None;
    }
    let cand = vec![Candidate { start: first + 2, wbits: ((cmf >> 4) + 8).max(9), levels: zlib_levels(flg) }];
    let inner = analyze_streams(&virt, cand, Some(Image::from_png(d)))?;
    let layout = PngLayout { first, lens, virtual_len: virt.len() };
    let len = transform(&virt, &inner).ok()?.len() + png_header(&layout).len();
    let seg = (Segment::Png { len: len as u64 }, 0, d.len());
    Some(Recipe { segments: vec![seg], corrections: Vec::new(), jpeg: None, png: Some(Box::new((layout, inner))), embedded: Vec::new() })
}

fn png_header(l: &PngLayout) -> Vec<u8> {
    let mut o = Vec::new();
    put_varint(&mut o, 1);
    put_varint(&mut o, l.first as u64);
    put_varint(&mut o, l.virtual_len as u64);
    put_varint(&mut o, l.lens.len() as u64);
    for &n in &l.lens {
        put_varint(&mut o, u64::from(n));
    }
    o
}

/// Glued file of a PNG, from the original (for the transform).
fn png_virtual(d: &[u8], l: &PngLayout) -> Result<Vec<u8>> {
    let mut v = Vec::with_capacity(l.virtual_len);
    v.extend_from_slice(d.get(..l.first).ok_or_else(|| anyhow::anyhow!("PNG modifié"))?);
    let mut pos = l.first;
    for &n in &l.lens {
        let n = n as usize;
        v.extend_from_slice(d.get(pos + 8..pos + 8 + n).ok_or_else(|| anyhow::anyhow!("PNG modifié"))?);
        pos += 12 + n;
    }
    v.extend_from_slice(d.get(pos..).ok_or_else(|| anyhow::anyhow!("PNG modifié"))?);
    ensure!(v.len() == l.virtual_len, "PNG modifié");
    Ok(v)
}

/// Rebuilds a PNG from its segment data, without exceeding `size`.
fn png_rebuild(p: &[u8], size: u64) -> Result<Vec<u8>> {
    let bad = || anyhow::anyhow!("image PNG invalide");
    let mut c = Cursor::new(p);
    ensure!(c.varint()? == 1, "image PNG de version inconnue");
    let first = usize::try_from(c.varint()?)?;
    let vlen = c.varint()?;
    ensure!(vlen <= size, bad());
    let n = usize::try_from(c.varint()?)?;
    ensure!(n <= c.remaining(), bad());
    let mut lens = Vec::with_capacity(n);
    for _ in 0..n {
        lens.push(u32::try_from(c.varint()?)?);
    }
    let inner = c.bytes(c.remaining())?;
    // The glued file contains no other PNG (no recursion).
    let virt = rebuild_inner(inner, vlen, false, false)?;
    let total: u64 = lens.iter().map(|&n| u64::from(n)).sum();
    ensure!(first as u64 + total <= vlen, bad());
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&virt[..first]);
    let mut pos = first;
    for &n in &lens {
        let data = &virt[pos..pos + n as usize];
        out.extend_from_slice(&n.to_be_bytes());
        let start = out.len();
        out.extend_from_slice(b"IDAT");
        out.extend_from_slice(data);
        let crc = zlib::crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
        pos += n as usize;
        ensure!(out.len() as u64 <= size, bad());
    }
    out.extend_from_slice(&virt[pos..]);
    ensure!(out.len() as u64 == size, "image PNG de taille inattendue");
    Ok(out)
}

/// Transformed form: the recipe, then the data of each segment.
pub fn transform(data: &[u8], recipe: &Recipe) -> Result<Vec<u8>> {
    transform_with(data, recipe, recipe.jpeg.as_deref())
}

/// Like `transform`, with the recompressed photo supplied separately (the
/// writer reads it back from its temporary file).
pub fn transform_with(data: &[u8], recipe: &Recipe, photo: Option<&[u8]>) -> Result<Vec<u8>> {
    let changed = || anyhow::anyhow!("le fichier a changé pendant la compression");
    // Data of each segment first (corrections are regenerated: the predictor is
    // deterministic), then the recipe, then the data.
    let mut payloads: Vec<Option<(Vec<u8>, Vec<u8>)>> = Vec::with_capacity(recipe.segments.len());
    let mut kept = recipe.corrections.iter();
    let mut kept_emb = recipe.embedded.iter();
    for &(s, start, len) in &recipe.segments {
        let part = data.get(start..start + len).ok_or_else(|| anyhow::anyhow!("recette hors du fichier"))?;
        payloads.push(match s {
            Segment::Copy { .. } => None,
            Segment::Deflate { raw_len, .. } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                if raw.len() as u64 != raw_len {
                    return Err(changed());
                }
                Some((raw, Vec::new()))
            }
            Segment::ImageDeflate { raw_len, image, coded_len, .. } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                ensure!(raw.len() as u64 == raw_len, "le fichier a changé pendant la compression");
                let coded = pngmodel::encode(image, &raw)?;
                ensure!(coded.len() as u64 == coded_len, "le fichier a changé pendant la compression");
                Some((coded, Vec::new()))
            }
            Segment::ImagePredicted { raw_len, corr_len, image, coded_len } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                let packed = kept.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let corr = zstd::bulk::decompress(packed, corr_len as usize)?;
                let coded = pngmodel::encode(image, &raw)?;
                if raw.len() as u64 != raw_len || corr.len() as u64 != corr_len || coded.len() as u64 != coded_len {
                    return Err(changed());
                }
                Some((coded, corr))
            }
            Segment::Predicted { raw_len, corr_len, .. } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                let packed = kept.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let corr = zstd::bulk::decompress(packed, corr_len as usize)?;
                if raw.len() as u64 != raw_len || corr.len() as u64 != corr_len {
                    return Err(changed());
                }
                Some((raw, corr))
            }
            Segment::Jpeg { len } => {
                let p = photo.ok_or_else(|| anyhow::anyhow!("photo recompressée absente"))?;
                ensure!(p.len() as u64 == len, "photo recompressée incohérente");
                Some((p.to_vec(), Vec::new()))
            }
            Segment::DeflateNested { raw_len, len, .. } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                ensure!(raw.len() as u64 == raw_len, "le fichier a changé pendant la compression");
                let r = kept_emb.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let t = transform(&raw, r)?;
                ensure!(t.len() as u64 == len, "le fichier a changé pendant la compression");
                Some((t, Vec::new()))
            }
            Segment::PredictedNested { raw_len, corr_len, len } => {
                let (raw, _) = inflate_raw(part).ok_or_else(changed)?;
                let packed = kept.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let corr = zstd::bulk::decompress(packed, corr_len as usize)?;
                let r = kept_emb.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let t = transform(&raw, r)?;
                if raw.len() as u64 != raw_len || corr.len() as u64 != corr_len || t.len() as u64 != len {
                    return Err(changed());
                }
                Some((t, corr))
            }
            Segment::Embedded { len, .. } => {
                let r = kept_emb.next().ok_or_else(|| anyhow::anyhow!("recette incomplète"))?;
                let t = transform(part, r)?;
                ensure!(t.len() as u64 == len, "le fichier a changé pendant la compression");
                Some((t, Vec::new()))
            }
            Segment::Font { len } | Segment::Audio { len } | Segment::Code { len } => {
                let t = match s {
                    Segment::Font { .. } => crate::font::encode(part),
                    Segment::Audio { .. } => crate::audio::encode(part),
                    _ => crate::bcj::encode(part),
                };
                let t = t.ok_or_else(changed)?;
                ensure!(t.len() as u64 == len, "le fichier a changé pendant la compression");
                Some((t, Vec::new()))
            }
            Segment::Png { len } => {
                let (layout, inner) = &**recipe.png.as_ref().ok_or_else(|| anyhow::anyhow!("recette PNG absente"))?;
                let mut out = png_header(layout);
                out.extend(transform(&png_virtual(part, layout)?, inner)?);
                ensure!(out.len() as u64 == len, "le fichier a changé pendant la compression");
                Some((out, Vec::new()))
            }
        });
    }
    let mut out = Vec::with_capacity(data.len() * 3);
    put_varint(&mut out, VERSION);
    put_varint(&mut out, recipe.segments.len() as u64);
    for (s, _, _) in &recipe.segments {
        match *s {
            Segment::Copy { len } => {
                put_varint(&mut out, 0);
                put_varint(&mut out, len);
            }
            Segment::Deflate { raw_len, params } => {
                put_varint(&mut out, if params.chunk == 0 { 1 } else { 2 });
                put_varint(&mut out, raw_len);
                out.extend_from_slice(&[params.level, params.mem, params.wbits]);
                if params.chunk != 0 {
                    put_varint(&mut out, u64::from(params.chunk));
                }
            }
            Segment::Predicted { raw_len, corr_len, .. } => {
                put_varint(&mut out, 3);
                put_varint(&mut out, raw_len);
                put_varint(&mut out, corr_len);
            }
            Segment::Jpeg { len } => {
                put_varint(&mut out, 4);
                put_varint(&mut out, len);
            }
            Segment::Png { len } => {
                put_varint(&mut out, 5);
                put_varint(&mut out, len);
            }
            Segment::Font { len } => {
                put_varint(&mut out, 11);
                put_varint(&mut out, len);
            }
            Segment::Audio { len } => {
                put_varint(&mut out, 12);
                put_varint(&mut out, len);
            }
            Segment::Code { len } => {
                put_varint(&mut out, 13);
                put_varint(&mut out, len);
            }
            Segment::Embedded { len, size } => {
                put_varint(&mut out, 8);
                put_varint(&mut out, len);
                put_varint(&mut out, size);
            }
            Segment::DeflateNested { raw_len, params, len } => {
                put_varint(&mut out, 9);
                put_varint(&mut out, raw_len);
                out.extend_from_slice(&[params.level, params.mem, params.wbits]);
                put_varint(&mut out, u64::from(params.chunk));
                put_varint(&mut out, len);
            }
            Segment::PredictedNested { raw_len, corr_len, len } => {
                put_varint(&mut out, 10);
                put_varint(&mut out, raw_len);
                put_varint(&mut out, corr_len);
                put_varint(&mut out, len);
            }
            Segment::ImageDeflate { raw_len, params, image, coded_len } => {
                put_varint(&mut out, 6);
                put_varint(&mut out, raw_len);
                out.extend_from_slice(&[params.level, params.mem, params.wbits]);
                put_varint(&mut out, u64::from(params.chunk));
                out.extend_from_slice(&image.encode());
                put_varint(&mut out, coded_len);
            }
            Segment::ImagePredicted { raw_len, corr_len, image, coded_len } => {
                put_varint(&mut out, 7);
                put_varint(&mut out, raw_len);
                put_varint(&mut out, corr_len);
                out.extend_from_slice(&image.encode());
                put_varint(&mut out, coded_len);
            }
        }
    }
    for (&(_, start, len), p) in recipe.segments.iter().zip(&payloads) {
        match p {
            None => out.extend_from_slice(&data[start..start + len]),
            Some((raw, corr)) => {
                out.extend_from_slice(raw);
                out.extend_from_slice(corr);
            }
        }
    }
    Ok(out)
}

/// Rebuilds the original file from its transformed form. `size` is the
/// expected size (given by the index): nothing is ever written beyond it.
pub fn rebuild(stored: &[u8], size: u64) -> Result<Vec<u8>> {
    rebuild_inner(stored, size, true, true)
}

/// `allow_png` / `allow_embedded`: segments allowed at this nesting level
/// (a single level of embedded files, no PNG inside the glued file of a PNG).
fn rebuild_inner(stored: &[u8], size: u64, allow_png: bool, allow_embedded: bool) -> Result<Vec<u8>> {
    let mut c = Cursor::new(stored);
    if c.varint()? != VERSION {
        bail!("forme transformée de version inconnue");
    }
    let n = c.varint()? as usize;
    let mut segments = Vec::with_capacity(n.min(c.remaining()));
    for _ in 0..n {
        segments.push(match c.varint()? {
            0 => Segment::Copy { len: c.varint()? },
            t @ (1 | 2) => {
                let raw_len = c.varint()?;
                let b = c.bytes(3)?;
                let chunk = if t == 2 { u32::try_from(c.varint()?).unwrap_or(u32::MAX).max(1) } else { 0 };
                let params = Params { level: b[0], mem: b[1], wbits: b[2], chunk };
                if !params.valid() {
                    bail!("réglages de reconstruction invalides");
                }
                Segment::Deflate { raw_len, params }
            }
            3 => {
                let raw_len = c.varint()?;
                let corr_len = c.varint()?;
                if raw_len > MAX_PREDICTED as u64 {
                    bail!("flux prédit trop grand");
                }
                Segment::Predicted { raw_len, corr_len }
            }
            4 => Segment::Jpeg { len: c.varint()? },
            11 => Segment::Font { len: c.varint()? },
            12 => Segment::Audio { len: c.varint()? },
            13 => Segment::Code { len: c.varint()? },
            5 if allow_png => Segment::Png { len: c.varint()? },
            8 if allow_embedded => Segment::Embedded { len: c.varint()?, size: c.varint()? },
            9 if allow_embedded => {
                let raw_len = c.varint()?;
                let b = c.bytes(3)?;
                let chunk = u32::try_from(c.varint()?).unwrap_or(u32::MAX);
                let params = Params { level: b[0], mem: b[1], wbits: b[2], chunk };
                ensure!(params.valid() && raw_len <= MAX_STORED, "réglages de reconstruction invalides");
                Segment::DeflateNested { raw_len, params, len: c.varint()? }
            }
            10 if allow_embedded => {
                let raw_len = c.varint()?;
                let corr_len = c.varint()?;
                ensure!(raw_len <= MAX_PREDICTED as u64, "flux prédit trop grand");
                Segment::PredictedNested { raw_len, corr_len, len: c.varint()? }
            }
            6 => {
                let raw_len = c.varint()?;
                let b = c.bytes(3)?;
                let chunk = u32::try_from(c.varint()?).unwrap_or(u32::MAX);
                let params = Params { level: b[0], mem: b[1], wbits: b[2], chunk };
                ensure!(params.valid(), "réglages de reconstruction invalides");
                let image = Image::decode(c.bytes(9)?)?;
                ensure!(image.raw_len() == Some(raw_len as usize), "image PNG incohérente");
                Segment::ImageDeflate { raw_len, params, image, coded_len: c.varint()? }
            }
            7 => {
                let raw_len = c.varint()?;
                let corr_len = c.varint()?;
                ensure!(raw_len <= MAX_PREDICTED as u64, "flux prédit trop grand");
                let image = Image::decode(c.bytes(9)?)?;
                ensure!(image.raw_len() == Some(raw_len as usize), "image PNG incohérente");
                Segment::ImagePredicted { raw_len, corr_len, image, coded_len: c.varint()? }
            }
            t => bail!("segment de type inconnu ({t})"),
        });
    }
    // Data of each segment, following the recipe.
    let mut parts = Vec::with_capacity(segments.len());
    for s in segments {
        let len = match s {
            Segment::Copy { len }
            | Segment::Deflate { raw_len: len, .. }
            | Segment::Jpeg { len }
            | Segment::Png { len }
            | Segment::Font { len }
            | Segment::Audio { len }
            | Segment::Code { len }
            | Segment::Embedded { len, .. }
            | Segment::DeflateNested { len, .. } => len,
            Segment::PredictedNested { len, corr_len, .. } => {
                len.checked_add(corr_len).ok_or_else(|| anyhow::anyhow!("forme transformée invalide"))?
            }
            Segment::ImageDeflate { coded_len, .. } => coded_len,
            Segment::ImagePredicted { coded_len, corr_len, .. } => {
                coded_len.checked_add(corr_len).ok_or_else(|| anyhow::anyhow!("forme transformée invalide"))?
            }
            Segment::Predicted { raw_len, corr_len, .. } => {
                raw_len.checked_add(corr_len).ok_or_else(|| anyhow::anyhow!("forme transformée invalide"))?
            }
        };
        parts.push((s, c.bytes(usize::try_from(len)?)?));
    }
    if c.remaining() != 0 {
        bail!("données en trop après la reconstruction");
    }
    // The streams are independent: redone on all cores, then put end to end.
    let copied: u64 = parts.iter().map(|(s, _)| if let Segment::Copy { len } = s { *len } else { 0 }).sum();
    let budget = size.checked_sub(copied).ok_or_else(|| anyhow::anyhow!("fichier reconstruit trop long"))?;
    let rebuilt: Vec<Option<Vec<u8>>> = parts
        .par_iter()
        .map(|(s, part)| match s {
            Segment::Copy { .. } => Ok(None),
            Segment::Deflate { params, .. } => {
                let mut v = Vec::new();
                zlib::deflate_into(part, *params, budget, &mut v)?;
                Ok(Some(v))
            }
            Segment::Png { .. } => {
                let v = png_rebuild(part, budget)?;
                Ok(Some(v))
            }
            Segment::Font { .. } => Ok(Some(crate::font::decode(part, budget)?)),
            Segment::Audio { .. } => Ok(Some(crate::audio::decode(part, budget)?)),
            Segment::Code { .. } => Ok(Some(crate::bcj::decode(part, budget)?)),
            Segment::Embedded { size, .. } => {
                ensure!(*size <= budget, "fichier reconstruit trop long");
                Ok(Some(rebuild_inner(part, *size, true, false)?))
            }
            Segment::DeflateNested { raw_len, params, .. } => {
                let raw = rebuild_inner(part, *raw_len, true, false)?;
                let mut v = Vec::new();
                zlib::deflate_into(&raw, *params, budget, &mut v)?;
                Ok(Some(v))
            }
            Segment::PredictedNested { raw_len, len, .. } => {
                let (t, corr) = part.split_at(*len as usize);
                let raw = rebuild_inner(t, *raw_len, true, false)?;
                let v = crate::deflate::encode(&crate::predict::replay(&raw, corr)?)?;
                ensure!(v.len() as u64 <= budget, "fichier reconstruit trop long");
                Ok(Some(v))
            }
            Segment::ImageDeflate { params, image, .. } => {
                let raw = pngmodel::decode(*image, part)?;
                let mut v = Vec::new();
                zlib::deflate_into(&raw, *params, budget, &mut v)?;
                Ok(Some(v))
            }
            Segment::ImagePredicted { image, coded_len, .. } => {
                let (coded, corr) = part.split_at(*coded_len as usize);
                let raw = pngmodel::decode(*image, coded)?;
                let v = crate::deflate::encode(&crate::predict::replay(&raw, corr)?)?;
                ensure!(v.len() as u64 <= budget, "fichier reconstruit trop long");
                Ok(Some(v))
            }
            Segment::Jpeg { .. } => {
                let v = jpeg_rebuild(part, budget)?;
                ensure!(v.len() as u64 <= budget, "fichier reconstruit trop long");
                Ok(Some(v))
            }
            Segment::Predicted { raw_len, .. } => {
                let (raw, corr) = part.split_at(*raw_len as usize);
                let v = crate::deflate::encode(&crate::predict::replay(raw, corr)?)?;
                if v.len() as u64 > budget {
                    bail!("fichier reconstruit trop long");
                }
                Ok(Some(v))
            }
        })
        .collect::<Result<_>>()?;
    let total = rebuilt.iter().flatten().map(|v| v.len() as u64).sum::<u64>() + copied;
    if total != size {
        bail!("fichier reconstruit de taille inattendue");
    }
    let mut out = Vec::with_capacity(size as usize);
    for ((_, part), r) in parts.iter().zip(&rebuilt) {
        out.extend_from_slice(r.as_deref().unwrap_or(part));
    }
    Ok(out)
}

/// A stream to try: start of the Deflate core, window, and possible levels.
struct Candidate {
    start: usize,
    wbits: u8,
    levels: &'static [u8],
}

/// All levels, most common first.
const ALL_LEVELS: &[u8] = &[6, 9, 1, 2, 3, 4, 5, 7, 8];

/// Possible levels according to the hint zlib writes in the header of its
/// streams (FLEVEL bits): those it rules out are not tried.
fn zlib_levels(flg: u8) -> &'static [u8] {
    match flg >> 6 {
        0 => &[1],
        1 => &[2, 3, 4, 5],
        2 => &[6],
        _ => &[9, 7, 8],
    }
}

/// Most common settings first; the last ones that succeeded in the same file
/// before anything else.
fn find_params(raw: &[u8], original: &[u8], wbits: u8, levels: &[u8], last: Option<Params>) -> Option<Params> {
    const MEMS: [u8; 9] = [8, 9, 7, 6, 5, 4, 3, 2, 1];
    let last = last.filter(|p| p.wbits == wbits && levels.contains(&p.level));
    let whole = MEMS.iter().flat_map(|&mem| levels.iter().map(move |&level| Params { level, mem, wbits, chunk: 0 }));
    // In slices: only if the stream was flushed along the way (empty stored
    // block of a Z_SYNC_FLUSH); the slice size is the position, in the
    // decompressed data, of the first flush.
    let chunked = sync_chunk(original).into_iter().flat_map(move |chunk| {
        [8u8, 9].into_iter().flat_map(move |mem| levels.iter().map(move |&level| Params { level, mem, wbits, chunk }))
    });
    last.into_iter()
        .chain(whole.chain(chunked).filter(|&p| Some(p) != last))
        .find(|&p| zlib::reproduces(raw, original, p))
}

/// Probable slice size of a stream compressed in slices: position, in the
/// decompressed data, of the first empty stored block (Z_SYNC_FLUSH).
fn sync_chunk(original: &[u8]) -> Option<u32> {
    if memfind(original, b"\x00\x00\xFF\xFF").is_none() {
        return None;
    }
    let s = crate::deflate::decode(original, MAX_STORED as usize).ok()?;
    let mut pos = 0u64;
    for b in &s.blocks {
        match &b.kind {
            crate::deflate::BlockKind::Stored { data, .. } if data.is_empty() && !b.last => {
                return u32::try_from(pos).ok().filter(|c| CHUNK_RANGE.contains(c));
            }
            crate::deflate::BlockKind::Stored { data, .. } => pos += data.len() as u64,
            _ => {}
        }
        for t in &b.tokens {
            pos += match *t {
                crate::deflate::Token::Literal(_) => 1,
                crate::deflate::Token::Match { len, .. } => u64::from(len),
            };
        }
    }
    None
}

/// Start of each raw Deflate stream found in a PDF, a ZIP or a gzip.
pub fn stream_starts(data: &[u8]) -> Vec<usize> {
    let c = if data.starts_with(b"%PDF-") {
        pdf_streams(data)
    } else if data.starts_with(b"PK\x03\x04") {
        zip_streams(data)
    } else if data.starts_with(b"\x1F\x8B\x08") {
        gzip_streams(data)
    } else if data.starts_with(b"#!") || data.starts_with(b"<?") {
        phar_streams(data)
    } else {
        Vec::new()
    };
    c.into_iter().map(|c| c.start).collect()
}

/// Raw Deflate compression by our zlib, with these settings.
pub fn deflate_with(raw: &[u8], p: Params, out: &mut Vec<u8>) -> Result<()> {
    zlib::deflate_into(raw, p, u64::MAX, out)
}

/// zlib settings that give back exactly `original`, all levels tried (for
/// the measurement tools).
pub fn find_any_params(raw: &[u8], original: &[u8], wbits: u8) -> Option<Params> {
    find_params(raw, original, wbits, ALL_LEVELS, None)
}

/// Decompresses a raw Deflate stream. Returns the contents and the number of
/// compressed bytes read; None if it is not a complete and valid stream.
pub fn inflate_raw(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    zlib::inflate_raw(data, MAX_STORED)
}

/// PDF: `stream` … `endstream` streams starting with a zlib header.
fn pdf_streams(d: &[u8]) -> Vec<Candidate> {
    let mut v = Vec::new();
    let mut i = 0;
    while let Some(p) = memfind(&d[i..], b"stream") {
        let mut s = i + p + 6;
        i = s;
        if d.get(s) == Some(&b'\r') {
            s += 1;
        }
        if d.get(s) != Some(&b'\n') {
            continue;
        }
        s += 1;
        let (Some(&cmf), Some(&flg)) = (d.get(s), d.get(s + 1)) else { break };
        // zlib header: method 8, window ≤ 32 KB, no dictionary, valid checksum.
        if cmf & 0x0F != 8 || cmf >> 4 > 7 || flg & 0x20 != 0 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
            continue;
        }
        v.push(Candidate { start: s + 2, wbits: ((cmf >> 4) + 8).max(9), levels: zlib_levels(flg) });
    }
    v
}

/// ZIP (and Office, OpenDocument, JAR, EPUB…): "deflate" entries of the
/// central directory.
fn zip_streams(d: &[u8]) -> Vec<Candidate> {
    let mut v = Vec::new();
    // End of the central directory: within the last 64 KB (comment included).
    let tail = d.len().saturating_sub(65_557);
    let Some(end) = d[tail..].windows(4).rposition(|w| w == b"PK\x05\x06").map(|p| p + tail) else { return v };
    let le16 = |o: usize| d.get(o..o + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])));
    let le32 = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let (Some(n), Some(mut pos)) = (le16(end + 10), le32(end + 16)) else { return v };
    for _ in 0..n {
        if d.get(pos..pos + 4) != Some(b"PK\x01\x02") {
            break;
        }
        let (Some(method), Some(local), Some(a), Some(b), Some(c)) =
            (le16(pos + 10), le32(pos + 42), le16(pos + 28), le16(pos + 30), le16(pos + 32))
        else {
            break;
        };
        pos += 46 + a + b + c;
        if method != 8 || d.get(local..local + 4) != Some(b"PK\x03\x04") {
            continue;
        }
        let (Some(name), Some(extra)) = (le16(local + 26), le16(local + 28)) else { continue };
        let start = local + 30 + name + extra;
        if start < d.len() {
            // The ZIP level hint (bits 1 and 2) is not reliable from one program to
            // another: all levels are tried.
            v.push(Candidate { start, wbits: 15, levels: ALL_LEVELS });
        }
    }
    v.sort_unstable_by_key(|c| c.start);
    v
}

/// gzip: one stream per member, after the header (optional fields included).
fn gzip_streams(d: &[u8]) -> Vec<Candidate> {
    let mut v = Vec::new();
    let mut pos = 0;
    while d.get(pos..pos + 3) == Some(b"\x1F\x8B\x08") {
        let Some(&flags) = d.get(pos + 3) else { break };
        let mut s = pos + 10;
        if flags & 0x04 != 0 {
            let Some(x) = d.get(s..s + 2) else { break };
            s += 2 + usize::from(u16::from_le_bytes([x[0], x[1]]));
        }
        for bit in [0x08, 0x10] {
            if flags & bit != 0 {
                let Some(z) = d.get(s..).and_then(|r| r.iter().position(|&b| b == 0)) else { return v };
                s += z + 1;
            }
        }
        if flags & 0x02 != 0 {
            s += 2;
        }
        if s >= d.len() {
            break;
        }
        // XFL byte written by zlib: 2 = level 9, 4 = level 1, otherwise 2 to 8.
        let levels: &'static [u8] = match d.get(pos + 8).copied().unwrap_or(0) {
            2 => &[9],
            4 => &[1],
            _ => &[6, 2, 3, 4, 5, 7, 8],
        };
        v.push(Candidate { start: s, wbits: 15, levels });
        // Next member: after the stream and its 8 check bytes.
        let Some((_, used)) = inflate_raw(&d[s..]) else { break };
        pos = s + used + 8;
    }
    v
}

/// PHP archive (PHAR, native format): after the stub ending with
/// `__HALT_COMPILER();`, a manifest describes each file, then their data
/// follows end to end. Files marked "gz" are raw Deflate streams (PHP's
/// zlib, level 6 by default).
fn phar_streams(d: &[u8]) -> Vec<Candidate> {
    const HALT: &[u8] = b"__HALT_COMPILER();";
    const GZ: u32 = 0x1000;
    let Some(h) = memfind(d, HALT) else { return Vec::new() };
    let mut m = h + HALT.len();
    // End of the stub: optional "?>", preceded by a space, followed by a line
    // break.
    for tail in [&b" ?>"[..], b"?>"] {
        if d[m..].starts_with(tail) {
            m += tail.len();
            break;
        }
    }
    for eol in [&b"\r\n"[..], b"\n"] {
        if d[m..].starts_with(eol) {
            m += eol.len();
            break;
        }
    }
    let le32 = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let (Some(len), Some(n), Some(alias)) = (le32(m), le32(m + 4), le32(m + 14)) else { return Vec::new() };
    let mut pos = m + 18 + alias;
    let Some(meta) = le32(pos) else { return Vec::new() };
    pos += 4 + meta;
    let mut v = Vec::new();
    let mut data = m + 4 + len;
    for _ in 0..n {
        let Some(name) = le32(pos) else { return Vec::new() };
        pos += 4 + name;
        let (Some(packed), Some(flags), Some(meta)) = (le32(pos + 8), le32(pos + 16), le32(pos + 20)) else { return Vec::new() };
        pos += 24 + meta;
        if pos > m + 4 + len || data + packed > d.len() {
            return Vec::new();
        }
        if flags as u32 & GZ != 0 {
            v.push(Candidate { start: data, wbits: 15, levels: ALL_LEVELS });
        }
        data += packed;
    }
    v
}

fn memfind(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Access to the embedded zlib.
mod zlib {
    use super::Params;
    use anyhow::{bail, Result};
    use libz_sys::*;
    use std::os::raw::c_int;

    extern "C" fn alloc(_: voidpf, items: uInt, size: uInt) -> voidpf {
        // calloc through Rust: no dependency on the libc on Windows.
        let Some(len) = (items as usize).checked_mul(size as usize) else { return std::ptr::null_mut() };
        let layout = match std::alloc::Layout::from_size_align(len + 16, 16) {
            Ok(l) => l,
            Err(_) => return std::ptr::null_mut(),
        };
        unsafe {
            let p = std::alloc::alloc_zeroed(layout);
            if p.is_null() {
                return std::ptr::null_mut();
            }
            // The size is stored in front of the block, to free it.
            (p as *mut usize).write(len);
            p.add(16) as voidpf
        }
    }

    extern "C" fn free(_: voidpf, p: voidpf) {
        if p.is_null() {
            return;
        }
        unsafe {
            let base = (p as *mut u8).sub(16);
            let len = (base as *mut usize).read();
            std::alloc::dealloc(base, std::alloc::Layout::from_size_align_unchecked(len + 16, 16));
        }
    }

    fn stream() -> z_stream {
        z_stream {
            next_in: std::ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc: alloc,
            zfree: free,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        }
    }

    /// zlib compressor in raw Deflate mode.
    struct Deflater(Box<z_stream>);

    impl Deflater {
        fn new(p: Params) -> Option<Self> {
            let mut z = Box::new(stream());
            let size = std::mem::size_of::<z_stream>() as c_int;
            let r = unsafe {
                deflateInit2_(
                    &mut *z,
                    c_int::from(p.level),
                    Z_DEFLATED,
                    -c_int::from(p.wbits),
                    c_int::from(p.mem),
                    Z_DEFAULT_STRATEGY,
                    zlibVersion(),
                    size,
                )
            };
            (r == Z_OK).then_some(Self(z))
        }

        /// Dictionary: the bytes preceding the slice.
        fn set_dictionary(&mut self, dict: &[u8]) -> bool {
            unsafe { deflateSetDictionary(&mut *self.0, dict.as_ptr(), dict.len() as uInt) == Z_OK }
        }

        /// Compresses a slice and ends it with Z_SYNC_FLUSH.
        fn sync(&mut self, input: &[u8], mut sink: impl FnMut(&[u8]) -> bool) -> bool {
            let mut buf = [0u8; 4096];
            let z = &mut *self.0;
            z.next_in = input.as_ptr() as *mut u8;
            z.avail_in = input.len() as uInt;
            loop {
                z.next_out = buf.as_mut_ptr();
                z.avail_out = buf.len() as uInt;
                let r = unsafe { deflate(z, Z_SYNC_FLUSH) };
                let n = buf.len() - z.avail_out as usize;
                if n > 0 && !sink(&buf[..n]) {
                    return false;
                }
                if r != Z_OK && r != Z_BUF_ERROR {
                    return false;
                }
                // Everything is out when zlib leaves room in the buffer.
                if z.avail_in == 0 && z.avail_out != 0 {
                    return true;
                }
            }
        }

        /// Compresses `input` entirely; `sink` receives the output as it goes and
        /// can stop everything by returning false.
        fn run(&mut self, input: &[u8], mut sink: impl FnMut(&[u8]) -> bool) -> bool {
            let mut buf = [0u8; 4096];
            let mut rest = input;
            loop {
                // avail_in is a 32-bit integer: the input is given in slices.
                let take = rest.len().min(1 << 30);
                let z = &mut *self.0;
                z.next_in = rest.as_ptr() as *mut u8;
                z.avail_in = take as uInt;
                let flush = if take == rest.len() { Z_FINISH } else { Z_NO_FLUSH };
                loop {
                    z.next_out = buf.as_mut_ptr();
                    z.avail_out = buf.len() as uInt;
                    let r = unsafe { deflate(z, flush) };
                    let n = buf.len() - z.avail_out as usize;
                    if n > 0 && !sink(&buf[..n]) {
                        return false;
                    }
                    match r {
                        Z_STREAM_END => return true,
                        Z_OK | Z_BUF_ERROR => {}
                        _ => return false,
                    }
                    if z.avail_in == 0 && z.avail_out != 0 && flush == Z_NO_FLUSH {
                        break;
                    }
                }
                rest = &rest[take..];
            }
        }
    }

    impl Drop for Deflater {
        fn drop(&mut self) {
            unsafe { deflateEnd(&mut *self.0) };
        }
    }

    /// Compresses `raw` with these settings, in one go or in slices.
    fn compress(raw: &[u8], p: Params, mut sink: impl FnMut(&[u8]) -> bool) -> bool {
        if p.chunk == 0 {
            return Deflater::new(p).is_some_and(|mut d| d.run(raw, sink));
        }
        let (chunk, window) = (p.chunk as usize, 1usize << p.wbits);
        let mut start = 0;
        loop {
            let end = (start + chunk).min(raw.len());
            let Some(mut d) = Deflater::new(p) else { return false };
            if start > 0 && !d.set_dictionary(&raw[start.saturating_sub(window)..start]) {
                return false;
            }
            if end == raw.len() {
                return d.run(&raw[start..end], &mut sink);
            }
            if !d.sync(&raw[start..end], &mut sink) {
                return false;
            }
            start = end;
        }
    }

    /// Does `raw` recompressed with these settings give back exactly
    /// `original`? Compared as we go, stopping at the first difference.
    pub fn reproduces(raw: &[u8], original: &[u8], p: Params) -> bool {
        let mut pos = 0;
        let ended = compress(raw, p, |out| {
            let ok = original.get(pos..pos + out.len()) == Some(out);
            pos += out.len();
            ok
        });
        ended && pos == original.len()
    }

    /// Recompresses `raw` after `out`, without exceeding `budget` bytes.
    pub fn deflate_into(raw: &[u8], p: Params, budget: u64, out: &mut Vec<u8>) -> Result<()> {
        let mut written = 0u64;
        let ended = compress(raw, p, |chunk| {
            written += chunk.len() as u64;
            if written > budget {
                return false;
            }
            out.extend_from_slice(chunk);
            true
        });
        if !ended {
            bail!("reconstruction impossible");
        }
        Ok(())
    }

    /// Decompresses a raw Deflate stream, without exceeding `max` bytes.
    pub fn inflate_raw(data: &[u8], max: u64) -> Option<(Vec<u8>, usize)> {
        let mut z = Box::new(stream());
        let size = std::mem::size_of::<z_stream>() as c_int;
        if unsafe { inflateInit2_(&mut *z, -15, zlibVersion(), size) } != Z_OK {
            return None;
        }
        let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(3).min(64 << 20));
        let mut result = None;
        let input = &data[..data.len().min(u32::MAX as usize)];
        z.next_in = input.as_ptr() as *mut u8;
        z.avail_in = input.len() as uInt;
        loop {
            if out.capacity() - out.len() < 1 << 16 {
                out.reserve(out.len().max(1 << 16));
            }
            let room = (out.capacity() - out.len()).min(u32::MAX as usize);
            z.next_out = unsafe { out.as_mut_ptr().add(out.len()) };
            z.avail_out = room as uInt;
            let r = unsafe { inflate(&mut *z, Z_NO_FLUSH) };
            let n = room - z.avail_out as usize;
            unsafe { out.set_len(out.len() + n) };
            if out.len() as u64 > max {
                break;
            }
            match r {
                Z_STREAM_END => {
                    result = Some((std::mem::take(&mut out), z.total_in as usize));
                    break;
                }
                Z_OK if n > 0 || z.avail_in > 0 => {}
                _ => break,
            }
        }
        unsafe { inflateEnd(&mut *z) };
        result
    }

    /// CRC-32 (the one of PNG and ZIP).
    pub fn crc32(data: &[u8]) -> u32 {
        let mut crc: uLong = 0;
        for chunk in data.chunks(1 << 30) {
            crc = unsafe { libz_sys::crc32(crc, chunk.as_ptr(), chunk.len() as uInt) };
        }
        crc as u32
    }

    pub fn version() -> String {
        unsafe { std::ffi::CStr::from_ptr(zlibVersion()) }.to_string_lossy().into_owned()
    }
}

pub fn zlib_version() -> String {
    zlib::version()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gz(raw: &[u8], level: u8, mem: u8) -> Vec<u8> {
        // Raw Deflate stream made by our zlib.
        let mut out = Vec::new();
        zlib::deflate_into(raw, Params { level, mem, wbits: 15, chunk: 0 }, u64::MAX, &mut out).unwrap();
        out
    }

    fn texte() -> Vec<u8> {
        (0..20_000).map(|i| format!("ligne {i} : bonjour de CN Technology\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn zlib_figee() {
        // Rebuilding depends on zlib's exact algorithm: if an update changes it,
        // this test breaks before any archive becomes impossible to rebuild.
        assert_eq!(zlib_version(), ZLIB_VERSION);
        let raw = texte();
        let mut empreintes = Vec::new();
        for (level, mem) in [(1, 8), (6, 8), (9, 9), (4, 3)] {
            empreintes.push(blake3::hash(&gz(&raw, level, mem)).to_hex()[..16].to_string());
        }
        assert_eq!(empreintes, EMPREINTES_ZLIB);
    }

    /// Checked on 2026-10-16 with an independent zlib (Python, zlib 1.3.1): same
    /// bytes as our zlib 1.3.2.
    const EMPREINTES_ZLIB: [&str; 4] = ["a39b6d09330a86a6", "f53d275b69191aac", "dabad50729e7a951", "878705a06781eeac"];

    #[test]
    fn pdf_reconstruit() {
        let raw = texte();
        let mut pdf = b"%PDF-1.7\n1 0 obj\n<< /Filter /FlateDecode >>\nstream\n\x78\xda".to_vec();
        pdf.extend_from_slice(&gz(&raw, 9, 8));
        pdf.extend_from_slice(b"\x01\x02\x03\x04\nendstream\nendobj\n%%EOF\n");
        let r = analyze(&pdf).unwrap();
        assert!(matches!(r.segments[1].0, Segment::Deflate { params: Params { level: 9, mem: 8, wbits: 15, chunk: 0 }, .. }));
        let t = transform(&pdf, &r).unwrap();
        assert!(t.len() > raw.len());
        assert_eq!(rebuild(&t, pdf.len() as u64).unwrap(), pdf);
        // Wrong expected size: refused.
        assert!(rebuild(&t, pdf.len() as u64 - 1).is_err());
        assert!(rebuild(&t, pdf.len() as u64 + 1).is_err());
    }

    #[test]
    fn gzip_reconstruit() {
        let raw = texte();
        let mut g = b"\x1F\x8B\x08\x08\0\0\0\0\0\x03nom.txt\0".to_vec();
        g.extend_from_slice(&gz(&raw, 6, 8));
        g.extend_from_slice(&[0; 8]);
        let r = analyze(&g).unwrap();
        let t = transform(&g, &r).unwrap();
        assert_eq!(rebuild(&t, g.len() as u64).unwrap(), g);
    }

    #[test]
    fn phar_reconstruit() {
        // Two compressed files ("gz") and one file stored as is.
        let fichiers: [(&[u8], Vec<u8>, bool); 3] = [
            (b"src/a.php", texte(), true),
            (b"LICENSE", b"MIT".to_vec(), false),
            (b"src/b.php", texte()[1000..].to_vec(), true),
        ];
        let mut manifeste = Vec::new();
        manifeste.extend_from_slice(&3u32.to_le_bytes());
        manifeste.extend_from_slice(&[0x11, 0x00]); // API version
        manifeste.extend_from_slice(&0x10000u32.to_le_bytes()); // signature present
        manifeste.extend_from_slice(&0u32.to_le_bytes()); // no alias
        manifeste.extend_from_slice(&0u32.to_le_bytes()); // no metadata
        let mut donnees = Vec::new();
        for (nom, contenu, compresse) in &fichiers {
            let stocke = if *compresse { gz(contenu, 6, 8) } else { contenu.clone() };
            manifeste.extend_from_slice(&(nom.len() as u32).to_le_bytes());
            manifeste.extend_from_slice(nom);
            for v in [contenu.len(), 0, stocke.len(), 0, if *compresse { 0x1000 | 0o644 } else { 0o644 }, 0] {
                manifeste.extend_from_slice(&(v as u32).to_le_bytes());
            }
            donnees.extend_from_slice(&stocke);
        }
        let mut phar = b"#!/usr/bin/env php\n<?php Phar::mapPhar('x.phar'); __HALT_COMPILER(); ?>\r\n".to_vec();
        phar.extend_from_slice(&(manifeste.len() as u32).to_le_bytes());
        phar.extend_from_slice(&manifeste);
        phar.extend_from_slice(&donnees);
        phar.extend_from_slice(&[7; 20]);
        phar.extend_from_slice(b"\x02\0\0\0GBMB");
        let r = analyze(&phar).unwrap();
        let refaits = r.segments.iter().filter(|s| matches!(s.0, Segment::Deflate { .. })).count();
        assert_eq!(refaits, 2);
        let t = transform(&phar, &r).unwrap();
        assert_eq!(rebuild(&t, phar.len() as u64).unwrap(), phar);
        // Truncated manifest: nothing is searched, no crash.
        assert!(analyze(&phar[..100]).is_none());
    }

    #[test]
    fn forme_abimee_refusee() {
        let raw = texte();
        let mut g = b"\x1F\x8B\x08\0\0\0\0\0\0\x03".to_vec();
        g.extend_from_slice(&gz(&raw, 6, 8));
        g.extend_from_slice(&[0; 8]);
        let t = transform(&g, &analyze(&g).unwrap()).unwrap();
        // Each byte of the recipe modified: error or different result, never a
        // crash.
        for i in 0..40.min(t.len()) {
            let mut u = t.clone();
            u[i] ^= 0x55;
            if let Ok(x) = rebuild(&u, g.len() as u64) {
                assert_ne!(x, g);
            }
        }
        // Truncated.
        assert!(rebuild(&t[..t.len() - 1], g.len() as u64).is_err());
    }

    #[test]
    fn zip_par_tranches_reconstruit() {
        // ZIP entry compressed like LibreOffice: 128 KB slices, 32 KB dictionary,
        // Z_SYNC_FLUSH between slices.
        let raw: Vec<u8> = (0..60_000).flat_map(|i| format!("<c r=\"A{i}\"><v>{}</v></c>", i * 7 % 1000).into_bytes()).collect();
        let p = Params { level: 6, mem: 8, wbits: 15, chunk: 128 << 10 };
        let mut body = Vec::new();
        zlib::deflate_into(&raw, p, u64::MAX, &mut body).unwrap();
        assert!(memfind(&body, b"\x00\x00\xFF\xFF").is_some());
        let mut d = b"PK\x03\x04\x14\x00\x00\x00\x08\x00".to_vec();
        d.extend_from_slice(&[0; 16]);
        d.extend_from_slice(&[1, 0, 0, 0]);
        d.push(b'x');
        let local = 0usize;
        d.extend_from_slice(&body);
        let central = d.len();
        d.extend_from_slice(b"PK\x01\x02");
        d.extend_from_slice(&[0; 6]);
        d.extend_from_slice(&[8, 0]);
        d.extend_from_slice(&[0; 16]);
        d.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        d.extend_from_slice(&(local as u32).to_le_bytes());
        d.push(b'x');
        let end = d.len();
        d.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
        d.extend_from_slice(&((end - central) as u32).to_le_bytes());
        d.extend_from_slice(&(central as u32).to_le_bytes());
        d.extend_from_slice(&[0, 0]);
        let r = analyze(&d).unwrap();
        assert!(r.segments.iter().any(|s| matches!(s.0, Segment::Deflate { params, .. } if params == p)));
        let t = transform(&d, &r).unwrap();
        assert_eq!(rebuild(&t, d.len() as u64).unwrap(), d);
    }

    /// Image from the phpxmlrpc documentation (BSD licence, see
    /// tests/donnees/photo-phpxmlrpc-LICENCE.txt).
    const PHOTO: &[u8] = include_bytes!("../tests/donnees/photo-phpxmlrpc.jpg");

    #[test]
    fn photo_recompressee_a_l_identique() {
        let j = crate::jpeg::decode(PHOTO).unwrap();
        assert_eq!(crate::jpeg::encode(&j).unwrap(), PHOTO);
        let r = analyze(PHOTO).unwrap();
        assert!(matches!(r.segments[..], [(Segment::Jpeg { .. }, 0, _)]));
        let t = transform(PHOTO, &r).unwrap();
        assert!(t.len() < PHOTO.len());
        assert_eq!(rebuild(&t, PHOTO.len() as u64).unwrap(), PHOTO);
        // Wrong expected size: refused.
        assert!(rebuild(&t, PHOTO.len() as u64 - 1).is_err());
    }

    /// The same, as progressive JPEG (jpegtran), with and without restarts.
    const PROGRESSIVES: [&[u8]; 2] = [
        include_bytes!("../tests/donnees/photo-phpxmlrpc-progressive.jpg"),
        include_bytes!("../tests/donnees/photo-phpxmlrpc-progressive-reprises.jpg"),
    ];

    #[test]
    fn photos_progressives_a_l_identique() {
        for p in PROGRESSIVES {
            let j = crate::jpeg::decode(p).unwrap();
            assert!(j.progressive && j.scans.len() >= 5);
            // Same coefficients as the original sequential photo.
            assert_eq!(j.blocks, crate::jpeg::decode(PHOTO).unwrap().blocks);
            assert_eq!(crate::jpeg::encode(&j).unwrap(), p);
            let t = transform(p, &analyze(p).unwrap()).unwrap();
            assert!(t.len() < p.len());
            assert_eq!(rebuild(&t, p.len() as u64).unwrap(), p);
            // Damaged: error or different result, without a crash.
            for i in (0..t.len()).step_by(97) {
                let mut u = t.clone();
                u[i] ^= 0x10;
                let _ = rebuild(&u, p.len() as u64);
            }
        }
    }

    #[test]
    fn photo_abimee_sans_plantage() {
        let t = transform(PHOTO, &analyze(PHOTO).unwrap()).unwrap();
        let mut x = 7u64;
        for _ in 0..300 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let mut u = t.clone();
            let i = (x >> 33) as usize % u.len();
            u[i] ^= 1 << (x % 8);
            if let Ok(back) = rebuild(&u, PHOTO.len() as u64) {
                // An error not detected here would be caught by the file hash.
                let _ = back;
            }
        }
        for n in [3, 100, 600, PHOTO.len() / 2, PHOTO.len() - 3] {
            assert!(crate::jpeg::decode(&PHOTO[..n]).is_err());
            let _ = analyze(&PHOTO[..n]);
        }
    }

    /// Made-up PNG: 64 × 300 greyscale image, zlib stream cut into 1000-byte
    /// IDAT chunks.
    fn png() -> Vec<u8> {
        let (w, h) = (64u32, 300u32);
        let mut raw = Vec::new();
        for y in 0..h {
            raw.push(0); // "none" filter
            raw.extend((0..w).map(|x| ((x * 3 + y * 7) % 256) as u8 / 4 * 4));
        }
        let mut z = vec![0x78, 0xDA];
        zlib::deflate_into(&raw, Params { level: 9, mem: 8, wbits: 15, chunk: 0 }, u64::MAX, &mut z).unwrap();
        // Adler-32 of the contents.
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&(b << 16 | a).to_be_bytes());
        let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = zlib::crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        };
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        chunk(&mut p, b"IHDR", &ihdr);
        for part in z.chunks(1000) {
            chunk(&mut p, b"IDAT", part);
        }
        chunk(&mut p, b"IEND", &[]);
        p
    }

    #[test]
    fn png_reconstruit_a_l_identique() {
        let p = png();
        let r = analyze(&p).unwrap();
        assert!(matches!(r.segments[..], [(Segment::Png { .. }, 0, _)]));
        let t = transform(&p, &r).unwrap();
        assert_eq!(rebuild(&t, p.len() as u64).unwrap(), p);
        assert!(rebuild(&t, p.len() as u64 + 1).is_err());
        // Damaged transformed form: error or different result, without a crash.
        for i in (0..t.len()).step_by(7) {
            let mut u = t.clone();
            u[i] ^= 0x5A;
            if let Ok(x) = rebuild(&u, p.len() as u64) {
                let _ = x;
            }
        }
    }

    /// Made-up ZIP: the test photo stored as is, then compressed.
    fn zip_avec_photos() -> Vec<u8> {
        let mut deflated = Vec::new();
        zlib::deflate_into(PHOTO, Params { level: 6, mem: 8, wbits: 15, chunk: 0 }, u64::MAX, &mut deflated).unwrap();
        let entries: [(&[u8], u16, &[u8]); 2] = [(b"a.jpg", 0, PHOTO), (b"b.jpg", 8, &deflated)];
        let (mut z, mut central) = (Vec::new(), Vec::new());
        for (name, method, data) in entries {
            let local = z.len() as u32;
            let crc = zlib::crc32(PHOTO);
            let mut h = b"PK\x03\x04\x14\x00\x00\x00".to_vec();
            h.extend_from_slice(&method.to_le_bytes());
            h.extend_from_slice(&[0; 4]);
            h.extend_from_slice(&crc.to_le_bytes());
            h.extend_from_slice(&(data.len() as u32).to_le_bytes());
            h.extend_from_slice(&(PHOTO.len() as u32).to_le_bytes());
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&[0, 0]);
            h.extend_from_slice(name);
            z.extend_from_slice(&h);
            z.extend_from_slice(data);
            let mut c = b"PK\x01\x02\x14\x00\x14\x00\x00\x00".to_vec();
            c.extend_from_slice(&method.to_le_bytes());
            c.extend_from_slice(&[0; 4]);
            c.extend_from_slice(&crc.to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(PHOTO.len() as u32).to_le_bytes());
            c.extend_from_slice(&(name.len() as u16).to_le_bytes());
            c.extend_from_slice(&[0; 12]);
            c.extend_from_slice(&local.to_le_bytes());
            c.extend_from_slice(name);
            central.extend_from_slice(&c);
        }
        let at = z.len() as u32;
        z.extend_from_slice(&central);
        z.extend_from_slice(b"PK\x05\x06\0\0\0\0\x02\0\x02\0");
        z.extend_from_slice(&(central.len() as u32).to_le_bytes());
        z.extend_from_slice(&at.to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        z
    }

    #[test]
    fn photos_dans_un_zip() {
        let z = zip_avec_photos();
        let r = analyze(&z).unwrap();
        let kinds: Vec<_> = r.segments.iter().map(|s| std::mem::discriminant(&s.0)).collect();
        assert!(r.segments.iter().any(|s| matches!(s.0, Segment::Embedded { .. })));
        assert!(r.segments.iter().any(|s| matches!(s.0, Segment::DeflateNested { .. })), "{kinds:?}");
        let t = transform(&z, &r).unwrap();
        assert!(t.len() < z.len());
        assert_eq!(rebuild(&t, z.len() as u64).unwrap(), z);
    }

    #[test]
    fn photo_dans_un_pdf() {
        let mut pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /XObject /Subtype /Image /Filter /DCTDecode >>\nstream\r\n".to_vec();
        pdf.extend_from_slice(PHOTO);
        pdf.extend_from_slice(b"\r\nendstream\nendobj\n%%EOF\n");
        let r = analyze(&pdf).unwrap();
        assert!(r.segments.iter().any(|s| matches!(s.0, Segment::Embedded { .. })));
        let t = transform(&pdf, &r).unwrap();
        assert!(t.len() < pdf.len());
        assert_eq!(rebuild(&t, pdf.len() as u64).unwrap(), pdf);
    }

    #[test]
    fn gzip_tronque_sans_plantage() {
        for n in 0..=10 {
            let g = &b"\x1F\x8B\x08\x00\x00\x00\x00\x00\x02\x03"[..n];
            assert!(analyze(g).is_none());
        }
    }

    #[test]
    fn programme_inconnu_predit() {
        // Stream that does not come from zlib (hand-made "stored" Deflate,
        // repetitive contents): zlib cannot redo it, the predictor can.
        let mut g = b"\x1F\x8B\x08\0\0\0\0\0\0\x03".to_vec();
        let data = vec![b'a'; 1000];
        g.extend_from_slice(&[0x01, 0xE8, 0x03, 0x17, 0xFC]);
        g.extend_from_slice(&data);
        g.extend_from_slice(&[0; 8]);
        let r = analyze(&g).unwrap();
        assert!(r.segments.iter().any(|s| matches!(s.0, Segment::Predicted { .. })));
        let t = transform(&g, &r).unwrap();
        assert!(t.len() < g.len() + 64);
        assert_eq!(rebuild(&t, g.len() as u64).unwrap(), g);
    }

}
