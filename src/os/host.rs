//! The non-Windows backend: a host filesystem dressed up as a `C:` volume, so
//! the whole engine — index layout, ranking, live diffs, trigram content
//! search, the daemon protocol — can be built and tested off a Windows box.
//!
//! Index paths still look like `C:\a\b`; every call here translates one to a
//! real path (`/a/b`, or under `$FSEARCH_ROOT`). Nothing in this file ships in
//! the Windows build.

use crate::os::{Event, HISTORY_DONE, ROOT, SEP, Stat, join};
use crate::walk::{FLAG_CLOUD, FLAG_HIDDEN, FLAG_MOUNT, KIND_DIR, KIND_FILE, KIND_LINK, Listing};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub const MAX_DEPTH: u32 = 96;

/// The allocator the Windows build installs. Here it is the system one: the
/// point of it is returning Windows' big VirtualAlloc blocks, and the host
/// malloc already does that.
pub struct MmapAlloc;

unsafe impl GlobalAlloc for MmapAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        unsafe { System.realloc(p, l, n) }
    }
}

pub fn release_memory() {}
pub fn no_materialize() {}
pub fn set_interactive() {}
pub fn set_user_initiated() {}
pub fn set_utility() {}
pub fn enable_backup_privileges() {}

/// The folder standing in for `C:`, or `/`.
fn base() -> PathBuf {
    match crate::os::redirect() {
        Some(r) => PathBuf::from(crate::os::os_from_bytes(&r)),
        None => PathBuf::from("/"),
    }
}

/// `C:\a\b` -> `<base>/a/b`; ROOT -> `<base>`.
pub fn host(path: &[u8]) -> PathBuf {
    let b = base();
    if path == ROOT {
        return b;
    }
    let mut p = b.to_path_buf();
    let rest = if crate::os::is_drive(path) { b"".as_slice() } else { &path[2.min(path.len())..] };
    for c in rest.split(|&x| x == SEP).filter(|c| !c.is_empty()) {
        p.push(crate::os::os_from_bytes(c));
    }
    p
}

/// The other way: a real path under `base()` -> its index path.
pub fn logical(p: &Path) -> Vec<u8> {
    let b = base();
    let rel = p.strip_prefix(&b).unwrap_or(p);
    let mut out = b"C:".to_vec();
    for c in rel.components() {
        out.push(SEP);
        out.extend_from_slice(c.as_os_str().as_bytes());
    }
    out
}

pub fn canonical(s: &str) -> String {
    let p = if s.starts_with("C:") || s.starts_with('\\') { host(s.as_bytes()) } else { PathBuf::from(s) };
    let c = std::fs::canonicalize(&p).unwrap_or(p);
    String::from_utf8_lossy(&logical(&c)).into_owned()
}

pub fn volumes() -> Vec<Vec<u8>> {
    vec![b"C:".to_vec()]
}

pub fn endpoint() -> String {
    format!("/tmp/fsearch-{}.sock", std::process::id())
}

pub struct DirHandle {
    p: PathBuf,
}

pub fn open_dir(path: &[u8]) -> Option<DirHandle> {
    if path == ROOT {
        return None;
    }
    let p = host(path);
    p.is_dir().then_some(DirHandle { p })
}

pub fn read_dir_batch(h: &DirHandle, l: &mut Listing) {
    let Ok(rd) = std::fs::read_dir(&h.p) else { return };
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        let Ok(lm) = std::fs::symlink_metadata(e.path()) else { continue };
        let ft = lm.file_type();
        let mut kind = if ft.is_dir() {
            KIND_DIR
        } else if ft.is_symlink() {
            KIND_LINK
        } else if ft.is_file() {
            KIND_FILE
        } else {
            crate::walk::KIND_OTHER
        };
        // A symlink to a directory stands in for a reparse point: not descended into.
        if ft.is_symlink() && md.is_dir() {
            kind = KIND_DIR | FLAG_MOUNT;
        }
        if lm.permissions().readonly() && kind & 3 == KIND_FILE {
            kind |= FLAG_HIDDEN;
        }
        let mtime =
            md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
        l.push(e.file_name().as_bytes(), kind, if md.is_dir() { 0 } else { md.len() }, mtime);
    }
}

pub fn lstat(path: &[u8]) -> Option<Stat> {
    if path == ROOT {
        return None;
    }
    let md = std::fs::symlink_metadata(host(path)).ok()?;
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        KIND_LINK
    } else if ft.is_dir() {
        KIND_DIR
    } else if ft.is_file() {
        KIND_FILE
    } else {
        crate::walk::KIND_OTHER
    };
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
    Some(Stat { kind, size: md.len(), mtime })
}

pub fn open_regular(path: &[u8]) -> Option<std::fs::File> {
    let st = lstat(path)?;
    (st.kind & 3 == KIND_FILE && st.kind & FLAG_CLOUD == 0).then(|| std::fs::File::open(host(path)).ok())?
}

pub fn gated(_home: &str) -> Vec<Vec<u8>> {
    Vec::new()
}

pub fn has_full_disk_access() -> bool {
    true
}

pub fn set_login(_exe: &Path, _on: bool) -> io::Result<()> {
    Ok(())
}

// ------------------------------------------------------------- change stream

pub struct Stream {
    stop: Arc<AtomicBool>,
    failures: Arc<Mutex<Vec<String>>>,
}

impl Stream {
    pub fn failures(&self) -> Vec<String> {
        self.failures.lock().unwrap().clone()
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn current_pos() -> crate::os::WatchPos {
    crate::os::WatchPos::new()
}

/// No host change stream: say history is done and let the tests drive updates
/// through `Live::apply_dir` directly.
pub fn watch(_pos: crate::os::WatchPos, _latency: f64, tx: Sender<Vec<Event>>) -> Stream {
    let _ = tx.send(vec![Event { path: Vec::new(), flags: HISTORY_DONE, id: 0, vol: 0 }]);
    Stream { stop: Arc::new(AtomicBool::new(false)), failures: Arc::new(Mutex::new(Vec::new())) }
}

// ------------------------------------------------------------------- plumbing

pub struct Conn(UnixStream);

impl Conn {
    pub fn try_clone(&self) -> io::Result<Conn> {
        self.0.try_clone().map(Conn)
    }
    pub fn shutdown_write(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
    /// Wait for data without consuming any: MSG_PEEK leaves the byte in the
    /// socket, and the read timeout bounds the wait.
    pub fn wait_readable(&self, timeout: std::time::Duration) -> bool {
        use std::os::unix::io::AsRawFd;
        let _ = self.0.set_read_timeout(Some(timeout));
        let mut b = [0u8; 1];
        let n = unsafe { libc::recv(self.0.as_raw_fd(), b.as_mut_ptr() as *mut core::ffi::c_void, 1, libc::MSG_PEEK) };
        let _ = self.0.set_read_timeout(None);
        n > 0
    }
}
impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub struct Listener(std::os::unix::net::UnixListener);

pub fn listen() -> io::Result<Listener> {
    let _ = std::fs::remove_file(endpoint());
    Ok(Listener(std::os::unix::net::UnixListener::bind(endpoint())?))
}

impl Listener {
    pub fn accept(&mut self) -> io::Result<Conn> {
        self.0.accept().map(|(s, _)| Conn(s))
    }
}

pub fn connect_once() -> Option<Conn> {
    UnixStream::connect(endpoint()).ok().map(Conn)
}

pub fn spawn_daemon(log: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let o = f.try_clone()?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    unsafe {
        cmd.pre_exec(|| {
            libc_setsid();
            Ok(())
        });
    }
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::from(f)).stderr(std::process::Stdio::from(o)).spawn()?;
    Ok(())
}

fn libc_setsid() {
    // The host backend only ever runs under `cargo test`, where detaching is
    // neither possible nor wanted.
}

/// Join, re-exported for symmetry with the Windows backend's use of it.
pub(crate) fn _unused_join(a: &[u8], b: &[u8]) -> Vec<u8> {
    join(a, b)
}

pub fn utf8_console() {}

pub fn console_is_ours() -> bool {
    false
}

pub fn login_state() -> String {
    "n/a on this platform".to_string()
}
