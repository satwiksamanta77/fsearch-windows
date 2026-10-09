//! The platform layer: everything that touches the operating system, behind
//! one API, plus the path conventions the rest of the crate uses.
//!
//! Paths inside fsearch are byte strings joined with [`SEP`], exactly as a
//! user writes them: `C:\Users\me\main.rs`. A drive root is just its drive
//! token (`C:`); the whole index hangs off a virtual root ([`ROOT`]) with one
//! entry per volume, which is what lets `in:` stay one contiguous range over
//! several disks. Only the platform layer turns one of those into a name the
//! OS accepts.

#[cfg(windows)]
#[path = "os/windows.rs"]
mod imp;
#[cfg(not(windows))]
#[path = "os/host.rs"]
mod imp;

pub use imp::{
    Conn, DirHandle, Listener, MAX_DEPTH, MmapAlloc, Stream, canonical, connect_once, current_pos, enable_backup_privileges, endpoint, gated,
    has_full_disk_access, listen, lstat, no_materialize, open_dir, open_regular, read_dir_batch, release_memory, set_interactive, set_login,
    set_user_initiated, set_utility, spawn_daemon, utf8_console, volumes, watch,
};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// The path separator used everywhere inside fsearch.
pub const SEP: u8 = b'\\';
/// The virtual root of the whole index: no name on disk, one child per volume.
pub const ROOT: &[u8] = b"\\";

/// What the platform reported about one path.
pub struct Stat {
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
}

impl Stat {
    pub fn into_oent(self, path: &[u8]) -> crate::live::OEnt {
        crate::live::OEnt::new(path, self.kind, self.size, self.mtime)
    }
}

/// A filesystem change, reduced to the one thing the live index acts on: the
/// directory that now needs relisting. `id` is opaque (0 on Windows, whose
/// change stream has no replayable cursor); `vol` says which stream sent it.
pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
    pub vol: u32,
}

/// This directory changed in ways one listing cannot show.
pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
/// The change stream lost events; recover from folder mtimes instead.
pub const USER_DROPPED: u32 = 0x2;
pub const KERNEL_DROPPED: u32 = 0x4;
/// Everything from before the stream started has been delivered.
pub const HISTORY_DONE: u32 = 0x10;

/// Where each change stream stands, per volume. Windows' change stream has no
/// replayable history, so this records what the index was built against
/// rather than a cursor to resume from.
pub type WatchPos = BTreeMap<u32, (u64, u64)>;

/// Index only this folder, as the `C:` volume. A development and test hook;
/// unset (the default) means every real volume on the machine.
pub fn redirect() -> Option<Vec<u8>> {
    static R: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    R.get_or_init(|| std::env::var_os("FSEARCH_ROOT").map(|v| bytes_from_path(Path::new(&v)))).clone()
}

/// Write via a temp file, then swap it in. The swap renames the old file out
/// of the way first: Windows refuses to replace a file another process still
/// has mapped, but it will rename one (Rust opens with FILE_SHARE_DELETE), and
/// the mapping follows the old name.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(bytes)?;
        f.flush()?;
        f.into_inner().map_err(|e| e.into_error())?.sync_data()?;
    }
    replace(&tmp, path)
}

/// `rename(from, to)` that also works when `to` exists and may be mapped.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let old = to.with_extension("old");
    let _ = std::fs::remove_file(&old);
    let _ = std::fs::rename(to, &old);
    match std::fs::rename(from, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(&old);
            Ok(())
        }
        // Put it back rather than leave no index at all.
        Err(e) => {
            let _ = std::fs::rename(&old, to);
            Err(e)
        }
    }
}

/// Take the exclusive, non-blocking lock on an open file: one owner per index,
/// and the lock dies with the process.
pub fn try_lock(f: &std::fs::File) -> bool {
    f.try_lock().is_ok()
}

/// Join a name onto a directory path.
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir == ROOT {
        return name.to_vec();
    }
    let mut p = Vec::with_capacity(dir.len() + 1 + name.len());
    p.extend_from_slice(dir);
    if !dir.ends_with(&[SEP]) {
        p.push(SEP);
    }
    p.extend_from_slice(name);
    p
}

/// Drop trailing separators (`C:\` -> `C:`), leaving ROOT alone.
pub fn normalize(path: &[u8]) -> Vec<u8> {
    let mut p = path.to_vec();
    while p.len() > 1 && p.last() == Some(&SEP) {
        p.pop();
    }
    p
}

/// A drive token: `C:`, the index's name for the root of that volume.
pub fn is_drive(name: &[u8]) -> bool {
    name.len() == 2 && name[1] == b':' && name[0].is_ascii_alphabetic()
}

/// The directory part of a path.
pub fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == SEP) {
        Some(0) => ROOT,
        Some(p) => &path[..p],
        None => b"",
    }
}

/// The last component of a path.
pub fn file_name(path: &[u8]) -> &[u8] {
    &path[path.iter().rposition(|&b| b == SEP).map_or(0, |p| p + 1)..]
}

/// Key range holding everything strictly under `path`, plus the prefix length
/// a direct child of `path` stops at. ROOT covers the whole map.
pub fn subtree_bounds(path: &[u8]) -> (Vec<u8>, Option<Vec<u8>>, usize) {
    if path == ROOT {
        return (Vec::new(), None, 0);
    }
    let lo = join(path, b"");
    let mut hi = lo.clone();
    *hi.last_mut().unwrap() += 1; // every key starting with `path\SEP` sorts below SEP + 1
    let n = lo.len();
    (lo, Some(hi), n)
}

/// `subtree_bounds` as `BTreeMap::range` bounds.
pub fn subtree_range(path: &[u8]) -> (std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>, usize) {
    let (lo, hi, n) = subtree_bounds(path);
    (std::ops::Bound::Included(lo), hi.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded), n)
}

/// Bytes of an OS string. Windows names are UTF-16 and every one has a UTF-8
/// encoding, so this round-trips; anything unpaired goes lossy.
pub fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}

pub fn os_from_bytes(b: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(b).into_owned())
}

pub fn path_from_bytes(b: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(b))
}

pub fn bytes_from_path(p: &Path) -> Vec<u8> {
    os_bytes(p.as_os_str())
}

/// Undo the `\\?\` verbatim prefix `std::fs::canonicalize` adds on Windows, so
/// a resolved path matches the spelling the index uses.
pub fn strip_verbatim(s: &str) -> String {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{r}");
    }
    s.strip_prefix(r"\\?\").map_or_else(|| s.to_string(), str::to_string)
}
