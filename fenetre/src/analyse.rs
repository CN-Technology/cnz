//! What the folder to compress contains: volume, number of files and broad
//! families (from the extension), counted on a separate thread while the
//! compression is being set up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// JPEG photos, documents (Office, PDF, OpenDocument), code and text, others.
pub const FAMILLES: usize = 4;

#[derive(Clone, Default)]
pub struct Analyse {
    pub octets: u64,
    pub fichiers: u64,
    /// Bytes per family.
    pub familles: [u64; FAMILLES],
    pub finie: bool,
}

pub fn famille(nom: &str) -> usize {
    let ext = nom.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "jfif" => 0,
        "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "doc" | "xls" | "ppt" | "pdf" | "rtf" | "epub" => 1,
        "txt" | "md" | "csv" | "json" | "xml" | "html" | "htm" | "css" | "js" | "ts" | "php" | "py" | "rs" | "c" | "h" | "cpp" | "java" | "go" | "rb" | "sh" | "sql" | "yml" | "yaml"
        | "toml" | "ini" | "log" | "vue" | "jsx" | "tsx" | "scss" | "svg" | "tex" => 2,
        _ => 3,
    }
}

/// Starts the count; the shared result fills in as it goes.
/// `arret` stops the count (screen left).
pub fn lancer(sources: Vec<PathBuf>, ctx: eframe::egui::Context) -> (Arc<Mutex<Analyse>>, Arc<AtomicBool>) {
    let a = Arc::new(Mutex::new(Analyse::default()));
    let arret = Arc::new(AtomicBool::new(false));
    let (a2, arret2) = (a.clone(), arret.clone());
    std::thread::spawn(move || {
        let mut local = Analyse::default();
        let mut pile: Vec<PathBuf> = sources;
        let mut n = 0u64;
        while let Some(p) = pile.pop() {
            if arret2.load(Ordering::Relaxed) {
                return;
            }
            let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
            if m.is_dir() {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    pile.extend(rd.flatten().map(|e| e.path()));
                }
            } else if m.is_file() {
                let nom = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                local.octets += m.len();
                local.fichiers += 1;
                local.familles[famille(&nom)] += m.len();
            }
            n += 1;
            if n % 2000 == 0 {
                *a2.lock().unwrap() = local.clone();
                ctx.request_repaint();
            }
        }
        local.finie = true;
        *a2.lock().unwrap() = local;
        ctx.request_repaint();
    });
    (a, arret)
}

/// Displayable name of a source.
pub fn nom(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}
