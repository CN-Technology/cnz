//! Recent archives of the home screen: created or opened in the window.
//! Kept in a small text file next to the language choice
//! (`%APPDATA%\CNZ\recentes`, or `~/.config/cnz/recentes`), one per line:
//! path, original size of the contents (0 if unknown), "abimee" or nothing.

use std::path::{Path, PathBuf};

const MAX: usize = 12;

#[derive(Clone)]
pub struct Recente {
    pub chemin: PathBuf,
    /// Size of the contents once extracted, if known.
    pub brut: Option<u64>,
    /// The archive was damaged the last time it was opened.
    pub abimee: bool,
}

fn fichier() -> Option<PathBuf> {
    crate::langue::dossier_reglages().map(|d| d.join("recentes"))
}

/// Recent archives that still exist, from newest to oldest.
pub fn liste() -> Vec<Recente> {
    let Some(texte) = fichier().and_then(|f| std::fs::read_to_string(f).ok()) else { return Vec::new() };
    texte
        .lines()
        .filter_map(|l| {
            let mut c = l.split('\t');
            let chemin = PathBuf::from(c.next()?);
            let brut = c.next().and_then(|n| n.parse().ok()).filter(|&n: &u64| n > 0);
            let abimee = c.next() == Some("abimee");
            chemin.is_file().then_some(Recente { chemin, brut, abimee })
        })
        .collect()
}

fn ecrire(liste: &[Recente]) {
    let Some(f) = fichier() else { return };
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let texte: String = liste
        .iter()
        .take(MAX)
        .filter_map(|r| {
            let c = r.chemin.to_str()?;
            (!c.contains(['\t', '\n'])).then(|| format!("{c}\t{}\t{}\n", r.brut.unwrap_or(0), if r.abimee { "abimee" } else { "" }))
        })
        .collect();
    let _ = std::fs::write(f, texte);
}

/// Puts the archive at the top of the list (original size kept if not given).
pub fn noter(chemin: &Path, brut: Option<u64>, abimee: bool) {
    let chemin = std::fs::canonicalize(chemin).unwrap_or_else(|_| chemin.to_path_buf());
    let mut l = liste();
    let ancien = l.iter().position(|r| r.chemin == chemin).map(|i| l.remove(i));
    let brut = brut.or(ancien.and_then(|a| a.brut));
    l.insert(0, Recente { chemin, brut, abimee });
    ecrire(&l);
}

pub fn effacer() {
    ecrire(&[]);
}
