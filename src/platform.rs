//! What differs from one system to another: file names, permissions,
//! symbolic links, case sensitivity. The rest of CNz only depends on this
//! module, which makes it identical on Linux, Android, macOS and Windows.
//!
//! In the archive, paths are bytes separated by "/". On Unix these are the
//! bytes of the name as they are; on Windows, the UTF-8 of the name.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::path::Path;

/// File name (a single component) → archive bytes.
pub fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        s.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        s.to_string_lossy().into_owned().into_bytes()
    }
}

/// Archive bytes → system file name.
pub fn bytes_to_os(b: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(b).to_os_string()
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Target of a symbolic link, with "/" as separator whatever the system.
pub fn link_target(link: &Path) -> io::Result<Vec<u8>> {
    let target = os_to_bytes(fs::read_link(link)?.as_os_str());
    Ok(if cfg!(windows) { target.into_iter().map(|c| if c == b'\\' { b'/' } else { c }).collect() } else { target })
}

/// Unix-style permissions. On Windows, they are reconstructed: read-only or
/// not, and 755 for folders.
pub fn mode_of(meta: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let base = if meta.is_dir() { 0o755 } else { 0o644 };
        if meta.permissions().readonly() {
            base & !0o222
        } else {
            base
        }
    }
}

/// Modification date: seconds since 1970 (negative before) and nanoseconds.
pub fn mtime_of(meta: &Metadata) -> (i64, u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.mtime(), meta.mtime_nsec() as u32)
    }
    #[cfg(not(unix))]
    {
        match meta.modified().map(|t| t.duration_since(std::time::UNIX_EPOCH)) {
            Ok(Ok(d)) => (d.as_secs() as i64, d.subsec_nanos()),
            Ok(Err(e)) => {
                let d = e.duration();
                if d.subsec_nanos() == 0 {
                    (-(d.as_secs() as i64), 0)
                } else {
                    (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
                }
            }
            Err(_) => (0, 0),
        }
    }
}

/// Applies the permissions. On Windows, only read-only has an equivalent.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let meta = fs::metadata(path)?;
        if meta.is_dir() {
            return Ok(());
        }
        let mut perms = meta.permissions();
        perms.set_readonly(mode & 0o222 == 0);
        fs::set_permissions(path, perms)
    }
}

/// Creates a symbolic link. On Windows we need to know whether the target is
/// a folder, and the right to create links (developer mode or administrator).
pub fn make_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(bytes_to_os(target), link)
    }
    #[cfg(windows)]
    {
        let target = String::from_utf8_lossy(target).replace('/', "\\");
        let resolved = link.parent().unwrap_or(Path::new(".")).join(&target);
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(&target, link)
        } else {
            std::os::windows::fs::symlink_file(&target, link)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(io::ErrorKind::Unsupported, "liens symboliques non pris en charge"))
    }
}

/// Does the disk of `dir` mix up upper and lower case? (Windows and macOS by
/// default, USB sticks in FAT/exFAT…) This is checked by creating a probe
/// file rather than assumed from the system.
pub fn is_case_insensitive(dir: &Path) -> bool {
    let lower = dir.join(format!(".cnz-casse-{}", std::process::id()));
    let upper = dir.join(format!(".CNZ-CASSE-{}", std::process::id()));
    if fs::write(&lower, b"").is_err() {
        return cfg!(any(windows, target_os = "macos"));
    }
    let insensitive = upper.exists();
    let _ = fs::remove_file(&lower);
    insensitive
}

/// Names reserved by Windows, whatever the extension ("con.txt" too).
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Makes a name acceptable to Windows: forbidden characters replaced by
/// "_", trailing dot or space replaced, reserved name prefixed with "_".
/// The backslash is replaced too: on Windows it is a separator, and "..\x"
/// would leave the destination folder.
pub fn windows_name(name: &str) -> Cow<'_, str> {
    let mut out: String =
        name.chars().map(|c| if (c as u32) < 32 || "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect();
    if out.ends_with('.') || out.ends_with(' ') {
        out.pop();
        out.push('_');
    }
    let stem = out.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out.insert(0, '_');
    }
    if out == name {
        Cow::Borrowed(name)
    } else {
        Cow::Owned(out)
    }
}

/// Inserts "(n)" before the extension: "photo.jpg" → "photo (2).jpg".
pub fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(p) if p > 0 => format!("{} ({n}){}", &name[..p], &name[p..]),
        _ => format!("{name} ({n})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noms_windows() {
        assert_eq!(windows_name("facture.pdf"), "facture.pdf");
        assert_eq!(windows_name("réunion 14:30.txt"), "réunion 14_30.txt");
        assert_eq!(windows_name("con.txt"), "_con.txt");
        assert_eq!(windows_name("LPT1"), "_LPT1");
        assert_eq!(windows_name("console.txt"), "console.txt");
        assert_eq!(windows_name("fin."), "fin_");
        assert_eq!(windows_name("..\\..\\piege"), ".._.._piege");
        assert_eq!(windows_name("a?b*c"), "a_b_c");
    }

    #[test]
    fn numerotation() {
        assert_eq!(numbered("photo.jpg", 2), "photo (2).jpg");
        assert_eq!(numbered("LISEZMOI", 3), "LISEZMOI (3)");
        assert_eq!(numbered(".env", 2), ".env (2)");
    }
}
