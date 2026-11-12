//! Optional metadata of an entry: owner, creation date, extended
//! attributes… They are stored in the index as "tag + length + value"
//! fields. A reader ignores the tags it does not know: new ones can be added
//! without breaking old versions.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use crate::format::{put_varint, Cursor};
use crate::index::{Entry, Field, Kind};

/// User ID (Unix).
pub const UID: u64 = 1;
/// Group ID (Unix).
pub const GID: u64 = 2;
/// User name, takes precedence over the ID when restoring.
pub const USER: u64 = 3;
/// Group name, takes precedence over the ID when restoring.
pub const GROUP: u64 = 4;
/// Creation date: seconds (zigzag) then nanoseconds.
pub const CREATED: u64 = 5;
/// Extended attribute: name length, name, then value.
pub const XATTR: u64 = 6;
/// Windows attributes (hidden, system, archive…), as GetFileAttributes.
pub const WIN_ATTRS: u64 = 7;

fn varint_field(tag: u64, v: u64) -> Field {
    let mut data = Vec::new();
    put_varint(&mut data, v);
    Field { tag, data }
}

pub fn get_varint(fields: &[Field], tag: u64) -> Option<u64> {
    fields.iter().find(|f| f.tag == tag).and_then(|f| Cursor::new(&f.data).varint().ok())
}

pub fn get_bytes(fields: &[Field], tag: u64) -> Option<&[u8]> {
    fields.iter().find(|f| f.tag == tag).map(|f| f.data.as_slice())
}

/// Reads the files' metadata, with a cache of user names: without it, each
/// file would read the system's account list again.
#[derive(Default)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Collector {
    users: HashMap<u32, Option<String>>,
    groups: HashMap<u32, Option<String>>,
}

impl Collector {
    pub fn collect(&mut self, path: &Path, meta: &fs::Metadata) -> Vec<Field> {
        let mut fields = Vec::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let (uid, gid) = (meta.uid(), meta.gid());
            fields.push(varint_field(UID, u64::from(uid)));
            fields.push(varint_field(GID, u64::from(gid)));
            if let Some(name) = self.users.entry(uid).or_insert_with(|| names::user_name(uid)) {
                fields.push(Field { tag: USER, data: name.clone().into_bytes() });
            }
            if let Some(name) = self.groups.entry(gid).or_insert_with(|| names::group_name(gid)) {
                fields.push(Field { tag: GROUP, data: name.clone().into_bytes() });
            }
            if !meta.file_type().is_symlink() {
                if let Ok(list) = xattr::list(path) {
                    for name in list {
                        if let Ok(Some(value)) = xattr::get(path, &name) {
                            use std::os::unix::ffi::OsStrExt;
                            let name = name.as_bytes();
                            let mut data = Vec::new();
                            put_varint(&mut data, name.len() as u64);
                            data.extend_from_slice(name);
                            data.extend_from_slice(&value);
                            fields.push(Field { tag: XATTR, data });
                        }
                    }
                }
            }
        }
        if let Ok(d) = meta.created().map(|t| t.duration_since(UNIX_EPOCH)) {
            if let Ok(d) = d {
                let mut data = Vec::new();
                put_varint(&mut data, (d.as_secs() as i64 as u64) << 1);
                put_varint(&mut data, u64::from(d.subsec_nanos()));
                fields.push(Field { tag: CREATED, data });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            fields.push(varint_field(WIN_ATTRS, u64::from(meta.file_attributes())));
        }
        #[cfg(not(unix))]
        let _ = path;
        fields
    }
}

/// Restores the extended attributes and the owner of an extracted entry.
/// Permissions and date are restored afterwards by the caller: changing the
/// owner may clear some special permissions.
#[cfg_attr(not(unix), allow(dead_code))]
/// Usable by several cores at once (protected caches, atomic counter).
pub struct Restorer {
    restore_owner: bool,
    users: Mutex<HashMap<Vec<u8>, Option<u32>>>,
    groups: Mutex<HashMap<Vec<u8>, Option<u32>>>,
    /// Extended attributes refused by the destination system.
    xattr_failures: AtomicU64,
}

impl Restorer {
    pub fn new(restore_owner: bool) -> Self {
        Self {
            restore_owner,
            users: Mutex::new(HashMap::new()),
            groups: Mutex::new(HashMap::new()),
            xattr_failures: AtomicU64::new(0),
        }
    }

    pub fn xattr_failures(&self) -> u64 {
        self.xattr_failures.load(Ordering::Relaxed)
    }

    pub fn apply(&self, path: &Path, entry: &Entry, problems: &mut Vec<String>) {
        #[cfg(unix)]
        {
            if !matches!(entry.kind, Kind::Symlink { .. }) {
                for f in entry.fields.iter().filter(|f| f.tag == XATTR) {
                    let mut c = Cursor::new(&f.data);
                    let Ok(n) = c.varint() else { continue };
                    let Ok(name) = c.bytes(n as usize) else { continue };
                    let value = &f.data[f.data.len() - c.remaining()..];
                    use std::os::unix::ffi::OsStrExt;
                    if xattr::set(path, std::ffi::OsStr::from_bytes(name), value).is_err() {
                        self.xattr_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            if self.restore_owner {
                // The name wins over the ID: the same user does not necessarily have the
                // same ID from one machine to another.
                let uid = get_bytes(&entry.fields, USER)
                    .and_then(|n| *self.users.lock().unwrap().entry(n.to_vec()).or_insert_with(|| names::user_id(n)))
                    .or_else(|| get_varint(&entry.fields, UID).map(|v| v as u32));
                let gid = get_bytes(&entry.fields, GROUP)
                    .and_then(|n| *self.groups.lock().unwrap().entry(n.to_vec()).or_insert_with(|| names::group_id(n)))
                    .or_else(|| get_varint(&entry.fields, GID).map(|v| v as u32));
                if let Err(e) = std::os::unix::fs::lchown(path, uid, gid) {
                    problems.push(format!("{} : propriétaire non restauré ({e})", path.display()));
                }
            }
        }
        #[cfg(windows)]
        if let Some(attrs) = get_varint(&entry.fields, WIN_ATTRS) {
            // Hidden, system, archive. Read-only is set later (with the permissions):
            // it would prevent setting the dates.
            const KEEP: u32 = 0x2 | 0x4 | 0x20;
            let attrs = attrs as u32 & KEEP;
            if attrs != 0 {
                use std::os::windows::ffi::OsStrExt;
                let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                unsafe { windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(wide.as_ptr(), attrs) };
            }
        }
        // Creation date: only Windows and macOS can change it.
        #[cfg(any(windows, target_os = "macos"))]
        if matches!(entry.kind, Kind::File { .. }) {
            if let Some(data) = get_bytes(&entry.fields, CREATED) {
                let mut c = Cursor::new(data);
                if let (Ok(secs), Ok(nanos)) = (c.varint(), c.varint()) {
                    let t = UNIX_EPOCH + std::time::Duration::new(secs >> 1, nanos.min(999_999_999) as u32);
                    #[cfg(windows)]
                    use std::os::windows::fs::FileTimesExt;
                    #[cfg(target_os = "macos")]
                    use std::os::macos::fs::FileTimesExt;
                    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
                        let _ = f.set_times(fs::FileTimes::new().set_created(t));
                    }
                }
            }
        }
        #[cfg(not(unix))]
        let _ = (path, entry, problems);
    }
}

/// True if the program runs as administrator (root).
pub fn is_admin() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Mapping between IDs and user and group names.
#[cfg(unix)]
mod names {
    use std::ffi::{CStr, CString};

    const BUF: usize = 16 * 1024;

    pub fn user_name(uid: u32) -> Option<String> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; BUF];
        let mut res = std::ptr::null_mut();
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
        if rc != 0 || res.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned())
    }

    pub fn group_name(gid: u32) -> Option<String> {
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; BUF];
        let mut res = std::ptr::null_mut();
        let rc = unsafe { libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut res) };
        if rc != 0 || res.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(grp.gr_name) }.to_string_lossy().into_owned())
    }

    pub fn user_id(name: &[u8]) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; BUF];
        let mut res = std::ptr::null_mut();
        let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
        (rc == 0 && !res.is_null()).then_some(pwd.pw_uid)
    }

    pub fn group_id(name: &[u8]) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; BUF];
        let mut res = std::ptr::null_mut();
        let rc = unsafe { libc::getgrnam_r(name.as_ptr(), &mut grp, buf.as_mut_ptr(), buf.len(), &mut res) };
        (rc == 0 && !res.is_null()).then_some(grp.gr_gid)
    }
}
