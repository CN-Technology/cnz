//! Reading, verifying and extracting a .cnz archive.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use zeroize::Zeroizing;

use crate::crypto::{index_number, Crypto, Params};
use crate::format::*;
use crate::index::{merge, Entry, Extent, Index, Kind, Layer};
use crate::meta::Restorer;
use crate::platform;
use crate::progress::Progress;
use crate::timing::{Chrono, Step};
use unicode_normalization::UnicodeNormalization;

pub struct Archive {
    file: File,
    len: u64,
    crypto: Option<Crypto>,
    params: Option<Params>,
    footer: Option<Footer>,
    /// Index layers, from the first version to the last.
    layers: Vec<Layer>,
    /// State of the selected version (the last one by default).
    pub index: Index,
    /// Remarks on the state of the archive (backup index used, etc.).
    pub notes: Vec<String>,
}

#[derive(Default)]
pub struct ExtractOptions {
    /// Restore the files' owner (by default only as administrator).
    pub restore_owner: bool,
    /// Maximum memory to use (None: half of the free memory).
    pub max_memory: Option<u64>,
    /// Progress and stop request, shared with the interface.
    pub progress: std::sync::Arc<Progress>,
}

#[derive(Default)]
pub struct Report {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
    pub problems: Vec<String>,
    /// Entries renamed on extraction (forbidden name, or case duplicate):
    /// (name in the archive, name on disk).
    pub renamed: Vec<(String, String)>,
    /// Duration of each step (--chrono option).
    pub steps: Vec<Step>,
}

impl Archive {
    /// Opens an unencrypted archive.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_password(path, &mut || bail!("archive chiffrée : mot de passe requis"))
    }

    /// Opens an archive; `password` is only called if the archive is encrypted.
    pub fn open_with_password(path: &Path, password: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("impossible d'ouvrir {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut h = [0u8; HEADER_LEN];
        file.read_exact(&mut h).context("ce fichier n'est pas une archive .cnz (trop court)")?;
        let flags = check_header(&h)?;
        let mut archive = Archive {
            file,
            len,
            crypto: None,
            params: None,
            footer: None,
            layers: Vec::new(),
            index: Index::default(),
            notes: Vec::new(),
        };
        if flags & FLAG_ENCRYPTED != 0 {
            let params = archive.load_params()?;
            archive.crypto = Some(Crypto::open(&password()?, &params)?);
            archive.params = Some(params);
        }
        let latest = archive.load_latest_layer()?;
        archive.layers = archive.load_chain(latest)?;
        archive.index = merge(&archive.layers)?;
        Ok(archive)
    }

    /// All versions, from the first to the last.
    pub fn versions(&self) -> &[Layer] {
        &self.layers
    }

    /// Puts the archive in the state of an earlier version (numbered from 1).
    pub fn select_version(&mut self, version: u64) -> Result<()> {
        if version == 0 || version > self.layers.len() as u64 {
            bail!("version {version} inexistante : l'archive a {} version(s)", self.layers.len());
        }
        self.index = merge(&self.layers[..version as usize])?;
        Ok(())
    }

    /// Walks back up the chain of layers from the most recent one.
    fn load_chain(&mut self, latest: Layer) -> Result<Vec<Layer>> {
        let mut chain = vec![latest];
        while let Some((primary, backup)) = chain.last().unwrap().previous {
            let number = chain.last().unwrap().number;
            let layer = match self.read_index_at(primary) {
                Ok(l) => l,
                Err(e) if backup == 0 => {
                    return Err(e).context(format!(
                        "la version {} de l'archive est illisible : lancez « cnz reparer »",
                        number - 1
                    ))
                }
                Err(_) => self
                    .read_index_at(backup)
                    .with_context(|| format!("la version {} de l'archive est illisible", number - 1))?,
            };
            // Each layer refers to the previous one: strictly decreasing numbers, which
            // rules out any loop in a booby-trapped archive.
            if layer.number + 1 != number {
                bail!("chaîne d'index incohérente");
            }
            chain.push(layer);
        }
        chain.reverse();
        Ok(chain)
    }

    pub fn is_encrypted(&self) -> bool {
        self.crypto.is_some()
    }

    /// End of the useful contents: start of the recovery data, or of the footer.
    /// This is where an addition resumes writing.
    pub(crate) fn end_of_content(&self) -> Result<u64> {
        let footer = self.footer.context("pied d'archive illisible")?;
        if footer.recovery_offset == 0 {
            return Ok(self.len - FOOTER_LEN as u64);
        }
        let (header, payload) = read_block_at(&self.file, self.len, footer.recovery_offset)?;
        if header.kind != BlockKind::Recovery {
            bail!("table de récupération introuvable");
        }
        Ok(crate::recovery::Table::decode(&payload)?.layout.protected_len)
    }

    pub(crate) fn crypto_clone(&self) -> Option<Crypto> {
        self.crypto.clone()
    }

    pub(crate) fn params_clone(&self) -> Option<Params> {
        self.params.clone()
    }

    /// Data block no. `b`, as it is stored (CRC checked).
    pub(crate) fn read_stored_block(&self, b: usize) -> Result<(BlockHeader, Vec<u8>)> {
        read_block_at(&self.file, self.len, self.index.blocks[b])
    }

    /// Compressed data of a block: seal checked and decrypted if needed.
    pub(crate) fn open_payload(&self, header: &BlockHeader, payload: Vec<u8>, number: u64) -> Result<Vec<u8>> {
        match &self.crypto {
            Some(c) => c.unseal(header.kind, header.codec, header.raw_len, number, &payload),
            None => Ok(payload),
        }
    }

    /// Stored size (header included) of each data block.
    pub(crate) fn stored_block_sizes(&self) -> Result<Vec<u64>> {
        let mut h = [0u8; BLOCK_HEADER_LEN];
        self.index
            .blocks
            .iter()
            .map(|&offset| {
                read_exact_at(&self.file, &mut h, offset)?;
                Ok(BLOCK_HEADER_LEN as u64 + BlockHeader::decode(&h)?.comp_len)
            })
            .collect()
    }

    /// What is needed to extend the archive: state of the last version,
    /// encryption key and parameters, position of the last index layer (main and
    /// copy) and its number.
    pub(crate) fn into_parts(self) -> Result<(Index, Option<Crypto>, Option<Params>, (u64, u64), u64)> {
        let footer = self.footer.context("pied d'archive illisible")?;
        let number = self.layers.last().map_or(0, |l| l.number);
        let index = merge(&self.layers)?;
        Ok((index, self.crypto, self.params, (footer.index_offset, footer.backup_index_offset), number))
    }

    /// Encryption parameters: at the head of the archive, otherwise their backup copy.
    fn load_params(&mut self) -> Result<Params> {
        if let Ok(p) = self.read_params_at(HEADER_LEN as u64) {
            return Ok(p);
        }
        for offset in self.scan_blocks(BlockKind::Params)?.into_iter().rev() {
            if let Ok(p) = self.read_params_at(offset) {
                self.notes.push("paramètres de chiffrement abîmés en tête : copie de secours utilisée".into());
                return Ok(p);
            }
        }
        bail!("paramètres de chiffrement illisibles : l'archive ne peut pas être déchiffrée")
    }

    fn read_params_at(&mut self, offset: u64) -> Result<Params> {
        let (header, payload) = self.read_block(offset)?;
        if header.kind != BlockKind::Params {
            bail!("ce bloc ne contient pas les paramètres de chiffrement");
        }
        Params::decode(&payload)
    }

    /// Last index layer: the main one, otherwise its copy, otherwise a search
    /// through the whole archive.
    fn load_latest_layer(&mut self) -> Result<Layer> {
        match self.read_footer() {
            Ok(f) => {
                self.footer = Some(f);
                if f.archive_len != self.len {
                    self.notes.push(format!("taille inattendue : {} octets au lieu de {}", self.len, f.archive_len));
                }
                match self.read_index_at(f.index_offset) {
                    Ok(i) => return Ok(i),
                    Err(e) if f.backup_index_offset == 0 => {
                        // No copy: the index is protected by the recovery data. Above all do not
                        // look for another index, it might be that of an older version.
                        bail!("index abîmé ({e}) : lancez « cnz reparer »");
                    }
                    Err(e) => self.notes.push(format!("index principal illisible ({e})")),
                }
                match self.read_index_at(f.backup_index_offset) {
                    Ok(i) => {
                        self.notes.push("copie de secours de l'index utilisée".into());
                        return Ok(i);
                    }
                    Err(e) => self.notes.push(format!("copie de secours de l'index illisible ({e})")),
                }
            }
            Err(e) => self.notes.push(format!("pied d'archive illisible ({e})")),
        }
        self.notes.push("index retrouvé en parcourant toute l'archive".into());
        self.scan_for_index()
    }

    fn read_footer(&mut self) -> Result<Footer> {
        if self.len < (HEADER_LEN + FOOTER_LEN) as u64 {
            bail!("archive trop courte");
        }
        let mut f = [0u8; FOOTER_LEN];
        self.file.seek(SeekFrom::Start(self.len - FOOTER_LEN as u64))?;
        self.file.read_exact(&mut f)?;
        Footer::decode(&f)
    }

    fn read_index_at(&mut self, offset: u64) -> Result<Layer> {
        let (header, payload) = self.read_block(offset)?;
        if header.kind != BlockKind::Index {
            bail!("ce bloc n'est pas un index");
        }
        Layer::decode(&unpack(&header, payload, self.crypto.as_ref(), index_number(offset))?)
    }

    /// Reads a block (header + compressed payload) and checks its CRC.
    fn read_block(&mut self, offset: u64) -> Result<(BlockHeader, Vec<u8>)> {
        read_block_at(&self.file, self.len, offset)
    }

    /// Keeps the last valid index layer found while walking through the whole archive.
    fn scan_for_index(&mut self) -> Result<Layer> {
        for offset in self.scan_blocks(BlockKind::Index)?.into_iter().rev() {
            if let Ok(index) = self.read_index_at(offset) {
                return Ok(index);
            }
        }
        bail!("aucun index lisible : l'archive est trop endommagée")
    }

    /// Walks through the archive block by block and returns the position of the
    /// blocks of the requested type. A damaged area is skipped by looking for
    /// the next block signature.
    fn scan_blocks(&mut self, kind: BlockKind) -> Result<Vec<u64>> {
        let mut pos = HEADER_LEN as u64;
        let mut found = Vec::new();
        let mut h = [0u8; BLOCK_HEADER_LEN];
        while pos + BLOCK_HEADER_LEN as u64 <= self.len {
            self.file.seek(SeekFrom::Start(pos))?;
            self.file.read_exact(&mut h)?;
            match BlockHeader::decode(&h) {
                Ok(header) => {
                    if header.kind == kind {
                        found.push(pos);
                    }
                    pos += BLOCK_HEADER_LEN as u64 + header.comp_len;
                }
                Err(_) => match self.find_block_magic(pos + 1)? {
                    Some(next) => pos = next,
                    None => break,
                },
            }
        }
        Ok(found)
    }

    fn find_block_magic(&mut self, from: u64) -> Result<Option<u64>> {
        let mut buf = vec![0u8; 1 << 20];
        let mut pos = from;
        while pos < self.len {
            let n = (self.len - pos).min(buf.len() as u64) as usize;
            self.file.seek(SeekFrom::Start(pos))?;
            self.file.read_exact(&mut buf[..n])?;
            if let Some(i) = buf[..n].windows(4).position(|w| w == BLOCK_MAGIC) {
                return Ok(Some(pos + i as u64));
            }
            if n < 4 {
                break;
            }
            pos += (n - 3) as u64;
        }
        Ok(None)
    }

    /// Extracts the archive into `dest`, or only verifies everything if `dest` is None.
    pub fn extract(&mut self, dest: Option<&Path>, opts: &ExtractOptions) -> Result<Report> {
        let index = std::mem::take(&mut self.index);
        let result = self.extract_with(&index, dest, opts);
        self.index = index;
        result
    }

    fn extract_with(&mut self, index: &Index, dest: Option<&Path>, opts: &ExtractOptions) -> Result<Report> {
        let entries = &index.entries;
        let mut report = Report::default();
        let mut chrono = Chrono::new();

        // Safety: no path may be able to leave the destination folder.
        let mut refused = vec![false; entries.len()];
        for (i, e) in entries.iter().enumerate() {
            if let Err(why) = check_path(&e.path) {
                refused[i] = true;
                report.problems.push(format!("{} : {why}, ignoré", show(&e.path)));
            }
        }

        // Name of each entry on the destination disk.
        let mut targets: Vec<Option<PathBuf>> = vec![None; entries.len()];
        if let Some(d) = dest {
            fs::create_dir_all(d).with_context(|| format!("impossible de créer {}", d.display()))?;
            let mut namer = Namer::new(cfg!(windows), platform::is_case_insensitive(d));
            for (i, e) in entries.iter().enumerate() {
                if !refused[i] {
                    targets[i] = Some(d.join(namer.resolve(&e.path)));
                }
            }
            report.renamed = namer.renamed;
        }
        let target = |i: usize| targets[i].clone();
        // Folders first, in order (a parent before its contents)…
        for (i, e) in entries.iter().enumerate() {
            if let (false, Kind::Dir, Some(t)) = (refused[i], &e.kind, &targets[i]) {
                if let Err(err) = fs::create_dir_all(t) {
                    report.problems.push(format!("{} : impossible de créer ({err})", t.display()));
                    refused[i] = true;
                }
            }
        }
        // … then all empty files, on all cores. A file that cannot be created (name
        // refused by the disk, permissions…) is reported, but does not prevent
        // extracting all the others.
        let failures: Vec<(usize, String)> = entries
            .par_iter()
            .enumerate()
            .filter(|(i, e)| !refused[*i] && matches!(e.kind, Kind::File { .. }))
            .filter_map(|(i, _)| {
                let t = targets[i].as_ref()?;
                t.parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|()| File::create(t).map(drop))
                    .err()
                    .map(|err| (i, format!("{} : impossible de créer ({err})", t.display())))
            })
            .collect();
        for (i, message) in failures {
            report.problems.push(message);
            refused[i] = true;
        }

        chrono.lap("1. Préparer noms, dossiers, fichiers");

        // For each block, the list of file chunks it contains.
        let mut per_block: Vec<Vec<(usize, u64, &Extent)>> = vec![Vec::new(); index.blocks.len()];
        let mut remaining = vec![0usize; entries.len()];
        for (i, e) in entries.iter().enumerate() {
            if let Kind::File { extents, .. } = &e.kind {
                let mut file_offset = 0u64;
                for x in extents {
                    per_block[x.block as usize].push((i, file_offset, x));
                    file_offset += x.len;
                }
                remaining[i] = extents.len();
            }
        }

        let mut damaged = vec![false; entries.len()];
        let mut written = vec![0u64; entries.len()];
        // Rebuilt files (stage 4): their transformed form is gathered here, chunk
        // by chunk, then the file is rebuilt once it is complete.
        let staging: Vec<Option<Mutex<Vec<u8>>>> = entries
            .iter()
            .map(|e| matches!(e.kind, Kind::File { rebuilt: Some(_), .. }).then(|| Mutex::new(Vec::new())))
            .collect();
        let finish = |i: usize, damaged: &[bool], written: &[u64], report: &mut Report, ready: &mut Vec<usize>| {
            if refused[i] {
                return;
            }
            let Kind::File { size, .. } = &entries[i].kind else { return };
            if !damaged[i] && written[i] == entries[i].kind.stored_len() {
                if staging[i].is_some() {
                    ready.push(i);
                } else {
                    report.files += 1;
                    report.bytes += size;
                }
            } else {
                if let Some(buf) = &staging[i] {
                    *buf.lock().unwrap() = Vec::new();
                }
                report.problems.push(format!("{} : contenu endommagé", show(&entries[i].path)));
            }
        };
        let mut ready = Vec::new();

        // Empty files have no chunk: they are finished right away.
        for (i, e) in entries.iter().enumerate() {
            if matches!(e.kind, Kind::File { .. }) && remaining[i] == 0 {
                finish(i, &damaged, &written, &mut report, &mut ready);
            }
        }

        // Sequential reading of the blocks, parallel decompression in batches. As
        // many blocks in memory as the budget allows.
        let (mut largest_raw, mut cm) = (0, false);
        let mut h = [0u8; BLOCK_HEADER_LEN];
        for &offset in &index.blocks {
            if read_exact_at(&self.file, &mut h, offset).is_ok() {
                if let Ok(header) = BlockHeader::decode(&h) {
                    largest_raw = largest_raw.max(header.raw_len);
                    cm |= header.codec == Codec::Cm;
                }
            }
        }
        let batch = crate::resources::extract_batch(largest_raw, cm, opts.max_memory);

        // Each core takes a whole block and does everything itself: reading,
        // decryption, decompression, verification of each chunk and writing. The
        // main thread only does the summary, in order.
        let crypto = self.crypto.as_ref();
        let (file, file_len) = (&self.file, self.len);
        let total: u64 = entries.iter().map(|e| e.kind.stored_len()).sum();
        opts.progress.start(if dest.is_some() { "extraction" } else { "vérification" }, total);
        let mut first = 0;
        while first < index.blocks.len() {
            // Stop requested: files already extracted stay in place.
            opts.progress.check()?;
            let range = first..(first + batch).min(index.blocks.len());
            let seen = &damaged;
            let done: Vec<(Option<String>, Vec<Outcome>)> = range
                .clone()
                .into_par_iter()
                .map(|b| {
                    let data = read_block_at(file, file_len, index.blocks[b]).and_then(|(header, payload)| {
                        if header.kind != BlockKind::Data {
                            bail!("ce bloc n'est pas un bloc de données");
                        }
                        let check = || -> Result<()> { Ok(opts.progress.check()?) };
                        unpack_checked(&header, payload, crypto, b as u64, &check)
                    });
                    let block_error = data.as_ref().err().map(|e| format!("bloc {b} illisible : {e}"));
                    let outcomes = per_block[b]
                        .iter()
                        .map(|&(i, file_offset, x)| {
                            if refused[i] || seen[i] {
                                return Outcome::Skipped;
                            }
                            // Each chunk is checked against its own hash before being written.
                            let piece = data.as_ref().ok().and_then(|d| {
                                let end = x.offset.checked_add(x.len)?;
                                d.get(x.offset as usize..end as usize)
                            });
                            match piece.filter(|p| crate::index::hash(p) == x.hash) {
                                None => Outcome::Damaged(None),
                                Some(piece) => match (&staging[i], &targets[i]) {
                                    (Some(buf), _) => {
                                        let mut buf = buf.lock().unwrap();
                                        let start = file_offset as usize;
                                        let end = start + piece.len();
                                        if buf.len() < end {
                                            buf.resize(end, 0);
                                        }
                                        buf[start..end].copy_from_slice(piece);
                                        Outcome::Written(x.len)
                                    }
                                    (None, Some(t)) => match write_at(t, file_offset, piece) {
                                        Ok(()) => Outcome::Written(x.len),
                                        Err(e) => Outcome::Damaged(Some(format!("{} : {e}", t.display()))),
                                    },
                                    (None, None) => Outcome::Written(x.len),
                                },
                            }
                        })
                        .collect();
                    (block_error, outcomes)
                })
                .collect();
            // Stop requested during this batch: interruption, not "unreadable block".
            opts.progress.check()?;

            for (b, (block_error, outcomes)) in range.clone().zip(done) {
                if let Some(e) = block_error {
                    report.problems.push(e);
                }
                for (&(i, _, _), outcome) in per_block[b].iter().zip(outcomes) {
                    match outcome {
                        Outcome::Skipped => {}
                        Outcome::Written(n) => {
                            written[i] += n;
                            opts.progress.advance(n);
                        }
                        Outcome::Damaged(message) => {
                            damaged[i] = true;
                            report.problems.extend(message);
                        }
                    }
                    remaining[i] -= 1;
                    if remaining[i] == 0 {
                        finish(i, &damaged, &written, &mut report, &mut ready);
                    }
                }
            }
            // Rebuilt files that became complete: rebuilt on all cores, checked against
            // the hash of the original file, then written.
            let rebuilt: Vec<(usize, Result<u64>)> = std::mem::take(&mut ready)
                .into_par_iter()
                .map(|i| (i, rebuild_entry(&entries[i], &staging[i], targets[i].as_deref())))
                .collect();
            for (i, r) in rebuilt {
                match r {
                    Ok(size) => {
                        report.files += 1;
                        report.bytes += size;
                    }
                    Err(e) => report.problems.push(format!("{} : contenu endommagé ({e})", show(&entries[i].path))),
                }
            }
            first = range.end;
        }

        chrono.lap("2. Lire, décompresser, vérifier, écrire");

        // Symbolic links last: no file can be written through them.
        let restorer = Restorer::new(opts.restore_owner);
        for (i, e) in entries.iter().enumerate() {
            if refused[i] {
                continue;
            }
            match &e.kind {
                Kind::Dir => report.dirs += 1,
                Kind::Symlink { target: link } => {
                    report.links += 1;
                    if let Some(t) = target(i) {
                        if let Some(parent) = t.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        match platform::make_symlink(link, &t) {
                            Ok(()) => {
                                restorer.apply(&t, e, &mut report.problems);
                                let _ = filetime::set_symlink_file_times(&t, file_time(e), file_time(e));
                            }
                            Err(err) => report.problems.push(format!("{} : {err}", t.display())),
                        }
                    }
                }
                Kind::File { .. } => {}
            }
        }

        // Permissions and dates: folders last, from the deepest to the highest,
        // otherwise writing their contents would change their date.
        if dest.is_some() {
            // Files on all cores…
            let problems: Vec<String> = entries
                .par_iter()
                .enumerate()
                .filter(|(i, e)| !refused[*i] && matches!(e.kind, Kind::File { .. }))
                .flat_map_iter(|(i, e)| {
                    let t = targets[i].as_ref().unwrap();
                    let mut problems = Vec::new();
                    restorer.apply(t, e, &mut problems);
                    restore_meta(t, e);
                    problems
                })
                .collect();
            report.problems.extend(problems);
            // … then folders, one by one.
            for (i, e) in entries.iter().enumerate().rev() {
                if !refused[i] && matches!(e.kind, Kind::Dir) {
                    let t = target(i).unwrap();
                    restorer.apply(&t, e, &mut report.problems);
                    restore_meta(&t, e);
                }
            }
        }
        chrono.lap("3. Liens, droits et dates");
        report.steps = chrono.steps;
        let xattr_failures = restorer.xattr_failures();
        if xattr_failures > 0 {
            report.problems.push(format!(
                "{xattr_failures} attribut(s) étendu(s) non restauré(s) : le disque de destination ne les accepte pas"
            ));
        }
        Ok(report)
    }
}

/// Rebuilds a file from its complete transformed form, and writes it if
/// there is a destination. Returns its size.
fn rebuild_entry(e: &Entry, staging: &Option<Mutex<Vec<u8>>>, target: Option<&Path>) -> Result<u64> {
    let Kind::File { size, rebuilt: Some(r), .. } = &e.kind else { unreachable!() };
    let stored = std::mem::take(&mut *staging.as_ref().unwrap().lock().unwrap());
    let data = crate::rebuild::rebuild(&stored, *size)
        .with_context(|| format!("reconstruction impossible (zlib {})", crate::rebuild::zlib_version()))?;
    drop(stored);
    if crate::index::hash(&data) != r.hash {
        bail!("le fichier reconstruit ne correspond pas à l'original (zlib {})", crate::rebuild::zlib_version());
    }
    if let Some(t) = target {
        fs::write(t, &data).with_context(|| format!("{}", t.display()))?;
    }
    Ok(*size)
}

/// Fate of a file chunk during extraction.
enum Outcome {
    /// File refused or already damaged: chunk ignored.
    Skipped,
    Written(u64),
    /// Unreadable or wrong chunk, possibly with a write error.
    Damaged(Option<String>),
}

/// Checks the seal and decrypts (encrypted archive), then decompresses.
fn unpack(header: &BlockHeader, payload: Vec<u8>, crypto: Option<&Crypto>, number: u64) -> Result<Vec<u8>> {
    unpack_checked(header, payload, crypto, number, &|| Ok(()))
}

/// Like `unpack`, with `check` called during a long decompression (stop
/// request).
fn unpack_checked(
    header: &BlockHeader,
    payload: Vec<u8>,
    crypto: Option<&Crypto>,
    number: u64,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<Vec<u8>> {
    let payload = match crypto {
        Some(c) => c.unseal(header.kind, header.codec, header.raw_len, number, &payload)?,
        None => payload,
    };
    decompress_checked(header, payload, check)
}

/// Decompresses data already decrypted.
pub(crate) fn decompress(header: &BlockHeader, payload: Vec<u8>) -> Result<Vec<u8>> {
    decompress_checked(header, payload, &|| Ok(()))
}

/// Like `decompress`; `check` is called every 256 KiB by the slow
/// context-mixing engine (a Ctrl+C does not wait for the end of the block).
pub(crate) fn decompress_checked(header: &BlockHeader, payload: Vec<u8>, check: &(dyn Fn() -> Result<()> + Sync)) -> Result<Vec<u8>> {
    let raw = match header.codec {
        Codec::Store => payload,
        Codec::Zstd => {
            // Memory only grows at the pace of the data actually decompressed: a header
            // announcing 1 GB for a 100-byte payload does not make 1 GB be reserved in
            // advance.
            let mut raw = Vec::with_capacity((header.raw_len as usize).min(payload.len().saturating_mul(8)));
            zstd::stream::read::Decoder::with_buffer(&payload[..])?.take(header.raw_len + 1).read_to_end(&mut raw)?;
            raw
        }
        Codec::Cm => crate::cm::decompress_with(&payload, usize::try_from(header.raw_len)?, &mut || check())?,
        Codec::ZstdDict => {
            let mut raw = Vec::with_capacity((header.raw_len as usize).min(payload.len().saturating_mul(8)));
            zstd::stream::read::Decoder::with_dictionary(&payload[..], crate::dict::get())?
                .take(header.raw_len + 1)
                .read_to_end(&mut raw)?;
            raw
        }
    };
    if raw.len() as u64 != header.raw_len {
        bail!("taille décompressée incorrecte");
    }
    Ok(raw)
}

fn write_at(path: &PathBuf, offset: u64, data: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)
}

fn file_time(e: &Entry) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(e.mtime_s, e.mtime_ns.min(999_999_999))
}

fn restore_meta(path: &Path, e: &Entry) {
    let _ = filetime::set_file_mtime(path, file_time(e));
    let _ = platform::set_mode(path, e.mode);
}

const MAX_PATH_LEN: usize = 65_535;
const MAX_PATH_DEPTH: usize = 4096;

/// Rejects any path that could leave the destination folder.
pub fn check_path(path: &[u8]) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("chemin vide");
    }
    // Far beyond what Linux (4,096) and Windows (32,767) accept, but bounded: a
    // path 500,000 levels deep must not crash CNz.
    if path.len() > MAX_PATH_LEN || path.iter().filter(|&&c| c == b'/').count() >= MAX_PATH_DEPTH {
        return Err("chemin trop long refusé");
    }
    if path[0] == b'/' {
        return Err("chemin absolu refusé");
    }
    if path.contains(&0) {
        return Err("caractère nul refusé");
    }
    if path.split(|&c| c == b'/').any(|c| c.is_empty() || c == b"." || c == b"..") {
        return Err("chemin dangereux refusé");
    }
    Ok(())
}

pub fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Readable timestamp, in local time.
pub fn format_time(secs: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(secs, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => "????-??-?? ??:??".into(),
    }
}

/// Names of the entries on the destination disk: adapted to the system
/// (names forbidden on Windows) and de-duplicated if the disk mixes up upper
/// and lower case ("Photo.jpg" and "photo.jpg" become "photo (2).jpg").
pub(crate) struct Namer {
    windows: bool,
    fold: bool,
    done: HashMap<Vec<u8>, PathBuf>,
    taken: HashMap<(PathBuf, String), Vec<u8>>,
    pub(crate) renamed: Vec<(String, String)>,
}

impl Namer {
    pub(crate) fn new(windows: bool, fold: bool) -> Self {
        Self { windows, fold, done: HashMap::new(), taken: HashMap::new(), renamed: Vec::new() }
    }

    /// Relative path on disk of an archive entry. Parent folders are resolved
    /// first: a renamed folder takes its contents along.
    pub(crate) fn resolve(&mut self, path: &[u8]) -> PathBuf {
        if let Some(p) = self.done.get(path) {
            return p.clone();
        }
        let (parent, name) = match path.iter().rposition(|&c| c == b'/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => (&b""[..], path),
        };
        let parent_final = if parent.is_empty() { PathBuf::new() } else { self.resolve(parent) };
        let original = platform::bytes_to_os(name);
        let mut wanted = original.clone();
        if self.windows {
            let s = wanted.to_string_lossy().into_owned();
            wanted = OsString::from(platform::windows_name(&s).into_owned());
        }
        let mut chosen = wanted.clone();
        if self.fold {
            // Same name apart from case and accents (NFC): numbered.
            let base = wanted.to_string_lossy().into_owned();
            let key = |n: &OsString| n.to_string_lossy().nfc().collect::<String>().to_lowercase();
            let mut n = 2;
            loop {
                let k = (parent_final.clone(), key(&chosen));
                match self.taken.get(&k) {
                    Some(owner) if owner.as_slice() != path => {
                        chosen = OsString::from(platform::numbered(&base, n));
                        n += 1;
                    }
                    _ => {
                        self.taken.insert(k, path.to_vec());
                        break;
                    }
                }
            }
        }
        let final_path = parent_final.join(&chosen);
        if chosen != original {
            self.renamed.push((show(path), final_path.to_string_lossy().into_owned()));
        }
        self.done.insert(path.to_vec(), final_path.clone());
        final_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chemins_dangereux_refuses() {
        for p in [&b""[..], b"/etc/passwd", b"../x", b"a/../../x", b"a//b", b"./a", b"a\0b"] {
            assert!(check_path(p).is_err(), "{:?}", show(p));
        }
        assert!(check_path(&b"a/".repeat(5000)[..9999]).is_err());
        assert!(check_path(&vec![b'a'; 70_000]).is_err());
        for p in [&b"a"[..], b"dossier/fichier.txt", b"..cache", b"a/.env"] {
            assert!(check_path(p).is_ok(), "{:?}", show(p));
        }
    }

    #[test]
    fn dates() {
        assert_eq!(format_time(1_760_524_800).len(), 16);
    }
}

#[cfg(test)]
mod namer_tests {
    use super::*;

    #[test]
    fn doublons_de_casse_renommes() {
        let mut n = Namer::new(false, true);
        assert_eq!(n.resolve(b"Docs/Photo.jpg"), Path::new("Docs/Photo.jpg"));
        assert_eq!(n.resolve(b"Docs/photo.jpg"), Path::new("Docs/photo (2).jpg"));
        // The "docs" folder is a duplicate of "Docs": its contents follow it.
        assert_eq!(n.resolve(b"docs/a.txt"), Path::new("docs (2)/a.txt"));
        // Composed or decomposed "é": same name for macOS.
        assert_eq!(n.resolve("café".as_bytes()), Path::new("café"));
        assert_eq!(n.resolve("cafe\u{301}".as_bytes()), PathBuf::from("cafe\u{301} (2)"));
        assert_eq!(n.renamed.len(), 3);
    }

    #[test]
    fn noms_windows_a_l_extraction() {
        let mut n = Namer::new(true, true);
        assert_eq!(n.resolve(b"rapport 14:30/con.txt"), Path::new("rapport 14_30/_con.txt"));
        assert_eq!(n.resolve(b"a\\..\\..\\x"), Path::new("a_.._.._x"));
    }

    #[test]
    fn disque_sensible_a_la_casse_inchange() {
        let mut n = Namer::new(false, false);
        assert_eq!(n.resolve(b"Photo.jpg"), Path::new("Photo.jpg"));
        assert_eq!(n.resolve(b"photo.jpg"), Path::new("photo.jpg"));
        assert!(n.renamed.is_empty());
    }
}
