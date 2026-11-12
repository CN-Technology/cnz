use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use zeroize::Zeroizing;

use cnz::index::Kind;
use cnz::meta;
use cnz::recovery;
use cnz::read::{format_time, show, Archive, ExtractOptions, Report};
use cnz::progress::{Cancelled, Progress};
use cnz::write::{append, compact, compress_many, CompactStats, Options};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "cnz", version, about = "CNz — compression et extraction d'archives .cnz (CN Technology)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Pas de barre de progression
    #[arg(long, global = true)]
    silencieux: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Crée une archive .cnz à partir d'un dossier, d'un fichier, ou de plusieurs. Pour un
    /// destinataire sans CNz, nommez l'archive en .zip, .7z, .tar.gz, .tar.xz, .tar.zst,
    /// .tar.bz2, .tar (ou .gz, .xz, .zst, .bz2 pour un seul fichier) : elle est créée dans ce format
    #[command(visible_alias = "c")]
    Compresser {
        archive: PathBuf,
        /// Un dossier (son contenu est mis à la racine), ou plusieurs fichiers et dossiers
        /// (chacun rangé sous son nom)
        #[arg(required = true)]
        sources: Vec<PathBuf>,
        /// Niveau de compression, de 1 (rapide) à 22 (maximum)
        #[arg(short, long, default_value_t = 19, value_parser = clap::value_parser!(i32).range(1..=22))]
        niveau: i32,
        /// Taille des blocs en Mo (par défaut : automatique, selon le volume et les cœurs)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1024))]
        bloc_mo: Option<u64>,
        /// Mémoire maximale à utiliser, en Mo (par défaut : la moitié de la mémoire libre)
        #[arg(long, value_parser = clap::value_parser!(u64).range(64..))]
        memoire_max: Option<u64>,
        /// Affiche la durée de chaque étape et le nombre de cœurs utilisés
        #[arg(long)]
        chrono: bool,
        /// Chiffre l'archive (contenu et noms de fichiers) avec un mot de passe
        #[arg(short = 'p', long)]
        chiffrer: bool,
        /// Données de récupération en % (0 à 50, 0 = aucune) : permettent de réparer l'archive
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=50))]
        recuperation: u32,
        /// Garantit la reconstruction de N blocs entiers perdus (calcule le % nécessaire)
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=100))]
        recuperation_blocs: u32,
        /// Stocke les photos JPEG telles quelles, sans les recompresser (~20 % de place en plus
        /// pour les photos, mais compression et extraction bien plus rapides)
        #[arg(long)]
        photos_telles_quelles: bool,
        /// Mode maximum : moteur à mélange de contextes de CNz, archive 15 à 30 % plus petite
        /// sur le texte, le code et les données, mais compression et extraction bien plus lentes
        /// (environ 1 Mo/s par cœur)
        #[arg(long)]
        maximum: bool,
        /// Mode IA (prototype) : mode maximum aidé par un expert d'IA installé (fichiers .cnzia
        /// dans $CNZ_MODELES ou modeles/ à côté du programme) ; quelques Ko par seconde
        #[arg(long)]
        ia: bool,
    },
    /// Ajoute (ou remplace) des fichiers dans une archive existante, sans la recompresser
    #[command(visible_alias = "a")]
    Ajouter {
        archive: PathBuf,
        source: PathBuf,
        /// Niveau de compression, de 1 (rapide) à 22 (maximum)
        #[arg(short, long, default_value_t = 19, value_parser = clap::value_parser!(i32).range(1..=22))]
        niveau: i32,
        /// Taille des blocs en Mo (par défaut : automatique, selon le volume et les cœurs)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1024))]
        bloc_mo: Option<u64>,
        /// Mémoire maximale à utiliser, en Mo (par défaut : la moitié de la mémoire libre)
        #[arg(long, value_parser = clap::value_parser!(u64).range(64..))]
        memoire_max: Option<u64>,
        /// Données de récupération en % (0 à 50, 0 = aucune)
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=50))]
        recuperation: u32,
        /// Garantit la reconstruction de N blocs entiers perdus
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=100))]
        recuperation_blocs: u32,
        /// Réécrire l'archive sans les anciennes versions (sinon elles restent récupérables)
        #[arg(long)]
        compacter: bool,
        /// Stocke les photos JPEG telles quelles, sans les recompresser
        #[arg(long)]
        photos_telles_quelles: bool,
        /// Mode maximum pour les fichiers ajoutés (voir « cnz c --maximum »)
        #[arg(long)]
        maximum: bool,
        /// Mode IA pour les fichiers ajoutés (voir « cnz c --ia »)
        #[arg(long)]
        ia: bool,
    },
    /// Réécrit l'archive en ne gardant que la dernière version de chaque fichier
    Compacter {
        archive: PathBuf,
        /// Niveau de compression des blocs à recompresser
        #[arg(short, long, default_value_t = 19, value_parser = clap::value_parser!(i32).range(1..=22))]
        niveau: i32,
        /// Données de récupération en % (0 à 50, 0 = aucune)
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=50))]
        recuperation: u32,
        /// Garantit la reconstruction de N blocs entiers perdus
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=100))]
        recuperation_blocs: u32,
    },
    /// Extrait une archive .cnz, ou une archive d'un autre programme (ZIP, 7z, RAR, tar, gz, bz2, xz, lzma, lz, zst)
    #[command(visible_alias = "x")]
    Extraire {
        archive: PathBuf,
        #[arg(default_value = ".")]
        destination: PathBuf,
        /// Restaurer le propriétaire des fichiers (automatique en administrateur)
        #[arg(long)]
        proprietaires: bool,
        /// Extraire une version antérieure (voir « cnz lister --versions »)
        #[arg(long)]
        version: Option<u64>,
        /// Mémoire maximale à utiliser, en Mo (par défaut : la moitié de la mémoire libre)
        #[arg(long, value_parser = clap::value_parser!(u64).range(64..))]
        memoire_max: Option<u64>,
        /// Affiche la durée de chaque étape et le nombre de cœurs utilisés
        #[arg(long)]
        chrono: bool,
    },
    /// Liste le contenu d'une archive (.cnz, ZIP, 7z, tar…) sans rien extraire
    #[command(visible_alias = "l")]
    Lister {
        archive: PathBuf,
        /// Liste les versions de l'archive au lieu de son contenu
        #[arg(long)]
        versions: bool,
        /// Contenu d'une version antérieure
        #[arg(long)]
        version: Option<u64>,
    },
    /// Vérifie l'intégrité d'une archive (.cnz, ZIP, 7z, tar…) sans rien extraire
    #[command(visible_alias = "t")]
    Tester {
        archive: PathBuf,
        /// Vérifier une version antérieure
        #[arg(long)]
        version: Option<u64>,
    },
    /// Répare une archive abîmée grâce à ses données de récupération
    #[command(visible_alias = "r")]
    Reparer {
        archive: PathBuf,
        /// Archive réparée à créer (par défaut : nom.repare.cnz) ; l'originale n'est jamais modifiée
        sortie: Option<PathBuf>,
    },
    /// Ajoute (ou retire) CNz au clic droit de l'explorateur de fichiers, pour l'utilisateur
    /// (Windows, Linux : Nautilus, Dolphin, Nemo)
    Menu {
        #[arg(value_enum)]
        action: MenuAction,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum MenuAction {
    /// « Compresser avec CNz », « Compresser en ZIP », « Extraire ici » (.cnz, .zip, .7z, .rar…) et ouverture des archives
    Installer,
    /// Retire ces entrées
    Desinstaller,
    /// Ouvrir les archives (.zip, .7z, .rar…) avec CNz par défaut : sous Windows, ouvre le réglage
    /// « Applications par défaut » (Windows réserve ce choix à l'utilisateur)
    ParDefaut,
}

/// Environment variable for scripts. Less safe than typing on the keyboard:
/// other programs in the same session can read it.
const PASSWORD_ENV: &str = "CNZ_MOT_DE_PASSE";

fn ask_password() -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var(PASSWORD_ENV) {
        return Ok(Zeroizing::new(p));
    }
    Ok(Zeroizing::new(rpassword::prompt_password("Mot de passe : ")?))
}

fn new_password() -> Result<Zeroizing<String>> {
    if let Ok(p) = std::env::var(PASSWORD_ENV) {
        return Ok(Zeroizing::new(p));
    }
    let first = Zeroizing::new(rpassword::prompt_password("Mot de passe : ")?);
    let second = Zeroizing::new(rpassword::prompt_password("Confirmez le mot de passe : ")?);
    if first != second {
        anyhow::bail!("les deux mots de passe sont différents");
    }
    Ok(first)
}

/// Password asked only once, even if the archive is opened several times.
fn cached_password() -> impl FnMut() -> Result<Zeroizing<String>> {
    let mut cache: Option<Zeroizing<String>> = None;
    move || {
        if cache.is_none() {
            cache = Some(ask_password()?);
        }
        Ok(cache.clone().unwrap())
    }
}

fn print_compact(c: &CompactStats) {
    println!(
        "compactage : {} version(s) → 1 ; {} bloc(s) recopié(s), {} recompressé(s), {} supprimé(s) — archive {} → {}",
        c.versions,
        c.copied,
        c.repacked,
        c.dropped,
        mo(c.before),
        mo(c.after)
    );
}

/// After a failed test: tells whether the archive can be repaired.
fn diagnose(archive: &std::path::Path) -> Result<()> {
    let r = recovery::repair(archive, None)?;
    if !r.has_recovery {
        eprintln!("pas de données de récupération : l'archive ne peut pas être réparée.");
    } else if r.damaged == 0 {
        eprintln!("les données de récupération ne trouvent aucune tranche abîmée.");
    } else if r.lost == 0 {
        eprintln!("{} tranche(s) abîmée(s), toutes réparables : lancez « cnz reparer {} ».", r.damaged, archive.display());
    } else {
        eprintln!("{} tranche(s) abîmée(s), dont {} irréparable(s) : « cnz reparer » sauvera le reste.", r.damaged, r.lost);
    }
    Ok(())
}

fn repaired_name(archive: &std::path::Path) -> PathBuf {
    let stem = archive.file_stem().map_or("archive".into(), |s| s.to_string_lossy().into_owned());
    archive.with_file_name(format!("{stem}.repare.cnz"))
}

fn print_steps(steps: &[cnz::timing::Step]) {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("\n{:<40} {:>8} {:>10} {:>12}", "étape", "temps", "calcul", "cœurs actifs");
    for s in steps {
        let active = if s.wall > 0.0 { s.cpu / s.wall } else { 0.0 };
        println!("{:<40} {:>7.2}s {:>9.2}s {:>8.1} / {cores}", s.name, s.wall, s.cpu, active);
    }
}

fn mo(bytes: u64) -> String {
    format!("{:.2} Mo", bytes as f64 / 1e6)
}

fn print_report(notes: &[String], report: &Report, action: &str, seconds: f64) -> ExitCode {
    for note in notes {
        eprintln!("attention : {note}");
    }
    for (from, to) in &report.renamed {
        eprintln!("renommé : {from} → {to}");
    }
    println!(
        "{action} : {} fichiers ({}), {} dossiers, {} liens en {seconds:.1} s",
        report.files,
        mo(report.bytes),
        report.dirs,
        report.links
    );
    if report.problems.is_empty() {
        println!("tout est intact.");
        ExitCode::SUCCESS
    } else {
        eprintln!("{} problème(s) :", report.problems.len());
        for p in &report.problems {
            eprintln!("  - {p}");
        }
        ExitCode::from(2)
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let start = Instant::now();
    let progress = Arc::new(Progress::default());
    install_ctrl_c(&progress);
    let bar = !cli.silencieux && std::io::stderr().is_terminal();
    let busy = |f: &mut dyn FnMut() -> Result<()>| with_progress(&progress, bar, f);
    match cli.command {
        Command::Compresser {
            archive,
            sources,
            niveau,
            bloc_mo,
            memoire_max,
            chrono,
            chiffrer,
            recuperation,
            recuperation_blocs,
            photos_telles_quelles,
            maximum,
            ia,
        } => {
            if let Some(format) = cnz::creer::FormatCree::d_apres_nom(&archive) {
                if maximum || ia {
                    eprintln!("note : --maximum et --ia ne valent que pour les archives .cnz");
                }
                if chiffrer && !format.chiffrable() {
                    anyhow::bail!("le format .{} ne sait pas protéger par mot de passe : choisissez .zip, .7z ou .cnz", format.extension());
                }
                let password = if chiffrer { Some(new_password()?) } else { None };
                let mut bilan = Default::default();
                let done = busy(&mut || {
                    bilan = cnz::creer::creer(&archive, &sources, password.as_ref().map(|p| p.as_str()), &progress)?;
                    Ok(())
                });
                if let Err(e) = &done {
                    if e.downcast_ref::<Cancelled>().is_some() {
                        eprintln!("compression interrompue à votre demande : rien n'a été écrit.");
                        return Ok(ExitCode::from(130));
                    }
                }
                done?;
                let b: cnz::creer::Bilan = bilan;
                for s in &b.ignores {
                    eprintln!("ignoré : {s}");
                }
                let ratio = if b.brut > 0 { 100.0 * b.archive as f64 / b.brut as f64 } else { 0.0 };
                println!(
                    "archive .{} créée : {} fichiers, {} dossiers, {} liens ; {} → {} ({ratio:.1} %) en {:.1} s",
                    format.extension(),
                    b.fichiers,
                    b.dossiers,
                    b.liens,
                    mo(b.brut),
                    mo(b.archive),
                    start.elapsed().as_secs_f64()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let password = if chiffrer { Some(new_password()?) } else { None };
            let opts = Options {
                level: niveau,
                block_size: bloc_mo.map(|m| m << 20),
                max_memory: memoire_max.map(|m| m << 20),
                password,
                recovery_percent: recuperation,
                recovery_blocks: recuperation_blocs,
                photos: !photos_telles_quelles,
                maximum,
                ia,
                progress: progress.clone(),
            };
            let mut stats = Default::default();
            busy(&mut || {
                stats = compress_many(&sources, &archive, &opts)?;
                Ok(())
            })?;
            for s in &stats.skipped {
                eprintln!("ignoré : {s}");
            }
            let ratio = if stats.raw_bytes > 0 { 100.0 * stats.archive_bytes as f64 / stats.raw_bytes as f64 } else { 0.0 };
            println!(
                "{} : {} fichiers, {} dossiers, {} liens — {} → {} ({ratio:.1} %), {} blocs, {:.1} s",
                archive.display(),
                stats.files,
                stats.dirs,
                stats.links,
                mo(stats.raw_bytes),
                mo(stats.archive_bytes),
                stats.blocks,
                start.elapsed().as_secs_f64()
            );
            if let Some(p) = stats.plan {
                println!("blocs de {} Mo, {} cœur(s) au travail", p.block_size >> 20, p.workers);
            }
            if stats.duplicate_bytes > 0 {
                println!(
                    "doublons : {} fichier(s) identique(s) et morceaux répétés, {} stockés une seule fois",
                    stats.duplicate_files,
                    mo(stats.duplicate_bytes)
                );
            }
            if stats.rebuilt_files > 0 {
                println!(
                    "flux Deflate refaits à l'identique : {} fichier(s) (PDF, ZIP, Office, .gz), {}",
                    stats.rebuilt_files,
                    mo(stats.rebuilt_bytes)
                );
            }
            if stats.photos > 0 {
                println!("photos JPEG recompressées sans perte : {} photo(s), {}", stats.photos, mo(stats.photo_bytes));
            }
            if stats.recovery_bytes > 0 {
                let base = stats.archive_bytes - stats.recovery_bytes;
                print!(
                    "récupération : {} ({:.1} %), répare jusqu'à ~{}",
                    mo(stats.recovery_bytes),
                    100.0 * stats.recovery_bytes as f64 / base as f64,
                    mo(stats.recovery_bytes)
                );
                if recuperation_blocs > 0 {
                    print!(", garantit {recuperation_blocs} bloc(s) entier(s) (le plus gros : {})", mo(stats.largest_block));
                }
                println!();
            }
            if chrono {
                print_steps(&stats.steps);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Ajouter {
            archive,
            source,
            niveau,
            bloc_mo,
            memoire_max,
            recuperation,
            recuperation_blocs,
            compacter,
            photos_telles_quelles,
            maximum,
            ia,
        } => {
            let before = std::fs::metadata(&archive)?.len();
            let mut password = cached_password();
            let opts = Options {
                level: niveau,
                block_size: bloc_mo.map(|m| m << 20),
                max_memory: memoire_max.map(|m| m << 20),
                password: None,
                recovery_percent: recuperation,
                recovery_blocks: recuperation_blocs,
                photos: !photos_telles_quelles,
                maximum,
                ia,
                progress: progress.clone(),
            };
            let mut stats = Default::default();
            busy(&mut || {
                stats = append(&archive, &source, &opts, &mut password)?;
                Ok(())
            })?;
            for s in &stats.skipped {
                eprintln!("ignoré : {s}");
            }
            println!(
                "{} : {} fichier(s) ajouté(s) ({}), {} entrée(s) remplacée(s) — archive {} → {}, {:.1} s",
                archive.display(),
                stats.files,
                mo(stats.raw_bytes),
                stats.replaced,
                mo(before),
                mo(stats.archive_bytes),
                start.elapsed().as_secs_f64()
            );
            if compacter {
                let mut c = Default::default();
                busy(&mut || {
                    c = compact(&archive, &opts, &mut password)?;
                    Ok(())
                })?;
                print_compact(&c);
            } else if stats.replaced > 0 {
                println!("les anciennes versions restent récupérables (« cnz lister --versions ») ; « cnz compacter » les supprime.");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Compacter { archive, niveau, recuperation, recuperation_blocs } => {
            let opts = Options {
                level: niveau,
                block_size: None,
                max_memory: None,
                password: None,
                recovery_percent: recuperation,
                recovery_blocks: recuperation_blocs,
                photos: true,
                maximum: false,
                ia: false,
                progress: progress.clone(),
            };
            let mut c = Default::default();
            busy(&mut || {
                c = compact(&archive, &opts, &mut ask_password)?;
                Ok(())
            })?;
            print_compact(&c);
            println!("({:.1} s)", start.elapsed().as_secs_f64());
            Ok(ExitCode::SUCCESS)
        }
        Command::Extraire { archive, destination, proprietaires, version, memoire_max, chrono } => {
            if let Some(format) = cnz::autres::reconnaitre(&archive)? {
                eprintln!("archive {} : extraction par CNz", format.nom());
                let mut report = Default::default();
                let done = busy(&mut || {
                    report = cnz::autres::extraire(&archive, &destination, &progress, &mut ask_password)?;
                    Ok(())
                });
                if let Err(e) = &done {
                    if e.downcast_ref::<Cancelled>().is_some() {
                        eprintln!("extraction interrompue à votre demande : les fichiers déjà extraits restent en place.");
                        return Ok(ExitCode::from(130));
                    }
                }
                done?;
                return Ok(print_report(&[], &report, "extrait", start.elapsed().as_secs_f64()));
            }
            let mut a = Archive::open_with_password(&archive, &mut ask_password)?;
            if let Some(v) = version {
                a.select_version(v)?;
            }
            let opts = ExtractOptions {
                restore_owner: proprietaires || cnz::meta::is_admin(),
                max_memory: memoire_max.map(|m| m << 20),
                progress: progress.clone(),
            };
            let mut report = Default::default();
            let done = busy(&mut || {
                report = a.extract(Some(&destination), &opts)?;
                Ok(())
            });
            if let Err(e) = &done {
                if e.downcast_ref::<Cancelled>().is_some() {
                    eprintln!("extraction interrompue à votre demande : les fichiers déjà extraits restent en place.");
                    return Ok(ExitCode::from(130));
                }
            }
            done?;
            if chrono {
                print_steps(&report.steps);
            }
            Ok(print_report(&a.notes, &report, "extrait", start.elapsed().as_secs_f64()))
        }
        Command::Tester { archive, version } => {
            if cnz::autres::reconnaitre(&archive)?.is_some() {
                let mut report = Default::default();
                busy(&mut || {
                    report = cnz::autres::verifier(&archive, &progress, &mut ask_password)?;
                    Ok(())
                })?;
                return Ok(print_report(&[], &report, "vérifié", start.elapsed().as_secs_f64()));
            }
            let code = match Archive::open_with_password(&archive, &mut ask_password) {
                Ok(mut a) => {
                    if let Some(v) = version {
                        a.select_version(v)?;
                    }
                    let opts = ExtractOptions { progress: progress.clone(), ..Default::default() };
                    let mut report = Default::default();
                    busy(&mut || {
                        report = a.extract(None, &opts)?;
                        Ok(())
                    })?;
                    print_report(&a.notes, &report, "vérifié", start.elapsed().as_secs_f64())
                }
                Err(e) => {
                    eprintln!("erreur : {e:#}");
                    ExitCode::FAILURE
                }
            };
            if code != ExitCode::SUCCESS {
                diagnose(&archive)?;
            }
            Ok(code)
        }
        Command::Menu { action } => {
            let done = match action {
                MenuAction::Installer => cnz::menu::install()?,
                MenuAction::Desinstaller => cnz::menu::uninstall()?,
                MenuAction::ParDefaut => cnz::menu::set_default()?,
            };
            if done.is_empty() {
                println!("rien à retirer");
            }
            for d in done {
                println!("{d}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Reparer { archive, sortie } => {
            let out = sortie.unwrap_or_else(|| repaired_name(&archive));
            let check = recovery::repair(&archive, None)?;
            if !check.has_recovery {
                eprintln!("cette archive ne contient pas de données de récupération lisibles : réparation impossible");
                return Ok(ExitCode::FAILURE);
            }
            if check.damaged == 0 {
                println!("{} tranches vérifiées : aucune n'est abîmée, rien à réparer.", check.slices);
                return Ok(ExitCode::SUCCESS);
            }
            let r = recovery::repair(&archive, Some(&out))?;
            println!("{} tranche(s) abîmée(s) sur {}, {} reconstruite(s).", r.damaged, r.slices, r.repaired);
            if r.lost > 0 {
                eprintln!("{} tranche(s) irrécupérable(s) : trop de dégâts pour les données de récupération.", r.lost);
            }
            println!("archive réparée : {}", out.display());
            Ok(if r.lost == 0 { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
        Command::Lister { archive, versions, version } => {
            if cnz::autres::reconnaitre(&archive)?.is_some() {
                let mut out = std::io::stdout().lock();
                for e in cnz::autres::lister(&archive, &mut ask_password)? {
                    let kind = if e.dossier { "dossier" } else if e.lien { "lien" } else { "fichier" };
                    let size = e.taille.filter(|_| !e.dossier).map(|s| s.to_string()).unwrap_or_default();
                    if writeln!(out, "{kind:<8} {size:>12}  {}", e.chemin).is_err() {
                        break;
                    }
                }
                return Ok(ExitCode::SUCCESS);
            }
            let mut a = Archive::open_with_password(&archive, &mut ask_password)?;
            if versions {
                for l in a.versions() {
                    let files = l.entries.iter().filter(|e| matches!(e.kind, Kind::File { .. })).count();
                    println!(
                        "version {:<3} {}  {} entrée(s) dont {} fichier(s), {} remplacée(s), {} bloc(s)",
                        l.number,
                        format_time(l.created as i64),
                        l.entries.len(),
                        files,
                        l.removed.len(),
                        l.blocks.len()
                    );
                }
                return Ok(ExitCode::SUCCESS);
            }
            if let Some(v) = version {
                a.select_version(v)?;
            }
            for note in &a.notes {
                eprintln!("attention : {note}");
            }
            let mut out = std::io::stdout().lock();
            for e in &a.index.entries {
                let owner = match (meta::get_bytes(&e.fields, meta::USER), meta::get_bytes(&e.fields, meta::GROUP)) {
                    (Some(u), Some(g)) => format!("{}:{}", show(u), show(g)),
                    _ => String::new(),
                };
                let (kind, size, extra) = match &e.kind {
                    Kind::File { size, .. } => ("fichier", size.to_string(), String::new()),
                    Kind::Dir => ("dossier", String::new(), String::new()),
                    Kind::Symlink { target } => ("lien", String::new(), format!(" -> {}", show(target))),
                };
                // Output cut off (for example "cnz l … | head"): stop without an error.
                if writeln!(out, "{kind:<8} {owner:<18} {size:>12}  {}  {}{extra}", format_time(e.mtime_s), show(&e.path)).is_err() {
                    break;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => {
            eprintln!("interrompu à votre demande : aucune archive n'a été abîmée.");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("erreur : {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// First Ctrl+C: clean stop in under a second (CNz tidies everything up).
/// Second Ctrl+C: immediate stop.
fn install_ctrl_c(progress: &Arc<Progress>) {
    let progress = progress.clone();
    let _ = ctrlc::set_handler(move || {
        if progress.cancel() {
            eprintln!("\narrêt immédiat.");
            std::process::exit(130);
        }
        eprintln!("\narrêt demandé : CNz s’arrête et remet tout en ordre… (Ctrl+C à nouveau pour forcer)");
    });
}

/// Runs `f` while showing a progress bar (if `show`), updated ten times per
/// second from the engine's counters.
fn with_progress(progress: &Arc<Progress>, show: bool, f: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    if !show {
        return f();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let display = {
        let (progress, stop) = (progress.clone(), stop.clone());
        std::thread::spawn(move || {
            let style = ProgressStyle::with_template("{msg:<13} [{bar:32}] {percent:>3} %  {prefix}  reste {eta}")
                .unwrap()
                .progress_chars("=> ");
            let mut bar: Option<ProgressBar> = None;
            let shown_from = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !stop.load(Ordering::Relaxed) {
                let (phase, done, total) = progress.snapshot();
                // The bar only appears once the work has started (not while a password is
                // being typed), and only if the operation lasts more than half a second.
                if !phase.is_empty() && total > 0 && std::time::Instant::now() >= shown_from {
                    let b = bar.get_or_insert_with(|| ProgressBar::new(total).with_style(style.clone()));
                    if b.message() != phase {
                        b.set_message(phase);
                        b.reset();
                    }
                    b.set_length(total);
                    b.set_position(done);
                    b.set_prefix(format!("{} / {}", mo(done), mo(total)));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if let Some(b) = bar {
                b.finish_and_clear();
            }
        })
    };
    let result = f();
    stop.store(true, Ordering::Relaxed);
    let _ = display.join();
    result
}
