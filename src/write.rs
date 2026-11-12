//! Creating a .cnz archive.
//!
//! Steps: 1) walk through the source, 2) recognise each file by its
//! contents and store it next to similar ones (the dispatcher), already
//! compressed contents apart; spot duplicates (whole files, chunks of large
//! files) to store them only once; recognise the Deflate streams that can
//! be redone identically (PDF, ZIP, Office, .gz) to store their
//! decompressed contents, 3) cut everything into blocks, 4) read each block
//! only once, compute the hash of each chunk and compress, on all cores,
//! 5) write the index (twice), 6) compute the recovery data, then write the
//! footer.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use zeroize::Zeroizing;

use crate::classify::{self, Family};
use crate::crypto::{index_number, Crypto, Params};
use crate::dedup::{self, Chunk};
use crate::format::*;
use crate::index::{self, Entry, Extent, Kind, Layer, Rebuilt, HASH_LEN};
use crate::meta::Collector;
use crate::platform;
use crate::read::Archive;
use crate::rebuild::{self, Recipe};
use crate::recovery;
use crate::resources;
use crate::progress::{Cancelled, Progress};
use crate::timing::{Chrono, Step};

pub struct Options {
    /// zstd level, from 1 (fast) to 22 (maximum).
    pub level: i32,
    /// Size of the data blocks, in bytes (None: automatic, according to the
    /// volume and the number of cores).
    pub block_size: Option<u64>,
    /// Maximum memory to use (None: half of the free memory).
    pub max_memory: Option<u64>,
    /// Password: the archive is encrypted, file names included.
    pub password: Option<Zeroizing<String>>,
    /// Recovery data, as a % of the archive (0 = none).
    pub recovery_percent: u32,
    /// Guarantee the rebuild of this number of whole lost blocks (0 = no).
    /// The more demanding of the two settings wins.
    pub recovery_blocks: u32,
    /// Recompress JPEG photos (stage 5); otherwise they are stored as they are
    /// (faster).
    pub photos: bool,
    /// "Maximum" mode (stage 7): the normal stream is compressed by the
    /// context-mixing engine (much smaller, much slower, for compression as for
    /// extraction).
    pub maximum: bool,
    /// AI mode (prototype, implies `maximum`): an installed expert (see `ia.rs`)
    /// helps the engine on text blocks; very slow.
    pub ia: bool,
    /// Progress and stop request, shared with the interface.
    pub progress: Arc<Progress>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            level: 19,
            block_size: None,
            max_memory: None,
            password: None,
            recovery_percent: 3,
            recovery_blocks: 0,
            photos: true,
            maximum: false,
            ia: false,
            progress: Arc::default(),
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub raw_bytes: u64,
    pub archive_bytes: u64,
    pub blocks: u64,
    pub skipped: Vec<String>,
    /// Size of the recovery data.
    pub recovery_bytes: u64,
    /// Size of the largest stored block.
    pub largest_block: u64,
    /// Addition: existing entries replaced by a new version.
    pub replaced: u64,
    /// Block size and number of cores used.
    pub plan: Option<resources::Plan>,
    /// Deduplication: files identical to another one, and bytes not stored
    /// (identical files and repeated chunks).
    pub duplicate_files: u64,
    pub duplicate_bytes: u64,
    /// Deflate rebuilding: files stored decompressed, and their original size.
    pub rebuilt_files: u64,
    pub rebuilt_bytes: u64,
    /// Recompressed JPEG photos (counted separately), and their original size.
    pub photos: u64,
    pub photo_bytes: u64,
    pub steps: Vec<Step>,
}

/// A block ready to write: header, data and hash of each chunk.
type Built = (BlockHeader, Vec<u8>, Vec<[u8; HASH_LEN]>);

/// Compressed blocks waiting to be written.
struct Queue {
    done: BTreeMap<usize, Result<Built>>,
    written: usize,
    stop: bool,
}

/// Already compressed contents (photos, videos, archives…): 8 MB blocks.
/// They keep the cores busy that stay free during the large text blocks,
/// and damage there loses only little data (the largest stored block, which
/// sets the cost of the "whole blocks" guarantee, stays a text block).
const COMPRESSED_BLOCK: u64 = 8 << 20;

/// Engine of a data block.
#[derive(Clone, Copy)]
enum Engine {
    /// zstd at this level (and the built-in dictionary for a small block).
    Zstd(i32),
    /// Context-mixing engine ("maximum" mode), with an AI expert if `true` and
    /// if the block is text.
    Max(bool),
}

/// How to compress the blocks of one write.
#[derive(Clone, Copy)]
struct Method {
    level: i32,
    maximum: bool,
    ia: bool,
    /// The first blocks (normal stream); the following ones are the already
    /// compressed contents.
    normal_blocks: usize,
}

impl Method {
    /// "Maximum" mode: in-house engine for the normal stream; the already
    /// compressed contents (videos, archives…) would gain nothing from it, they
    /// stay with zstd.
    fn engine(self, i: usize) -> Engine {
        if (self.maximum || self.ia) && i < self.normal_blocks {
            Engine::Max(self.ia)
        } else {
            Engine::Zstd(self.level)
        }
    }
}

/// A file chunk to copy into a block.
struct Piece {
    entry: usize,
    /// Number of the chunk in the file's list of extents.
    extent: usize,
    file_offset: u64,
    len: u64,
    /// Size and modification date noted while walking through: if the file
    /// changed in the meantime, we stop (a copy would otherwise refer to
    /// contents that are no longer its own).
    file_size: u64,
    mtime: (i64, u32),
    /// Rebuilt file: its recipe; the chunk is taken from its transformed form
    /// (and `file_offset` refers to it).
    recipe: Option<Arc<Recipe>>,
    /// Recompressed photo: temporary file, position and length.
    photo: Option<(Arc<PathBuf>, u64, u64)>,
}

/// Temporary file of the photos recompressed during the analysis (stage 5):
/// keeping them in memory would cost almost their size. Deleted at the end,
/// whatever happens.
struct PhotoSpill {
    path: Arc<PathBuf>,
    file: Mutex<Option<(File, u64)>>,
}

impl PhotoSpill {
    fn new(archive: &Path) -> Self {
        Self { path: Arc::new(PathBuf::from(format!("{}.photos.tmp", archive.display()))), file: Mutex::new(None) }
    }

    /// Adds a photo; returns its position.
    fn push(&self, data: &[u8]) -> Result<u64> {
        let mut f = self.file.lock().unwrap();
        if f.is_none() {
            let file = File::create(&*self.path).with_context(|| format!("impossible de créer {}", self.path.display()))?;
            *f = Some((file, 0));
        }
        let (file, pos) = f.as_mut().unwrap();
        file.write_all(data)?;
        let at = *pos;
        *pos += data.len() as u64;
        Ok(at)
    }

    fn read(path: &Path, at: u64, len: u64) -> Result<Vec<u8>> {
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(at))?;
        let mut v = vec![0u8; len as usize];
        f.read_exact(&mut v)?;
        Ok(v)
    }
}

impl Drop for PhotoSpill {
    fn drop(&mut self) {
        if let Ok(mut f) = self.file.lock() {
            if f.take().is_some() {
                let _ = fs::remove_file(&*self.path);
            }
        }
    }
}

/// References set by deduplication, to be completed once the blocks are
/// written (this is when the hashes of the chunks are known).
#[derive(Default)]
struct Links {
    /// (entry, extent) that reuses (entry, extent) of a chunk already stored.
    extents: Vec<(usize, usize, usize, usize)>,
    /// Identical file → its original: it reuses all its extents.
    copies: Vec<(usize, usize)>,
}

impl Links {
    fn apply(&self, entries: &mut [Entry]) {
        let extents = |e: &Entry| match &e.kind {
            Kind::File { extents, .. } => extents.clone(),
            _ => unreachable!(),
        };
        for &(i, k, j, l) in &self.extents {
            let hash = extents(&entries[j])[l].hash;
            if let Kind::File { extents, .. } = &mut entries[i].kind {
                extents[k].hash = hash;
            }
        }
        // A copy reuses everything from the original: extents and rebuild.
        for &(copy, original) in &self.copies {
            entries[copy].kind = entries[original].kind.clone();
        }
    }
}

struct Scan {
    entries: Vec<Entry>,
    /// Real path on disk, for ordinary files.
    sources: Vec<Option<PathBuf>>,
    /// The archive being written, which must not include itself.
    exclude: Option<PathBuf>,
    stats: Stats,
    collector: Collector,
}

pub fn compress(source: &Path, archive: &Path, opts: &Options) -> Result<Stats> {
    compress_many(&[source.to_path_buf()], archive, opts)
}

/// Compresses several files or folders into an archive. A single folder:
/// its contents are at the root of the archive. Several items: each one is
/// stored under its name ("photos/…", "quote.pdf").
pub fn compress_many(sources: &[PathBuf], archive: &Path, opts: &Options) -> Result<Stats> {
    if opts.block_size.is_some_and(|b| b == 0 || b > MAX_BLOCK_RAW) {
        bail!("taille de bloc invalide");
    }
    let mut chrono = Chrono::new();
    let (mut entries, sources, mut stats) = scan_sources(sources, archive, true)?;
    chrono.lap("1. Lister les fichiers");
    let mut order = sort_files(&entries, &sources);
    chrono.lap("2. Ranger");
    let mut found = find_duplicates(&entries, &sources, &mut order, &opts.progress)?;
    chrono.lap("2 bis. Repérer les doublons");
    let spill = PhotoSpill::new(archive);
    find_rebuilds(&mut entries, &sources, &mut order, &mut found, &mut stats, &spill, opts)?;
    chrono.lap("2 ter. Flux Deflate à reconstruire");
    let plan = resources::plan(stats.raw_bytes, opts.block_size, opts.max_memory, opts.maximum || opts.ia);
    stats.plan = Some(plan);
    let (plans, links, normal_blocks) = plan_blocks(&mut entries, &order, &found, plan.block_size, 0, &mut stats, &spill);
    let method = Method { level: opts.level, maximum: opts.maximum, ia: opts.ia, normal_blocks };
    let to_compress: u64 = plans.iter().flatten().map(|p| p.len).sum();
    chrono.lap("3. Découper sur le papier");

    // Writing to a temporary file, renamed only at the end: an interrupted
    // compression never leaves a half-written archive.
    let partial = PathBuf::from(format!("{}.partiel", archive.display()));
    let result = (|| {
        let (crypto, params) = match &opts.password {
            Some(p) => {
                let (params, crypto) = Crypto::create(p)?;
                (Some(crypto), Some(params))
            }
            None => (None, None),
        };
        let mut target = Target::create(&partial, crypto, params)?;
        opts.progress.start("compression", to_compress);
        target.write_blocks(&mut entries, &sources, &plans, method, plan.workers, &opts.progress)?;
        links.apply(&mut entries);
        chrono.lap("4. Lire, empreintes et compresser");
        target.finish(NewLayer { number: 1, previous: None, removed: Vec::new(), entries }, opts, &mut chrono)
    })();
    match result {
        Ok(w) => {
            fs::rename(&partial, archive)?;
            w.fill(&mut stats);
            stats.steps = chrono.steps;
            Ok(stats)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Adds the contents of `source` to an existing archive, without
/// recompressing what it already holds. A file with the same name replaces
/// the old one.
///
/// The addition writes a new index layer (a new version) that holds only
/// its novelties and refers to the previous one. Old versions stay readable;
/// if the addition is interrupted, the archive reads back in its previous
/// state (by walking through the blocks).
pub fn append(
    archive: &Path,
    source: &Path,
    opts: &Options,
    password: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<Stats> {
    append_sources(archive, &[source.to_path_buf()], true, opts, password)
}

/// Like `append`, but each added file or folder keeps its name (a "photos"
/// folder arrives under "photos/", not at the root).
pub fn append_many(
    archive: &Path,
    sources: &[PathBuf],
    opts: &Options,
    password: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<Stats> {
    append_sources(archive, sources, false, opts, password)
}

fn append_sources(
    archive: &Path,
    sources_a_ajouter: &[PathBuf],
    a_plat: bool,
    opts: &Options,
    password: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<Stats> {
    if opts.block_size.is_some_and(|b| b == 0 || b > MAX_BLOCK_RAW) {
        bail!("taille de bloc invalide");
    }
    let mut chrono = Chrono::new();
    let old = Archive::open_with_password(archive, password)?;
    if !old.notes.is_empty() {
        bail!("archive abîmée ({}) : lancez « cnz reparer » avant d'y ajouter des fichiers", old.notes.join(", "));
    }
    let cut = old.end_of_content()?;
    let old_largest = old.stored_block_sizes()?.into_iter().max().unwrap_or(0);
    let (old_index, crypto, params, latest, number) = old.into_parts()?;

    let (mut entries, sources, mut stats) = scan_sources(sources_a_ajouter, archive, a_plat)?;
    chrono.lap("1. Lister les fichiers");
    let mut order = sort_files(&entries, &sources);
    chrono.lap("2. Ranger");
    let first_block = old_index.blocks.len() as u64;
    let mut found = find_duplicates(&entries, &sources, &mut order, &opts.progress)?;
    chrono.lap("2 bis. Repérer les doublons");
    let spill = PhotoSpill::new(archive);
    find_rebuilds(&mut entries, &sources, &mut order, &mut found, &mut stats, &spill, opts)?;
    chrono.lap("2 ter. Flux Deflate à reconstruire");
    let plan = resources::plan(stats.raw_bytes, opts.block_size, opts.max_memory, opts.maximum || opts.ia);
    stats.plan = Some(plan);
    let (plans, links, normal_blocks) =
        plan_blocks(&mut entries, &order, &found, plan.block_size, first_block, &mut stats, &spill);
    let method = Method { level: opts.level, maximum: opts.maximum, ia: opts.ia, normal_blocks };
    let to_compress: u64 = plans.iter().flatten().map(|p| p.len).sum();
    chrono.lap("3. Découper sur le papier");

    // The end of the archive (recovery data and footer) is put aside: in case
    // of error or interruption, it is put back and the archive becomes exactly
    // what it was again.
    let tail = PathBuf::from(format!("{}.fin", archive.display()));
    save_tail(archive, cut, &tail)?;
    let result = (|| -> Result<Written> {
        let mut target = Target::extend(archive, cut, crypto, params, old_index.blocks, old_largest)?;
        opts.progress.start("compression", to_compress);
        target.write_blocks(&mut entries, &sources, &plans, method, plan.workers, &opts.progress)?;
        links.apply(&mut entries);
        chrono.lap("4. Lire, empreintes et compresser");

        // Entries of the previous version that this version replaces. An old
        // folder replaced by a file also takes its contents along.
        let new_paths: HashSet<&[u8]> = entries.iter().map(|e| e.path.as_slice()).collect();
        let not_dirs: Vec<&[u8]> =
            entries.iter().filter(|e| !matches!(e.kind, Kind::Dir)).map(|e| e.path.as_slice()).collect();
        let removed: Vec<Vec<u8>> = old_index
            .entries
            .into_iter()
            .filter(|e| {
                new_paths.contains(e.path.as_slice())
                    || not_dirs.iter().any(|p| e.path.starts_with(p) && e.path.get(p.len()) == Some(&b'/'))
            })
            .map(|e| e.path)
            .collect();
        stats.replaced = removed.len() as u64;

        let layer = NewLayer { number: number + 1, previous: Some(latest), removed, entries };
        target.finish(layer, opts, &mut chrono)
    })();
    match result {
        Ok(w) => {
            let _ = fs::remove_file(&tail);
            w.fill(&mut stats);
            stats.steps = chrono.steps;
            Ok(stats)
        }
        Err(e) => {
            restore_tail(archive, cut, &tail).context("impossible de remettre l'archive dans son état d'origine")?;
            Err(e)
        }
    }
}

/// Copies the end of the archive, from `cut`, into the `tail` file.
fn save_tail(archive: &Path, cut: u64, tail: &Path) -> Result<()> {
    let mut src = File::open(archive)?;
    src.seek(SeekFrom::Start(cut))?;
    let mut dst = File::create(tail).with_context(|| format!("impossible de créer {}", tail.display()))?;
    io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;
    Ok(())
}

/// Cuts the archive at `cut` and puts its original end back.
fn restore_tail(archive: &Path, cut: u64, tail: &Path) -> Result<()> {
    let mut f = OpenOptions::new().write(true).open(archive)?;
    f.set_len(cut)?;
    f.seek(SeekFrom::Start(cut))?;
    io::copy(&mut File::open(tail)?, &mut f)?;
    f.sync_all()?;
    fs::remove_file(tail)?;
    Ok(())
}

#[derive(Debug, Default)]
pub struct CompactStats {
    pub before: u64,
    pub after: u64,
    /// Versions present before compacting.
    pub versions: u64,
    /// Fully useful blocks, copied without recompression.
    pub copied: u64,
    /// Blocks mixing useful and stale data, recompressed.
    pub repacked: u64,
    /// Fully stale blocks, deleted.
    pub dropped: u64,
}

/// Rewrites the archive keeping only the last version of each file.
///
/// A fully useful block is copied as is (without recompression; an encrypted
/// archive is only resealed with its new number). A block mixing useful and
/// stale data is decompressed, cleared of the stale part and recompressed. A
/// fully stale block disappears. The archive is written alongside, then
/// replaces the original only at the end.
pub fn compact(
    archive: &Path,
    opts: &Options,
    password: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<CompactStats> {
    let old = Archive::open_with_password(archive, password)?;
    if !old.notes.is_empty() {
        bail!("archive abîmée ({}) : lancez « cnz reparer » avant de la compacter", old.notes.join(", "));
    }
    let mut stats =
        CompactStats { before: fs::metadata(archive)?.len(), versions: old.versions().len() as u64, ..Default::default() };
    let mut entries = old.index.entries.clone();
    let nblocks = old.index.blocks.len();

    // Chunks still useful in each old block, and new number of the kept blocks.
    let mut live: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nblocks];
    for (i, e) in entries.iter().enumerate() {
        if let Kind::File { extents, .. } = &e.kind {
            for (k, x) in extents.iter().enumerate() {
                live[x.block as usize].push((i, k));
            }
        }
    }
    let mut new_number = vec![None; nblocks];
    let mut kept = 0u64;
    for (b, pieces) in live.iter().enumerate() {
        if !pieces.is_empty() {
            new_number[b] = Some(kept);
            kept += 1;
        }
    }

    let partial = PathBuf::from(format!("{}.partiel", archive.display()));
    let result = (|| -> Result<Written> {
        let crypto = old.crypto_clone();
        let mut target = Target::create(&partial, crypto.clone(), old.params_clone())?;
        let blocks: Vec<usize> = (0..nblocks).collect();
        let batch = rayon::current_num_threads().max(1) * 2;
        let sizes = old.stored_block_sizes()?;
        opts.progress.start("compactage", sizes.iter().sum());
        for chunk in blocks.chunks(batch) {
            opts.progress.check()?;
            let done: Vec<Result<Option<Repacked>>> = chunk
                .par_iter()
                .map(|&b| {
                    let Some(number) = new_number[b] else { return Ok(None) };
                    repack_block(&old, b, number, &live[b], &entries, crypto.as_ref(), opts.level, &opts.progress)
                        .map(Some)
                })
                .collect();
            for (&b, r) in chunk.iter().zip(done) {
                opts.progress.advance(sizes[b]);
                let Some(r) = r? else {
                    stats.dropped += 1;
                    continue;
                };
                if r.copied {
                    stats.copied += 1;
                } else {
                    stats.repacked += 1;
                }
                target.push_block(&r.header, &r.payload)?;
                for (&(i, k), &offset) in live[b].iter().zip(&r.offsets) {
                    if let Kind::File { extents, .. } = &mut entries[i].kind {
                        extents[k].block = new_number[b].unwrap();
                        extents[k].offset = offset;
                    }
                }
            }
        }
        let mut chrono = Chrono::new();
        target.finish(NewLayer { number: 1, previous: None, removed: Vec::new(), entries }, opts, &mut chrono)
    })();
    // The old archive must be closed before being replaced: Windows refuses to
    // replace an open file.
    drop(old);
    match result {
        Ok(w) => {
            fs::rename(&partial, archive)?;
            stats.after = w.len;
            Ok(stats)
        }
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

struct Repacked {
    header: BlockHeader,
    payload: Vec<u8>,
    /// New position of each useful chunk, in the order of `live`.
    offsets: Vec<u64>,
    copied: bool,
}

fn repack_block(
    old: &Archive,
    b: usize,
    number: u64,
    pieces: &[(usize, usize)],
    entries: &[Entry],
    crypto: Option<&Crypto>,
    level: i32,
    progress: &Progress,
) -> Result<Repacked> {
    let extent = |&(i, k): &(usize, usize)| match &entries[i].kind {
        Kind::File { extents, .. } => extents[k].clone(),
        _ => unreachable!(),
    };
    let (header, stored) = old.read_stored_block(b)?;
    // Useful areas of the block (the same chunk may serve several entries).
    let ranges: BTreeMap<u64, u64> = pieces.iter().map(|p| extent(p)).map(|x| (x.offset, x.len)).collect();
    let live_len: u64 = ranges.values().sum();

    if live_len == header.raw_len {
        // Fully useful block: copied without recompression.
        let comp = old.open_payload(&header, stored, b as u64)?;
        let payload = match crypto {
            Some(c) => c.seal(BlockKind::Data, header.codec, header.raw_len, number, &comp)?,
            None => comp,
        };
        let header = BlockHeader { comp_len: payload.len() as u64, payload_crc: crc32c::crc32c(&payload), ..header };
        let offsets = pieces.iter().map(|p| extent(p).offset).collect();
        return Ok(Repacked { header, payload, offsets, copied: true });
    }

    // Mixed block: only the useful areas are kept, then it is recompressed.
    let comp = old.open_payload(&header, stored, b as u64)?;
    let ia = header.codec == Codec::Cm && crate::cm::uses_ia(&comp);
    let raw = crate::read::decompress(&header, comp)?;
    let mut fresh = Vec::with_capacity(live_len as usize);
    let mut moved = BTreeMap::new();
    for (&offset, &len) in &ranges {
        moved.insert(offset, fresh.len() as u64);
        fresh.extend_from_slice(raw.get(offset as usize..(offset + len) as usize).context("morceau hors du bloc")?);
    }
    // Compacting counts its progress per block, not per slice; a "maximum"
    // mode block stays one (with an expert if it had one).
    let engine = if header.codec == Codec::Cm { Engine::Max(ia) } else { Engine::Zstd(level) };
    let (codec, mut payload) = compress_block(&fresh, engine, progress, false)?;
    if let Some(c) = crypto {
        payload = c.seal(BlockKind::Data, codec, fresh.len() as u64, number, &payload)?;
    }
    let header = BlockHeader {
        kind: BlockKind::Data,
        codec,
        raw_len: fresh.len() as u64,
        comp_len: payload.len() as u64,
        payload_crc: crc32c::crc32c(&payload),
    };
    let offsets = pieces.iter().map(|p| moved[&extent(p).offset]).collect();
    Ok(Repacked { header, payload, offsets, copied: false })
}

/// 1. Walk through the source (without including the archive itself).
/// `a_plat`: a folder given alone has its contents put at the root;
/// otherwise each item is stored under its name.
fn scan_sources(roots: &[PathBuf], archive: &Path, a_plat: bool) -> Result<(Vec<Entry>, Vec<Option<PathBuf>>, Stats)> {
    let exclude = archive
        .parent()
        .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p })
        .and_then(|p| fs::canonicalize(p).ok())
        .zip(archive.file_name())
        .map(|(dir, name)| dir.join(name));
    let mut scan =
        Scan { entries: Vec::new(), sources: Vec::new(), exclude, stats: Stats::default(), collector: Collector::default() };
    let single = a_plat && roots.len() == 1;
    let mut names = HashSet::new();
    for source in roots {
        let meta = fs::symlink_metadata(source).with_context(|| format!("impossible de lire {}", source.display()))?;
        if single && meta.is_dir() {
            walk(source, &[], &mut scan)?;
            continue;
        }
        // The real name ("." or "dossier/.." have none otherwise).
        let real = if source.file_name().is_some() { source.clone() } else { fs::canonicalize(source)? };
        let name = platform::os_to_bytes(real.file_name().context("nom de fichier invalide")?);
        if !names.insert(name.clone()) {
            bail!("deux éléments à compresser s'appellent « {} » : renommez-en un", String::from_utf8_lossy(&name));
        }
        add_entry(source, name.clone(), &meta, &mut scan)?;
        if meta.is_dir() {
            walk(source, &name, &mut scan)?;
        }
    }
    Ok((scan.entries, scan.sources, scan.stats))
}

/// 2. The dispatcher: each file is recognised by its contents, then sorted
/// by family, by extension, next to similar ones, finally by name. Already
/// compressed contents form a second, separate stream.
fn sort_files(entries: &[Entry], sources: &[Option<PathBuf>]) -> Order {
    let files: Vec<usize> = (0..entries.len()).filter(|&i| sources[i].is_some()).collect();
    let profiles: Vec<classify::Profile> = files
        .par_iter()
        .map(|&i| {
            let Kind::File { size, .. } = &entries[i].kind else { unreachable!() };
            classify::profile(sources[i].as_ref().unwrap(), *size)
        })
        .collect();
    let mut sorted: Vec<usize> = (0..files.len()).collect();
    sorted.sort_by_cached_key(|&k| (profiles[k].family, sort_key(&entries[files[k]].path)));

    let mut order = Order { digests: vec![None; entries.len()], ..Default::default() };
    order.families = vec![None; entries.len()];
    for (k, &i) in files.iter().enumerate() {
        order.digests[i] = profiles[k].digest;
        order.families[i] = Some(profiles[k].family);
    }
    let mut start = 0;
    while start < sorted.len() {
        // A series: same family, same extension.
        let run_key = |k: usize| (profiles[k].family, extension(&entries[files[k]].path).to_ascii_lowercase());
        let key = run_key(sorted[start]);
        let mut end = start + 1;
        while end < sorted.len() && run_key(sorted[end]) == key {
            end += 1;
        }
        let run = &sorted[start..end];
        if key.0 == Family::Compressed {
            order.compressed.extend(run.iter().map(|&k| files[k]));
        } else {
            let sigs: Vec<_> = run.iter().map(|&k| profiles[k].signature).collect();
            order.normal.extend(classify::group_similar(&sigs).into_iter().map(|p| files[run[p]]));
        }
        start = end;
    }
    order
}

/// Order of the files: the normal stream, then the already compressed contents.
#[derive(Default)]
struct Order {
    normal: Vec<usize>,
    compressed: Vec<usize>,
    /// Hash taken by the dispatcher, for each entry.
    digests: Vec<Option<classify::Digest>>,
    /// Family recognised by the dispatcher, for each entry.
    families: Vec<Option<Family>>,
    /// Rebuilt files: their recipe.
    recipes: HashMap<usize, Arc<Recipe>>,
    /// Recompressed photos: position and length in the temporary file.
    photos: HashMap<usize, (u64, u64)>,
}

/// 2a. Duplicates: identical whole files (removed from the order) and chunks
/// of the large files.
fn find_duplicates(entries: &[Entry], sources: &[Option<PathBuf>], order: &mut Order, progress: &Progress) -> Result<dedup::Found> {
    let files: Vec<Option<(PathBuf, u64, Option<classify::Digest>)>> = entries
        .iter()
        .zip(sources)
        .zip(&order.digests)
        .map(|((e, s), d)| match (&e.kind, s) {
            (Kind::File { size, .. }, Some(p)) => Some((p.clone(), *size, *d)),
            _ => None,
        })
        .collect();
    dedup::find(&files, &mut order.normal, &mut order.compressed, progress)
}

/// 2b. Deflate rebuilding: PDFs, ZIPs (Office included) and .gz files whose
/// streams can be redone identically will be stored in transformed form; so
/// will photos, fonts, WAV sounds and programs. They join the normal stream
/// (their decompressed contents compress well) and are not cut into chunks
/// by deduplication.
fn find_rebuilds(
    entries: &mut [Entry],
    sources: &[Option<PathBuf>],
    order: &mut Order,
    found: &mut dedup::Found,
    stats: &mut Stats,
    spill: &PhotoSpill,
    opts: &Options,
) -> Result<()> {
    let progress = &opts.progress;
    let candidates: Vec<usize> = order
        .normal
        .iter()
        .chain(&order.compressed)
        .copied()
        .filter(|&i| {
            matches!(order.families[i], Some(Family::Document | Family::Compressed | Family::Binary | Family::Executable))
                && (64..=rebuild::MAX_FILE).contains(&entries[i].kind.stored_len())
        })
        .collect();
    let total: u64 = candidates.iter().map(|&i| entries[i].kind.stored_len()).sum();
    progress.start("analyse des flux", total);
    let prepared: Vec<(usize, Option<rebuild::Prepared>, Option<(u64, u64)>)> = candidates
        .par_iter()
        .map(|&i| {
            progress.check()?;
            let size = entries[i].kind.stored_len();
            let mut p = rebuild::prepare(sources[i].as_ref().unwrap(), size, opts.photos);
            // A recompressed photo goes straight into the temporary file.
            let photo = match p.as_mut().and_then(|p| p.recipe.jpeg.take()) {
                Some(data) => Some((spill.push(&data)?, data.len() as u64)),
                None => None,
            };
            progress.advance(size);
            Ok((i, p, photo))
        })
        .collect::<Result<_>>()?;
    let mut moved = Vec::new();
    for (i, p, photo) in prepared {
        let Some(p) = p else { continue };
        if let Some(at) = photo {
            order.photos.insert(i, at);
        }
        if let Kind::File { size, rebuilt, .. } = &mut entries[i].kind {
            *rebuilt = Some(Rebuilt { stored: p.stored, hash: p.hash });
            if photo.is_some() {
                stats.photos += 1;
                stats.photo_bytes += *size;
            } else {
                stats.rebuilt_files += 1;
                stats.rebuilt_bytes += *size;
            }
        }
        found.chunks.remove(&i);
        // A recompressed photo no longer compresses: it stays in the "already
        // compressed" stream.
        if photo.is_none() {
            moved.push(i);
        }
        order.recipes.insert(i, Arc::new(p.recipe));
    }
    // Files coming from the "already compressed" stream join the normal stream,
    // at the end, sorted by extension then by name.
    let from_compressed: HashSet<usize> = moved.iter().copied().filter(|i| order.compressed.contains(i)).collect();
    if !from_compressed.is_empty() {
        order.compressed.retain(|i| !from_compressed.contains(i));
        let mut extra: Vec<usize> = from_compressed.into_iter().collect();
        extra.sort_by_cached_key(|&i| sort_key(&entries[i].path));
        order.normal.extend(extra);
    }
    Ok(())
}

/// 3. Cutting into blocks. A large file may span several blocks.
/// Blocks are numbered from `first_block`.
fn plan_blocks(
    entries: &mut [Entry],
    order: &Order,
    found: &dedup::Found,
    block_size: u64,
    first_block: u64,
    stats: &mut Stats,
    spill: &PhotoSpill,
) -> (Vec<Vec<Piece>>, Links, usize) {
    let mut cutter = Cutter { plans: Vec::new(), current: Vec::new(), fill: 0, block_size, first_block };
    let mut links = Links::default();
    let mut seen: HashMap<[u8; 32], (usize, usize, usize)> = HashMap::new();
    for &i in &order.normal {
        match found.chunks.get(&i) {
            Some(chunks) => cutter.place_chunks(entries, i, chunks, &mut seen, &mut links, stats),
            None => {
                let size = entries[i].kind.stored_len();
                cutter.place(entries, i, 0, size);
            }
        }
    }
    cutter.close();
    let normal_blocks = cutter.plans.len();
    cutter.block_size = block_size.min(COMPRESSED_BLOCK);
    for &i in &order.compressed {
        // Stored size: that of the transformed form for a recompressed photo.
        let size = entries[i].kind.stored_len();
        cutter.place(entries, i, 0, size);
    }
    cutter.close();
    for &(copy, _) in &found.copies {
        stats.duplicate_files += 1;
        stats.duplicate_bytes += file_size(&entries[copy]);
    }
    links.copies = found.copies.clone();
    let mut plans = cutter.plans;
    for p in plans.iter_mut().flatten() {
        p.recipe = order.recipes.get(&p.entry).cloned();
        p.photo = order.photos.get(&p.entry).map(|&(at, len)| (spill.path.clone(), at, len));
    }
    (plans, links, normal_blocks)
}

fn file_size(e: &Entry) -> u64 {
    match &e.kind {
        Kind::File { size, .. } => *size,
        _ => unreachable!(),
    }
}

/// Fills the blocks one after the other.
struct Cutter {
    plans: Vec<Vec<Piece>>,
    current: Vec<Piece>,
    fill: u64,
    block_size: u64,
    first_block: u64,
}

impl Cutter {
    /// Stores `len` bytes of file `i`, from `off`, after the blocks. Returns the
    /// number of the first extent created and their count.
    fn place(&mut self, entries: &mut [Entry], i: usize, mut off: u64, len: u64) -> (usize, usize) {
        let mtime = (entries[i].mtime_s, entries[i].mtime_ns);
        let Kind::File { size, extents, .. } = &mut entries[i].kind else { unreachable!() };
        let (size, first, end) = (*size, extents.len(), off + len);
        while off < end {
            let take = (self.block_size - self.fill).min(end - off);
            let block = self.first_block + self.plans.len() as u64;
            extents.push(Extent { block, offset: self.fill, len: take, hash: [0; HASH_LEN] });
            self.current.push(Piece {
                entry: i,
                extent: extents.len() - 1,
                file_offset: off,
                len: take,
                file_size: size,
                mtime,
                recipe: None,
                photo: None,
            });
            self.fill += take;
            off += take;
            if self.fill == self.block_size {
                self.close();
            }
        }
        (first, extents.len() - first)
    }

    /// Stores a large file chunk by chunk: a chunk already stored becomes a
    /// reference to the place where it lies.
    fn place_chunks(
        &mut self,
        entries: &mut [Entry],
        i: usize,
        chunks: &[Chunk],
        seen: &mut HashMap<[u8; 32], (usize, usize, usize)>,
        links: &mut Links,
        stats: &mut Stats,
    ) {
        for c in chunks {
            match seen.get(&c.hash) {
                Some(&(j, first, n)) => {
                    let Kind::File { extents: from, .. } = &entries[j].kind else { unreachable!() };
                    let copied = from[first..first + n].to_vec();
                    let Kind::File { extents, .. } = &mut entries[i].kind else { unreachable!() };
                    for (t, x) in copied.into_iter().enumerate() {
                        links.extents.push((i, extents.len(), j, first + t));
                        extents.push(x);
                    }
                    stats.duplicate_bytes += c.len;
                }
                None => {
                    let (first, n) = self.place(entries, i, c.offset, c.len);
                    seen.insert(c.hash, (i, first, n));
                }
            }
        }
    }

    /// Finishes the current block.
    fn close(&mut self) {
        if !self.current.is_empty() {
            self.plans.push(std::mem::take(&mut self.current));
        }
        self.fill = 0;
    }
}

struct Written {
    len: u64,
    blocks: u64,
    recovery_bytes: u64,
    largest_block: u64,
}

impl Written {
    fn fill(&self, stats: &mut Stats) {
        stats.archive_bytes = self.len;
        stats.blocks = self.blocks;
        stats.recovery_bytes = self.recovery_bytes;
        stats.largest_block = self.largest_block;
    }
}

/// Index layer to write to finish the archive.
struct NewLayer {
    number: u64,
    previous: Option<(u64, u64)>,
    removed: Vec<Vec<u8>>,
    entries: Vec<Entry>,
}

/// The archive being written: a new one, or an existing one being extended.
struct Target {
    path: PathBuf,
    out: BufWriter<File>,
    pos: u64,
    crypto: Option<Crypto>,
    /// Encryption parameter block, copied before the footer.
    params_block: Option<(BlockHeader, Vec<u8>)>,
    /// Position of each data block (old ones included).
    blocks: Vec<u64>,
    /// Number of blocks already present before this write.
    first_block: u64,
    largest: u64,
}

fn params_block(params: &Params) -> (BlockHeader, Vec<u8>) {
    let payload = params.encode();
    let header = BlockHeader {
        kind: BlockKind::Params,
        codec: Codec::Store,
        raw_len: payload.len() as u64,
        comp_len: payload.len() as u64,
        payload_crc: crc32c::crc32c(&payload),
    };
    (header, payload)
}

impl Target {
    /// New archive: header, then encryption parameters if needed.
    fn create(path: &Path, crypto: Option<Crypto>, params: Option<Params>) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("impossible de créer {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let params_block = params.as_ref().map(params_block);
        out.write_all(&encode_header(if crypto.is_some() { FLAG_ENCRYPTED } else { 0 }))?;
        let mut pos = HEADER_LEN as u64;
        if let Some((header, payload)) = &params_block {
            out.write_all(&header.encode())?;
            out.write_all(payload)?;
            pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
        }
        Ok(Self { path: path.to_path_buf(), out, pos, crypto, params_block, blocks: Vec::new(), first_block: 0, largest: 0 })
    }

    /// Existing archive: the recovery data and the footer are removed, then we
    /// write after them. The old index stays in place.
    fn extend(
        path: &Path,
        cut: u64,
        crypto: Option<Crypto>,
        params: Option<Params>,
        blocks: Vec<u64>,
        largest: u64,
    ) -> Result<Self> {
        let mut file = OpenOptions::new().write(true).open(path).with_context(|| format!("impossible d'écrire dans {}", path.display()))?;
        file.set_len(cut)?;
        file.seek(SeekFrom::Start(cut))?;
        Ok(Self {
            path: path.to_path_buf(),
            out: BufWriter::with_capacity(1 << 20, file),
            pos: cut,
            crypto,
            params_block: params.as_ref().map(params_block),
            first_block: blocks.len() as u64,
            blocks,
            largest,
        })
    }

    /// Writes a block that is already prepared (compacting).
    fn push_block(&mut self, header: &BlockHeader, payload: &[u8]) -> Result<()> {
        self.blocks.push(self.pos);
        self.largest = self.largest.max((BLOCK_HEADER_LEN + payload.len()) as u64);
        self.out.write_all(&header.encode())?;
        self.out.write_all(payload)?;
        self.pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
        Ok(())
    }

    /// 4. Fills, compresses (and encrypts) the blocks on all cores.
    ///
    /// Continuous queue: as soon as a core has finished a block, it takes the
    /// next one. Blocks are written in order; to bound memory, a core does not
    /// start a block too far ahead of the last block written.
    fn write_blocks(
        &mut self,
        entries: &mut [Entry],
        sources: &[Option<PathBuf>],
        plans: &[Vec<Piece>],
        method: Method,
        workers: usize,
        progress: &Progress,
    ) -> Result<()> {
        let first_block = self.blocks.len() as u64;
        let in_flight = workers * 2;
        let next = AtomicUsize::new(0);
        let queue = Mutex::new(Queue { done: BTreeMap::new(), written: 0, stop: false });
        let changed = Condvar::new();
        let crypto = self.crypto.as_ref();
        let (out, pos, blocks, largest) = (&mut self.out, &mut self.pos, &mut self.blocks, &mut self.largest);

        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= plans.len() {
                        return;
                    }
                    {
                        let mut q = queue.lock().unwrap();
                        while i >= q.written + in_flight && !q.stop {
                            q = changed.wait(q).unwrap();
                        }
                        // Stop requested: no new block is started.
                        if q.stop || progress.is_cancelled() {
                            q.stop = true;
                            changed.notify_all();
                            return;
                        }
                    }
                    let block = build_block(&plans[i], sources, method.engine(i), crypto, first_block + i as u64, progress);
                    queue.lock().unwrap().done.insert(i, block);
                    changed.notify_all();
                });
            }

            // The main thread writes the blocks in order, as they come.
            let mut write_next = || -> Result<()> {
                let mut q = queue.lock().unwrap();
                while q.written < plans.len() {
                    let wanted = q.written;
                    let Some(block) = q.done.remove(&wanted) else {
                        if q.stop || progress.is_cancelled() {
                            return Err(Cancelled.into());
                        }
                        // Regular wake-up to react quickly to a stop request.
                        q = changed.wait_timeout(q, Duration::from_millis(100)).unwrap().0;
                        continue;
                    };
                    drop(q);
                    let (header, payload, hashes) = block?;
                    for (piece, hash) in plans[wanted].iter().zip(hashes) {
                        if let Kind::File { extents, .. } = &mut entries[piece.entry].kind {
                            extents[piece.extent].hash = hash;
                        }
                    }
                    blocks.push(*pos);
                    *largest = (*largest).max((BLOCK_HEADER_LEN + payload.len()) as u64);
                    out.write_all(&header.encode())?;
                    out.write_all(&payload)?;
                    *pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
                    q = queue.lock().unwrap();
                    q.written += 1;
                    changed.notify_all();
                }
                Ok(())
            };
            let result = write_next();
            if result.is_err() {
                queue.lock().unwrap().stop = true;
                changed.notify_all();
            }
            result
        })
    }

    /// 5 and 6. Index layer (twice), copy of the parameters, recovery data, then
    /// the footer.
    fn finish(mut self, new: NewLayer, opts: &Options, chrono: &mut Chrono) -> Result<Written> {
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let layer = Layer {
            number: new.number,
            created,
            previous: new.previous,
            first_block: self.first_block,
            blocks: self.blocks[self.first_block as usize..].to_vec(),
            removed: new.removed,
            entries: new.entries,
        };
        let raw = layer.encode();
        // Moderate level: the index is small, the maximum level cost almost a
        // second for a few KB saved.
        let comp = zstd::bulk::compress(&raw, 9)?;
        let index_offset = self.pos;
        let mut backup_index_offset = 0;
        // Backup copy only without recovery data: the latter already repairs the
        // index, and the copy cost 1.5 % of the archive.
        let with_recovery = opts.recovery_percent > 0 || opts.recovery_blocks > 0;
        let copies = if with_recovery { 1 } else { 2 };
        for copy in 0..copies {
            // Each copy is sealed with its own position.
            let payload = match &self.crypto {
                Some(c) => c.seal(BlockKind::Index, Codec::Zstd, raw.len() as u64, index_number(self.pos), &comp)?,
                None => comp.clone(),
            };
            let header = BlockHeader {
                kind: BlockKind::Index,
                codec: Codec::Zstd,
                raw_len: raw.len() as u64,
                comp_len: payload.len() as u64,
                payload_crc: crc32c::crc32c(&payload),
            };
            if copy == 1 {
                backup_index_offset = self.pos;
            }
            self.largest = self.largest.max((BLOCK_HEADER_LEN + payload.len()) as u64);
            self.out.write_all(&header.encode())?;
            self.out.write_all(&payload)?;
            self.pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
        }
        // Backup copy of the parameters: without them (and their salt), an
        // encrypted archive would be lost even with the right password.
        if let Some((header, payload)) = &self.params_block {
            self.out.write_all(&header.encode())?;
            self.out.write_all(payload)?;
            self.pos += (BLOCK_HEADER_LEN + payload.len()) as u64;
        }
        chrono.lap("5. Écrire l'index");

        // Recovery data, computed over everything that precedes.
        let mut recovery_offset = 0;
        let protected_len = self.pos;
        if with_recovery {
            self.out.flush()?;
            let written = File::open(&self.path)?;
            let layout = recovery::Layout::for_blocks(self.pos, opts.recovery_percent, opts.recovery_blocks, self.largest);
            opts.progress.check()?;
            opts.progress.start("récupération", self.pos);
            let (table_offset, end) = recovery::write(&written, layout, &mut self.out, self.pos, &opts.progress)?;
            recovery_offset = table_offset;
            self.pos = end;
        }
        chrono.lap("6. Données de récupération");
        let footer =
            Footer { index_offset, backup_index_offset, recovery_offset, archive_len: self.pos + FOOTER_LEN as u64 };
        self.out.write_all(&footer.encode())?;
        self.out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        Ok(Written {
            len: footer.archive_len,
            blocks: self.blocks.len() as u64,
            recovery_bytes: self.pos - protected_len,
            largest_block: self.largest,
        })
    }
}

/// Compresses a block: zstd, and for a small block also zstd with the
/// built-in dictionary; keeps the smallest, or the block as is if
/// compression brings nothing (photos, videos...).
fn compress_block(raw: &[u8], engine: Engine, progress: &Progress, count: bool) -> Result<(Codec, Vec<u8>)> {
    let level = match engine {
        Engine::Zstd(level) => level,
        Engine::Max(ia) => {
            // With an expert: the expert moves the progress bar (the pass without the
            // expert, much faster, is a safety net: the smaller of the two is kept).
            let expert = if ia { crate::ia::choisir(raw) } else { None };
            let avance = |n, compte: bool| {
                progress.check()?;
                if count && compte {
                    progress.advance(n);
                }
                Ok(())
            };
            let mut c = crate::cm::compress_with(raw, &mut |n| avance(n, expert.is_none()))?;
            if let Some(x) = expert {
                let ci = crate::cm::compress_ia(raw, x, &mut |n| avance(n, true))?;
                if ci.len() < c.len() {
                    c = ci;
                }
            }
            return Ok(if c.len() < raw.len() { (Codec::Cm, c) } else { (Codec::Store, raw.to_vec()) });
        }
    };
    let mut best = (Codec::Zstd, zstd_block(raw, level, None, progress, count)?);
    if raw.len() <= crate::dict::SMALL_BLOCK {
        let d = zstd_block(raw, level, Some(crate::dict::get()), progress, false)?;
        if d.len() < best.1.len() {
            best = (Codec::ZstdDict, d);
        }
    }
    Ok(if best.1.len() < raw.len() { best } else { (Codec::Store, raw.to_vec()) })
}

/// Compresses with zstd, in 1 MB slices: between two slices we check whether
/// a stop is requested. Same result as in one go (zstd keeps its memory from
/// one slice to the next), but a Ctrl+C is taken into account in under a
/// second instead of waiting for the end of the block.
/// If `count` is true, each compressed slice moves the progress bar.
fn zstd_block(raw: &[u8], level: i32, dict: Option<&[u8]>, progress: &Progress, count: bool) -> Result<Vec<u8>> {
    let out = Vec::with_capacity(raw.len() / 2);
    let mut encoder = match dict {
        Some(d) => zstd::stream::write::Encoder::with_dictionary(out, level, d)?,
        None => zstd::stream::write::Encoder::new(out, level)?,
    };
    encoder.set_pledged_src_size(Some(raw.len() as u64))?;
    if level >= 10 {
        // 32 MB window: zstd can find repetitions across the whole block.
        encoder.set_parameter(zstd::zstd_safe::CParameter::WindowLog(25))?;
        encoder.set_parameter(zstd::zstd_safe::CParameter::EnableLongDistanceMatching(true))?;
    }
    for slice in raw.chunks(1 << 20) {
        progress.check()?;
        encoder.write_all(slice)?;
        if count {
            progress.advance(slice.len() as u64);
        }
    }
    Ok(encoder.finish()?)
}

fn build_block(
    pieces: &[Piece],
    sources: &[Option<PathBuf>],
    engine: Engine,
    crypto: Option<&Crypto>,
    number: u64,
    progress: &Progress,
) -> Result<Built> {
    let total: u64 = pieces.iter().map(|p| p.len).sum();
    let mut raw = Vec::with_capacity(total as usize);
    let mut hashes = Vec::with_capacity(pieces.len());
    // Transformed form of the rebuilt files, made once per block.
    let mut transformed: HashMap<usize, Vec<u8>> = HashMap::new();
    for p in pieces {
        let path = sources[p.entry].as_ref().unwrap();
        if let Some(recipe) = &p.recipe {
            if !transformed.contains_key(&p.entry) {
                let t = transform_file(path, p, recipe, p.photo.as_ref())?;
                transformed.insert(p.entry, t);
            }
            let t = &transformed[&p.entry];
            let part = usize::try_from(p.file_offset)
                .ok()
                .and_then(|o| t.get(o..o + p.len as usize))
                .with_context(|| format!("{} a changé pendant la compression", path.display()))?;
            hashes.push(index::hash(part));
            raw.extend_from_slice(part);
            continue;
        }
        let mut f = File::open(path).with_context(|| format!("impossible d'ouvrir {}", path.display()))?;
        let meta = f.metadata()?;
        if meta.len() != p.file_size || platform::mtime_of(&meta) != p.mtime {
            bail!("{} a changé pendant la compression", path.display());
        }
        f.seek(SeekFrom::Start(p.file_offset))?;
        let start = raw.len();
        raw.resize(start + p.len as usize, 0);
        f.read_exact(&mut raw[start..])
            .with_context(|| format!("{} a changé pendant la compression", path.display()))?;
        // Hash computed during this single read.
        hashes.push(index::hash(&raw[start..]));
    }

    // A block of recompressed photos no longer compresses: stored as is,
    // without wasting time in zstd.
    let (codec, mut payload) = if pieces.iter().all(|p| p.photo.is_some()) {
        progress.advance(total);
        (Codec::Store, raw)
    } else {
        let (codec, comp) = compress_block(&raw, engine, progress, true)?;
        (codec, if codec == Codec::Store { raw } else { comp })
    };
    // Compress first, encrypt afterwards: encrypted data no longer compresses.
    if let Some(c) = crypto {
        payload = c.seal(BlockKind::Data, codec, total, number, &payload)?;
    }
    let header = BlockHeader {
        kind: BlockKind::Data,
        codec,
        raw_len: total,
        comp_len: payload.len() as u64,
        payload_crc: crc32c::crc32c(&payload),
    };
    Ok((header, payload, hashes))
}

/// Reads a file to rebuild again and makes its transformed form.
fn transform_file(path: &Path, p: &Piece, recipe: &Recipe, photo: Option<&(Arc<PathBuf>, u64, u64)>) -> Result<Vec<u8>> {
    let changed = || format!("{} a changé pendant la compression", path.display());
    let mut f = File::open(path).with_context(|| format!("impossible d'ouvrir {}", path.display()))?;
    let meta = f.metadata()?;
    if meta.len() != p.file_size || platform::mtime_of(&meta) != p.mtime {
        bail!("{}", changed());
    }
    let mut data = Vec::with_capacity(p.file_size as usize);
    f.read_to_end(&mut data)?;
    if data.len() as u64 != p.file_size {
        bail!("{}", changed());
    }
    let photo = match photo {
        Some((spill, at, len)) => Some(PhotoSpill::read(spill, *at, *len)?),
        None => None,
    };
    rebuild::transform_with(&data, recipe, photo.as_deref()).with_context(changed)
}

fn walk(dir: &Path, rel: &[u8], scan: &mut Scan) -> Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("impossible de lire le dossier {}", dir.display()))?
        .map(|d| d.map(|d| d.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let full = dir.join(&name);
        let mut path = rel.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(&platform::os_to_bytes(&name));
        if scan.exclude.is_some() && scan.exclude == fs::canonicalize(&full).ok() {
            continue;
        }
        let meta = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) => {
                scan.stats.skipped.push(format!("{} : {e}", full.display()));
                continue;
            }
        };
        add_entry(&full, path.clone(), &meta, scan)?;
        if meta.is_dir() {
            walk(&full, &path, scan)?;
        }
    }
    Ok(())
}

fn add_entry(full: &Path, path: Vec<u8>, meta: &fs::Metadata, scan: &mut Scan) -> Result<()> {
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        scan.stats.links += 1;
        Kind::Symlink { target: platform::link_target(full)? }
    } else if ft.is_dir() {
        scan.stats.dirs += 1;
        Kind::Dir
    } else if ft.is_file() {
        scan.stats.files += 1;
        scan.stats.raw_bytes += meta.len();
        Kind::File { size: meta.len(), extents: Vec::new(), rebuilt: None }
    } else {
        scan.stats.skipped.push(format!("{} : fichier spécial ignoré", full.display()));
        return Ok(());
    };
    let fields = scan.collector.collect(full, meta);
    let (mtime_s, mtime_ns) = platform::mtime_of(meta);
    scan.sources.push(ft.is_file().then(|| full.to_path_buf()));
    scan.entries.push(Entry {
        path,
        mode: platform::mode_of(meta),
        mtime_s,
        mtime_ns,
        kind,
        fields,
    });
    Ok(())
}

/// Sort key: extension, then file name, then full path.
fn sort_key(path: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let name = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
    (extension(path).to_ascii_lowercase(), name.to_ascii_lowercase(), path.to_vec())
}

fn extension(path: &[u8]) -> &[u8] {
    let name = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
    match name.iter().rposition(|&c| c == b'.') {
        Some(p) if p > 0 => &name[p + 1..],
        _ => &[],
    }
}
