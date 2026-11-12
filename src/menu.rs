//! Right-click in the file explorer: "Compress with CNz" on files and
//! folders, "Extract here" on .cnz archives and on those of other programs
//! (ZIP, 7z, tar…, see `autres.rs`), and opening of .cnz files with the
//! graphical application (cnz-fenetre). For other formats, CNz adds itself
//! to the menu without becoming the default program.
//!
//! Everything is installed for the current user only, without administrator
//! rights: user registry on Windows; file type, launcher and Nautilus,
//! Dolphin and Nemo entries on Linux.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Archives of other programs offered in the right-click menu.
#[cfg(feature = "rar")]
const AUTRES: [&str; 14] = ["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "lzma", "lz", "zst", "tzst"];
#[cfg(not(feature = "rar"))]
const AUTRES: [&str; 13] = ["zip", "7z", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "lzma", "lz", "zst", "tzst"];

/// Location of the graphical application: next to this program.
pub fn window_program() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("emplacement de CNz introuvable")?;
    let name = if cfg!(windows) { "cnz-fenetre.exe" } else { "cnz-fenetre" };
    let p = exe.parent().context("emplacement de CNz introuvable")?.join(name);
    if !p.is_file() {
        bail!("{} introuvable : l'application graphique doit être installée à côté de cnz", p.display());
    }
    Ok(p)
}

/// Installs the entries; returns what was done.
pub fn install() -> Result<Vec<String>> {
    let gui = window_program()?;
    platform::install(&gui)
}

/// Removes the entries; returns what was done.
pub fn uninstall() -> Result<Vec<String>> {
    platform::uninstall()
}

/// CNz as the default program for other programs' archives: Windows forbids
/// doing it on the user's behalf, so the "Default apps" setting is opened on
/// CNz; on Linux, it is done (the user asked for it).
pub fn set_default() -> Result<Vec<String>> {
    platform::set_default()
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::process::Command;

    const CLASSES: &str = r"HKCU\Software\Classes";

    fn reg(args: &[&str]) -> Result<()> {
        let out = Command::new("reg").args(args).output().context("reg.exe introuvable")?;
        if !out.status.success() {
            bail!("reg {} : {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn set(key: &str, name: Option<&str>, value: &str) -> Result<()> {
        let key = format!(r"{CLASSES}\{key}");
        match name {
            None => reg(&["add", &key, "/ve", "/d", value, "/f"]),
            Some(n) => reg(&["add", &key, "/v", n, "/d", value, "/f"]),
        }
    }

    pub fn install(gui: &Path) -> Result<Vec<String>> {
        let g = gui.display().to_string();
        let open = format!("\"{g}\" \"%1\"");
        for target in [r"*\shell\CNZ.Compresser", r"Directory\shell\CNZ.Compresser"] {
            set(target, None, "Compresser avec CNz")?;
            set(target, Some("Icon"), &g)?;
            set(&format!(r"{target}\command"), None, &open)?;
        }
        // One-click ZIP, to send it to someone who does not have CNz.
        for target in [r"*\shell\CNZ.Zip", r"Directory\shell\CNZ.Zip"] {
            set(target, None, "Compresser en ZIP (CNz)")?;
            set(target, Some("Icon"), &g)?;
            set(&format!(r"{target}\command"), None, &format!("\"{g}\" --zip \"%1\""))?;
        }
        set(".cnz", None, "CNZ.Archive")?;
        set("CNZ.Archive", None, "Archive CNz")?;
        set(r"CNZ.Archive\DefaultIcon", None, &format!("{g},0"))?;
        set(r"CNZ.Archive\shell\open\command", None, &open)?;
        set(r"CNZ.Archive\shell\CNZ.Extraire", None, "Extraire ici")?;
        set(r"CNZ.Archive\shell\CNZ.Extraire\command", None, &format!("\"{g}\" --extraire-ici \"%1\""))?;
        // Other formats: verbs added to the file type, without touching the program
        // that opens them by default.
        for ext in AUTRES {
            let base = format!(r"SystemFileAssociations\.{ext}\shell");
            set(&format!(r"{base}\CNZ.Extraire"), None, "Extraire ici avec CNz")?;
            set(&format!(r"{base}\CNZ.Extraire"), Some("Icon"), &g)?;
            set(&format!(r"{base}\CNZ.Extraire\command"), None, &format!("\"{g}\" --extraire-ici \"%1\""))?;
            set(&format!(r"{base}\CNZ.Ouvrir"), None, "Ouvrir avec CNz")?;
            set(&format!(r"{base}\CNZ.Ouvrir"), Some("Icon"), &g)?;
            set(&format!(r"{base}\CNZ.Ouvrir\command"), None, &open)?;
        }
        // CNz as a candidate to open other archives ("Open with", and the "Default
        // apps" setting); the choice stays with the user.
        set("CNZ.AutreArchive", None, "Archive (CNz)")?;
        set(r"CNZ.AutreArchive\DefaultIcon", None, &format!("{g},0"))?;
        set(r"CNZ.AutreArchive\shell\open\command", None, &open)?;
        let caps = r"HKCU\Software\CN Technology\CNZ\Capabilities";
        reg(&["add", caps, "/v", "ApplicationName", "/d", "CNz", "/f"])?;
        reg(&["add", caps, "/v", "ApplicationDescription", "/d", "Compresser et extraire des archives (.cnz, ZIP, 7z, RAR, tar…)", "/f"])?;
        reg(&["add", &format!(r"{caps}\FileAssociations"), "/v", ".cnz", "/d", "CNZ.Archive", "/f"])?;
        for ext in AUTRES {
            reg(&["add", &format!(r"{CLASSES}\.{ext}\OpenWithProgids"), "/v", "CNZ.AutreArchive", "/t", "REG_NONE", "/f"])?;
            reg(&["add", &format!(r"{caps}\FileAssociations"), "/v", &format!(".{ext}"), "/d", "CNZ.AutreArchive", "/f"])?;
        }
        reg(&["add", r"HKCU\Software\RegisteredApplications", "/v", "CNZ", "/d", r"Software\CN Technology\CNZ\Capabilities", "/f"])?;
        Ok(vec![
            "« Compresser avec CNz » et « Compresser en ZIP (CNz) » sur les fichiers et les dossiers".into(),
            "« Extraire ici » sur les archives .cnz".into(),
            "archives .cnz ouvertes par CNz (double-clic)".into(),
            format!("« Extraire ici avec CNz » et « Ouvrir avec CNz » sur .{}", AUTRES.join(", .")),
            "CNz proposé dans « Ouvrir avec » et dans « Applications par défaut » (cnz menu par-defaut)".into(),
        ])
    }

    pub fn uninstall() -> Result<Vec<String>> {
        let mut done = Vec::new();
        let mut keys: Vec<String> =
            [r"*\shell\CNZ.Compresser", r"Directory\shell\CNZ.Compresser", r"*\shell\CNZ.Zip", r"Directory\shell\CNZ.Zip", "CNZ.Archive", ".cnz"]
                .map(String::from)
                .to_vec();
        for ext in AUTRES {
            keys.push(format!(r"SystemFileAssociations\.{ext}\shell\CNZ.Extraire"));
            keys.push(format!(r"SystemFileAssociations\.{ext}\shell\CNZ.Ouvrir"));
        }
        keys.push("CNZ.AutreArchive".into());
        for key in keys {
            if reg(&["delete", &format!(r"{CLASSES}\{key}"), "/f"]).is_ok() {
                done.push(format!("retiré : {key}"));
            }
        }
        for ext in AUTRES {
            let _ = reg(&["delete", &format!(r"{CLASSES}\.{ext}\OpenWithProgids"), "/v", "CNZ.AutreArchive", "/f"]);
        }
        if reg(&["delete", r"HKCU\Software\RegisteredApplications", "/v", "CNZ", "/f"]).is_ok() {
            done.push("retiré : CNz des applications par défaut".into());
        }
        let _ = reg(&["delete", r"HKCU\Software\CN Technology\CNZ", "/f"]);
        Ok(done)
    }

    pub fn set_default() -> Result<Vec<String>> {
        // Explorer opens "ms-settings:" addresses (Windows settings).
        Command::new("explorer").arg("ms-settings:defaultapps?registeredAppUser=CNZ").spawn().context("réglages de Windows introuvables")?;
        Ok(vec!["réglage « Applications par défaut » ouvert sur CNz : choisissez CNz pour les types voulus (.zip, .7z, .rar…)".into()])
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn data_home() -> Result<PathBuf> {
        if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(d));
        }
        let home = std::env::var_os("HOME").context("dossier personnel inconnu (HOME)")?;
        Ok(PathBuf::from(home).join(".local/share"))
    }

    /// Quoted path for an Exec line (freedesktop specification).
    fn quoted(p: &Path) -> String {
        let s = p.display().to_string();
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`").replace('$', "\\$"))
    }

    /// Single-quoted path for a shell script.
    fn sh_quoted(p: &Path) -> String {
        format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
    }

    fn write(path: &Path, content: &str, executable: bool) -> Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, content).with_context(|| format!("{}", path.display()))?;
        if executable {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    /// Types of other programs' archives: CNz appears in "Open with", without
    /// becoming the default program.
    const AUTRES_MIME: &str = "application/zip;application/x-7z-compressed;application/x-tar;application/gzip;\
        application/x-compressed-tar;application/x-bzip2;application/x-bzip-compressed-tar;application/x-xz;\
        application/x-xz-compressed-tar;application/x-lzma;application/x-lzip;application/zstd;application/x-zstd-compressed-tar;\
        application/vnd.rar;application/x-rar;";

    /// Installed files (relative to the user's data folder).
    const FILES: [&str; 10] = [
        "mime/packages/cnz.xml",
        "applications/cnz-fenetre.desktop",
        "nautilus/scripts/Compresser avec CNz",
        "nautilus/scripts/Extraire ici (CNz)",
        "kio/servicemenus/cnz.desktop",
        "nemo/actions/cnz-compresser.nemo_action",
        "nemo/actions/cnz-extraire.nemo_action",
        "nautilus/scripts/Compresser en ZIP (CNz)",
        "nemo/actions/cnz-zip.nemo_action",
        "icons/hicolor/256x256/apps/cnz.png",
    ];

    /// Icon of the launcher and menus (installateur/icone.png).
    const ICONE: &[u8] = include_bytes!("../installateur/icone.png");

    /// Old names of the Nautilus scripts (before the "CNz" spelling), removed on
    /// installation as on uninstallation.
    const ANCIENS: [&str; 3] = [
        "nautilus/scripts/Compresser avec CNZ",
        "nautilus/scripts/Extraire ici (CNZ)",
        "nautilus/scripts/Compresser en ZIP (CNZ)",
    ];

    pub fn install(gui: &Path) -> Result<Vec<String>> {
        let d = data_home()?;
        for f in ANCIENS {
            let _ = fs::remove_file(d.join(f));
        }
        let (q, sq) = (quoted(gui), sh_quoted(gui));
        let icone = d.join(FILES[9]);
        if let Some(p) = icone.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&icone, ICONE)?;
        write(
            &d.join(FILES[0]),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-cnz">
    <comment>Archive CNz</comment>
    <magic priority="60"><match type="string" offset="0" value="\x89CNZ"/></magic>
    <glob pattern="*.cnz"/>
  </mime-type>
</mime-info>
"#,
            false,
        )?;
        write(
            &d.join(FILES[1]),
            &format!(
                "[Desktop Entry]\nType=Application\nName=CNz\nComment=Compresser et extraire des archives (.cnz, ZIP, 7z, tar…)\nExec={q} %f\n\
                 Icon=cnz\nTerminal=false\nMimeType=application/x-cnz;{AUTRES_MIME}\nCategories=Utility;Archiving;\n"
            ),
            false,
        )?;
        write(&d.join(FILES[2]), &format!("#!/bin/sh\nexec {sq} \"$@\"\n"), true)?;
        write(&d.join(FILES[3]), &format!("#!/bin/sh\nexec {sq} --extraire-ici \"$1\"\n"), true)?;
        write(
            &d.join(FILES[4]),
            &format!(
                "[Desktop Entry]\nType=Service\nMimeType=all/all;\nActions=cnzCompresser;cnzZip;cnzExtraire;\n\
                 X-KDE-ServiceTypes=KonqPopupMenu/Plugin\n\n\
                 [Desktop Action cnzCompresser]\nName=Compresser avec CNz\nIcon=cnz\nExec={q} %F\n\n\
                 [Desktop Action cnzZip]\nName=Compresser en ZIP (CNz)\nIcon=cnz\nExec={q} --zip %F\n\n\
                 [Desktop Action cnzExtraire]\nName=Extraire ici (CNz)\nIcon=cnz\nExec={q} --extraire-ici %f\n"
            ),
            true,
        )?;
        write(
            &d.join(FILES[5]),
            &format!("[Nemo Action]\nName=Compresser avec CNz\nExec={q} %F\nSelection=notnone\nExtensions=any;dir;\nIcon-Name=cnz\n"),
            false,
        )?;
        write(
            &d.join(FILES[6]),
            &format!(
                "[Nemo Action]\nName=Extraire ici (CNz)\nExec={q} --extraire-ici %F\nSelection=s\nExtensions=cnz;{};\nIcon-Name=cnz\n",
                AUTRES.join(";")
            ),
            false,
        )?;
        write(&d.join(FILES[7]), &format!("#!/bin/sh\nexec {sq} --zip \"$@\"\n"), true)?;
        write(
            &d.join(FILES[8]),
            &format!("[Nemo Action]\nName=Compresser en ZIP (CNz)\nExec={q} --zip %F\nSelection=notnone\nExtensions=any;dir;\nIcon-Name=cnz\n"),
            false,
        )?;
        refresh(&d);
        Ok(vec![
            format!("type de fichier .cnz et lanceur CNz ({})", d.display()),
            "« Compresser avec CNz », « Compresser en ZIP (CNz) » et « Extraire ici » : Nautilus (scripts), Dolphin, Nemo".into(),
            format!("« Ouvrir avec » et « Extraire ici » aussi pour .{}", AUTRES.join(", .")),
        ])
    }

    pub fn uninstall() -> Result<Vec<String>> {
        let d = data_home()?;
        let mut done = Vec::new();
        for f in FILES.iter().chain(ANCIENS.iter()) {
            if fs::remove_file(d.join(f)).is_ok() {
                done.push(format!("retiré : {f}"));
            }
        }
        refresh(&d);
        Ok(done)
    }

    pub fn set_default() -> Result<Vec<String>> {
        if !data_home()?.join(FILES[1]).is_file() {
            bail!("lancez d'abord « cnz menu installer »");
        }
        let types: Vec<&str> = AUTRES_MIME.split(';').map(str::trim).filter(|m| !m.is_empty()).collect();
        let ok = std::process::Command::new("xdg-mime").args(["default", "cnz-fenetre.desktop"]).args(&types).status().is_ok_and(|s| s.success());
        if !ok {
            bail!("xdg-mime introuvable ou en échec : choisissez CNz dans « Ouvrir avec » de votre explorateur");
        }
        Ok(vec![format!("CNz ouvre désormais par défaut : {}", types.join(", "))])
    }

    /// Updates the file type and launcher databases, if the tools are there
    /// (otherwise the desktop will do it at the next session).
    fn refresh(d: &Path) {
        let _ = std::process::Command::new("update-mime-database").arg(d.join("mime")).output();
        let _ = std::process::Command::new("update-desktop-database").arg(d.join("applications")).output();
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn install(_gui: &Path) -> Result<Vec<String>> {
        bail!("pas encore disponible sous macOS : ouvrez l'application CNz et glissez-y vos fichiers")
    }

    pub fn uninstall() -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub fn set_default() -> Result<Vec<String>> {
        bail!("pas encore disponible sous macOS : clic droit sur une archive, « Lire les informations », « Ouvrir avec »")
    }
}
