//! Extraction of other programs' archives: ZIP, 7z, RAR, tar, and single
//! compressed files (gzip, bzip2, xz, lzma, lzip, zstd), including a
//! compressed tar (.tar.gz, .tgz, .tar.xz…). RAR is read by `rars` ("rar"
//! feature, free Rust reader).
//!
//! The format is recognised by its signature, not its extension (except
//! .lzma, which has none). Same protections as for .cnz (`read.rs`): no path
//! leaves the destination folder, names adapted to the system, symbolic
//! links created last and only if they point inside.

use std::fs::{self, File};
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use zeroize::Zeroizing;

use crate::platform;
use crate::progress::Progress;
use crate::read::{check_path, show, Namer, Report};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    SeptZip,
    Rar,
    Tar,
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Lzip,
    Zstd,
}

impl Format {
    pub fn nom(self) -> &'static str {
        match self {
            Format::Zip => "ZIP",
            Format::SeptZip => "7z",
            Format::Rar => "RAR",
            Format::Tar => "tar",
            Format::Gzip => "gzip",
            Format::Bzip2 => "bzip2",
            Format::Xz => "xz",
            Format::Lzma => "lzma",
            Format::Lzip => "lzip",
            Format::Zstd => "zstd",
        }
    }
}

/// Recognised extensions (right-click menu, dialogs).
pub const EXTENSIONS: &[&str] =
    &["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "lzma", "tlz", "lz", "zst", "tzst", "jar", "apk", "epub"];

/// Valid tar header (correct checksum): also recognises old tars, without
/// the "ustar" mark.
fn est_tar(h: &[u8]) -> bool {
    if h.len() < 512 || h[0] == 0 {
        return false;
    }
    let champ = &h[148..156];
    let texte: String = champ.iter().take_while(|&&c| c != 0 && c != b' ').map(|&c| c as char).collect();
    let Ok(attendu) = u32::from_str_radix(texte.trim(), 8) else { return false };
    let somme: u32 = h[..512].iter().enumerate().map(|(i, &c)| if (148..156).contains(&i) { 32 } else { u32::from(c) }).sum();
    somme == attendu
}

/// Format from the first bytes; `None` if unknown (or .cnz).
pub fn reconnaitre(chemin: &Path) -> Result<Option<Format>> {
    let mut tete = Vec::with_capacity(512);
    File::open(chemin)?.take(512).read_to_end(&mut tete)?;
    let commence = |s: &[u8]| tete.starts_with(s);
    let ext = chemin.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    Ok(Some(if commence(b"PK\x03\x04") || commence(b"PK\x05\x06") || commence(b"PK\x07\x08") {
        Format::Zip
    } else if commence(b"7z\xBC\xAF\x27\x1C") {
        Format::SeptZip
    } else if cfg!(feature = "rar") && commence(b"Rar!\x1A\x07") {
        Format::Rar
    } else if commence(b"\x1F\x8B") {
        Format::Gzip
    } else if commence(b"BZh") {
        Format::Bzip2
    } else if commence(b"\xFD7zXZ\x00") {
        Format::Xz
    } else if commence(b"LZIP") {
        Format::Lzip
    } else if commence(b"\x28\xB5\x2F\xFD") {
        Format::Zstd
    } else if est_tar(&tete) {
        Format::Tar
    } else if (ext == "lzma" || ext == "tlz") && tete.len() >= 13 && tete[0] < 225 {
        Format::Lzma
    } else {
        return Ok(None);
    }))
}

/// Reading of the archive file that moves the progress bar (bytes read).
struct Compteur<'a, R> {
    inner: R,
    progress: &'a Progress,
}

impl<R: Read> Read for Compteur<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.advance(n as u64);
        Ok(n)
    }
}

impl<R: Seek> Seek for Compteur<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Writes the entries into the destination folder, safely.
struct Ecrivain<'a> {
    /// `None`: verification only (everything is read, nothing is written).
    dest: Option<PathBuf>,
    /// Selected entries (normalised paths; a selected folder takes all its
    /// contents); `None`: everything. With `un`, a single entry, and we stop as
    /// soon as it is written (`ecrit`).
    seul: Option<Vec<Vec<u8>>>,
    un: bool,
    ecrit: Option<PathBuf>,
    namer: Namer,
    report: Report,
    progress: &'a Progress,
    /// Symbolic links to create at the end: (location, target).
    liens: Vec<(PathBuf, Vec<u8>, Vec<u8>)>,
}

impl<'a> Ecrivain<'a> {
    fn new(dest: Option<&Path>, seul: Option<Vec<Vec<u8>>>, progress: &'a Progress) -> Result<Self> {
        if let Some(d) = dest {
            fs::create_dir_all(d).with_context(|| format!("impossible de créer {}", d.display()))?;
        }
        Ok(Self {
            dest: dest.map(Path::to_path_buf),
            seul,
            un: false,
            ecrit: None,
            namer: Namer::new(cfg!(windows), dest.is_some_and(platform::is_case_insensitive)),
            report: Report::default(),
            progress,
            liens: Vec::new(),
        })
    }

    /// Path in the archive → safe relative path ("/" as separator). Leading "/"
    /// and "." are removed (as tar does); a ".." makes the entry rejected.
    fn normaliser(&mut self, brut: &[u8]) -> Option<Vec<u8>> {
        let brut: Vec<u8> = brut.iter().map(|&c| if c == b'\\' { b'/' } else { c }).collect();
        let parties: Vec<&[u8]> = brut.split(|&c| c == b'/').filter(|p| !p.is_empty() && *p != b".").collect();
        let propre = parties.join(&b'/');
        if propre.is_empty() {
            return None;
        }
        // Drive letter ("C:"): absolute Windows path.
        let lecteur = propre.len() >= 2 && propre[1] == b':';
        match check_path(&propre) {
            Ok(()) if !lecteur => Some(propre),
            Ok(()) => {
                self.report.problems.push(format!("{} : chemin absolu refusé, ignoré", show(&brut)));
                None
            }
            Err(why) => {
                self.report.problems.push(format!("{} : {why}, ignoré", show(&brut)));
                None
            }
        }
    }

    /// Is the entry part of the selection (itself, or inside a selected folder)?
    fn retenu(&self, c: &[u8]) -> bool {
        self.seul.as_ref().is_none_or(|l| l.iter().any(|p| c == p.as_slice() || (c.starts_with(p) && c.get(p.len()) == Some(&b'/'))))
    }

    /// Location on disk; `None` during verification, or if this is not the
    /// requested entry.
    fn cible(&mut self, chemin: &[u8]) -> Option<PathBuf> {
        if !self.retenu(chemin) {
            return None;
        }
        let d = self.dest.clone()?;
        Some(d.join(self.namer.resolve(chemin)))
    }

    /// Where to write a file: `Ok(None)` if it is rejected or not requested,
    /// `Ok(Some(None))` during verification, otherwise its location (parent
    /// folders created).
    #[cfg_attr(not(feature = "rar"), allow(dead_code))]
    fn place(&mut self, brut: &[u8]) -> Option<Option<PathBuf>> {
        let c = self.normaliser(brut)?;
        if !self.retenu(&c) {
            return None;
        }
        let Some(t) = self.cible(&c) else { return Some(None) };
        if let Err(e) = t.parent().map_or(Ok(()), fs::create_dir_all) {
            self.report.problems.push(format!("{} : impossible de créer ({e})", t.display()));
            return None;
        }
        Some(Some(t))
    }

    fn dossier(&mut self, brut: &[u8]) {
        let Some(c) = self.normaliser(brut) else { return };
        if self.seul.is_some() {
            return;
        }
        let Some(t) = self.cible(&c) else {
            self.report.dirs += 1;
            return;
        };
        match fs::create_dir_all(&t) {
            Ok(()) => self.report.dirs += 1,
            Err(e) => self.report.problems.push(format!("{} : impossible de créer ({e})", t.display())),
        }
    }

    /// Copies the contents; a read error (damaged data, wrong password) is
    /// returned as is to the caller.
    fn fichier(&mut self, brut: &[u8], contenu: &mut dyn Read, mtime: Option<i64>, mode: Option<u32>) -> Result<()> {
        self.progress.check()?;
        let Some(c) = self.normaliser(brut) else {
            io::copy(contenu, &mut io::sink())?;
            return Ok(());
        };
        if !self.retenu(&c) {
            return Ok(());
        }
        let Some(t) = self.cible(&c) else {
            // Verification: read everything, to check the data.
            let n = io::copy(contenu, &mut io::sink()).map_err(|e| anyhow!(e).context(format!("{} : lecture impossible", show(&c))))?;
            self.report.bytes += n;
            self.report.files += 1;
            return Ok(());
        };
        let mut f = match t.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| File::create(&t)) {
            Ok(f) => f,
            Err(e) => {
                self.report.problems.push(format!("{} : impossible de créer ({e})", t.display()));
                io::copy(contenu, &mut io::sink())?;
                return Ok(());
            }
        };
        let mut buf = vec![0u8; 256 << 10];
        loop {
            let n = match contenu.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    drop(f);
                    let _ = fs::remove_file(&t);
                    return Err(anyhow!(e).context(format!("{} : lecture impossible", show(&c))));
                }
            };
            f.write_all(&buf[..n]).with_context(|| format!("écriture de {}", t.display()))?;
            self.report.bytes += n as u64;
            self.progress.check()?;
        }
        drop(f);
        if let Some(m) = mode.map(|m| m & 0o777).filter(|&m| m != 0) {
            let _ = platform::set_mode(&t, m);
        }
        if let Some(s) = mtime {
            let _ = filetime::set_file_mtime(&t, filetime::FileTime::from_unix_time(s, 0));
        }
        self.report.files += 1;
        self.ecrit = Some(t);
        Ok(())
    }

    fn lien(&mut self, brut: &[u8], cible: &[u8]) {
        if self.seul.is_some() {
            return;
        }
        if let Some(c) = self.normaliser(brut) {
            match self.cible(&c) {
                Some(t) => self.liens.push((t, c, cible.to_vec())),
                None => self.report.links += 1,
            }
        }
    }

    /// Hard link (tar): copy of the file already extracted.
    fn lien_physique(&mut self, brut: &[u8], vers: &[u8]) {
        let (Some(c), Some(v)) = (self.normaliser(brut), self.normaliser(vers)) else { return };
        if self.seul.is_some() {
            return;
        }
        let (Some(t), Some(source)) = (self.cible(&c), self.cible(&v)) else {
            self.report.files += 1;
            return;
        };
        match t.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::copy(&source, &t)) {
            Ok(n) => {
                self.report.files += 1;
                self.report.bytes += n;
            }
            Err(e) => self.report.problems.push(format!("{} : copie de {} impossible ({e})", show(&c), show(&v))),
        }
    }

    /// Symbolic links, last: no file is written through a link of the archive.
    /// Only relative links that stay inside the destination folder are created.
    fn finir(mut self) -> Report {
        for (t, chemin, cible) in std::mem::take(&mut self.liens) {
            let profondeur = chemin.iter().filter(|&&c| c == b'/').count() as i64;
            let mut niveau = profondeur;
            let mut sort = cible.first() == Some(&b'/') || cible.contains(&b'\\') || cible.contains(&0) || cible.get(1) == Some(&b':');
            for p in cible.split(|&c| c == b'/') {
                match p {
                    b"" | b"." => {}
                    b".." => niveau -= 1,
                    _ => niveau += 1,
                }
                sort |= niveau < 0;
            }
            if sort {
                self.report.problems.push(format!("{} → {} : lien vers l'extérieur refusé", show(&chemin), show(&cible)));
                continue;
            }
            #[cfg(unix)]
            {
                let _ = t.parent().map(fs::create_dir_all);
                match std::os::unix::fs::symlink(platform::bytes_to_os(&cible), &t) {
                    Ok(()) => self.report.links += 1,
                    Err(e) => self.report.problems.push(format!("{} : lien impossible ({e})", t.display())),
                }
            }
            // Windows: creating a link needs special rights; the target file (which is
            // in the archive) is copied instead, like 7-Zip does.
            #[cfg(not(unix))]
            {
                let source = t.parent().map(|p| p.join(platform::bytes_to_os(&cible)));
                match source.filter(|s| s.is_file()).map(|s| fs::copy(s, &t)) {
                    Some(Ok(n)) => {
                        self.report.files += 1;
                        self.report.bytes += n;
                    }
                    _ => self.report.renamed.push((show(&chemin), format!("lien vers {} non créé", show(&cible)))),
                }
            }
        }
        let mut renommes = std::mem::take(&mut self.namer.renamed);
        renommes.append(&mut self.report.renamed);
        self.report.renamed = renommes;
        self.report
    }
}

/// ZIP date (local time, no time zone) in Unix seconds.
fn date_zip(d: zip::DateTime) -> Option<i64> {
    use chrono::TimeZone;
    let jour = chrono::NaiveDate::from_ymd_opt(i32::from(d.year()), u32::from(d.month()), u32::from(d.day()))?;
    let t = jour.and_hms_opt(u32::from(d.hour()), u32::from(d.minute()), u32::from(d.second()))?;
    Some(chrono::Local.from_local_datetime(&t).earliest()?.timestamp())
}

/// Extracts `archive` (format recognised by `reconnaitre`) into `dest`.
/// `mot_de_passe` is only called if the archive is encrypted.
pub fn extraire(archive: &Path, dest: &Path, progress: &Progress, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Report> {
    let mut e = Ecrivain::new(Some(dest), None, progress)?;
    executer(archive, &mut e, "Extraction", mot_de_passe)?;
    Ok(e.finir())
}

/// Reads everything back and checks the data, without writing anything.
pub fn verifier(archive: &Path, progress: &Progress, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Report> {
    let mut e = Ecrivain::new(None, None, progress)?;
    executer(archive, &mut e, "Vérification", mot_de_passe)?;
    Ok(e.finir())
}

/// Extracts the single entry `chemin` (as given by `lister`) into
/// `dossier`; returns the location of the written file.
pub fn extraire_un(
    archive: &Path,
    chemin: &str,
    dossier: &Path,
    progress: &Progress,
    mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<PathBuf> {
    let mut e = Ecrivain::new(Some(dossier), Some(vec![chemin.as_bytes().to_vec()]), progress)?;
    e.un = true;
    executer(archive, &mut e, "Extraction", mot_de_passe)?;
    e.ecrit.take().ok_or_else(|| anyhow!("{chemin} : absent de l'archive"))
}

/// Extracts the selected entries (paths as given by `lister`; a folder takes
/// all its contents) into `dossier`, in place.
pub fn extraire_choix(
    archive: &Path,
    chemins: &[String],
    dossier: &Path,
    progress: &Progress,
    mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>,
) -> Result<Report> {
    let mut e = Ecrivain::new(Some(dossier), Some(chemins.iter().map(|c| c.trim_end_matches('/').as_bytes().to_vec()).collect()), progress)?;
    executer(archive, &mut e, "Extraction", mot_de_passe)?;
    Ok(e.finir())
}

fn executer(archive: &Path, e: &mut Ecrivain, phase: &'static str, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<()> {
    let format = reconnaitre(archive)?.ok_or_else(|| anyhow!("{} : format d'archive non reconnu", archive.display()))?;
    let progress = e.progress;
    progress.start(phase, fs::metadata(archive)?.len());
    if format == Format::SeptZip {
        return septzip_extraire(archive, e, mot_de_passe);
    }
    #[cfg(feature = "rar")]
    if format == Format::Rar {
        return rar::extraire(archive, e, mot_de_passe);
    }
    let entree = Compteur { inner: BufReader::with_capacity(1 << 20, File::open(archive)?), progress };
    match format {
        Format::Zip => zip_extraire(entree, e, mot_de_passe),
        Format::Tar => tar_extraire(entree, e),
        _ => flux_extraire(decompresseur(entree, format)?, archive, format, e),
    }
}

fn decompresseur<'a, R: Read + 'a>(entree: R, format: Format) -> Result<Box<dyn Read + 'a>> {
    Ok(match format {
        Format::Gzip => Box::new(flate2::read::MultiGzDecoder::new(entree)),
        Format::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(entree)),
        Format::Xz => Box::new(lzma_rust2::XzReader::new(entree, true)),
        Format::Lzip => Box::new(lzma_rust2::LzipReader::new(entree)),
        Format::Lzma => Box::new(lzma_rust2::LzmaReader::new_mem_limit(entree, u32::MAX, None)?),
        Format::Zstd => Box::new(zstd::stream::read::Decoder::new(entree)?),
        _ => bail!("pas un fichier compressé seul"),
    })
}

/// Name of the file inside a single compressed file: the archive's name
/// without its extension ("notes.txt.gz" → "notes.txt").
fn nom_sans_extension(archive: &Path) -> String {
    let nom = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bas = nom.to_lowercase();
    [".gz", ".bz2", ".xz", ".lzma", ".lz", ".zst", ".tgz", ".tbz2", ".txz", ".tzst", ".tlz"]
        .iter()
        .find(|x| bas.ends_with(*x) && bas.len() > x.len())
        .map_or_else(|| format!("{nom}.extrait"), |x| nom[..nom.len() - x.len()].to_string())
}

/// Proposed extraction folder: the archive's name without its extensions
/// ("site.tar.gz" → "site").
pub fn nom_du_dossier(archive: &Path) -> String {
    let nom = nom_sans_extension(archive);
    let nom = nom.strip_suffix(".extrait").unwrap_or(&nom);
    let bas = nom.to_lowercase();
    [".tar", ".zip", ".7z", ".jar", ".apk", ".epub"]
        .iter()
        .find(|x| bas.ends_with(*x) && bas.len() > x.len())
        .map_or(nom.to_string(), |x| nom[..nom.len() - x.len()].to_string())
}

/// An archive entry, to show the contents without extracting anything.
pub struct Entree {
    /// Path as accepted by `extraire_un`.
    pub chemin: String,
    /// Size once extracted (unknown for some single compressed files).
    pub taille: Option<u64>,
    pub dossier: bool,
    pub lien: bool,
}

/// Contents of the archive, without extracting anything. For a compressed
/// tar, everything must be decompressed to read the headers (nothing is
/// written).
pub fn lister(archive: &Path, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Vec<Entree>> {
    let format = reconnaitre(archive)?.ok_or_else(|| anyhow!("{} : format d'archive non reconnu", archive.display()))?;
    let progress = Progress::default();
    let mut e = Ecrivain::new(None, None, &progress)?;
    let propre = |e: &mut Ecrivain, brut: &[u8]| e.normaliser(brut).map(|c| String::from_utf8_lossy(&c).into_owned());
    let mut liste = Vec::new();
    match format {
        Format::Zip => {
            let mut z = zip::ZipArchive::new(BufReader::new(File::open(archive)?)).context("archive ZIP illisible")?;
            for i in 0..z.len() {
                let f = z.by_index_raw(i)?;
                let nom = f.name().map(|n| n.into_owned().into_bytes()).unwrap_or_else(|_| f.name_raw().to_vec());
                if let Some(chemin) = propre(&mut e, &nom) {
                    liste.push(Entree { chemin, taille: Some(f.size()), dossier: f.is_dir(), lien: f.is_symlink() });
                }
            }
        }
        #[cfg(feature = "rar")]
        Format::Rar => {
            for (nom, taille, dossier) in rar::lister(archive, mot_de_passe)? {
                if let Some(chemin) = propre(&mut e, &nom) {
                    liste.push(Entree { chemin, taille: Some(taille), dossier, lien: false });
                }
            }
        }
        Format::SeptZip => {
            let (a, _) = ouvrir_7z(archive, &progress, mot_de_passe)?;
            for x in &a.archive().files {
                if let Some(chemin) = propre(&mut e, x.name().as_bytes()) {
                    liste.push(Entree { chemin, taille: Some(x.size()), dossier: x.is_directory(), lien: false });
                }
            }
        }
        _ => {
            let entree = BufReader::with_capacity(1 << 20, File::open(archive)?);
            let mut flux: Box<dyn Read> = if format == Format::Tar { Box::new(entree) } else { decompresseur(entree, format)? };
            let mut tete = Vec::with_capacity(512);
            (&mut flux).take(512).read_to_end(&mut tete).with_context(|| format!("données {} illisibles", format.nom()))?;
            if !est_tar(&tete) {
                liste.push(Entree { chemin: nom_sans_extension(archive), taille: None, dossier: false, lien: false });
            } else {
                let mut a = tar::Archive::new(Cursor::new(tete).chain(flux));
                for x in a.entries().context("archive tar illisible")? {
                    let x = x.context("archive tar abîmée")?;
                    let genre = x.header().entry_type();
                    if !matches!(genre, tar::EntryType::Directory | tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse | tar::EntryType::Symlink | tar::EntryType::Link) {
                        continue;
                    }
                    if let Some(chemin) = propre(&mut e, &x.path_bytes()) {
                        let dossier = genre == tar::EntryType::Directory;
                        liste.push(Entree { chemin, taille: Some(x.size()), dossier, lien: genre == tar::EntryType::Symlink });
                    }
                }
            }
        }
    }
    Ok(liste)
}

/// Does the archive require a password (encrypted ZIP or 7z)?
pub fn est_chiffree(archive: &Path) -> Result<bool> {
    Ok(match reconnaitre(archive)? {
        Some(Format::Zip) => {
            let mut z = zip::ZipArchive::new(BufReader::new(File::open(archive)?)).context("archive ZIP illisible")?;
            (0..z.len()).any(|i| z.by_index_raw(i).is_ok_and(|f| f.encrypted()))
        }
        #[cfg(feature = "rar")]
        Some(Format::Rar) => rar::est_chiffree(archive),
        Some(Format::SeptZip) => ouvrir_7z(archive, &Progress::default(), &mut || bail!(BesoinMotDePasse)).is_err_and(|e| e.is::<BesoinMotDePasse>()),
        _ => false,
    })
}

/// Single compressed file: a tar inside is extracted as such, otherwise a
/// single file is written, named after the archive without its extension.
fn flux_extraire(mut flux: Box<dyn Read + '_>, archive: &Path, format: Format, e: &mut Ecrivain) -> Result<()> {
    let mut tete = Vec::with_capacity(512);
    (&mut flux).take(512).read_to_end(&mut tete).with_context(|| format!("données {} illisibles", format.nom()))?;
    let mut tout = Cursor::new(tete.clone()).chain(flux);
    if est_tar(&tete) {
        return tar_extraire(tout, e);
    }
    e.fichier(nom_sans_extension(archive).as_bytes(), &mut tout, None, None)
}

fn tar_extraire<R: Read>(lecteur: R, e: &mut Ecrivain) -> Result<()> {
    let mut a = tar::Archive::new(lecteur);
    for entree in a.entries().context("archive tar illisible")? {
        let mut x = entree.context("archive tar abîmée")?;
        let chemin = x.path_bytes().into_owned();
        let h = x.header();
        let (mtime, mode, genre) = (h.mtime().ok().map(|t| t as i64), h.mode().ok(), h.entry_type());
        let lien = x.link_name_bytes().map(|l| l.into_owned());
        match genre {
            tar::EntryType::Directory => e.dossier(&chemin),
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => e.fichier(&chemin, &mut x, mtime, mode)?,
            tar::EntryType::Symlink => e.lien(&chemin, &lien.unwrap_or_default()),
            tar::EntryType::Link => e.lien_physique(&chemin, &lien.unwrap_or_default()),
            // Devices, pipes, metadata headers: nothing to write.
            _ => {}
        }
    }
    Ok(())
}

fn zip_extraire<R: Read + Seek>(lecteur: R, e: &mut Ecrivain, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<()> {
    let mut z = zip::ZipArchive::new(lecteur).context("archive ZIP illisible")?;
    let mut mdp: Option<Zeroizing<String>> = None;
    for i in 0..z.len() {
        let chiffre = z.by_index_raw(i).map(|f| f.encrypted()).unwrap_or(false);
        if chiffre && mdp.is_none() {
            mdp = Some(mot_de_passe()?);
        }
        let mut f = match (&mdp, chiffre) {
            (Some(p), true) => match z.by_index_decrypt(i, p.as_bytes()) {
                Err(zip::result::ZipError::InvalidPassword) => bail!("mot de passe incorrect"),
                r => r,
            },
            _ => z.by_index(i),
        }
        .with_context(|| format!("entrée n° {} illisible", i + 1))?;
        let nom = f.name().map(|n| n.into_owned().into_bytes()).unwrap_or_else(|_| f.name_raw().to_vec());
        let mtime = f.last_modified().and_then(date_zip);
        if f.is_dir() {
            e.dossier(&nom);
        } else if f.is_symlink() {
            let mut cible = Vec::new();
            f.read_to_end(&mut cible)?;
            e.lien(&nom, &cible);
        } else {
            let mode = f.unix_mode();
            e.fichier(&nom, &mut f, mtime, mode)?;
        }
    }
    Ok(())
}

type Lecteur7z<'a> = sevenz_rust2::ArchiveReader<Compteur<'a, BufReader<File>>>;

/// Opens a 7z, with the password if needed (file list or encrypted data).
/// Also returns "encrypted or not".
fn ouvrir_7z<'a>(archive: &Path, progress: &'a Progress, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<(Lecteur7z<'a>, bool)> {
    use sevenz_rust2::{ArchiveReader, EncoderMethod, Error, Password};
    let ouvrir = |p: Password| -> Result<Lecteur7z<'a>, Error> {
        let f = BufReader::with_capacity(1 << 20, File::open(archive)?);
        ArchiveReader::new(Compteur { inner: f, progress }, p)
    };
    let aes = |a: &Lecteur7z| a.archive().blocks.iter().any(|b| b.coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256));
    let r = match ouvrir(Password::empty()) {
        Ok(a) if !aes(&a) => return Ok((a, false)),
        Ok(_) | Err(Error::PasswordRequired) => {
            let p = mot_de_passe()?;
            ouvrir(Password::from(p.as_str()))
        }
        Err(err) => Err(err),
    };
    let a = r.map_err(|err| match err {
        Error::PasswordRequired => anyhow!(BesoinMotDePasse),
        Error::MaybeBadPassword(_) => anyhow!("mot de passe incorrect, ou archive abîmée"),
        other => anyhow!("archive 7z illisible : {other}"),
    })?;
    Ok((a, true))
}

fn septzip_extraire(archive: &Path, e: &mut Ecrivain, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<()> {
    let (mut a, chiffree) = ouvrir_7z(archive, e.progress, mot_de_passe)?;
    let mut erreur: Option<anyhow::Error> = None;
    let r = a.for_each_entries(|x, contenu| {
        let nom = x.name().as_bytes().to_vec();
        if x.is_anti_item() {
            return Ok(true);
        }
        if x.is_directory() {
            e.dossier(&nom);
            return Ok(true);
        }
        let mtime = x.has_last_modified_date.then(|| (u64::from(x.last_modified_date()) / 10_000_000) as i64 - 11_644_473_600);
        if let Err(err) = e.fichier(&nom, contenu, mtime, None) {
            erreur = Some(err);
            return Ok(false);
        }
        // A single entry was requested and it is written: no need to go on.
        Ok(!e.un || e.ecrit.is_none())
    });
    if let Some(err) = erreur {
        // With a wrong password, the decrypted data is noise: the decompressor
        // fails on the first file.
        if chiffree && err.downcast_ref::<crate::progress::Cancelled>().is_none() && e.report.files == 0 {
            bail!("mot de passe incorrect, ou archive abîmée");
        }
        return Err(err);
    }
    use sevenz_rust2::Error;
    r.map_err(|err| match err {
        Error::PasswordRequired => anyhow!(BesoinMotDePasse),
        Error::MaybeBadPassword(_) => anyhow!("mot de passe incorrect, ou archive abîmée"),
        other => anyhow!("archive 7z abîmée : {other}"),
    })
}

/// The archive is encrypted down to its file list: the password is needed
/// before it can be opened.
#[derive(Debug)]
pub struct BesoinMotDePasse;

impl std::fmt::Display for BesoinMotDePasse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("cette archive est protégée par un mot de passe")
    }
}

impl std::error::Error for BesoinMotDePasse {}

/// RAR (every version, from RAR 1.3 to RAR 7, encrypted ones included, in
/// one or several parts), read by `rars`: a free reader written in Rust
/// (Apache-2.0), without UnRAR code.
#[cfg(feature = "rar")]
mod rar {
    use super::*;
    use rars::{ArchiveReadOptions, ArchiveReader, AttrSource, ErrorKind, ExtractionDecision};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::SystemTime;

    fn nom(brut: &[u8], unicode: bool) -> Vec<u8> {
        let n = rars::filename::decoded_name(brut, unicode, None).map(|c| c.into_owned()).unwrap_or_else(|_| brut.to_vec());
        n.into_iter().map(|c| if c == b'\\' { b'/' } else { c }).collect()
    }

    fn erreur(err: rars::Error) -> anyhow::Error {
        match err.kind() {
            ErrorKind::PasswordRequired => anyhow!(BesoinMotDePasse),
            ErrorKind::BadPassword => anyhow!("mot de passe incorrect"),
            ErrorKind::ChecksumMismatch => anyhow!("archive RAR abîmée (ou mot de passe incorrect)"),
            _ => anyhow!("archive RAR illisible ({err})"),
        }
    }

    /// The parts of a multi-volume archive, in order, starting from any of
    /// them: "name.part1.rar, name.part2.rar…" or "name.rar, name.r00,
    /// name.r01…". A single item if the archive is not split.
    pub(super) fn volumes(archive: &Path) -> Vec<PathBuf> {
        let dossier = archive.parent().map(Path::to_path_buf).unwrap_or_default();
        let nom = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bas = nom.to_ascii_lowercase();
        // New naming: …partN.rar
        if let Some(fin) = bas.strip_suffix(".rar") {
            if let Some(i) = fin.rfind(".part") {
                let chiffres = &fin[i + 5..];
                if !chiffres.is_empty() && chiffres.bytes().all(|c| c.is_ascii_digit()) {
                    let largeur = chiffres.len();
                    let tete = &nom[..i + 5];
                    let queue = &nom[nom.len() - 4..];
                    let mut liste = Vec::new();
                    for k in 1.. {
                        let p = dossier.join(format!("{tete}{k:0largeur$}{queue}"));
                        if !p.is_file() {
                            break;
                        }
                        liste.push(p);
                    }
                    if liste.len() > 1 {
                        return liste;
                    }
                    return vec![archive.to_path_buf()];
                }
            }
        }
        // Old naming: name.rar then name.r00, name.r01…
        let (tete, ancien) = if bas.ends_with(".rar") {
            (&nom[..nom.len() - 4], true)
        } else if bas.len() > 4 && bas.as_bytes()[bas.len() - 4] == b'.' && bas.as_bytes()[bas.len() - 3] == b'r' && bas[bas.len() - 2..].bytes().all(|c| c.is_ascii_digit()) {
            (&nom[..nom.len() - 4], true)
        } else {
            (nom.as_str(), false)
        };
        if ancien {
            let premier = dossier.join(format!("{tete}.rar"));
            let mut liste = vec![premier.clone()];
            for k in 0..100 {
                let p = dossier.join(format!("{tete}.r{k:02}"));
                if !p.is_file() {
                    break;
                }
                liste.push(p);
            }
            if liste.len() > 1 && premier.is_file() {
                return liste;
            }
        }
        vec![archive.to_path_buf()]
    }

    /// Opens all the parts; the password is only asked for if the headers are encrypted.
    fn ouvrir(archive: &Path, mdp: Option<&[u8]>) -> Result<Vec<rars::Archive>> {
        volumes(archive)
            .iter()
            .map(|v| ArchiveReader::read_path_with_options(v, ArchiveReadOptions::with_optional_password(mdp)).map_err(erreur))
            .collect()
    }

    fn membres(a: &[rars::Archive]) -> Result<Vec<rars::ArchiveMember>> {
        if a.len() == 1 {
            Ok(a[0].members().collect())
        } else {
            rars::volume_members(a).map_err(erreur)
        }
    }

    pub fn est_chiffree(archive: &Path) -> bool {
        let premier = volumes(archive).swap_remove(0);
        match ArchiveReader::read_path(premier) {
            Err(e) => e.kind() == ErrorKind::PasswordRequired,
            Ok(a) => a.members().any(|m| m.meta.is_encrypted),
        }
    }

    /// The password, if one is needed.
    fn mot_de_passe_si_besoin(archive: &Path, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Option<Zeroizing<String>>> {
        Ok(if est_chiffree(archive) { Some(mot_de_passe()?) } else { None })
    }

    pub fn lister(archive: &Path, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<Vec<(Vec<u8>, u64, bool)>> {
        let mdp = mot_de_passe_si_besoin(archive, mot_de_passe)?;
        let a = ouvrir(archive, mdp.as_ref().map(|p| p.as_bytes()))?;
        Ok(membres(&a)?
            .into_iter()
            .filter(|m| !m.meta.is_redirection)
            .map(|m| (nom(&m.meta.name, m.name_is_unicode()), m.meta.unpacked_size, m.meta.is_directory))
            .collect())
    }

    /// Writes while counting bytes for progress; stops if cancelled.
    struct Compte<W> {
        inner: W,
        n: Arc<AtomicU64>,
        annule: Arc<std::sync::atomic::AtomicBool>,
    }

    impl<W: Write> Write for Compte<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.annule.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("annulé"));
            }
            let k = self.inner.write(buf)?;
            self.n.fetch_add(k as u64, Ordering::Relaxed);
            Ok(k)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// What is restored once the files are closed.
    struct Finition {
        chemin: PathBuf,
        date: Option<SystemTime>,
        /// Original Unix permissions (owner read and write always kept).
        mode: Option<u32>,
    }

    fn mode_unix(attr: u64, source: AttrSource) -> Option<u32> {
        (source == AttrSource::Unix).then_some((attr as u32 & 0o777) | 0o600)
    }

    /// Decides what happens to an entry (folder, link, file); `None`: do not
    /// write it (but decode it if the archive is solid).
    fn ouvrir_sortie(
        e: &mut Ecrivain,
        chemin: &[u8],
        taille: u64,
        dossier: bool,
        lien: bool,
        date: Option<SystemTime>,
        mode: Option<u32>,
        n: &Arc<AtomicU64>,
        annule: &Arc<std::sync::atomic::AtomicBool>,
        finitions: &mut Vec<Finition>,
    ) -> std::result::Result<Option<Box<dyn Write>>, rars::Error> {
        if dossier {
            e.dossier(chemin);
            return Ok(None);
        }
        if lien {
            // Links are not recreated: no risk of leaving the folder.
            e.report.problems.push(format!("{} : lien symbolique ignoré", show(chemin)));
            return Ok(None);
        }
        match e.place(chemin) {
            None => Ok(None),
            Some(None) => {
                e.report.files += 1;
                e.report.bytes += taille;
                Ok(Some(Box::new(Compte { inner: std::io::sink(), n: n.clone(), annule: annule.clone() })))
            }
            Some(Some(t)) => {
                let f = File::create(&t).map_err(rars::Error::from)?;
                finitions.push(Finition { chemin: t.clone(), date, mode });
                e.report.files += 1;
                e.report.bytes += taille;
                e.ecrit = Some(t);
                Ok(Some(Box::new(Compte { inner: std::io::BufWriter::with_capacity(1 << 20, f), n: n.clone(), annule: annule.clone() })))
            }
        }
    }

    pub fn extraire(archive: &Path, e: &mut Ecrivain, mot_de_passe: &mut dyn FnMut() -> Result<Zeroizing<String>>) -> Result<()> {
        let mdp = mot_de_passe_si_besoin(archive, mot_de_passe)?;
        let mdp = mdp.as_ref().map(|p| p.as_bytes());
        let a = ouvrir(archive, mdp)?;
        let liste = membres(&a)?;
        let total: u64 = liste.iter().filter(|m| !m.meta.is_directory).map(|m| m.meta.unpacked_size).sum();
        e.progress.start(e.progress.snapshot().0, total);
        // A "solid" archive does not allow skipping a file: it is then decoded
        // without being written.
        let solide = liste.iter().any(|m| m.is_solid());
        let n = Arc::new(AtomicU64::new(0));
        let annule = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut deja = 0u64;
        let mut finitions = Vec::new();
        let options = ArchiveReadOptions::with_optional_password(mdp);
        let progress = e.progress;
        let suivre = |deja: &mut u64| {
            let k = n.load(Ordering::Relaxed);
            progress.advance(k - *deja);
            *deja = k;
            if progress.is_cancelled() {
                annule.store(true, Ordering::Relaxed);
            }
        };
        let r = if a.len() == 1 {
            a[0].extract_with_control(options, |m| {
                suivre(&mut deja);
                let chemin = nom(&m.meta.name, m.name_is_unicode());
                let mode = mode_unix(m.meta.file_attr, m.meta.attr_source());
                let sortie = ouvrir_sortie(e, &chemin, m.meta.unpacked_size, m.meta.is_directory, m.meta.is_redirection, m.meta.modification_time(), mode, &n, &annule, &mut finitions)?;
                Ok(match sortie {
                    Some(w) => ExtractionDecision::Extract(w),
                    None if solide && !m.meta.is_directory && !m.meta.is_redirection => ExtractionDecision::Extract(Box::new(std::io::sink())),
                    None => ExtractionDecision::Skip,
                })
            })
            .map(|_| ())
        } else {
            let famille = a[0].family();
            rars::extract_volumes_to_with_options(&a, options, |m| {
                suivre(&mut deja);
                let chemin = nom(&m.name, m.name_is_unicode);
                let date = rars::timestamp::extracted_system_time(famille, m.file_time, m.mtime_refinement);
                let mode = mode_unix(m.file_attr, m.attr_source);
                let taille = liste.iter().find(|x| x.meta.name == m.name).map_or(0, |x| x.meta.unpacked_size);
                Ok(ouvrir_sortie(e, &chemin, taille, m.is_directory, false, date, mode, &n, &annule, &mut finitions)?.unwrap_or_else(|| Box::new(std::io::sink())))
            })
        };
        if annule.load(Ordering::Relaxed) || progress.is_cancelled() {
            progress.check()?;
        }
        r.map_err(erreur)?;
        suivre(&mut deja);
        for f in finitions {
            #[cfg(unix)]
            if let Some(m) = f.mode {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&f.chemin, fs::Permissions::from_mode(m));
            }
            #[cfg(not(unix))]
            let _ = f.mode;
            if let Some(d) = f.date {
                let _ = filetime::set_file_mtime(&f.chemin, filetime::FileTime::from_system_time(d));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(entrees: &[(&[u8], tar::EntryType, &[u8], &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for &(nom, genre, lien, contenu) in entrees {
            let mut h = tar::Header::new_gnu();
            h.as_old_mut().name[..nom.len()].copy_from_slice(nom);
            h.as_old_mut().linkname[..lien.len()].copy_from_slice(lien);
            h.set_entry_type(genre);
            h.set_size(contenu.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append(&h, contenu).unwrap();
        }
        let tar = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    /// Small RARs made by RAR 7 (tests/donnees): plain and with an encrypted list.
    #[cfg(feature = "rar")]
    #[test]
    fn rar() {
        let donnees = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/donnees");
        let dest = std::env::temp_dir().join(format!("cnz-rar-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dest);
        let clair = donnees.join("petit.rar");
        assert_eq!(reconnaitre(&clair).unwrap(), Some(Format::Rar));
        assert!(!est_chiffree(&clair).unwrap());
        let liste = lister(&clair, &mut || panic!()).unwrap();
        assert!(liste.iter().any(|e| e.chemin == "dossier/note.txt" && e.taille == Some(22)));
        extraire(&clair, &dest.join("a"), &Progress::default(), &mut || panic!()).unwrap();
        assert_eq!(fs::read(dest.join("a/dossier/note.txt")).unwrap(), b"bonjour depuis un RAR\n");
        let chiffre = donnees.join("petit-chiffre.rar");
        assert!(est_chiffree(&chiffre).unwrap());
        assert!(extraire(&chiffre, &dest.join("b"), &Progress::default(), &mut || Ok(Zeroizing::new("faux".into()))).is_err());
        let un = extraire_un(&chiffre, "dossier/note.txt", &dest.join("c"), &Progress::default(), &mut || Ok(Zeroizing::new("secret".into()))).unwrap();
        assert_eq!(fs::read(un).unwrap(), b"bonjour depuis un RAR\n");
        // In three parts, opened from any of them.
        for depart in ["morceaux.part1.rar", "morceaux.part3.rar"] {
            let p = donnees.join(depart);
            assert_eq!(rar::volumes(&p).len(), 3);
            let liste = lister(&p, &mut || panic!()).unwrap();
            assert!(!liste.is_empty());
            let d = dest.join(depart);
            let r = extraire(&p, &d, &Progress::default(), &mut || panic!()).unwrap();
            assert!(r.files >= 1 && r.problems.is_empty(), "{:?}", r.problems);
            let ecrits: u64 = liste.iter().filter_map(|e| e.taille).sum();
            assert_eq!(r.bytes, ecrits);
        }
        fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn extraire_une_selection() {
        use tar::EntryType::{Directory, Regular};
        let dir = std::env::temp_dir().join(format!("cnz-choix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("choix.tar.gz");
        fs::write(
            &archive,
            tar_gz(&[
                (b"a/", Directory, b"", b""),
                (b"a/x.txt", Regular, b"", b"x"),
                (b"a/sous/y.txt", Regular, b"", b"y"),
                (b"ab.txt", Regular, b"", b"ab"),
                (b"b/z.txt", Regular, b"", b"z"),
            ]),
        )
        .unwrap();
        let dest = dir.join("sortie");
        let r = extraire_choix(&archive, &["a/".into(), "b/z.txt".into()], &dest, &Progress::default(), &mut || panic!()).unwrap();
        assert_eq!(r.files, 3);
        assert_eq!(fs::read(dest.join("a/sous/y.txt")).unwrap(), b"y");
        assert_eq!(fs::read(dest.join("b/z.txt")).unwrap(), b"z");
        // Selecting "a" does not take "ab.txt".
        assert!(!dest.join("ab.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tar_gz_et_pieges() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let dir = std::env::temp_dir().join(format!("cnz-autres-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("essai.tar.gz");
        fs::write(
            &archive,
            tar_gz(&[
                (b"./dossier/", Directory, b"", b""),
                (b"./dossier/a.txt", Regular, b"", b"bonjour"),
                (b"../evade.txt", Regular, b"", b"x"),
                (b"/absolu.txt", Regular, b"", b"y"),
                (b"dehors", Symlink, b"/etc", b""),
                (b"dossier/dedans", Symlink, b"a.txt", b""),
            ]),
        )
        .unwrap();
        assert_eq!(reconnaitre(&archive).unwrap(), Some(Format::Gzip));
        let dest = dir.join("x");
        let r = extraire(&archive, &dest, &Progress::default(), &mut || panic!("pas de mot de passe")).unwrap();
        assert_eq!(fs::read(dest.join("dossier/a.txt")).unwrap(), b"bonjour");
        assert_eq!(fs::read(dest.join("absolu.txt")).unwrap(), b"y");
        assert!(!dir.join("evade.txt").exists() && !dest.join("dehors").exists());
        assert!(r.problems.iter().any(|p| p.contains("evade")) && r.problems.iter().any(|p| p.contains("/etc")));
        #[cfg(unix)]
        assert_eq!(fs::read(dest.join("dossier/dedans")).unwrap(), b"bonjour");
        // A single compressed file keeps its name without the extension.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"texte seul").unwrap();
        fs::write(dir.join("note.txt.gz"), gz.finish().unwrap()).unwrap();
        extraire(&dir.join("note.txt.gz"), &dest, &Progress::default(), &mut || panic!()).unwrap();
        assert_eq!(fs::read(dest.join("note.txt")).unwrap(), b"texte seul");
        assert_eq!(reconnaitre(&dir.join("x/note.txt")).unwrap(), None);
        // List, verify, extract a single file.
        let liste = lister(&archive, &mut || panic!()).unwrap();
        assert!(liste.iter().any(|e| e.chemin == "dossier/a.txt" && e.taille == Some(7)));
        assert!(!liste.iter().any(|e| e.chemin.contains("evade")));
        let r = verifier(&archive, &Progress::default(), &mut || panic!()).unwrap();
        assert_eq!((r.files, r.bytes), (2, 8));
        let un = extraire_un(&archive, "dossier/a.txt", &dir.join("un"), &Progress::default(), &mut || panic!()).unwrap();
        assert_eq!(fs::read(&un).unwrap(), b"bonjour");
        assert!(!dir.join("un/absolu.txt").exists());
        assert!(!est_chiffree(&archive).unwrap());
        assert_eq!(nom_du_dossier(Path::new("/a/site.tar.gz")), "site");
        assert_eq!(nom_du_dossier(Path::new("/a/photos.zip")), "photos");
        fs::remove_dir_all(&dir).unwrap();
    }
}
