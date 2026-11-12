//! CNz — graphical application.
//!
//! Drop a folder or a file onto the window to compress it, or an archive
//! (.cnz, or ZIP, 7z, RAR, tar… from other programs) to explore it: folder
//! tree, checkable files, search, extraction of everything or of a
//! selection, adding files, versions, verification and repair. The work
//! runs on a separate thread: the window stays responsive, shows progress
//! and allows cancelling.
//!
//! Launch: `cnz-fenetre [PATH…]`, `cnz-fenetre --extraire-ici ARCHIVE`
//! or `cnz-fenetre --zip PATH…` (file explorer right-click menu: ZIP created
//! next to it, without asking).

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cnz::creer::FormatCree;
use cnz::index::Kind;
use cnz::progress::Progress;
use cnz::read::{Archive, ExtractOptions};
use cnz::recovery::RepairReport;
use cnz::write::{compress_many, Options};
use eframe::egui::{self, Align, Align2, Color32, Layout, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use zeroize::Zeroizing;

mod analyse;
mod langue;
mod recentes;
mod theme;

use langue::{t, tf};
use theme as th;
use theme::Icone;

/// Window icon: 64 × 64 RGBA pixels (taken from installateur/icone.png).
const ICON: &[u8] = include_bytes!("../../installateur/icone-64.rgba");

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CNz — CN Technology")
            .with_inner_size([1000.0, 660.0])
            .with_min_inner_size([720.0, 520.0])
            .with_drag_and_drop(true)
            .with_icon(Arc::new(egui::IconData { rgba: ICON.to_vec(), width: 64, height: 64 })),
        ..Default::default()
    };
    eframe::run_native(
        "CNz",
        options,
        Box::new(move |cc| {
            langue::charger();
            th::installer(&cc.egui_ctx);
            Ok(Box::new(App::from_args(cc.egui_ctx.clone(), &args)))
        }),
    )
}

/// What the window shows.
enum Screen {
    /// Home: drop zone, recent archives.
    Welcome,
    /// Settings before compressing.
    Compress(CompressForm),
    /// Open archive: explorer.
    Archive(ArchiveView),
    /// Versions of a .cnz archive.
    Versions(VersionsView),
    /// Damaged archive: diagnosis and repair.
    Repair(RepairView),
    /// Work running on another thread.
    Working(Job),
    /// Result of the last job.
    Done(Fin),
}

struct CompressForm {
    /// One folder or file, or several (each stored under its own name).
    sources: Vec<PathBuf>,
    archive: String,
    maximum: bool,
    encrypt: bool,
    password: Zeroizing<String>,
    confirm: Zeroizing<String>,
    recovery: u32,
    analyse: Arc<Mutex<analyse::Analyse>>,
    arret: Arc<AtomicBool>,
}

impl Drop for CompressForm {
    fn drop(&mut self) {
        self.arret.store(true, Ordering::Relaxed);
    }
}

/// Compression settings, copied for the worker thread.
struct Reglages {
    sources: Vec<PathBuf>,
    archive: PathBuf,
    maximum: bool,
    password: Option<Zeroizing<String>>,
    recovery: u32,
}

/// One entry of the archive (or a folder inferred from the paths).
struct Ligne {
    chemin: String,
    nom: String,
    parent: String,
    dossier: bool,
    lien: bool,
    taille: u64,
    date: Option<i64>,
}

struct ArchiveView {
    path: PathBuf,
    /// Archive from another program (ZIP, 7z, RAR, tar…), read by `cnz::autres`.
    autre: bool,
    /// Message from opening a file (double-click), written by its thread.
    status: Arc<Mutex<Option<String>>>,
    /// Encrypted archive: password to enter before opening it.
    needs_password: bool,
    password: Zeroizing<String>,
    error: Option<String>,
    lignes: Vec<Ligne>,
    /// Size and number of files of each folder (contents included).
    dossiers: HashMap<String, (u64, u64)>,
    total: u64,
    fichiers: u64,
    taille_archive: u64,
    /// Number of versions (.cnz); 0 for other formats.
    versions: usize,
    /// The archive had to be read partly from its backups: it is damaged.
    abimee: bool,
    dest: String,
    courant: String,
    recherche: String,
    choix: BTreeSet<String>,
    /// Displayed rows (indices into `lignes`) and what produced them.
    vue: Vec<usize>,
    vue_cle: Option<(String, String, usize)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Genre {
    Ajoute,
    Modifie,
    Supprime,
}

struct InfoVersion {
    numero: u64,
    date: i64,
    ajoutes: u64,
    modifies: u64,
    supprimes: u64,
    changements: Vec<(Genre, String, u64)>,
}

struct VersionsView {
    path: PathBuf,
    password: Zeroizing<String>,
    infos: Vec<InfoVersion>,
    choisie: usize,
    error: Option<String>,
}

struct RepairView {
    path: PathBuf,
    /// Why we are here (reading message), if there is one.
    raison: Option<String>,
    rapport: Option<RepairReport>,
    error: Option<String>,
    sortie: PathBuf,
    reparee: bool,
    handle: Option<JoinHandle<Result<RepairReport>>>,
}

struct Job {
    title: String,
    progress: Arc<Progress>,
    handle: Option<JoinHandle<Result<Fin>>>,
}

/// End of a job: message, and what can be done next.
#[derive(Default)]
struct Fin {
    ok: bool,
    message: String,
    /// Folder to open (extraction).
    dossier: Option<PathBuf>,
    /// Archive to explore (compression, adding, compacting).
    archive: Option<PathBuf>,
    /// .cnz archive in which verification found problems.
    reparer: Option<PathBuf>,
}

type Action = Box<dyn FnOnce(&mut App)>;

struct App {
    screen: Screen,
    /// To wake the window up from the worker thread.
    ctx: egui::Context,
    /// Recent archives, read again when returning to the home screen.
    recentes: Option<Vec<recentes::Recente>>,
}

impl App {
    fn from_args(ctx: egui::Context, args: &[String]) -> Self {
        let mut app = App { screen: Screen::Welcome, ctx, recentes: None };
        match args {
            [flag, archive] if flag == "--extraire-ici" => {
                let path = PathBuf::from(archive);
                if cnz::autres::est_chiffree(&path).unwrap_or(false) || est_cnz_chiffree(&path) {
                    // Password to enter first: the archive is shown.
                    app.open(path);
                } else {
                    let dest = default_extract_dir(&path);
                    app.start_extract(path, dest, Zeroizing::new(String::new()), false, None);
                }
            }
            [flag, sources @ ..] if flag == "--zip" && !sources.is_empty() => {
                let sources: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
                app.start_compress(Reglages { archive: zip_path(&sources), sources, maximum: false, password: None, recovery: 0 });
            }
            [] => {}
            paths => app.open_many(paths.iter().map(PathBuf::from).collect()),
        }
        app
    }

    fn compress_form(&self, sources: Vec<PathBuf>, archive: PathBuf) -> CompressForm {
        let (analyse, arret) = analyse::lancer(sources.clone(), self.ctx.clone());
        CompressForm {
            archive: archive.display().to_string(),
            sources,
            maximum: false,
            encrypt: false,
            password: Zeroizing::new(String::new()),
            confirm: Zeroizing::new(String::new()),
            recovery: 3,
            analyse,
            arret,
        }
    }

    /// Several items: to compress together (a single one: see `open`).
    fn open_many(&mut self, mut paths: Vec<PathBuf>) {
        if paths.len() == 1 {
            return self.open(paths.pop().unwrap());
        }
        // Archive proposed in the items' folder, named after that folder.
        let parent = paths[0].parent().map(Path::to_path_buf).unwrap_or_default();
        let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
        let name = parent.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "archive".into());
        let form = self.compress_form(paths, free_name(&parent, &name));
        self.screen = Screen::Compress(form);
    }

    /// Dropped folder or file: archive to open or content to compress.
    fn open(&mut self, path: PathBuf) {
        let signed = commence_par(&path, b"\x89CNZ");
        let autre = path.is_file() && !signed && cnz::autres::reconnaitre(&path).ok().flatten().is_some();
        let is_archive = path.is_file() && (signed || autre || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cnz")));
        if !is_archive {
            let archive = default_archive_path(&path);
            self.screen = Screen::Compress(self.compress_form(vec![path], archive));
            return;
        }
        let mut view = ArchiveView {
            dest: default_extract_dir(&path).display().to_string(),
            // Protected archive from another program: the password first.
            needs_password: autre && cnz::autres::est_chiffree(&path).unwrap_or(false),
            taille_archive: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            path,
            autre,
            status: Arc::default(),
            password: Zeroizing::new(String::new()),
            error: None,
            lignes: Vec::new(),
            dossiers: HashMap::new(),
            total: 0,
            fichiers: 0,
            versions: 0,
            abimee: false,
            courant: String::new(),
            recherche: String::new(),
            choix: BTreeSet::new(),
            vue: Vec::new(),
            vue_cle: None,
        };
        load_entries(&mut view);
        // Unreadable .cnz: go to the repair diagnosis.
        if !view.autre && !view.needs_password {
            if let Some(e) = view.error.take() {
                recentes::noter(&view.path, None, true);
                self.screen = Screen::Repair(RepairView::new(view.path, Some(e), &self.ctx));
                return;
            }
        }
        if view.error.is_none() && !view.needs_password {
            recentes::noter(&view.path, Some(view.total), view.abimee);
        }
        self.screen = Screen::Archive(view);
    }

    fn start_job(&mut self, title: &'static str, progress: Arc<Progress>, travail: impl FnOnce(Arc<Progress>) -> Result<Fin> + Send + 'static) {
        let ctx = self.ctx.clone();
        let p = progress.clone();
        let handle = std::thread::spawn(move || {
            let _wake = Wake(ctx);
            travail(p)
        });
        self.screen = Screen::Working(Job { title: t(title).into(), progress, handle: Some(handle) });
    }

    fn start_compress(&mut self, r: Reglages) {
        let progress = Arc::new(Progress::default());
        self.start_job("Compression", progress, move |progress| {
            let opts = Options { maximum: r.maximum, recovery_percent: r.recovery, password: r.password.clone(), progress, ..Default::default() };
            let archive = r.archive;
            let (fichiers, brut, taille, ignores) = if FormatCree::d_apres_nom(&archive).is_some() {
                let mdp = opts.password.as_ref().map(|p| p.as_str());
                let b = cnz::creer::creer(&archive, &r.sources, mdp, &opts.progress)?;
                (b.fichiers, b.brut, b.archive, b.ignores)
            } else {
                let s = compress_many(&r.sources, &archive, &opts)?;
                (s.files, s.raw_bytes, s.archive_bytes, Vec::new())
            };
            let ratio = if brut > 0 { 100.0 * taille as f64 / brut as f64 } else { 0.0 };
            let mut message = tf(
                "Archive créée : {}\n{}, {} compressés en {} ({} %).",
                &[&archive.display(), &count(fichiers), &human(brut), &human(taille), &langue::nombre(ratio)],
            );
            if !ignores.is_empty() {
                message.push_str(&tf("\nNon rangés :\n{}", &[&ignores.join("\n")]));
            }
            recentes::noter(&archive, Some(brut), false);
            Ok(Fin { ok: true, message, archive: Some(archive), ..Default::default() })
        });
    }

    /// Extraction (or verification) of the whole archive, or of the selected entries.
    fn start_extract(&mut self, path: PathBuf, dest: PathBuf, password: Zeroizing<String>, verify_only: bool, choix: Option<Vec<String>>) {
        let progress = Arc::new(Progress::default());
        let title = if verify_only { "Vérification" } else { "Extraction" };
        self.start_job(title, progress, move |progress| {
            let opts = ExtractOptions { progress, ..Default::default() };
            let mut pw = || if password.is_empty() { Err(anyhow!(t("cette archive est protégée par un mot de passe"))) } else { Ok(password.clone()) };
            let autre = cnz::autres::reconnaitre(&path)?.is_some();
            let r = if autre {
                match (&choix, verify_only) {
                    (_, true) => cnz::autres::verifier(&path, &opts.progress, &mut pw)?,
                    (Some(c), false) => cnz::autres::extraire_choix(&path, c, &dest, &opts.progress, &mut pw)?,
                    (None, false) => cnz::autres::extraire(&path, &dest, &opts.progress, &mut pw)?,
                }
            } else {
                let mut a = Archive::open_with_password(&path, &mut pw)?;
                if let Some(c) = &choix {
                    a.index.entries.retain(|e| choisi(c, &e.path));
                }
                a.extract(if verify_only { None } else { Some(&dest) }, &opts)?
            };
            let mut message = if verify_only {
                tf("Vérification terminée : {}, {}.", &[&count(r.files), &human(r.bytes)])
            } else {
                tf("Extraction terminée dans {}\n{}, {}.", &[&dest.display(), &count(r.files), &human(r.bytes)])
            };
            let ok = r.problems.is_empty();
            if !ok && !autre {
                recentes::noter(&path, None, true);
            }
            if ok {
                if verify_only {
                    message.push_str(t("\nTout est intact."));
                }
            } else {
                message.push_str(&tf("\nProblèmes :\n{}", &[&r.problems.join("\n")]));
            }
            Ok(Fin {
                ok,
                message,
                dossier: (!verify_only).then_some(dest),
                reparer: (!ok && !autre).then(|| path.clone()),
                archive: verify_only.then_some(path),
            })
        });
    }

    fn start_append(&mut self, path: PathBuf, sources: Vec<PathBuf>, password: Zeroizing<String>) {
        let progress = Arc::new(Progress::default());
        self.start_job("Ajout", progress, move |progress| {
            let opts = Options { progress, ..Default::default() };
            let mut pw = || if password.is_empty() { Err(anyhow!(t("mot de passe requis"))) } else { Ok(password.clone()) };
            let s = cnz::write::append_many(&path, &sources, &opts, &mut pw)?;
            let message = tf("Ajouté à {} : {}, {}.", &[&path.display(), &count(s.files), &human(s.raw_bytes)]);
            Ok(Fin { ok: true, message, archive: Some(path), ..Default::default() })
        });
    }

    fn start_restore(&mut self, path: PathBuf, version: u64, dest: PathBuf, password: Zeroizing<String>) {
        let progress = Arc::new(Progress::default());
        self.start_job("Extraction", progress, move |progress| {
            let opts = ExtractOptions { progress, ..Default::default() };
            let mut pw = || if password.is_empty() { Err(anyhow!(t("mot de passe requis"))) } else { Ok(password.clone()) };
            let mut a = Archive::open_with_password(&path, &mut pw)?;
            a.select_version(version)?;
            let r = a.extract(Some(&dest), &opts)?;
            let mut message = tf("Version {} extraite dans {}\n{}, {}.", &[&version, &dest.display(), &count(r.files), &human(r.bytes)]);
            if !r.problems.is_empty() {
                message.push_str(&tf("\nProblèmes :\n{}", &[&r.problems.join("\n")]));
            }
            Ok(Fin { ok: r.problems.is_empty(), message, dossier: Some(dest), ..Default::default() })
        });
    }

    fn start_compact(&mut self, path: PathBuf, password: Zeroizing<String>) {
        let progress = Arc::new(Progress::default());
        self.start_job("Compactage", progress, move |progress| {
            let opts = Options { progress, ..Default::default() };
            let mut pw = || if password.is_empty() { Err(anyhow!(t("mot de passe requis"))) } else { Ok(password.clone()) };
            let s = cnz::write::compact(&path, &opts, &mut pw)?;
            let message = tf("Archive compactée : {} → {}.", &[&human(s.before), &human(s.after)]);
            Ok(Fin { ok: true, message, archive: Some(path), ..Default::default() })
        });
    }
}

/// Is the entry (path as bytes) part of the selection (itself, or inside a selected folder)?
fn choisi(choix: &[String], chemin: &[u8]) -> bool {
    choix.iter().any(|c| {
        let c = c.as_bytes();
        chemin == c || (chemin.starts_with(c) && chemin.get(c.len()) == Some(&b'/'))
    })
}

fn commence_par(path: &Path, tete: &[u8]) -> bool {
    let mut h = vec![0u8; tete.len()];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut h)).is_ok() && h == tete
}

fn est_cnz_chiffree(path: &Path) -> bool {
    commence_par(path, b"\x89CNZ") && Archive::open(path).is_err_and(|e| e.to_string().contains("mot de passe"))
}

/// Wakes the window up when the worker thread ends, whatever the outcome
/// (success, error or cancellation).
struct Wake(egui::Context);

impl Drop for Wake {
    fn drop(&mut self) {
        self.0.request_repaint();
    }
}

impl RepairView {
    fn new(path: PathBuf, raison: Option<String>, ctx: &egui::Context) -> Self {
        let p = path.clone();
        let ctx = ctx.clone();
        let handle = std::thread::spawn(move || {
            let _wake = Wake(ctx);
            cnz::recovery::repair(&p, None)
        });
        RepairView { sortie: repaired_name(&path), path, raison, rapport: None, error: None, reparee: false, handle: Some(handle) }
    }
}

/// "photos.cnz" → "photos.repare.cnz" (like `cnz reparer`).
fn repaired_name(archive: &Path) -> PathBuf {
    let stem = archive.file_stem().map_or("archive".into(), |s| s.to_string_lossy().into_owned());
    let mut p = archive.with_file_name(format!("{stem}.repare.cnz"));
    let mut k = 2;
    while p.exists() {
        p = archive.with_file_name(format!("{stem}.repare ({k}).cnz"));
        k += 1;
    }
    p
}

/// Reads the list of entries of an archive (or notes that a password is needed).
fn load_entries(view: &mut ArchiveView) {
    let brutes: Vec<(String, bool, bool, u64, Option<i64>)>;
    if view.autre {
        if view.needs_password && view.password.is_empty() {
            return;
        }
        let pw = view.password.clone();
        match cnz::autres::lister(&view.path, &mut || if pw.is_empty() { Err(anyhow!(cnz::autres::BesoinMotDePasse)) } else { Ok(pw.clone()) }) {
            Ok(liste) => {
                view.needs_password = false;
                view.error = None;
                brutes = liste.into_iter().map(|e| (e.chemin, e.dossier, e.lien, e.taille.unwrap_or(0), None)).collect();
            }
            Err(e) if e.is::<cnz::autres::BesoinMotDePasse>() => return view.needs_password = true,
            Err(e) => return view.error = Some(format!("{e:#}")),
        }
    } else {
        let pw = view.password.clone();
        let mut asked = false;
        let opened = Archive::open_with_password(&view.path, &mut || {
            asked = true;
            if pw.is_empty() {
                Err(anyhow!(t("mot de passe requis")))
            } else {
                Ok(pw.clone())
            }
        });
        match opened {
            Ok(a) => {
                view.needs_password = false;
                view.error = None;
                view.versions = a.versions().len();
                view.abimee = !a.notes.is_empty();
                brutes = a
                    .index
                    .entries
                    .iter()
                    .map(|e| {
                        let (dossier, lien, size) = match &e.kind {
                            Kind::File { size, .. } => (false, false, *size),
                            Kind::Dir => (true, false, 0),
                            Kind::Symlink { .. } => (false, true, 0),
                        };
                        (String::from_utf8_lossy(&e.path).into_owned(), dossier, lien, size, Some(e.mtime_s))
                    })
                    .collect();
            }
            Err(_) if asked && pw.is_empty() => return view.needs_password = true,
            Err(e) => {
                if asked {
                    view.needs_password = true;
                    view.error = Some(t("mot de passe incorrect ou archive abîmée").into());
                } else {
                    view.error = Some(format!("{e:#}"));
                }
                return;
            }
        }
    }
    // Rows, folders inferred from the paths, folder sizes.
    let mut lignes = Vec::new();
    let mut connus: HashSet<String> = HashSet::new();
    let mut dossiers: HashMap<String, (u64, u64)> = HashMap::new();
    let mut total = 0;
    let mut fichiers = 0;
    let decoupe = |c: &str| -> (String, String) {
        match c.rsplit_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), c.to_string()),
        }
    };
    for (chemin, dossier, lien, taille, date) in brutes {
        let chemin = chemin.trim_end_matches('/').to_string();
        if chemin.is_empty() {
            continue;
        }
        // Parent folders.
        let mut p = chemin.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            let e = dossiers.entry(parent.to_string()).or_default();
            if !dossier {
                e.0 += taille;
                e.1 += 1;
            }
            p = parent;
        }
        if dossier {
            dossiers.entry(chemin.clone()).or_default();
            if !connus.insert(chemin.clone()) {
                continue;
            }
        } else {
            total += taille;
            fichiers += 1;
        }
        let (parent, nom) = decoupe(&chemin);
        lignes.push(Ligne { chemin, nom, parent, dossier, lien, taille, date });
    }
    for d in dossiers.keys() {
        if !connus.contains(d) {
            let (parent, nom) = decoupe(d);
            lignes.push(Ligne { chemin: d.clone(), nom, parent, dossier: true, lien: false, taille: 0, date: None });
        }
    }
    view.lignes = lignes;
    view.dossiers = dossiers;
    view.total = total;
    view.fichiers = fichiers;
    view.vue_cle = None;
}

impl ArchiveView {
    /// Recomputes the displayed rows if the folder or the search changed.
    fn mettre_a_jour_vue(&mut self) {
        let cle = (self.courant.clone(), self.recherche.to_lowercase(), self.lignes.len());
        if self.vue_cle.as_ref() == Some(&cle) {
            return;
        }
        let mut vue: Vec<usize> = if cle.1.is_empty() {
            (0..self.lignes.len()).filter(|&i| self.lignes[i].parent == self.courant).collect()
        } else {
            (0..self.lignes.len()).filter(|&i| self.lignes[i].chemin.to_lowercase().contains(&cle.1)).collect()
        };
        let l = &self.lignes;
        vue.sort_by(|&a, &b| l[b].dossier.cmp(&l[a].dossier).then_with(|| l[a].nom.to_lowercase().cmp(&l[b].nom.to_lowercase())));
        self.vue = vue;
        self.vue_cle = Some(cle);
    }

    /// Displayed size of a row (contents included for a folder).
    fn taille(&self, l: &Ligne) -> u64 {
        if l.dossier {
            self.dossiers.get(&l.chemin).map_or(0, |d| d.0)
        } else {
            l.taille
        }
    }

    /// Folders of the tree: those at the root, and the contents of each open
    /// folder (the current folder and its parents). (path, name, depth)
    fn arbre(&self) -> Vec<(String, String, usize)> {
        let mut ouverts: HashSet<&str> = HashSet::new();
        ouverts.insert("");
        let mut p = self.courant.as_str();
        loop {
            ouverts.insert(p);
            match p.rsplit_once('/') {
                Some((parent, _)) => p = parent,
                None => break,
            }
        }
        let mut enfants: HashMap<&str, Vec<&Ligne>> = HashMap::new();
        for l in self.lignes.iter().filter(|l| l.dossier) {
            enfants.entry(l.parent.as_str()).or_default().push(l);
        }
        for v in enfants.values_mut() {
            v.sort_by_key(|l| l.nom.to_lowercase());
        }
        let mut sortie = Vec::new();
        fn parcourir(p: &str, prof: usize, enfants: &HashMap<&str, Vec<&Ligne>>, ouverts: &HashSet<&str>, sortie: &mut Vec<(String, String, usize)>) {
            for l in enfants.get(p).map(Vec::as_slice).unwrap_or_default() {
                sortie.push((l.chemin.clone(), l.nom.clone(), prof));
                if ouverts.contains(l.chemin.as_str()) {
                    parcourir(&l.chemin, prof + 1, enfants, ouverts, sortie);
                }
            }
        }
        parcourir("", 1, &enfants, &ouverts, &mut sortie);
        sortie
    }
}

/// Version history of a .cnz archive, with what each version changed.
fn load_versions(path: &Path, password: &Zeroizing<String>) -> Result<Vec<InfoVersion>> {
    let pw = password.clone();
    let a = Archive::open_with_password(path, &mut || if pw.is_empty() { Err(anyhow!(t("mot de passe requis"))) } else { Ok(pw.clone()) })?;
    let mut presents: HashSet<Vec<u8>> = HashSet::new();
    let mut infos = Vec::new();
    for layer in a.versions() {
        let ajoutes_ici: HashSet<&Vec<u8>> = layer.entries.iter().map(|e| &e.path).collect();
        let mut changements = Vec::new();
        for r in &layer.removed {
            if !ajoutes_ici.contains(r) && presents.contains(r) {
                changements.push((Genre::Supprime, String::from_utf8_lossy(r).into_owned(), 0));
            }
        }
        for e in &layer.entries {
            let Kind::File { size, .. } = &e.kind else { continue };
            let genre = if presents.contains(&e.path) { Genre::Modifie } else { Genre::Ajoute };
            changements.push((genre, String::from_utf8_lossy(&e.path).into_owned(), *size));
        }
        for r in &layer.removed {
            presents.remove(r);
        }
        for e in &layer.entries {
            presents.insert(e.path.clone());
        }
        let n = |g: Genre| changements.iter().filter(|c| c.0 == g).count() as u64;
        let (ajoutes, modifies, supprimes) = (n(Genre::Ajoute), n(Genre::Modifie), n(Genre::Supprime));
        changements.sort_by(|a, b| (a.0 as u8, &a.1).cmp(&(b.0 as u8, &b.1)));
        infos.push(InfoVersion { numero: layer.number, date: layer.created as i64, ajoutes, modifies, supprimes, changements });
    }
    infos.reverse();
    Ok(infos)
}

/// Opens a file of the archive with the system's usual program: it is first
/// extracted alone into a temporary folder.
fn open_one(archive: PathBuf, autre: bool, chemin: String, password: Zeroizing<String>, status: Arc<Mutex<Option<String>>>, ctx: egui::Context) {
    *status.lock().unwrap() = Some(tf("Ouverture de {}…", &[&chemin]));
    std::thread::spawn(move || {
        let _wake = Wake(ctx);
        let r = (|| -> Result<PathBuf> {
            static NUMERO: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NUMERO.fetch_add(1, Ordering::Relaxed);
            let tmp = std::env::temp_dir().join("cnz-ouverts").join(format!("{}-{n}", std::process::id()));
            let mut pw = || if password.is_empty() { Err(anyhow!(t("mot de passe requis"))) } else { Ok(password.clone()) };
            let fichier = if autre {
                cnz::autres::extraire_un(&archive, &chemin, &tmp, &Progress::default(), &mut pw)?
            } else {
                let mut a = Archive::open_with_password(&archive, &mut pw)?;
                a.index.entries.retain(|e| e.path == chemin.as_bytes());
                let r = a.extract(Some(&tmp), &ExtractOptions::default())?;
                match r.renamed.first() {
                    Some((_, vers)) => PathBuf::from(vers),
                    None => tmp.join(&chemin),
                }
            };
            ouvrir_avec_le_systeme(&fichier)?;
            Ok(fichier)
        })();
        *status.lock().unwrap() = Some(match r {
            Ok(_) => tf("{} ouvert (copie temporaire).", &[&chemin]),
            Err(e) => tf("{} : ouverture impossible ({})", &[&chemin, &format!("{e:#}")]),
        });
    });
}

/// Opens a file or a folder with the system's usual program.
fn ouvrir_avec_le_systeme(p: &Path) -> Result<()> {
    let programme = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(programme).arg(p).spawn()?;
    Ok(())
}

/// "folder" → "folder.cnz" next to it; "folder (2).cnz" if it exists.
fn default_archive_path(source: &Path) -> PathBuf {
    let parent = source.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "archive".into());
    free_name(&parent, &name)
}

/// "name.cnz" in `dir`, or "name (2).cnz"… if it already exists.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    free_name_ext(dir, name, "cnz")
}

fn free_name_ext(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{name}.{ext}"));
    let mut k = 2;
    while p.exists() {
        p = dir.join(format!("{name} ({k}).{ext}"));
        k += 1;
    }
    p
}

/// "One-click" ZIP: "photos" → "photos.zip", "report.pdf" → "report.zip"
/// next to it; several items → named after their folder.
fn zip_path(sources: &[PathBuf]) -> PathBuf {
    let first = &sources[0];
    let parent = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = match sources {
        [one] if one.is_dir() => one.file_name().map(|n| n.to_string_lossy().into_owned()),
        [one] => one.file_stem().map(|n| n.to_string_lossy().into_owned()),
        _ => std::fs::canonicalize(&parent).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
    }
    .unwrap_or_else(|| "archive".into());
    free_name_ext(&parent, &name, "zip")
}

/// "photos.cnz" → folder "photos" next to it ("photos (2)" if it exists).
fn default_extract_dir(archive: &Path) -> PathBuf {
    let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = if archive.extension().is_some_and(|e| e.eq_ignore_ascii_case("cnz")) {
        archive.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "extrait".into())
    } else {
        // "site.tar.gz" → "site"
        cnz::autres::nom_du_dossier(archive)
    };
    libre(parent.join(&stem))
}

/// The folder, or "name (2)"… if it already exists.
fn libre(p: PathBuf) -> PathBuf {
    let mut q = p.clone();
    let mut k = 2;
    while q.exists() {
        q = PathBuf::from(format!("{} ({k})", p.display()));
        k += 1;
    }
    q
}

/// "1 file", "3 files", in the chosen language.
fn count(n: u64) -> String {
    langue::fichiers(n)
}

/// Readable size, in the chosen language.
fn human(n: u64) -> String {
    langue::taille(n)
}

/// Short date "2026-10-31 18:40".
fn date(secs: i64) -> String {
    cnz::read::format_time(secs)
}

/// Gain of an archive: "−86 %" (nothing if it is not smaller).
fn gain(brut: u64, archive: u64) -> Option<String> {
    (brut > 0 && archive < brut).then(|| format!("−{:.0} %", 100.0 * (1.0 - archive as f64 / brut as f64)))
}

/// Type badge of an archive from its name.
fn pastille_archive(nom: &str) -> String {
    let bas = nom.to_ascii_lowercase();
    if bas.ends_with(".cnz") {
        return "CNz".into();
    }
    for (fin, s) in [(".tar.gz", "TGZ"), (".tgz", "TGZ"), (".tar.xz", "TXZ"), (".tar.zst", "TZST"), (".tar.bz2", "TBZ")] {
        if bas.ends_with(fin) {
            return s.into();
        }
    }
    nom.rsplit_once('.').map(|(_, e)| e.to_uppercase()).unwrap_or_default().chars().take(4).collect()
}

/// Dashed rounded rectangle (drop zone).
fn pointilles(painter: &egui::Painter, r: Rect, rayon: f32, trait_: Stroke) {
    let mut pts = Vec::new();
    let coins = [(r.right_top() + Vec2::new(-rayon, rayon), -90.0_f32), (r.right_bottom() + Vec2::new(-rayon, -rayon), 0.0), (r.left_bottom() + Vec2::new(rayon, -rayon), 90.0), (r.left_top() + Vec2::new(rayon, rayon), 180.0)];
    for (c, debut) in coins {
        for k in 0..=8 {
            let a = (debut + 90.0 * k as f32 / 8.0).to_radians();
            pts.push(c + Vec2::new(a.cos(), a.sin()) * rayon);
        }
    }
    pts.push(pts[0]);
    painter.extend(egui::Shape::dashed_line(&pts, trait_, 8.0, 6.0));
}

/// Text truncated to the remaining width.
fn texte_tronque(ui: &mut egui::Ui, t: impl Into<RichText>) -> egui::Response {
    ui.add(egui::Label::new(t.into()).truncate().selectable(false))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Dropping a file or a folder (except during a job).
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        let occupe = matches!(self.screen, Screen::Working(_)) || matches!(&self.screen, Screen::Repair(r) if r.handle.is_some());
        if !occupe && !dropped.is_empty() {
            self.open_many(dropped);
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if matches!(self.screen, Screen::Welcome) && self.recentes.is_none() {
            self.recentes = Some(recentes::liste());
        } else if !matches!(self.screen, Screen::Welcome) {
            self.recentes = None;
        }

        let mut action: Option<Action> = None;
        let mut next: Option<Screen> = None;
        self.bandeau(ctx, &mut next);

        match &mut self.screen {
            Screen::Welcome => {
                let liste = self.recentes.clone().unwrap_or_default();
                let mut effacee = false;
                ecran_accueil(ctx, hovering, &liste, &mut action, &mut effacee);
                if effacee {
                    self.recentes = None;
                }
            }
            Screen::Compress(f) => ecran_compresser(ctx, f, &mut action, &mut next),
            Screen::Archive(v) => ecran_archive(ctx, v, &mut action, &mut next),
            Screen::Versions(v) => ecran_versions(ctx, v, &mut action, &mut next),
            Screen::Repair(r) => ecran_reparer(ctx, r, &mut action, &mut next),
            Screen::Working(job) => ecran_travail(ctx, job, &mut next),
            Screen::Done(fin) => ecran_fin(ctx, fin, &mut action, &mut next),
        }
        if let Some(a) = action {
            a(self);
        } else if let Some(s) = next {
            self.screen = s;
        }
    }
}

impl App {
    /// Night-blue top bar: logo, section, language.
    fn bandeau(&mut self, ctx: &egui::Context, next: &mut Option<Screen>) {
        let travail = matches!(self.screen, Screen::Working(_));
        let (rubrique, archive): (&str, Option<String>) = match &self.screen {
            Screen::Welcome => ("", None),
            Screen::Done(_) => ("", None),
            Screen::Compress(_) => (t("Compresser"), None),
            Screen::Archive(v) => ("", Some(analyse::nom(&v.path))),
            Screen::Versions(v) => (t("Versions"), Some(analyse::nom(&v.path))),
            Screen::Repair(r) => (t("Réparation"), Some(analyse::nom(&r.path))),
            Screen::Working(_) => (t("Au travail…"), None),
        };
        let resume = match &self.screen {
            Screen::Archive(v) if v.error.is_none() && !v.needs_password => Some((v.total, v.taille_archive)),
            _ => None,
        };
        let mut retour_archive = false;
        egui::TopBottomPanel::top("bandeau")
            .show_separator_line(false)
            .exact_height(56.0)
            .frame(egui::Frame::none().fill(th::NUIT).inner_margin(egui::Margin::symmetric(20.0, 0.0)))
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let r = th::logo_cnz(ui, 26.0);
                    if !travail && r.on_hover_text(t("Accueil")).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        *next = Some(Screen::Welcome);
                    }
                    let chevron = |ui: &mut egui::Ui| th::icone_ui(ui, Icone::Chevron, 14.0, th::BLEU_GRIS);
                    if let Some(a) = &archive {
                        ui.add_space(6.0);
                        chevron(ui);
                        let c = if rubrique.is_empty() { th::BLANC } else { th::NUIT_TEXTE };
                        let r = ui.add(egui::Label::new(RichText::new(a).font(th::titre(15.0)).color(c)).truncate().sense(Sense::click()));
                        if !rubrique.is_empty() && r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            retour_archive = true;
                        }
                    }
                    if !rubrique.is_empty() {
                        ui.add_space(6.0);
                        chevron(ui);
                        ui.label(RichText::new(rubrique).font(th::titre(15.0)).color(th::BLANC));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Language selector, in the top bar colours.
                        let w = &mut ui.visuals_mut().widgets;
                        for e in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
                            e.weak_bg_fill = th::NUIT;
                            e.bg_fill = th::NUIT_2;
                            e.bg_stroke = Stroke::new(1.0_f32, th::NUIT_TRAIT);
                            e.fg_stroke = Stroke::new(1.5_f32, th::NUIT_CLAIR);
                        }
                        w.hovered.weak_bg_fill = th::NUIT_2;
                        let mut choix = langue::actuelle();
                        egui::ComboBox::from_id_salt("langue")
                            .selected_text(RichText::new(langue::nom(choix)).color(th::NUIT_CLAIR))
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                ui.style_mut().visuals = egui::Visuals::light();
                                for i in 0..langue::NOMS.len() {
                                    ui.selectable_value(&mut choix, i, langue::nom(i));
                                }
                            })
                            .response
                            .on_hover_text(t("Langue"));
                        if choix != langue::actuelle() {
                            langue::choisir(choix);
                            th::polices(ui.ctx());
                        }
                        if let Some((brut, taille)) = resume {
                            ui.add_space(14.0);
                            if let Some(g) = gain(brut, taille) {
                                ui.label(RichText::new(g).font(th::titre(14.0)).color(th::CYAN));
                            }
                            ui.label(RichText::new(human(taille)).font(th::gras(13.0)).color(th::BLANC));
                            if brut > 0 {
                                let (r, _) = ui.allocate_exact_size(Vec2::new(120.0, 8.0), Sense::hover());
                                th::barre(ui.painter(), r, taille as f32 / brut as f32, th::CYAN, th::NUIT_TRAIT);
                                ui.label(RichText::new(human(brut)).size(12.0).color(th::NUIT_TEXTE));
                            }
                        }
                    });
                });
            });
        if retour_archive {
            let p = match &self.screen {
                Screen::Versions(v) => Some(v.path.clone()),
                Screen::Repair(r) if r.handle.is_none() => Some(r.path.clone()),
                _ => None,
            };
            if let Some(p) = p {
                let pw = match &self.screen {
                    Screen::Versions(v) => v.password.clone(),
                    _ => Zeroizing::new(String::new()),
                };
                self.open(p);
                if let Screen::Archive(v) = &mut self.screen {
                    if v.needs_password && !pw.is_empty() {
                        v.password = pw;
                        load_entries(v);
                    }
                }
            }
        }
    }
}

/// Page background with margins.
fn page() -> egui::Frame {
    egui::Frame::none().fill(th::FOND).inner_margin(egui::Margin::same(20.0))
}

fn ecran_accueil(ctx: &egui::Context, hovering: bool, recentes: &[recentes::Recente], action: &mut Option<Action>, effacee: &mut bool) {
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        let tout = ui.available_rect_before_wrap();
        let large = tout.width() >= 760.0;
        let (zone, cote) = if large {
            let w = 340.0;
            (Rect::from_min_max(tout.min, Pos2::new(tout.right() - w - 20.0, tout.bottom())), Rect::from_min_max(Pos2::new(tout.right() - w, tout.top()), tout.max))
        } else {
            let h = (tout.height() * 0.38).max(150.0);
            (Rect::from_min_max(tout.min, Pos2::new(tout.right(), tout.bottom() - h - 16.0)), Rect::from_min_max(Pos2::new(tout.left(), tout.bottom() - h), tout.max))
        };

        // Drop zone.
        let (fond, bord) = if hovering { (th::BLEU_PALE, Stroke::new(2.5_f32, th::ORANGE)) } else { (th::BLANC, Stroke::new(2.0_f32, th::BLEU_MOYEN)) };
        ui.painter().rect_filled(zone, 14.0, fond);
        pointilles(ui.painter(), zone.shrink(1.0), 14.0, bord);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(zone.shrink(24.0)), |ui| {
            ui.vertical_centered(|ui| {
                let haut = 88.0 + 18.0 + 34.0 + 50.0 + 24.0 + 44.0;
                ui.add_space(((ui.available_height() - haut) / 2.0).max(4.0));
                let (r, _) = ui.allocate_exact_size(Vec2::splat(88.0), Sense::hover());
                th::dessin_depot(ui.painter(), r.center(), hovering);
                ui.add_space(14.0);
                ui.label(RichText::new(t(if hovering { "Lâchez tout !" } else { "Déposez ici" })).font(th::titre(26.0)).color(th::NUIT));
                ui.label(RichText::new(t("un dossier, un fichier ou une archive")).size(15.0).color(th::DOUX));
                ui.label(RichText::new(".cnz · .zip · .7z · .rar · .tar.gz · .xz · .zst").size(13.0).color(th::PALE));
                ui.add_space(20.0);
                // Three buttons centred on one line, or stacked if space is short (the
                // width depends on the language).
                let textes = [t("Compresser un dossier"), t("Compresser un fichier"), t("Ouvrir une archive")];
                let largeur: f32 = textes.iter().map(|x| ui.fonts(|f| f.layout_no_wrap(x.to_string(), th::gras(14.0), Color32::BLACK).size().x) + 2.0 * ui.spacing().button_padding.x + 4.0).sum::<f32>() + 20.0;
                let boutons = |ui: &mut egui::Ui, action: &mut Option<Action>| {
                    if th::bouton(ui, textes[0]).clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_title(t("Dossier à compresser")).pick_folder() {
                            *action = Some(Box::new(move |app: &mut App| app.open(p)));
                        }
                    }
                    if th::bouton(ui, textes[1]).clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_title(t("Fichier à compresser")).pick_files() {
                            *action = Some(Box::new(move |app: &mut App| app.open_many(p)));
                        }
                    }
                    if th::bouton(ui, textes[2]).clicked() {
                        let mut ext = vec!["cnz"];
                        ext.extend_from_slice(cnz::autres::EXTENSIONS);
                        let d = rfd::FileDialog::new().set_title(t("Archive à ouvrir")).add_filter("Archive", &ext);
                        if let Some(p) = d.pick_file() {
                            *action = Some(Box::new(move |app: &mut App| app.open(p)));
                        }
                    }
                };
                if largeur <= ui.available_width() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.add_space(((ui.available_width() - largeur) / 2.0).max(0.0));
                        boutons(ui, action);
                    });
                } else {
                    boutons(ui, action);
                }
            });
        });

        // Recent archives.
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(cote), |ui| {
            th::carte().inner_margin(egui::Margin::symmetric(10.0, 14.0)).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(t("Archives récentes")).font(th::titre(15.0)).color(th::NUIT));
                    if !recentes.is_empty() {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(egui::Button::new(RichText::new(t("Effacer la liste")).size(12.0).color(th::DOUX)).frame(false)).clicked() {
                                recentes::effacer();
                                *effacee = true;
                            }
                        });
                    }
                });
                ui.add_space(4.0);
                if recentes.is_empty() {
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new(t("Les archives que vous créez ou ouvrez s'afficheront ici.")).color(th::PALE));
                    });
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for r in recentes {
                        if let Some(a) = ligne_recente(ui, r) {
                            *action = Some(a);
                        }
                    }
                });
            });
        });
    });
}

fn ligne_recente(ui: &mut egui::Ui, r: &recentes::Recente) -> Option<Action> {
    let nom = analyse::nom(&r.chemin);
    let meta = std::fs::metadata(&r.chemin).ok();
    let taille = meta.as_ref().map_or(0, |m| m.len());
    let quand = meta.and_then(|m| m.modified().ok()).and_then(|d| d.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| date(d.as_secs() as i64)).unwrap_or_default();
    let dossier = r.chemin.parent().map(analyse::nom).unwrap_or_default();
    let (rect, rep) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    if rep.hovered() {
        ui.painter().rect_filled(rect, 8.0, th::FOND);
    }
    let mut ouvrir = false;
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect.shrink2(Vec2::new(8.0, 0.0))), |ui| {
        ui.horizontal_centered(|ui| {
            if r.abimee {
                let (b, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter().rect_filled(b, 8.0, th::ORANGE_PALE);
                th::icone(ui.painter(), b.shrink(9.0), Icone::Alerte, th::ORANGE_FONCE);
            } else {
                let p = pastille_archive(&nom);
                let (fond, encre) = if p == "CNz" { (th::NUIT, th::CYAN) } else { (th::GRIS_PALE, th::NUIT) };
                th::pastille_type(ui, &p, fond, encre, Vec2::splat(36.0));
            }
            ui.add_space(4.0);
            let droite = 76.0;
            ui.allocate_ui(Vec2::new((ui.available_width() - droite).max(40.0), 44.0), |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(4.0);
                    texte_tronque(ui, RichText::new(&nom).font(th::gras(14.0)).color(th::ENCRE));
                    if r.abimee {
                        texte_tronque(ui, RichText::new(t("Abîmée : cliquez pour réparer")).size(12.0).color(th::ORANGE_FONCE));
                    } else {
                        texte_tronque(ui, RichText::new(format!("{dossier} · {quand}")).size(12.0).color(th::PALE));
                    }
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add_space(6.0);
                    ui.with_layout(Layout::top_down(Align::Max), |ui| {
                        ui.label(RichText::new(human(taille)).size(12.0).color(th::DOUX));
                        if let Some(g) = r.brut.and_then(|b| gain(b, taille)) {
                            ui.label(RichText::new(g).font(th::gras(12.0)).color(th::TURQUOISE_FONCE));
                        }
                    });
                });
            });
        });
    });
    if rep.on_hover_text(r.chemin.display().to_string()).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        ouvrir = true;
    }
    ouvrir.then(|| {
        let p = r.chemin.clone();
        let abimee = r.abimee;
        Box::new(move |app: &mut App| {
            if abimee {
                app.screen = Screen::Repair(RepairView::new(p, None, &app.ctx));
            } else {
                app.open(p)
            }
        }) as Action
    })
}

fn ecran_compresser(ctx: &egui::Context, f: &mut CompressForm, action: &mut Option<Action>, next: &mut Option<Screen>) {
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        let tout = ui.available_rect_before_wrap();
        let large = tout.width() >= 820.0;
        let w_gauche = if large { 300.0 } else { 0.0 };
        if large {
            let gauche = Rect::from_min_size(tout.min, Vec2::new(w_gauche, tout.height()));
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(gauche), |ui| panneau_source(ui, f));
        }
        let droite = Rect::from_min_max(Pos2::new(tout.left() + if large { w_gauche + 20.0 } else { 0.0 }, tout.top()), tout.max);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(droite), |ui| {
            th::carte().show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 70.0).show(ui, |ui| {
                    reglages_compression(ui, f);
                });
                // Footer: Cancel, Compress.
                ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                    ui.horizontal(|ui| {
                        let pw_ok = !f.encrypt || (!f.password.is_empty() && *f.password == *f.confirm);
                        if th::bouton_principal(ui, pw_ok && !f.archive.is_empty(), t("Compresser")) {
                            let r = Reglages {
                                sources: f.sources.clone(),
                                archive: PathBuf::from(&f.archive),
                                maximum: f.maximum,
                                password: f.encrypt.then(|| f.password.clone()),
                                recovery: f.recovery,
                            };
                            *action = Some(Box::new(move |app: &mut App| app.start_compress(r)));
                        }
                        if th::lien(ui, t("Annuler")).clicked() {
                            *next = Some(Screen::Welcome);
                        }
                    });
                    ui.add_space(6.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(r, 0.0, th::TRAIT);
                });
            });
        });
    });
}

/// Night-blue panel: what is being compressed, and what CNz finds in it.
fn panneau_source(ui: &mut egui::Ui, f: &CompressForm) {
    egui::Frame::none().fill(th::NUIT).rounding(14.0).inner_margin(egui::Margin::same(22.0)).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.spacing_mut().item_spacing.y = 6.0;
        let (titre, origine) = match f.sources.as_slice() {
            [one] => (analyse::nom(one), one.display().to_string()),
            many => (tf("{} éléments", &[&many.len()]), many[0].parent().map(|p| p.display().to_string()).unwrap_or_default()),
        };
        ui.horizontal(|ui| {
            let dossier = f.sources.len() > 1 || f.sources[0].is_dir();
            th::icone_ui(ui, if dossier { Icone::Dossier } else { Icone::Fichier }, 34.0, th::CYAN);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                texte_tronque(ui, RichText::new(titre).font(th::titre(19.0)).color(th::BLANC));
                texte_tronque(ui, RichText::new(origine).size(12.0).color(th::NUIT_TEXTE));
            });
        });
        ui.add_space(14.0);
        let a = f.analyse.lock().unwrap().clone();
        ui.label(RichText::new(human(a.octets)).font(th::titre(24.0)).color(th::BLANC));
        ui.label(RichText::new(if a.finie { count(a.fichiers) } else { tf("{} (comptage…)", &[&count(a.fichiers)]) }).color(th::NUIT_TEXTE));
        ui.add_space(14.0);
        ui.label(RichText::new(t("Ce que CNz y trouve")).size(12.0).color(th::NUIT_TEXTE));
        let couleurs = [th::CYAN, th::ORANGE, th::BLEU_MOYEN, th::BLEU_GRIS];
        let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
        let mut x = r.left();
        let total = a.octets.max(1) as f32;
        let visibles = a.familles.iter().filter(|&&o| o > 0).count();
        let mut k = 0;
        for (i, &o) in a.familles.iter().enumerate() {
            if o == 0 {
                continue;
            }
            k += 1;
            let w = (r.width() - 2.0 * (visibles as f32 - 1.0)) * o as f32 / total;
            let w = w.max(3.0);
            let seg = Rect::from_min_size(Pos2::new(x, r.top()), Vec2::new(w, r.height()));
            let rond = egui::Rounding { nw: if k == 1 { 5.0 } else { 0.0 }, sw: if k == 1 { 5.0 } else { 0.0 }, ne: if k == visibles { 5.0 } else { 0.0 }, se: if k == visibles { 5.0 } else { 0.0 } };
            ui.painter().rect_filled(seg, rond, couleurs[i]);
            x += w + 2.0;
        }
        if visibles == 0 {
            ui.painter().rect_filled(r, 5.0, th::NUIT_TRAIT);
        }
        ui.add_space(6.0);
        ui.spacing_mut().item_spacing.y = 3.0;
        let noms = [t("Photos JPEG"), t("Documents"), t("Code et texte"), t("Autres")];
        let notes = [t("recompressées sans perte"), t("reconstruits à l'identique"), "", ""];
        for i in 0..analyse::FAMILLES {
            if a.familles[i] == 0 && a.finie {
                continue;
            }
            ui.horizontal(|ui| {
                let (c, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().rect_filled(c, 3.0, couleurs[i]);
                ui.label(RichText::new(format!("{} · {}", noms[i], human(a.familles[i]))).size(13.0).color(th::NUIT_CLAIR));
            });
            if !notes[i].is_empty() && a.familles[i] > 0 {
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new(notes[i]).size(11.5).color(th::NUIT_TEXTE));
                });
                ui.add_space(4.0);
            }
        }
    });
}

/// Square checkbox, with its clickable label.
fn case_a_cocher(ui: &mut egui::Ui, valeur: &mut bool, texte: &str) -> egui::Response {
    let g = ui.painter().layout(texte.to_string(), th::texte(14.0), th::ENCRE, ui.available_width() - 28.0);
    let (r, mut rep) = ui.allocate_exact_size(Vec2::new(26.0 + g.size().x, g.size().y.max(22.0)), Sense::click());
    th::case(ui.painter(), Pos2::new(r.left() + 9.0, r.center().y), *valeur);
    ui.painter().galley(Pos2::new(r.left() + 26.0, r.center().y - g.size().y / 2.0), g, th::ENCRE);
    if rep.clicked() {
        *valeur = !*valeur;
        rep.mark_changed();
    }
    rep.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn etiquette(ui: &mut egui::Ui, texte: &str) {
    ui.label(RichText::new(texte).font(th::gras(13.0)).color(th::ENCRE));
}

/// Choice card (Normal / Maximum mode).
fn choix_mode(ui: &mut egui::Ui, titre: &str, aide: &str, actif: bool, largeur: f32) -> egui::Response {
    let (r, rep) = ui.allocate_exact_size(Vec2::new(largeur, 58.0), Sense::click());
    let (fond, bord) = if actif { (th::BLEU_PALE, Stroke::new(2.0_f32, th::NUIT)) } else if rep.hovered() { (th::FOND, Stroke::new(1.0_f32, th::BORD)) } else { (th::BLANC, Stroke::new(1.0_f32, th::BORD)) };
    ui.painter().rect(r, 10.0, fond, bord);
    ui.painter().text(r.left_top() + Vec2::new(14.0, 11.0), Align2::LEFT_TOP, titre, th::titre(14.0), th::NUIT);
    ui.painter().text(r.left_top() + Vec2::new(14.0, 33.0), Align2::LEFT_TOP, aide, th::texte(12.0), th::DOUX);
    rep.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn reglages_compression(ui: &mut egui::Ui, f: &mut CompressForm) {
    let col = 130.0;
    let ligne = |ui: &mut egui::Ui, nom: &str, contenu: &mut dyn FnMut(&mut egui::Ui)| {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(col, 36.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(col);
                etiquette(ui, nom);
            });
            contenu(ui);
        });
        ui.add_space(10.0);
    };
    ligne(ui, t("Archive"), &mut |ui| {
        let bouton = t("Parcourir…");
        let wb = ui.fonts(|fo| fo.layout_no_wrap(bouton.into(), th::gras(13.0), Color32::BLACK).size().x) + 2.0 * ui.spacing().button_padding.x + ui.spacing().item_spacing.x + 30.0;
        ui.add(egui::TextEdit::singleline(&mut f.archive).desired_width((ui.available_width() - wb).max(80.0)).margin(Vec2::new(10.0, 8.0)));
        if th::bouton_outil(ui, true, bouton).clicked() {
            let p = PathBuf::from(&f.archive);
            let mut d = rfd::FileDialog::new().set_title(t("Enregistrer l'archive")).add_filter("Archive", &["cnz", "zip", "7z", "tar", "gz", "tgz", "xz", "txz", "zst", "tzst", "bz2", "tbz2"]);
            if let Some(dir) = p.parent() {
                d = d.set_directory(dir);
            }
            if let Some(name) = p.file_name() {
                d = d.set_file_name(name.to_string_lossy());
            }
            if let Some(mut chosen) = d.save_file() {
                if chosen.extension().is_none() {
                    chosen.set_extension("cnz");
                }
                f.archive = chosen.display().to_string();
            }
        }
    });
    // Format from the archive name (none: .cnz).
    let format = FormatCree::d_apres_nom(Path::new(&f.archive));
    ligne(ui, t("Format"), &mut |ui| {
        let mut choix = format;
        egui::ComboBox::from_id_salt("format")
            .width(220.0)
            .selected_text(RichText::new(format.map_or(t("CNz (recommandé)").to_string(), |x| format!(".{}", x.extension()))).font(th::gras(13.0)).color(th::NUIT))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choix, None, t("CNz (recommandé) : le plus petit, réparable"));
                for x in FormatCree::PROPOSES {
                    let aide = match x {
                        FormatCree::Zip => t(" : s'ouvre partout, même sans logiciel"),
                        FormatCree::SeptZip => t(" : pour qui a 7-Zip"),
                        _ => t(" : Linux, Mac"),
                    };
                    ui.selectable_value(&mut choix, Some(x), format!(".{}{aide}", x.extension()));
                }
            })
            .response
            .on_hover_text(t("Pour un destinataire qui n'a pas CNz : ZIP s'ouvre partout."));
        if choix != format {
            f.archive = FormatCree::renommer(&f.archive, choix);
        }
    });
    if format.is_none() {
        ligne(ui, t("Mode"), &mut |ui| {
            let w = ((ui.available_width() - 10.0) / 2.0).clamp(120.0, 240.0);
            if choix_mode(ui, t("Normal"), t("Rapide"), !f.maximum, w).on_hover_text(t("Rapide à compresser et à extraire.")).clicked() {
                f.maximum = false;
            }
            ui.add_space(2.0);
            if choix_mode(ui, t("Maximum"), t("Plus petit, plus lent"), f.maximum, w)
                .on_hover_text(t("15 à 30 % plus petit sur le texte, le code et les documents,\nmais compression et extraction bien plus lentes (≈ 1 Mo/s par cœur)."))
                .clicked()
            {
                f.maximum = true;
            }
        });
        ligne(ui, t("Récupération"), &mut |ui| {
            ui.spacing_mut().slider_width = (ui.available_width() - 80.0).clamp(100.0, 300.0);
            let v = ui.visuals_mut();
            v.widgets.inactive.bg_fill = th::TRAIT;
            v.widgets.inactive.fg_stroke = egui::Stroke::new(2.0_f32, th::NUIT);
            v.widgets.hovered.bg_fill = th::BORD;
            v.widgets.active.bg_fill = th::BORD;
            v.selection.bg_fill = th::NUIT;
            ui.add(egui::Slider::new(&mut f.recovery, 0..=20).suffix(" %").trailing_fill(true));
        });
        ui.horizontal(|ui| {
            ui.add_space(col + 8.0);
            ui.add(egui::Label::new(RichText::new(t("Données de réparation : l'archive reste récupérable si elle est un peu abîmée.")).size(12.0).color(th::PALE)).wrap());
        });
        ui.add_space(10.0);
    }
    if format.is_none_or(FormatCree::chiffrable) {
        ligne(ui, t("Protection"), &mut |ui| {
            let texte = match format {
                Some(FormatCree::Zip) => t("Protéger par un mot de passe (AES-256)"),
                _ => t("Protéger par un mot de passe (noms de fichiers compris)"),
            };
            let r = case_a_cocher(ui, &mut f.encrypt, texte);
            if format == Some(FormatCree::Zip) {
                r.on_hover_text(t("Le ZIP protégé s'ouvre avec 7-Zip, WinRAR ou CNz,\nmais pas avec l'outil intégré de Windows."));
            }
        });
    } else {
        f.encrypt = false;
    }
    if f.encrypt {
        ligne(ui, t("Mot de passe"), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut *f.password).password(true).desired_width(260.0).margin(Vec2::new(10.0, 8.0)));
        });
        ligne(ui, t("Confirmation"), &mut |ui| {
            ui.add(egui::TextEdit::singleline(&mut *f.confirm).password(true).desired_width(260.0).margin(Vec2::new(10.0, 8.0)));
        });
        if !f.confirm.is_empty() && *f.password != *f.confirm {
            ui.horizontal(|ui| {
                ui.add_space(col + 8.0);
                ui.colored_label(th::ORANGE_FONCE, t("Les deux mots de passe diffèrent."));
            });
        }
    }
}

fn ecran_archive(ctx: &egui::Context, v: &mut ArchiveView, action: &mut Option<Action>, next: &mut Option<Screen>) {
    if v.needs_password || v.error.is_some() {
        egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.2).max(10.0));
                ui.allocate_ui(Vec2::new(ui.available_width().min(520.0), 300.0), |ui| {
                    th::carte().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if v.needs_password {
                            ui.horizontal(|ui| {
                                th::icone_ui(ui, Icone::Cadenas, 28.0, th::NUIT);
                                ui.label(RichText::new(t("Cette archive est protégée par un mot de passe.")).font(th::titre(15.0)).color(th::NUIT));
                            });
                            ui.add_space(10.0);
                            ui.horizontal(|ui| {
                                let r = ui.add(egui::TextEdit::singleline(&mut *v.password).password(true).hint_text(t("mot de passe")).desired_width(ui.available_width() - 150.0).margin(Vec2::new(10.0, 9.0)));
                                if !r.has_focus() && v.password.is_empty() {
                                    r.request_focus();
                                }
                                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                if th::bouton_principal(ui, true, t("Ouvrir")) || enter {
                                    let (p, pw) = (v.path.clone(), v.password.clone());
                                    load_entries(v);
                                    if v.error.is_none() && !v.needs_password {
                                        recentes::noter(&p, Some(v.total), v.abimee);
                                    }
                                    drop(pw);
                                }
                            });
                            if let Some(e) = &v.error {
                                ui.colored_label(th::ORANGE_FONCE, e);
                            }
                        } else if let Some(e) = &v.error {
                            ui.horizontal(|ui| {
                                th::icone_ui(ui, Icone::Alerte, 28.0, th::ORANGE_FONCE);
                                ui.label(RichText::new(tf("Archive illisible : {}", &[&e])).color(th::ENCRE));
                            });
                        }
                    });
                });
            });
        });
        return;
    }
    v.mettre_a_jour_vue();
    let cnz = !v.autre;

    // Toolbar.
    egui::TopBottomPanel::top("outils")
        .show_separator_line(true)
        .frame(egui::Frame::none().fill(th::BLANC).inner_margin(egui::Margin::symmetric(20.0, 12.0)).stroke(Stroke::new(1.0_f32, th::TRAIT)))
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let b = egui::Button::new(RichText::new(t("Extraire tout")).font(th::titre(13.0)).color(th::NUIT)).fill(th::ORANGE).stroke(Stroke::NONE).rounding(7.0).min_size(Vec2::new(0.0, 34.0));
                if ui.add(b).clicked() {
                    let (p, d, pw) = (v.path.clone(), PathBuf::from(&v.dest), v.password.clone());
                    *action = Some(Box::new(move |app: &mut App| app.start_extract(p, d, pw, false, None)));
                }
                let n = v.choix.len();
                if th::bouton_outil(ui, n > 0, &tf("Extraire la sélection ({})", &[&n])).clicked() {
                    let (p, d, pw, c) = (v.path.clone(), PathBuf::from(&v.dest), v.password.clone(), v.choix.iter().cloned().collect());
                    *action = Some(Box::new(move |app: &mut App| app.start_extract(p, d, pw, false, Some(c))));
                }
                ui.add_space(4.0);
                if cnz {
                    let r = ui.menu_button(RichText::new(format!("+  {}", t("Ajouter"))).font(th::gras(13.0)).color(th::NUIT), |ui| {
                        ui.style_mut().visuals = egui::Visuals::light();
                        let mut choisis: Option<Vec<PathBuf>> = None;
                        if ui.button(t("Des fichiers…")).clicked() {
                            choisis = rfd::FileDialog::new().set_title(t("Fichiers à ajouter")).pick_files();
                            ui.close_menu();
                        }
                        if ui.button(t("Un dossier…")).clicked() {
                            choisis = rfd::FileDialog::new().set_title(t("Dossier à ajouter")).pick_folder().map(|p| vec![p]);
                            ui.close_menu();
                        }
                        if let Some(s) = choisis {
                            let (p, pw) = (v.path.clone(), v.password.clone());
                            *action = Some(Box::new(move |app: &mut App| app.start_append(p, s, pw)));
                        }
                    });
                    r.response.on_hover_text(t("Les fichiers ajoutés forment une nouvelle version ; les précédentes restent lisibles."));
                    if th::bouton_outil(ui, true, &tf("Versions ({})", &[&v.versions])).clicked() {
                        let (p, pw) = (v.path.clone(), v.password.clone());
                        let r = load_versions(&p, &pw);
                        let (infos, error) = match r {
                            Ok(i) => (i, None),
                            Err(e) => (Vec::new(), Some(format!("{e:#}"))),
                        };
                        *next = Some(Screen::Versions(VersionsView { path: p, password: pw, infos, choisie: 0, error }));
                    }
                }
                if th::bouton_outil(ui, true, t("Vérifier")).on_hover_text(t("Relit tout et contrôle chaque fichier, sans rien écrire.")).clicked() {
                    let (p, pw) = (v.path.clone(), v.password.clone());
                    *action = Some(Box::new(move |app: &mut App| app.start_extract(p, PathBuf::new(), pw, true, None)));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut v.recherche).hint_text(t("Rechercher…")).desired_width(200.0).margin(egui::Margin { left: 34.0, right: 10.0, top: 8.0, bottom: 8.0 }));
                    th::icone(ui.painter(), Rect::from_center_size(r.rect.left_center() - Vec2::new(17.0, 0.0), Vec2::splat(16.0)), Icone::Loupe, th::PALE);
                });
            });
        });

    if v.abimee {
        egui::TopBottomPanel::top("abimee").show_separator_line(false).frame(egui::Frame::none().fill(th::ORANGE_PALE).inner_margin(egui::Margin::symmetric(20.0, 8.0))).show(ctx, |ui| {
            ui.horizontal(|ui| {
                th::icone_ui(ui, Icone::Alerte, 18.0, th::ORANGE_FONCE);
                ui.label(RichText::new(t("Cette archive est abîmée : une partie a été lue grâce aux copies de secours.")).color(th::ENCRE));
                if th::bouton_outil(ui, true, t("Réparer…")).clicked() {
                    let p = v.path.clone();
                    *action = Some(Box::new(move |app: &mut App| app.screen = Screen::Repair(RepairView::new(p, None, &app.ctx))));
                }
            });
        });
    }

    // Footer: selection, destination.
    egui::TopBottomPanel::bottom("pied")
        .show_separator_line(true)
        .frame(egui::Frame::none().fill(th::BLANC).inner_margin(egui::Margin::symmetric(16.0, 8.0)).stroke(Stroke::new(1.0_f32, th::TRAIT)))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let info = match v.status.lock().unwrap().clone() {
                    Some(s) => s,
                    None => tf("{} sélectionné(s) · {}, {} une fois extraits", &[&v.choix.len(), &count(v.fichiers), &human(v.total)]),
                };
                ui.label(RichText::new(info).size(12.0).color(th::DOUX));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if th::bouton_outil(ui, true, t("Parcourir…")).clicked() {
                        let mut d = rfd::FileDialog::new().set_title(t("Dossier où extraire"));
                        if let Some(dir) = Path::new(&v.dest).parent() {
                            d = d.set_directory(dir);
                        }
                        if let Some(chosen) = d.pick_folder() {
                            // In the chosen folder, a subfolder named after the archive.
                            v.dest = default_extract_dir(&chosen.join(v.path.file_name().unwrap_or_default())).display().to_string();
                        }
                    }
                    ui.add(egui::TextEdit::singleline(&mut v.dest).desired_width((ui.available_width() * 0.45).clamp(160.0, 380.0)).margin(Vec2::new(8.0, 6.0)));
                    ui.label(RichText::new(t("Extraire dans")).font(th::gras(12.0)).color(th::ENCRE));
                });
            });
        });

    // Folder tree.
    let largeur_arbre = if ctx.screen_rect().width() >= 820.0 { 210.0 } else { 0.0 };
    if largeur_arbre > 0.0 {
        egui::SidePanel::left("arbre")
            .resizable(false)
            .exact_width(largeur_arbre)
            .show_separator_line(true)
            .frame(egui::Frame::none().fill(th::BLANC).inner_margin(egui::Margin::symmetric(8.0, 10.0)).stroke(Stroke::new(1.0_f32, th::TRAIT)))
            .show(ctx, |ui| {
                let w_ligne = ui.available_width() - 4.0;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_max_width(w_ligne);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let racine = analyse::nom(&v.path);
                    let mut aller: Option<String> = None;
                    let mut entree = |ui: &mut egui::Ui, chemin: &str, nom: &str, prof: usize, actif: bool| {
                        let (r, rep) = ui.allocate_exact_size(Vec2::new(w_ligne, 30.0), Sense::click());
                        if actif {
                            ui.painter().rect_filled(r, 6.0, th::SELECTION);
                        } else if rep.hovered() {
                            ui.painter().rect_filled(r, 6.0, th::FOND);
                        }
                        let x = r.left() + 6.0 + 14.0 * prof as f32;
                        th::icone(ui.painter(), Rect::from_center_size(Pos2::new(x + 8.0, r.center().y), Vec2::splat(16.0)), Icone::Dossier, if actif { th::NUIT } else { th::PALE });
                        let police = if actif { th::gras(13.0) } else { th::texte(13.0) };
                        let g = ui.painter().layout(nom.to_string(), police, if actif { th::NUIT } else { th::ENCRE }, f32::INFINITY);
                        let place = r.right() - x - 26.0;
                        let g = if g.size().x > place {
                            let mut job = egui::text::LayoutJob::simple_singleline(nom.to_string(), if actif { th::gras(13.0) } else { th::texte(13.0) }, if actif { th::NUIT } else { th::ENCRE });
                            job.wrap = egui::text::TextWrapping::truncate_at_width(place);
                            ui.fonts(|f| f.layout_job(job))
                        } else {
                            g
                        };
                        ui.painter().galley(Pos2::new(x + 22.0, r.center().y - g.size().y / 2.0), g, th::ENCRE);
                        if rep.on_hover_text(if chemin.is_empty() { nom.to_string() } else { chemin.to_string() }).clicked() {
                            aller = Some(chemin.to_string());
                        }
                    };
                    entree(ui, "", &racine, 0, v.courant.is_empty() && v.recherche.is_empty());
                    for (chemin, nom, prof) in v.arbre() {
                        let actif = v.courant == chemin && v.recherche.is_empty();
                        entree(ui, &chemin, &nom, prof, actif);
                    }
                    if let Some(c) = aller {
                        v.courant = c;
                        v.recherche.clear();
                    }
                });
            });
    }

    // File table.
    egui::CentralPanel::default().frame(egui::Frame::none().fill(th::BLANC)).show(ctx, |ui| {
        let largeur = ui.available_width();
        let col_date = if largeur > 560.0 { 140.0 } else { 0.0 };
        let gauche = 16.0;
        let haut = 36.0;
        // Header.
        let (r, _) = ui.allocate_exact_size(Vec2::new(largeur, haut), Sense::hover());
        ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0_f32, th::TRAIT));
        let tous = !v.vue.is_empty() && v.vue.iter().all(|&i| v.choix.contains(&v.lignes[i].chemin));
        let case = Rect::from_center_size(Pos2::new(r.left() + gauche + 8.0, r.center().y), Vec2::splat(22.0));
        let rc = ui.interact(case, ui.id().with("tout"), Sense::click());
        th::case(ui.painter(), case.center(), tous);
        if rc.on_hover_text(t("Tout cocher")).clicked() {
            for &i in &v.vue {
                let c = v.lignes[i].chemin.clone();
                if tous {
                    v.choix.remove(&c);
                } else {
                    v.choix.insert(c);
                }
            }
        }
        let police = th::gras(12.0);
        let x_nom = r.left() + gauche + 30.0;
        let x_taille = r.right() - 16.0 - col_date - 12.0;
        ui.painter().text(Pos2::new(x_nom, r.center().y), Align2::LEFT_CENTER, if v.recherche.is_empty() { t("Nom") } else { t("Résultats de la recherche") }, police.clone(), th::DOUX);
        ui.painter().text(Pos2::new(x_taille, r.center().y), Align2::RIGHT_CENTER, t("Taille"), police.clone(), th::DOUX);
        if col_date > 0.0 {
            ui.painter().text(Pos2::new(r.right() - 16.0 - col_date, r.center().y), Align2::LEFT_CENTER, t("Modifié"), police.clone(), th::DOUX);
        }

        if v.vue.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(if v.recherche.is_empty() { t("Ce dossier est vide.") } else { t("Aucun fichier ne correspond.") }).color(th::PALE));
            });
            return;
        }
        let mut basculer: Option<String> = None;
        let mut entrer: Option<String> = None;
        let mut ouvrir: Option<String> = None;
        let lignes_h = 40.0;
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, lignes_h, v.vue.len(), |ui, rangs| {
            for &i in &v.vue[rangs] {
                let l = &v.lignes[i];
                let (r, rep) = ui.allocate_exact_size(Vec2::new(ui.available_width(), lignes_h), Sense::click());
                let coche = v.choix.contains(&l.chemin);
                let fond = if coche {
                    th::SELECTION
                } else if rep.hovered() {
                    th::FOND
                } else {
                    th::BLANC
                };
                ui.painter().rect_filled(r, 0.0, fond);
                ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0_f32, rvb_trait()));
                th::case(ui.painter(), Pos2::new(r.left() + gauche + 8.0, r.center().y), coche);
                let p = Rect::from_min_size(Pos2::new(x_nom, r.center().y - 10.0), Vec2::new(32.0, 20.0));
                if l.dossier {
                    ui.painter().rect_filled(p, 4.0, th::GRIS_PALE);
                    th::icone(ui.painter(), Rect::from_center_size(p.center(), Vec2::splat(16.0)), Icone::Dossier, th::NUIT);
                } else {
                    let (etiquette, f, e) = th::famille(&l.nom, false);
                    ui.painter().rect_filled(p, 4.0, f);
                    ui.painter().text(p.center(), Align2::CENTER_CENTER, etiquette, th::gras(9.0), e);
                }
                let nom = if v.recherche.is_empty() { l.nom.as_str() } else { l.chemin.as_str() };
                let place = x_taille - 90.0 - (x_nom + 44.0);
                let mut job = egui::text::LayoutJob::simple_singleline(nom.to_string(), if l.dossier { th::gras(13.0) } else { th::texte(13.0) }, th::ENCRE);
                job.wrap = egui::text::TextWrapping::truncate_at_width(place.max(40.0));
                let g = ui.fonts(|f| f.layout_job(job));
                ui.painter().galley(Pos2::new(x_nom + 44.0, r.center().y - g.size().y / 2.0), g, th::ENCRE);
                let taille = if l.lien { t("lien").to_string() } else { human(v.taille(l)) };
                ui.painter().text(Pos2::new(x_taille, r.center().y), Align2::RIGHT_CENTER, taille, th::texte(13.0), th::DOUX);
                if col_date > 0.0 {
                    if let Some(d) = l.date {
                        ui.painter().text(Pos2::new(r.right() - 16.0 - col_date, r.center().y), Align2::LEFT_CENTER, date(d), th::texte(12.0), th::PALE);
                    }
                }
                let aide = if l.dossier { t("Double-clic : ouvrir ce dossier") } else { t("Double-clic : ouvrir ce fichier") };
                let rep = rep.on_hover_text(aide);
                if rep.double_clicked() {
                    if l.dossier {
                        entrer = Some(l.chemin.clone());
                    } else if !l.lien {
                        ouvrir = Some(l.chemin.clone());
                    }
                } else if rep.clicked() {
                    basculer = Some(l.chemin.clone());
                }
            }
        });
        if let Some(c) = basculer {
            if !v.choix.remove(&c) {
                v.choix.insert(c);
            }
        }
        if let Some(c) = entrer {
            v.courant = c;
            v.recherche.clear();
        }
        if let Some(chemin) = ouvrir {
            let (p, autre, pw, status) = (v.path.clone(), v.autre, v.password.clone(), v.status.clone());
            *action = Some(Box::new(move |app: &mut App| open_one(p, autre, chemin, pw, status, app.ctx.clone())));
        }
    });
}

fn rvb_trait() -> Color32 {
    Color32::from_rgb(0xF0, 0xF2, 0xF6)
}

fn ecran_versions(ctx: &egui::Context, v: &mut VersionsView, action: &mut Option<Action>, _next: &mut Option<Screen>) {
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        if let Some(e) = &v.error {
            ui.colored_label(th::ORANGE_FONCE, e);
            return;
        }
        let tout = ui.available_rect_before_wrap();
        let w = 320.0_f32.min(tout.width() * 0.42);
        let gauche = Rect::from_min_size(tout.min, Vec2::new(w, tout.height()));
        let droite = Rect::from_min_max(Pos2::new(tout.left() + w + 20.0, tout.top()), tout.max);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(gauche), |ui| {
            th::carte().inner_margin(egui::Margin::same(16.0)).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.label(RichText::new(t("Historique")).font(th::titre(15.0)).color(th::NUIT));
                ui.add_space(4.0);
                let bas = if v.infos.len() > 1 { 110.0 } else { 0.0 };
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - bas).show(ui, |ui| {
                    for (k, info) in v.infos.iter().enumerate() {
                        let actif = k == v.choisie;
                        let (r, rep) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 74.0), Sense::click());
                        let (fond, bord) = if actif { (th::BLEU_PALE, Stroke::new(2.0_f32, th::NUIT)) } else if rep.hovered() { (th::FOND, Stroke::new(1.0_f32, th::TRAIT)) } else { (th::BLANC, Stroke::new(1.0_f32, th::TRAIT)) };
                        ui.painter().rect(r.shrink(1.0), 10.0, fond, bord);
                        let c = Pos2::new(r.left() + 20.0, r.top() + 20.0);
                        ui.painter().circle(c, 6.0, th::BLANC, Stroke::new(3.0_f32, if k == 0 { th::TURQUOISE } else { th::NUIT }));
                        let titre = if k == 0 { tf("Version {} · actuelle", &[&info.numero]) } else { tf("Version {}", &[&info.numero]) };
                        ui.painter().text(Pos2::new(r.left() + 38.0, r.top() + 12.0), Align2::LEFT_TOP, titre, th::titre(14.0), th::NUIT);
                        ui.painter().text(Pos2::new(r.left() + 38.0, r.top() + 32.0), Align2::LEFT_TOP, date(info.date), th::texte(12.0), th::DOUX);
                        let resume = if info.numero == 1 {
                            tf("Création : {}", &[&count(info.ajoutes)])
                        } else {
                            let mut parts = Vec::new();
                            for (n, modele) in [(info.ajoutes, "{} ajouté(s)"), (info.modifies, "{} modifié(s)"), (info.supprimes, "{} supprimé(s)")] {
                                if n > 0 {
                                    parts.push(tf(modele, &[&n]));
                                }
                            }
                            if parts.is_empty() { t("Aucun fichier changé.").to_string() } else { parts.join(", ") }
                        };
                        ui.painter().text(Pos2::new(r.left() + 38.0, r.top() + 50.0), Align2::LEFT_TOP, resume, th::texte(12.0), th::ENCRE);
                        if rep.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                            v.choisie = k;
                        }
                        ui.add_space(6.0);
                    }
                });
                if v.infos.len() > 1 {
                    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                        let b = egui::Button::new(RichText::new(t("Compacter : ne garder que la dernière")).font(th::gras(13.0)).color(th::NUIT)).fill(th::BLANC).stroke(Stroke::new(1.5_f32, th::NUIT)).rounding(8.0).min_size(Vec2::new(ui.available_width(), 40.0)).wrap();
                        if ui.add(b).clicked() {
                            let (p, pw) = (v.path.clone(), v.password.clone());
                            *action = Some(Box::new(move |app: &mut App| app.start_compact(p, pw)));
                        }
                        ui.label(RichText::new(t("Supprime les anciennes versions pour libérer de la place.")).size(12.0).color(th::DOUX));
                    });
                }
            });
        });
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(droite), |ui| {
            th::carte().inner_margin(egui::Margin::same(0.0)).show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                let Some(info) = v.infos.get(v.choisie) else { return };
                egui::Frame::none().inner_margin(egui::Margin::symmetric(16.0, 14.0)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(RichText::new(tf("Ce qui a changé dans la version {}", &[&info.numero])).font(th::titre(15.0)).color(th::NUIT));
                            ui.label(RichText::new(t("par rapport à la version précédente")).size(12.0).color(th::DOUX));
                        });
                        if v.choisie > 0 {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if th::bouton_principal(ui, true, t("Restaurer cette version…")) {
                                    let stem = v.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                    let parent = v.path.parent().map(Path::to_path_buf).unwrap_or_default();
                                    let proposee = libre(parent.join(tf("{} (version {})", &[&stem, &info.numero])));
                                    let d = rfd::FileDialog::new().set_title(t("Dossier où extraire")).set_directory(&parent).set_file_name(proposee.file_name().unwrap_or_default().to_string_lossy());
                                    // On Linux, the portal picks an existing folder: the subfolder is created in it.
                                    if let Some(choisi) = d.pick_folder() {
                                        let dest = libre(choisi.join(proposee.file_name().unwrap_or_default()));
                                        let (p, n, pw) = (v.path.clone(), info.numero, v.password.clone());
                                        *action = Some(Box::new(move |app: &mut App| app.start_restore(p, n, dest, pw)));
                                    }
                                }
                            });
                        }
                    });
                });
                let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                ui.painter().rect_filled(r, 0.0, th::TRAIT);
                let bas = 44.0;
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - bas).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if info.changements.is_empty() {
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.label(RichText::new(t("Aucun fichier changé.")).color(th::PALE));
                        });
                    }
                    for (genre, chemin, taille) in &info.changements {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::hover());
                        ui.painter().line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0_f32, rvb_trait()));
                        let (txt, fond, encre) = match genre {
                            Genre::Ajoute => (t("Ajouté"), th::TURQUOISE_PALE, th::TURQUOISE_FONCE),
                            Genre::Modifie => (t("Modifié"), th::VIOLET_PALE, th::VIOLET),
                            Genre::Supprime => (t("Supprimé"), th::ORANGE_PALE, Color32::from_rgb(0x9A, 0x3D, 0x0A)),
                        };
                        let g = ui.painter().layout_no_wrap(txt.to_string(), th::gras(11.0), encre);
                        let p = Rect::from_min_size(Pos2::new(r.left() + 16.0, r.center().y - 11.0), Vec2::new(g.size().x.max(60.0) + 16.0, 22.0));
                        ui.painter().rect_filled(p, 5.0, fond);
                        ui.painter().galley(p.center() - g.size() / 2.0, g, encre);
                        let mut job = egui::text::LayoutJob::simple_singleline(chemin.clone(), th::texte(13.0), th::ENCRE);
                        job.wrap = egui::text::TextWrapping::truncate_at_width((r.right() - 100.0 - p.right() - 12.0).max(40.0));
                        let g = ui.fonts(|f| f.layout_job(job));
                        ui.painter().galley(Pos2::new(p.right() + 12.0, r.center().y - g.size().y / 2.0), g, th::ENCRE);
                        if *genre != Genre::Supprime {
                            ui.painter().text(Pos2::new(r.right() - 16.0, r.center().y), Align2::RIGHT_CENTER, human(*taille), th::texte(12.0), th::DOUX);
                        }
                    }
                });
                egui::Frame::none().fill(Color32::from_rgb(0xF9, 0xFA, 0xFC)).inner_margin(egui::Margin::symmetric(16.0, 12.0)).rounding(egui::Rounding { sw: 14.0, se: 14.0, ..Default::default() }).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(t("Restaurer extrait cette version dans un dossier à part : l'archive n'est jamais modifiée.")).size(12.0).color(th::DOUX));
                });
            });
        });
    });
}

fn ecran_reparer(ctx: &egui::Context, r: &mut RepairView, action: &mut Option<Action>, next: &mut Option<Screen>) {
    // End of the diagnosis or of the repair.
    if r.handle.as_ref().is_some_and(|h| h.is_finished()) {
        match r.handle.take().unwrap().join().unwrap_or_else(|_| Err(anyhow!(t("erreur interne")))) {
            Ok(rap) => {
                if r.rapport.is_some() {
                    r.reparee = true;
                    recentes::noter(&r.sortie, None, false);
                }
                r.rapport = Some(rap);
            }
            Err(e) => r.error = Some(format!("{e:#}")),
        }
    }
    let en_cours = r.handle.is_some();
    if en_cours {
        ctx.request_repaint_after(Duration::from_millis(150));
    }
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.08).max(6.0));
            ui.allocate_ui(Vec2::new(ui.available_width().min(660.0), 460.0), |ui| {
                th::carte().inner_margin(egui::Margin::same(26.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        let (titre, texte, ok) = texte_reparation(r, en_cours);
                        ui.horizontal(|ui| {
                            let (b, _) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                            let (fond, encre) = if ok { (th::TURQUOISE_PALE, th::TURQUOISE_FONCE) } else { (th::ORANGE_PALE, th::ORANGE_FONCE) };
                            ui.painter().rect_filled(b, 12.0, fond);
                            if en_cours {
                                egui::Spinner::new().size(22.0).color(encre).paint_at(ui, b.shrink(13.0));
                            } else {
                                th::icone(ui.painter(), b.shrink(12.0), if ok { Icone::Coche } else { Icone::Alerte }, encre);
                            }
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(titre).font(th::titre(20.0)).color(th::NUIT));
                                ui.add(egui::Label::new(RichText::new(texte).size(14.0).color(th::ENCRE)).wrap());
                            });
                        });
                        ui.add_space(14.0);
                        if let Some(rap) = &r.rapport {
                            if rap.has_recovery && rap.slices > 0 {
                                carte_des_tranches(ui, rap, r.reparee);
                                ui.add_space(14.0);
                            }
                        }
                        if let Some(raison) = &r.raison {
                            ui.label(RichText::new(raison).size(12.0).color(th::PALE));
                            ui.add_space(8.0);
                        }
                        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                            let rap = r.rapport.as_ref();
                            let reparable = rap.is_some_and(|x| x.has_recovery && x.damaged > 0 && x.repaired > 0);
                            if r.reparee {
                                if th::bouton_principal(ui, true, t("Ouvrir l'archive réparée")) {
                                    let p = r.sortie.clone();
                                    *action = Some(Box::new(move |app: &mut App| app.open(p)));
                                }
                            } else if reparable && !en_cours {
                                if th::bouton_principal(ui, true, t("Réparer l'archive")) {
                                    let (p, s, c) = (r.path.clone(), r.sortie.clone(), ui.ctx().clone());
                                    r.handle = Some(std::thread::spawn(move || {
                                        let _wake = Wake(c);
                                        cnz::recovery::repair(&p, Some(&s))
                                    }));
                                }
                            }
                            if !en_cours && th::lien(ui, t("Retour à l'accueil")).clicked() {
                                *next = Some(Screen::Welcome);
                            }
                            if !en_cours && !r.reparee && rap.is_some_and(|x| x.damaged == 0) && r.raison.is_none() && th::lien(ui, t("Ouvrir l'archive")).clicked() {
                                let p = r.path.clone();
                                *action = Some(Box::new(move |app: &mut App| app.open(p)));
                            }
                        });
                    });
                });
            });
        });
    });
}

/// Title, text and tone (reassuring or not) of the repair screen.
fn texte_reparation(r: &RepairView, en_cours: bool) -> (String, String, bool) {
    if let Some(e) = &r.error {
        return (t("Réparation impossible").into(), e.clone(), false);
    }
    if en_cours && r.rapport.is_none() {
        return (t("Recherche des parties abîmées…").into(), t("CNz relit l'archive morceau par morceau.").into(), true);
    }
    if en_cours {
        return (t("Réparation en cours…").into(), t("CNz reconstruit les parties abîmées. L'original n'est pas touché : la version réparée est écrite à côté.").into(), true);
    }
    let Some(rap) = &r.rapport else { return (String::new(), String::new(), true) };
    if r.reparee {
        return (t("Archive réparée").into(), tf("Les parties abîmées ont été reconstruites et vérifiées. Fichier créé : {}", &[&analyse::nom(&r.sortie)]), true);
    }
    if !rap.has_recovery {
        return (t("Réparation impossible").into(), t("Cette archive n'a pas de données de récupération : CNz ne peut pas la reconstruire.").into(), false);
    }
    if rap.damaged == 0 {
        return (t("Aucune partie abîmée").into(), tf("Les {} parties de l'archive sont intactes.", &[&rap.slices]), true);
    }
    if rap.lost == 0 {
        return (t("Cette archive est abîmée").into(), tf("{} parties sur {} sont illisibles. Ses données de récupération suffisent pour tout reconstruire.", &[&rap.damaged, &rap.slices]), false);
    }
    (
        t("Cette archive est abîmée").into(),
        tf("{} parties sur {} sont illisibles, dont {} impossibles à reconstruire. La réparation sauvera le reste.", &[&rap.damaged, &rap.slices, &rap.lost]),
        false,
    )
}

/// Map of the archive: one square per group of slices.
fn carte_des_tranches(ui: &mut egui::Ui, rap: &RepairReport, reparee: bool) {
    egui::Frame::none().fill(th::FOND).rounding(12.0).inner_margin(egui::Margin::same(16.0)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let n = rap.slices;
        let cases = n.min(96) as usize;
        let par_ligne = 24usize.min(cases.max(1));
        ui.label(RichText::new(tf("État des {} parties de l'archive", &[&n])).font(th::gras(12.0)).color(th::DOUX));
        ui.add_space(4.0);
        let lignes = cases.div_ceil(par_ligne);
        let ecart = 4.0;
        let w = (ui.available_width() - ecart * (par_ligne as f32 - 1.0)) / par_ligne as f32;
        let (zone, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), lignes as f32 * (14.0 + ecart)), Sense::hover());
        let dedans = |liste: &[u64], a: u64, b: u64| {
            let i = liste.partition_point(|&x| x < a);
            i < liste.len() && liste[i] < b
        };
        for k in 0..cases {
            let a = k as u64 * n / cases as u64;
            let b = ((k + 1) as u64 * n / cases as u64).max(a + 1);
            let couleur = if dedans(&rap.lost_slices, a, b) {
                th::ORANGE
            } else if dedans(&rap.damaged_slices, a, b) {
                if reparee { th::TURQUOISE } else { th::ORANGE }
            } else {
                th::NUIT
            };
            let (l, c) = (k / par_ligne, k % par_ligne);
            let r = Rect::from_min_size(zone.min + Vec2::new(c as f32 * (w + ecart), l as f32 * (14.0 + ecart)), Vec2::new(w, 14.0));
            ui.painter().rect_filled(r, 3.0, couleur);
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for (c, txt) in [(th::NUIT, t("Intacte")), (th::ORANGE, t("Abîmée")), (th::TURQUOISE, t("Reconstruite"))] {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                ui.painter().rect_filled(r, 3.0, c);
                ui.label(RichText::new(txt).size(12.0).color(th::ENCRE));
                ui.add_space(10.0);
            }
        });
    });
}

fn ecran_travail(ctx: &egui::Context, job: &mut Job, next: &mut Option<Screen>) {
    let (phase, done, total) = job.progress.snapshot();
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.22).max(10.0));
            ui.allocate_ui(Vec2::new(ui.available_width().min(520.0), 260.0), |ui| {
                th::carte().inner_margin(egui::Margin::same(26.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&job.title).font(th::titre(22.0)).color(th::NUIT));
                    ui.label(RichText::new(if phase.is_empty() { t("préparation…").to_string() } else { langue::phase(phase) }).color(th::DOUX));
                    ui.add_space(14.0);
                    let frac = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
                    th::barre(ui.painter(), r, frac, th::NUIT, th::TRAIT);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:.0} %", frac * 100.0)).font(th::gras(13.0)).color(th::NUIT));
                        if total > 0 {
                            ui.label(RichText::new(format!("{} / {}", human(done), human(total))).size(12.0).color(th::PALE));
                        }
                    });
                    ui.add_space(14.0);
                    let cancelling = job.progress.is_cancelled();
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                        let b = egui::Button::new(RichText::new(t(if cancelling { "Arrêt en cours…" } else { "Annuler" })).font(th::gras(14.0)).color(th::NUIT)).fill(th::BLANC).stroke(Stroke::new(1.5_f32, th::NUIT)).rounding(8.0).min_size(Vec2::new(130.0, 42.0));
                        if ui.add_enabled(!cancelling, b).clicked() {
                            job.progress.cancel();
                        }
                    });
                });
            });
        });
    });
    if job.handle.as_ref().is_some_and(|h| h.is_finished()) {
        let result = job.handle.take().unwrap().join().unwrap_or_else(|_| Err(anyhow!(t("erreur interne"))));
        *next = Some(Screen::Done(match result {
            Ok(fin) => fin,
            Err(e) if e.downcast_ref::<cnz::progress::Cancelled>().is_some() => Fin { ok: false, message: t("Opération annulée.").into(), ..Default::default() },
            Err(e) => Fin { ok: false, message: format!("{e:#}"), ..Default::default() },
        }));
    } else {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn ecran_fin(ctx: &egui::Context, fin: &mut Fin, action: &mut Option<Action>, next: &mut Option<Screen>) {
    egui::CentralPanel::default().frame(page()).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.14).max(10.0));
            ui.allocate_ui(Vec2::new(ui.available_width().min(600.0), 400.0), |ui| {
                th::carte().inner_margin(egui::Margin::same(26.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        ui.horizontal(|ui| {
                            th::rond_resultat(ui, fin.ok);
                            ui.add_space(6.0);
                            ui.label(RichText::new(t(if fin.ok { "C'est fait !" } else { "Pas terminé" })).font(th::titre(24.0)).color(th::NUIT));
                        });
                        ui.add_space(10.0);
                        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&fin.message).size(14.0).color(th::ENCRE)).wrap());
                        });
                        ui.add_space(18.0);
                        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 46.0), Layout::right_to_left(Align::Center), |ui| {
                            if let Some(d) = &fin.dossier {
                                if th::bouton_principal(ui, true, t("Ouvrir le dossier")) {
                                    let _ = ouvrir_avec_le_systeme(d);
                                }
                            }
                            if let Some(p) = &fin.reparer {
                                if th::bouton_principal(ui, true, t("Réparer l'archive")) {
                                    let p = p.clone();
                                    *action = Some(Box::new(move |app: &mut App| app.screen = Screen::Repair(RepairView::new(p, None, &app.ctx))));
                                }
                            } else if let Some(p) = &fin.archive {
                                let principal = fin.dossier.is_none();
                                let clic = if principal { th::bouton_principal(ui, true, t("Voir l'archive")) } else { th::bouton(ui, t("Voir l'archive")).clicked() };
                                if clic {
                                    let p = p.clone();
                                    *action = Some(Box::new(move |app: &mut App| app.open(p)));
                                }
                            }
                            if th::lien(ui, t("Retour à l'accueil")).clicked() {
                                *next = Some(Screen::Welcome);
                            }
                        });
                    });
                });
            });
            ui.add_space(10.0);
            ui.label(RichText::new(t("ou glissez un autre dossier, fichier ou archive")).color(th::PALE));
        });
    });
}
