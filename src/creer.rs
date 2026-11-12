//! Creating archives in other programs' formats, to send them to someone who
//! does not have CNz: ZIP, 7z, tar (alone or compressed: .tar.gz, .tar.bz2,
//! .tar.xz, .tar.zst) and single compressed files (.gz, .bz2, .xz, .zst).
//! Never RAR: a closed format, which CNz can only read.
//!
//! The format comes from the archive name. Password: ZIP (AES-256) and 7z
//! (AES-256, file list encrypted too). The archive is written under a
//! temporary name, renamed only once complete.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::progress::Progress;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatCree {
    Zip,
    SeptZip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    TarZst,
    Gz,
    Bz2,
    Xz,
    Zst,
}

/// (end of the name, format), longest first.
const NOMS: [(&str, FormatCree); 15] = [
    (".tar.gz", FormatCree::TarGz),
    (".tar.bz2", FormatCree::TarBz2),
    (".tar.xz", FormatCree::TarXz),
    (".tar.zst", FormatCree::TarZst),
    (".tgz", FormatCree::TarGz),
    (".tbz2", FormatCree::TarBz2),
    (".txz", FormatCree::TarXz),
    (".tzst", FormatCree::TarZst),
    (".zip", FormatCree::Zip),
    (".7z", FormatCree::SeptZip),
    (".tar", FormatCree::Tar),
    (".gz", FormatCree::Gz),
    (".bz2", FormatCree::Bz2),
    (".xz", FormatCree::Xz),
    (".zst", FormatCree::Zst),
];

impl FormatCree {
    /// Format from the archive name; `None`: .cnz archive.
    pub fn d_apres_nom(archive: &Path) -> Option<FormatCree> {
        let nom = archive.file_name()?.to_string_lossy().to_lowercase();
        NOMS.iter().find(|(fin, _)| nom.ends_with(fin) && nom.len() > fin.len()).map(|&(_, f)| f)
    }

    pub fn extension(self) -> &'static str {
        NOMS.iter().find(|&&(_, f)| f == self).map_or("", |(fin, _)| &fin[1..])
    }

    /// Password protection possible.
    pub fn chiffrable(self) -> bool {
        matches!(self, FormatCree::Zip | FormatCree::SeptZip)
    }

    /// Formats offered in the application (folders and files).
    pub const PROPOSES: [FormatCree; 6] =
        [FormatCree::Zip, FormatCree::SeptZip, FormatCree::TarGz, FormatCree::TarXz, FormatCree::TarZst, FormatCree::TarBz2];

    /// `nom` with the extension of `format` (`None`: .cnz) instead of its own:
    /// "envoi.cnz" → "envoi.zip".
    pub fn renommer(nom: &str, format: Option<FormatCree>) -> String {
        let bas = nom.to_lowercase();
        let fin = NOMS.iter().map(|(f, _)| *f).chain([".cnz"]).find(|f| bas.ends_with(f) && bas.len() > f.len()).unwrap_or("");
        let base = &nom[..nom.len() - fin.len()];
        format!("{base}.{}", format.map_or("cnz", FormatCree::extension))
    }

    /// A single file (no folder): .gz, .bz2, .xz, .zst.
    pub fn fichier_seul(self) -> bool {
        matches!(self, FormatCree::Gz | FormatCree::Bz2 | FormatCree::Xz | FormatCree::Zst)
    }
}

/// Result of a creation.
#[derive(Default, Debug)]
pub struct Bilan {
    pub fichiers: u64,
    pub dossiers: u64,
    pub liens: u64,
    /// Size of the files, and of the archive.
    pub brut: u64,
    pub archive: u64,
    /// Items left aside (unreadable, special types…).
    pub ignores: Vec<String>,
}

enum Genre {
    Fichier(u64),
    Dossier,
    Lien(PathBuf),
}

struct Element {
    disque: PathBuf,
    /// Path in the archive, "/" as separator.
    nom: String,
    genre: Genre,
    meta: fs::Metadata,
}

/// Lists what has to be stored: each source under its own name, folders
/// walked through (symbolic links kept as links, never followed).
fn parcourir(sources: &[PathBuf], a_eviter: &[PathBuf], ignores: &mut Vec<String>) -> Result<Vec<Element>> {
    fn ajouter(disque: &Path, nom: String, a_eviter: &[PathBuf], out: &mut Vec<Element>, ignores: &mut Vec<String>) {
        let meta = match fs::symlink_metadata(disque) {
            Ok(m) => m,
            Err(e) => return ignores.push(format!("{} : illisible ({e})", disque.display())),
        };
        if a_eviter.iter().any(|a| fs::canonicalize(disque).is_ok_and(|c| &c == a)) {
            return;
        }
        let t = meta.file_type();
        if t.is_symlink() {
            match fs::read_link(disque) {
                Ok(cible) => out.push(Element { disque: disque.into(), nom, genre: Genre::Lien(cible), meta }),
                Err(e) => ignores.push(format!("{} : lien illisible ({e})", disque.display())),
            }
        } else if t.is_dir() {
            let mut enfants: Vec<PathBuf> = match fs::read_dir(disque) {
                Ok(d) => d.filter_map(|e| e.ok().map(|e| e.path())).collect(),
                Err(e) => return ignores.push(format!("{} : dossier illisible ({e})", disque.display())),
            };
            enfants.sort();
            out.push(Element { disque: disque.into(), nom: nom.clone(), genre: Genre::Dossier, meta });
            for enfant in enfants {
                let n = enfant.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                ajouter(&enfant, format!("{nom}/{n}"), a_eviter, out, ignores);
            }
        } else if t.is_file() {
            out.push(Element { disque: disque.into(), nom, genre: Genre::Fichier(meta.len()), meta });
        } else {
            ignores.push(format!("{} : type de fichier spécial, ignoré", disque.display()));
        }
    }
    let mut out = Vec::new();
    for s in sources {
        let complet = fs::canonicalize(s).with_context(|| format!("{} introuvable", s.display()))?;
        let nom = complet.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "racine".into());
        ajouter(s, nom, a_eviter, &mut out, ignores);
    }
    Ok(out)
}

/// File read while moving the progress bar; opened on first read (a "solid"
/// 7z receives all its files at once).
struct Lecture<'a> {
    chemin: PathBuf,
    f: Option<File>,
    progress: &'a Progress,
}

impl Read for Lecture<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.progress.is_cancelled() {
            return Err(io::Error::other(crate::progress::Cancelled));
        }
        if self.f.is_none() {
            self.f = Some(File::open(&self.chemin)?);
        }
        let n = self.f.as_mut().unwrap().read(buf)?;
        self.progress.advance(n as u64);
        Ok(n)
    }
}

fn mtime(meta: &fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(meta: &fs::Metadata) -> u32 {
    match (meta.is_dir(), meta.permissions().readonly()) {
        (true, _) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    }
}

/// Creates `archive` (format from its name) with `sources`.
pub fn creer(archive: &Path, sources: &[PathBuf], mot_de_passe: Option<&str>, progress: &Progress) -> Result<Bilan> {
    let format = FormatCree::d_apres_nom(archive).ok_or_else(|| anyhow!("{} : format non reconnu (.zip, .7z, .tar.gz…)", archive.display()))?;
    if mot_de_passe.is_some() && !format.chiffrable() {
        bail!("le format .{} ne sait pas protéger par mot de passe : choisissez .zip, .7z ou .cnz", format.extension());
    }
    let mut bilan = Bilan::default();
    // The archive itself (if it is inside one of the folders) is not stored in it.
    let provisoire = archive.with_file_name(format!("{}.partiel", archive.file_name().unwrap_or_default().to_string_lossy()));
    let parent = archive.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).with_context(|| format!("dossier {} introuvable", parent.display()))?;
    let a_eviter = [parent.join(archive.file_name().unwrap_or_default()), parent.join(provisoire.file_name().unwrap_or_default())];
    let elements = parcourir(sources, &a_eviter, &mut bilan.ignores)?;
    if format.fichier_seul() && !(elements.len() == 1 && matches!(elements[0].genre, Genre::Fichier(_))) {
        bail!("un fichier .{} ne contient qu'un seul fichier : pour un dossier ou plusieurs fichiers, choisissez .tar.{} ou .zip", format.extension(), format.extension());
    }
    let total = elements.iter().map(|e| if let Genre::Fichier(n) = e.genre { n } else { 0 }).sum();
    progress.start("Compression", total);
    let r = (|| -> Result<()> {
        let sortie = File::create(&provisoire).with_context(|| format!("impossible de créer {}", provisoire.display()))?;
        match format {
            FormatCree::Zip => zip(sortie, &elements, mot_de_passe, progress, &mut bilan)?,
            FormatCree::SeptZip => septzip(sortie, &elements, mot_de_passe, progress, &mut bilan)?,
            _ => {
                let mut flux = compresseur(BufWriter::with_capacity(1 << 20, sortie), format)?;
                if format.fichier_seul() {
                    let e = &elements[0];
                    let n = io::copy(&mut Lecture { chemin: e.disque.clone(), f: None, progress }, &mut flux)?;
                    bilan.fichiers += 1;
                    bilan.brut += n;
                } else {
                    tar(&mut flux, &elements, progress, &mut bilan)?;
                }
                flux.finir()?;
            }
        }
        Ok(())
    })();
    match r {
        Ok(()) => {
            fs::rename(&provisoire, archive).with_context(|| format!("impossible de nommer {}", archive.display()))?;
            bilan.archive = fs::metadata(archive)?.len();
            Ok(bilan)
        }
        Err(e) => {
            let _ = fs::remove_file(&provisoire);
            // A requested stop is passed up as is (it may come from a read).
            if e.chain().any(|c| c.is::<crate::progress::Cancelled>() || c.downcast_ref::<io::Error>().and_then(|io| io.get_ref()).is_some_and(|x| x.is::<crate::progress::Cancelled>())) {
                return Err(crate::progress::Cancelled.into());
            }
            Err(e)
        }
    }
}

/// Compressed stream that must be finished explicitly (end of frame).
trait Flux: Write {
    fn finir(self: Box<Self>) -> Result<()>;
}

impl<W: Write> Flux for BufWriter<W> {
    fn finir(mut self: Box<Self>) -> Result<()> {
        Ok(self.flush()?)
    }
}

impl<W: Write> Flux for flate2::write::GzEncoder<W> {
    fn finir(self: Box<Self>) -> Result<()> {
        self.finish()?.flush()?;
        Ok(())
    }
}

impl<W: Write> Flux for bzip2::write::BzEncoder<W> {
    fn finir(self: Box<Self>) -> Result<()> {
        self.finish()?.flush()?;
        Ok(())
    }
}

impl<W: Write> Flux for lzma_rust2::XzWriter<W> {
    fn finir(self: Box<Self>) -> Result<()> {
        self.finish()?.flush()?;
        Ok(())
    }
}

impl<W: Write> Flux for zstd::stream::write::Encoder<'static, W> {
    fn finir(self: Box<Self>) -> Result<()> {
        self.finish()?.flush()?;
        Ok(())
    }
}

/// Usual settings of each tool (those of gzip, bzip2, xz; zstd 19 like CNz
/// in normal mode).
fn compresseur(sortie: BufWriter<File>, format: FormatCree) -> Result<Box<dyn Flux>> {
    Ok(match format {
        FormatCree::Tar => Box::new(sortie),
        FormatCree::TarGz | FormatCree::Gz => Box::new(flate2::write::GzEncoder::new(sortie, flate2::Compression::new(6))),
        FormatCree::TarBz2 | FormatCree::Bz2 => Box::new(bzip2::write::BzEncoder::new(sortie, bzip2::Compression::best())),
        FormatCree::TarXz | FormatCree::Xz => Box::new(lzma_rust2::XzWriter::new(sortie, lzma_rust2::XzOptions::with_preset(6))?),
        FormatCree::TarZst | FormatCree::Zst => {
            let mut z = zstd::stream::write::Encoder::new(sortie, 19)?;
            z.include_checksum(true)?;
            Box::new(z)
        }
        FormatCree::Zip | FormatCree::SeptZip => unreachable!(),
    })
}

fn tar(sortie: &mut dyn Write, elements: &[Element], progress: &Progress, bilan: &mut Bilan) -> Result<()> {
    let mut b = tar::Builder::new(sortie);
    for e in elements {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(mtime(&e.meta).max(0) as u64);
        h.set_mode(mode(&e.meta));
        match &e.genre {
            Genre::Dossier => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                b.append_data(&mut h, format!("{}/", e.nom), io::empty())?;
                bilan.dossiers += 1;
            }
            Genre::Lien(cible) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                b.append_link(&mut h, &e.nom, cible)?;
                bilan.liens += 1;
            }
            Genre::Fichier(n) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(*n);
                // File modified while being read: exactly the announced size is stored
                // (padded with zeros if needed).
                let mut lecture = Lecture { chemin: e.disque.clone(), f: None, progress }.chain(io::repeat(0)).take(*n);
                b.append_data(&mut h, &e.nom, &mut lecture).with_context(|| format!("{} : lecture impossible", e.disque.display()))?;
                bilan.fichiers += 1;
                bilan.brut += n;
            }
        }
    }
    b.finish()?;
    Ok(())
}

fn date_zip(meta: &fs::Metadata) -> Option<zip::DateTime> {
    use chrono::{Datelike, TimeZone, Timelike};
    let t = chrono::Local.timestamp_opt(mtime(meta), 0).single()?;
    let annee = u16::try_from(t.year()).ok()?.clamp(1980, 2107);
    zip::DateTime::from_date_and_time(annee, t.month() as u8, t.day() as u8, t.hour() as u8, t.minute() as u8, t.second() as u8).ok()
}

fn zip(sortie: File, elements: &[Element], mot_de_passe: Option<&str>, progress: &Progress, bilan: &mut Bilan) -> Result<()> {
    use zip::write::FileOptions;
    let mut z = zip::ZipWriter::new(BufWriter::with_capacity(1 << 20, sortie));
    for e in elements {
        let mut o: FileOptions<'_, '_, ()> = FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(6))
            .unix_permissions(mode(&e.meta));
        if let Some(d) = date_zip(&e.meta) {
            o = o.last_modified_time(d);
        }
        if let Some(p) = mot_de_passe {
            o = o.with_aes_encryption(zip::AesMode::Aes256, p);
        }
        match &e.genre {
            Genre::Dossier => {
                z.add_directory(format!("{}/", e.nom), o)?;
                bilan.dossiers += 1;
            }
            Genre::Lien(cible) => {
                z.add_symlink(&e.nom, cible.to_string_lossy(), o)?;
                bilan.liens += 1;
            }
            Genre::Fichier(n) => {
                z.start_file(&e.nom, o.large_file(*n >= 1 << 32))?;
                let copie = io::copy(&mut Lecture { chemin: e.disque.clone(), f: None, progress }, &mut z);
                copie.with_context(|| format!("{} : lecture impossible", e.disque.display()))?;
                bilan.fichiers += 1;
                bilan.brut += n;
            }
        }
    }
    z.finish()?.flush()?;
    Ok(())
}

/// "Solid" 7z (like 7-Zip): all files in a single LZMA2 block.
fn septzip(sortie: File, elements: &[Element], mot_de_passe: Option<&str>, progress: &Progress, bilan: &mut Bilan) -> Result<()> {
    use sevenz_rust2::encoder_options::{AesEncoderOptions, Lzma2Options};
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter, Password, SourceReader};
    let mut w = ArchiveWriter::new(sortie).map_err(|e| anyhow!("7z : {e}"))?;
    let mut methodes = vec![Lzma2Options::from_level(6).into()];
    if let Some(p) = mot_de_passe {
        methodes.insert(0, AesEncoderOptions::new(Password::from(p)).into());
        w.set_encrypt_header(true);
    }
    w.set_content_methods(methodes);
    let (mut entrees, mut lectures) = (Vec::new(), Vec::new());
    for e in elements {
        match &e.genre {
            Genre::Dossier => {
                w.push_archive_entry::<&[u8]>(ArchiveEntry::from_path(&e.disque, e.nom.clone()), None).map_err(|x| anyhow!("7z : {x}"))?;
                bilan.dossiers += 1;
            }
            Genre::Lien(_) => bilan.ignores.push(format!("{} : lien symbolique non rangé (le format 7z ne les garde pas)", e.disque.display())),
            Genre::Fichier(0) => {
                w.push_archive_entry::<&[u8]>(ArchiveEntry::from_path(&e.disque, e.nom.clone()), None).map_err(|x| anyhow!("7z : {x}"))?;
                bilan.fichiers += 1;
            }
            Genre::Fichier(n) => {
                entrees.push(ArchiveEntry::from_path(&e.disque, e.nom.clone()));
                lectures.push(SourceReader::new(Lecture { chemin: e.disque.clone(), f: None, progress }));
                bilan.fichiers += 1;
                bilan.brut += n;
            }
        }
    }
    if !entrees.is_empty() {
        w.push_archive_entries(entrees, lectures).map_err(|x| anyhow!("7z : {x}"))?;
    }
    progress.check()?;
    w.finish()?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    #[test]
    fn creer_puis_relire() {
        let dir = std::env::temp_dir().join(format!("cnz-creer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let src = dir.join("envoi");
        fs::create_dir_all(src.join("sous")).unwrap();
        fs::write(src.join("a.txt"), b"bonjour").unwrap();
        fs::write(src.join("sous/été.txt"), "accents ç".repeat(1000)).unwrap();
        fs::write(src.join("vide.txt"), b"").unwrap();
        for (ext, mdp) in [("zip", None), ("zip", Some("secret")), ("7z", None), ("7z", Some("secret")), ("tar.gz", None), ("tar.xz", None), ("tar.zst", None), ("tar.bz2", None)] {
            let archive = dir.join(format!("x-{}.{ext}", mdp.is_some()));
            let b = creer(&archive, &[src.clone()], mdp, &Progress::default()).unwrap();
            assert_eq!((b.fichiers, b.dossiers), (3, 2), "{ext}");
            let dest = dir.join(format!("lu-{ext}-{}", mdp.is_some()));
            let mut pw = || mdp.map(|p| Zeroizing::new(p.to_string())).ok_or_else(|| anyhow!("pas de mot de passe attendu"));
            crate::autres::extraire(&archive, &dest, &Progress::default(), &mut pw).unwrap();
            assert_eq!(fs::read(dest.join("envoi/a.txt")).unwrap(), b"bonjour", "{ext}");
            assert_eq!(fs::read_to_string(dest.join("envoi/sous/été.txt")).unwrap(), "accents ç".repeat(1000), "{ext}");
            assert_eq!(fs::read(dest.join("envoi/vide.txt")).unwrap(), b"", "{ext}");
        }
        // Single file, refusal of a folder in .gz and of a password in .tar.gz.
        creer(&dir.join("a.txt.gz"), &[src.join("a.txt")], None, &Progress::default()).unwrap();
        assert!(creer(&dir.join("d.gz"), &[src.clone()], None, &Progress::default()).is_err());
        assert!(creer(&dir.join("d.tar.gz"), &[src.clone()], Some("x"), &Progress::default()).is_err());
        assert!(!dir.join("d.gz").exists() && !dir.join("d.gz.partiel").exists());
        assert_eq!(FormatCree::renommer("envoi.cnz", Some(FormatCree::TarGz)), "envoi.tar.gz");
        assert_eq!(FormatCree::renommer("envoi.tar.gz", None), "envoi.cnz");
        fs::remove_dir_all(&dir).unwrap();
    }
}
