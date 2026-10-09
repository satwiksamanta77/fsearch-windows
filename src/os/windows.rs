//! Windows: the Win32 half of [`crate::os`].
//!
//! Three syscalls do the work macOS gets from `getattrlistbulk` and FSEvents:
//!
//! - `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` fills a 256 KiB
//!   buffer with hundreds of entries at once, each already carrying its name,
//!   attributes, size and mtime, so a crawl costs one call per directory and
//!   no per-file stat. `FindFirstFileExW` stands in where it is unsupported.
//! - `ReadDirectoryChangesW`, one recursive watch per volume, is the change
//!   stream. It has no replayable history, so a restart recovers by relisting
//!   the folders whose mtime moved (see `engine::relist_changed`) — the same
//!   path the original takes when FSEvents history is gone.
//! - `CreateNamedPipeW` carries the daemon's JSON lines.

use crate::os::{Event, HISTORY_DONE, MUST_SCAN_SUBDIRS, ROOT, SEP, Stat, file_name, join};
use crate::walk::{FLAG_CLOUD, FLAG_HIDDEN, FLAG_MOUNT, KIND_DIR, KIND_FILE, KIND_LINK, Listing};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_CALL_NOT_IMPLEMENTED, ERROR_INVALID_FUNCTION, ERROR_INVALID_LEVEL, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING,
    ERROR_NOT_SUPPORTED, ERROR_NOTIFY_ENUM_DIR, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_SUCCESS, FILETIME, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_UNPINNED,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_BOTH_DIR_INFO,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FileIdBothDirectoryInfo, FindClose, FindExInfoBasic, FindExSearchNameMatch,
    FindFirstFileExW, FindNextFileW, FlushFileBuffers, GetDriveTypeW, GetFileAttributesExW, GetFileExInfoStandard, GetFileInformationByHandleEx,
    GetFileType, GetLogicalDrives, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadDirectoryChangesW, WIN32_FILE_ATTRIBUTE_DATA, WIN32_FIND_DATAW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{
    GetProcessHeap, HeapCompact, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe, WaitNamedPipeW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThread, OpenProcessToken, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST, WaitForSingleObject,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

/// `FILE_ATTRIBUTE_*` for cloud-files placeholders: listing one is free,
/// reading it would download it.
const CLOUD_ATTRS: u32 =
    FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_UNPINNED;
/// `IO_REPARSE_TAG_CLOUD` and its variants all live in the 0x9.... range.
const TAG_CLOUD_MASK: u32 = 0xF000_0000;
const TAG_CLOUD: u32 = 0x9000_0000;
/// Hard stop on nesting: a cloud placeholder we did follow cannot loop forever.
pub const MAX_DEPTH: u32 = 96;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const ACCESS_LIST_DIR: u32 = 0x0001;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const FIND_LARGE_FETCH: u32 = 2;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// One change buffer per watched volume; big enough that a busy checkout does
/// not overflow it into a full rescan.
const WATCH_BUF: usize = 1 << 18;

// ---------------------------------------------------------------- allocator

/// Big buffers (index builds, content batches) come straight from the OS and
/// go straight back, so a build's transient memory is returned instead of
/// sitting in the CRT heap as the daemon's footprint.
pub struct MmapAlloc;
const BIG: usize = 1 << 20;

/// VirtualAlloc hands back 64 KiB-aligned memory, which covers every
/// alignment Rust asks for in practice.
#[inline]
fn vm(l: &Layout) -> bool {
    l.size() >= BIG && l.align() <= 65536
}

unsafe impl GlobalAlloc for MmapAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if !vm(&l) {
            return unsafe { System.alloc(l) };
        }
        unsafe { VirtualAlloc(std::ptr::null(), l.size(), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as *mut u8 }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // Fresh committed pages are already zero.
        if vm(&l) { unsafe { self.alloc(l) } } else { unsafe { System.alloc_zeroed(l) } }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if vm(&l) {
            unsafe { VirtualFree(p as *mut c_void, 0, MEM_RELEASE) };
        } else {
            unsafe { System.dealloc(p, l) }
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let n = unsafe { Layout::from_size_align_unchecked(new, l.align()) };
        if !vm(&l) && !vm(&n) {
            return unsafe { System.realloc(p, l, new) };
        }
        let q = unsafe { self.alloc(n) };
        if !q.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                self.dealloc(p, l);
            }
        }
        q
    }
}

/// Hand freed heap memory back to the OS after big transient work.
pub fn release_memory() {
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
    }
}

// ------------------------------------------------------------------- naming

/// The pipe every client and the daemon meet on. Per session, so two users on
/// one machine (or one user over RDP) each get their own daemon.
pub fn endpoint() -> String {
    let mut sid = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) };
    format!(r"\\.\pipe\fsearch-s{sid}")
}

/// Folders this process cannot read without elevation, or that hold no user
/// data. Skipped up front: opening them only costs an access-denied each.
pub fn gated(home: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for v in volumes() {
        let v = String::from_utf8_lossy(&v).into_owned();
        for d in ["System Volume Information", "$Recycle.Bin", "Recovery", "Config.Msi", "Documents and Settings", "PerfLogs"] {
            out.push(format!("{v}\\{d}").into_bytes());
        }
        for d in ["Windows\\CSC", "Windows\\ServiceProfiles", "Windows\\Temp"] {
            out.push(format!("{v}\\{d}").into_bytes());
        }
    }
    // Other people's profiles: readable only by them and the machine.
    let home = home.as_bytes();
    let me = file_name(home).to_vec();
    let users = match home.iter().rposition(|&b| b == SEP) {
        Some(p) if p > 0 => home[..p].to_vec(),
        _ => return out,
    };
    if file_name(&users).eq_ignore_ascii_case(b"Users")
        && let Ok(rd) = std::fs::read_dir(PathBuf::from(crate::os::os_from_bytes(&users)))
    {
        for e in rd.flatten() {
            let n = crate::os::os_bytes(&e.file_name());
            if n != me && !matches!(n.as_slice(), b"Public" | b"Default" | b"Default User" | b"All Users") {
                out.push(join(&users, &n));
            }
        }
    }
    out
}

/// Whether this process runs elevated. Windows has no consent prompt the way
/// macOS does, so this only decides whether the folders `gated` lists are
/// worth trying at all.
pub fn has_full_disk_access() -> bool {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut el: TOKEN_ELEVATION = std::mem::zeroed();
        let mut n = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, &mut el as *mut _ as *mut c_void, std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut n);
        CloseHandle(tok);
        ok != 0 && el.TokenIsElevated != 0
    }
}

/// Elevated, take the backup and restore privileges so protected folders can
/// be listed rather than skipped. Best effort; a standard user just gets no.
pub fn enable_backup_privileges() {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok) == 0 {
            return;
        }
        for name in [SE_BACKUP_NAME, SE_RESTORE_NAME] {
            let mut luid = std::mem::zeroed();
            if LookupPrivilegeValueW(std::ptr::null(), name, &mut luid) == 0 {
                continue;
            }
            let mut tp: TOKEN_PRIVILEGES = std::mem::zeroed();
            tp.PrivilegeCount = 1;
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
            AdjustTokenPrivileges(tok, 0, &tp, std::mem::size_of::<TOKEN_PRIVILEGES>() as u32, std::ptr::null_mut(), std::ptr::null_mut());
        }
        CloseHandle(tok);
    }
}

/// Nothing to set: the crawl never reads file contents, and `open_regular`
/// refuses cloud placeholders by attribute, so a search can never make
/// OneDrive download something.
pub fn no_materialize() {}

pub fn set_interactive() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}
pub fn set_user_initiated() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}
pub fn set_utility() {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

// -------------------------------------------------------------- enumeration

/// Volumes to index, as index paths (`C:`). Fixed disks and whatever is
/// plugged in; not network shares, not optical drives, not the floppy probes.
pub fn volumes() -> Vec<Vec<u8>> {
    // Indexing one folder instead of the machine (see `os::redirect`): it is
    // the `C:` volume, and no other drive is scanned.
    if crate::os::redirect().is_some() {
        return vec![b"C:".to_vec()];
    }
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        let letter = b'A' + i;
        if mask & (1 << i) == 0 || letter == b'A' || letter == b'B' {
            continue;
        }
        let t = unsafe { GetDriveTypeW(wide_raw(&format!("{letter}:\\")).as_ptr()) };
        if t == DRIVE_FIXED || t == DRIVE_REMOVABLE {
            out.push(format!("{letter}:").into_bytes());
        }
    }
    if out.is_empty() {
        out.push(b"C:".to_vec());
    }
    out
}

/// An open directory being listed. Keeps its path because the fallback lister
/// works from a name, not a handle.
pub struct DirHandle {
    h: HANDLE,
    path: Vec<u8>,
}

impl Drop for DirHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.h) };
    }
}

pub fn open_dir(path: &[u8]) -> Option<DirHandle> {
    if path == ROOT {
        return None;
    }
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            ACCESS_LIST_DIR,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE).then_some(DirHandle { h, path: path.to_vec() })
}

thread_local! {
    /// One 256 KiB listing buffer per worker thread, 8-byte aligned for the
    /// LARGE_INTEGERs inside `FILE_ID_BOTH_DIR_INFO`.
    static BUF: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// One name-encoding buffer per worker thread.
    static NAME: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Append every remaining entry of an open directory to `l`, a bufferful at a
/// time. Falls back to `FindFirstFileExW` where the bulk class is unsupported.
pub fn read_dir_batch(h: &DirHandle, l: &mut Listing) {
    BUF.with_borrow_mut(|buf| {
        if buf.is_empty() {
            buf.resize(256 * 1024 / 8, 0);
        }
        let base = buf.as_mut_ptr() as *mut u8;
        let nbytes = (buf.len() * 8) as u32;
        loop {
            if unsafe { GetFileInformationByHandleEx(h.h, FileIdBothDirectoryInfo, base as *mut c_void, nbytes) } != 0 {
                let mut off = 0usize;
                loop {
                    let r = unsafe { &*(base.add(off) as *const FILE_ID_BOTH_DIR_INFO) };
                    let nlen = (r.FileNameLength as usize) / 2;
                    NAME.with_borrow_mut(|nm| {
                        nm.clear();
                        enc_utf8(nm, unsafe { std::slice::from_raw_parts(r.FileName.as_ptr(), nlen) });
                        push(l, nm, r.FileAttributes, r.EndOfFile.max(0) as u64, r.LastWriteTime, r.EaSize);
                    });
                    if r.NextEntryOffset == 0 {
                        break;
                    }
                    let next = off + r.NextEntryOffset as usize;
                    if next + std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() > nbytes as usize || next <= off {
                        return;
                    }
                    off = next;
                }
                continue;
            }
            let e = unsafe { GetLastError() };
            if l.ents.is_empty()
                && matches!(
                    e,
                    ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED | ERROR_INVALID_FUNCTION | ERROR_INVALID_LEVEL
                )
            {
                return find_fallback(h, l);
            }
            return;
        }
    })
}

/// The bulk directory class wants a filesystem with file ids. FAT and a few
/// filter drivers do not have them, so walk the old way there.
fn find_fallback(h: &DirHandle, l: &mut Listing) {
    let mut pat = join(&h.path, b"");
    pat.extend_from_slice(b"*");
    let w = wide(&pat);
    let mut d: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let f = unsafe {
        FindFirstFileExW(w.as_ptr(), FindExInfoBasic, &mut d as *mut _ as *mut c_void, FindExSearchNameMatch, std::ptr::null(), FIND_LARGE_FETCH)
    };
    if f == INVALID_HANDLE_VALUE {
        return;
    }
    loop {
        NAME.with_borrow_mut(|nm| {
            nm.clear();
            let n = d.cFileName.iter().position(|&c| c == 0).unwrap_or(d.cFileName.len());
            enc_utf8(nm, &d.cFileName[..n]);
            push(l, nm, d.dwFileAttributes, ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64, filetime(d.ftLastWriteTime), d.dwReserved0);
        });
        if unsafe { FindNextFileW(f, &mut d) } == 0 {
            break;
        }
    }
    unsafe { FindClose(f) };
}

/// Append one entry unless it is a dot name or too long for the index.
fn push(l: &mut Listing, name: &[u8], attrs: u32, size: u64, mtime_ft: i64, reparse_tag: u32) {
    if name.is_empty() || name.len() > u16::MAX as usize || name == b"." || name == b".." {
        return;
    }
    l.push(name, kind_of(attrs, reparse_tag), size, ft_unix(mtime_ft));
}

/// Kind and flags from Win32 attributes.
///
/// A reparse-point directory is not descended into — that is what keeps
/// junctions, symlinks and volume mount points from being indexed twice, or
/// in a loop — so it carries `FLAG_MOUNT`. The exception is a cloud-files
/// placeholder: a real folder whose contents list without downloading.
pub fn kind_of(attrs: u32, reparse_tag: u32) -> u8 {
    let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let cloud = attrs & CLOUD_ATTRS != 0 || reparse_tag & TAG_CLOUD_MASK == TAG_CLOUD;
    let mut k = if dir {
        KIND_DIR
    } else if reparse {
        KIND_LINK
    } else {
        KIND_FILE
    };
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        k |= FLAG_HIDDEN;
    }
    if cloud {
        k |= FLAG_CLOUD;
    }
    if dir && reparse && !cloud {
        k |= FLAG_MOUNT;
    }
    k
}

/// FILETIME (100 ns since 1601) to unix seconds, clamped to what the index stores.
pub fn ft_unix(v: i64) -> u32 {
    if v <= 0 {
        return 0;
    }
    ((v - 116_444_736_000_000_000) / 10_000_000).clamp(0, u32::MAX as i64) as u32
}

pub fn filetime(f: FILETIME) -> i64 {
    ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as u32 as i64
}

/// UTF-16 to UTF-8, appending. Unpaired surrogates (legal in a Windows name,
/// not in UTF-8) become U+FFFD, which is what printing them would do anyway.
pub fn enc_utf8(out: &mut Vec<u8>, s: &[u16]) {
    let mut i = 0;
    while i < s.len() {
        let c = s[i] as u32;
        i += 1;
        let c = if (0xD800..0xDC00).contains(&c) {
            match s.get(i) {
                Some(&lo) if (0xDC00..0xE000).contains(&(lo as u32)) => {
                    i += 1;
                    0x1_0000 + ((c - 0xD800) << 10) + (lo as u32 - 0xDC00)
                }
                _ => 0xFFFD,
            }
        } else if (0xDC00..0xE000).contains(&c) {
            0xFFFD
        } else {
            c
        };
        match c {
            c if c < 0x80 => out.push(c as u8),
            c if c < 0x800 => out.extend_from_slice(&[0xC0 | (c >> 6) as u8, 0x80 | (c & 0x3F) as u8]),
            c if c < 0x1_0000 => out.extend_from_slice(&[0xE0 | (c >> 12) as u8, 0x80 | ((c >> 6) & 0x3F) as u8, 0x80 | (c & 0x3F) as u8]),
            c => out.extend_from_slice(&[
                0xF0 | (c >> 18) as u8,
                0x80 | ((c >> 12) & 0x3F) as u8,
                0x80 | ((c >> 6) & 0x3F) as u8,
                0x80 | (c & 0x3F) as u8,
            ]),
        }
    }
}

/// Attributes of a path without following a reparse point (lstat semantics).
pub fn lstat(path: &[u8]) -> Option<Stat> {
    if path == ROOT {
        return None;
    }
    let mut d: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let w = wide(path);
    if unsafe { GetFileAttributesExW(w.as_ptr(), GetFileExInfoStandard, &mut d as *mut _ as *mut c_void) } == 0 {
        return None;
    }
    Some(Stat {
        kind: kind_of(d.dwFileAttributes, 0),
        size: ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64,
        mtime: ft_unix(filetime(d.ftLastWriteTime)),
    })
}

/// Open a regular file for reading, or nothing: never a directory, never a
/// reparse point, never a cloud placeholder (reading one would download it),
/// never a device that could block the open.
pub fn open_regular(path: &[u8]) -> Option<std::fs::File> {
    let st = lstat(path)?;
    if st.kind & 3 != KIND_FILE || st.kind & FLAG_CLOUD != 0 {
        return None;
    }
    let w = wide(path);
    let h =
        unsafe { CreateFileW(w.as_ptr(), GENERIC_READ, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, std::ptr::null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    if unsafe { GetFileType(h) } != FILE_TYPE_DISK {
        unsafe { CloseHandle(h) };
        return None;
    }
    Some(unsafe { std::fs::File::from_raw_handle(h as _) })
}

// ------------------------------------------------------------- change stream

/// A running set of volume watches; dropping it stops them.
pub struct Stream {
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<Option<std::thread::JoinHandle<()>>>>,
}

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Each thread polls `stop` every 40 ms, cancels its own pending read
        // and closes its own handle before it exits.
        for t in self.threads.lock().unwrap().drain(..) {
            if let Some(t) = t {
                let _ = t.join();
            }
        }
    }
}

/// Windows' change stream has no cursor to resume from; the position is only
/// a record of what the index was built against.
pub fn current_pos() -> crate::os::WatchPos {
    crate::os::WatchPos::new()
}

/// Watch every indexed volume from now on. Batches of "this folder changed"
/// arrive on `tx` until the returned stream is dropped.
pub fn watch(_pos: crate::os::WatchPos, latency: f64, tx: Sender<Vec<Event>>) -> Stream {
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    let ms = (latency * 1000.0).clamp(10.0, 5000.0) as u64;
    for v in volumes() {
        let (s, t) = (stop.clone(), tx.clone());
        threads.push(std::thread::Builder::new().name("fsearch-watch".into()).spawn(move || watch_volume(v, ms, s, t)).ok());
    }
    Stream { stop, threads: Mutex::new(threads) }
}

fn watch_handle(vol: &[u8]) -> Option<HANDLE> {
    let w = wide(&join(vol, b""));
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            ACCESS_LIST_DIR,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE).then_some(h)
}

/// One volume's watch. Owns its handle, and treats every failure as
/// retryable: a watch that quietly stops would leave the index stale, which
/// is worse than a missed batch.
fn watch_volume(vol: Vec<u8>, latency_ms: u64, stop: Arc<AtomicBool>, tx: Sender<Vec<Event>>) {
    let vid = vol.first().copied().unwrap_or(b'?') as u32;
    // There is no history to replay: say so at once, and the engine recovers
    // what it missed from folder mtimes instead.
    let _ = tx.send(vec![Event { path: Vec::new(), flags: HISTORY_DONE, id: 0, vol: vid }]);
    let mut h = match watch_handle(&vol) {
        Some(h) => h,
        None => {
            eprintln!("{} cannot watch {}: {}", crate::query::now_secs(), String::from_utf8_lossy(&vol), io::Error::last_os_error());
            return;
        }
    };
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if ev.is_null() {
        unsafe { CloseHandle(h) };
        return;
    }
    let mut buf: Vec<u32> = vec![0; WATCH_BUF / 4];
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev;
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE;
    let mut dirty: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut first: Option<Instant> = None;
    let mut armed = arm(h, &mut buf, filter, &mut ov);
    let mut fails = 0u32;
    while !stop.load(Ordering::Relaxed) {
        if !armed {
            // Re-arm, backing off, and reopen the volume if it will not take.
            fails += 1;
            if fails == 25 {
                eprintln!("{} watch on {} is not arming; reopening", crate::query::now_secs(), String::from_utf8_lossy(&vol));
                unsafe { CloseHandle(h) };
                h = match watch_handle(&vol) {
                    Some(h) => h,
                    None => {
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                };
            }
            std::thread::sleep(Duration::from_millis(40));
            armed = arm(h, &mut buf, filter, &mut ov);
            continue;
        }
        match unsafe { WaitForSingleObject(ev, 40) } {
            WAIT_OBJECT_0 => {
                let mut n = 0u32;
                if unsafe { GetOverlappedResult(h, &ov, &mut n, 0) } != 0 {
                    let len = (n as usize).min(WATCH_BUF);
                    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len) };
                    collect(bytes, &vol, &mut dirty);
                } else if unsafe { GetLastError() } == ERROR_NOTIFY_ENUM_DIR {
                    // The buffer overflowed: this volume needs a full rescan.
                    *dirty.entry(vol.clone()).or_default() |= MUST_SCAN_SUBDIRS;
                }
                fails = 0;
                armed = arm(h, &mut buf, filter, &mut ov);
            }
            WAIT_TIMEOUT => {}
            // WAIT_FAILED or an abandoned event: drop through and re-arm.
            _ => armed = false,
        }
        if !dirty.is_empty() && first.is_none() {
            first = Some(Instant::now());
        }
        if let Some(t) = first
            && t.elapsed() >= Duration::from_millis(latency_ms)
        {
            first = None;
            let batch: Vec<Event> = dirty.drain().map(|(p, f)| Event { path: p, flags: f, id: 0, vol: vid }).collect();
            if tx.send(batch).is_err() {
                break;
            }
        }
    }
    if armed {
        // Never leave I/O pending against a buffer we are about to drop.
        unsafe { CancelIoEx(h, &ov) };
        let mut n = 0u32;
        unsafe { GetOverlappedResult(h, &ov, &mut n, 1) };
    }
    unsafe {
        CloseHandle(ev);
        CloseHandle(h);
    }
}

fn arm(h: HANDLE, buf: &mut [u32], filter: u32, ov: &mut OVERLAPPED) -> bool {
    let mut n = 0u32;
    if unsafe { ReadDirectoryChangesW(h, buf.as_mut_ptr() as *mut c_void, (buf.len() * 4) as u32, 1, filter, &mut n, ov, None) } != 0 {
        return true;
    }
    // An overlapped read reports "queued" as a failure with ERROR_IO_PENDING.
    unsafe { GetLastError() == ERROR_IO_PENDING }
}

/// Turn notification records into the set of folders to relist: a record names
/// a path relative to the watched volume, and its parent is the folder whose
/// listing changed.
fn collect(buf: &[u8], vol: &[u8], dirty: &mut HashMap<Vec<u8>, u32>) {
    // The record header is the three DWORDs before FileName; size_of includes
    // the one-element FileName array, which is not part of the header.
    const HDR: usize = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
    let mut off = 0usize;
    NAME.with_borrow_mut(|nm| {
        while off + HDR <= buf.len() {
            let r = unsafe { &*(buf.as_ptr().add(off) as *const FILE_NOTIFY_INFORMATION) };
            let nbytes = r.FileNameLength as usize;
            if nbytes == 0 || off + HDR + nbytes > buf.len() {
                break;
            }
            nm.clear();
            enc_utf8(nm, unsafe { std::slice::from_raw_parts(r.FileName.as_ptr(), nbytes / 2) });
            let dir = match nm.iter().rposition(|&b| b == SEP) {
                Some(p) => join(vol, &nm[..p]),
                None => vol.to_vec(),
            };
            dirty.entry(dir).or_insert(0);
            if r.NextEntryOffset == 0 {
                break;
            }
            let next = off + r.NextEntryOffset as usize;
            if next <= off || next + HDR > buf.len() {
                break;
            }
            off = next;
        }
    });
}

// ------------------------------------------------------------------- plumbing

/// One connected client of the daemon's pipe.
pub struct Conn(std::fs::File);

impl Conn {
    pub fn try_clone(&self) -> io::Result<Conn> {
        self.0.try_clone().map(Conn)
    }
    /// True once at least one byte can be read without blocking, or `timeout`
    /// expires. A pipe has no read timeout of its own, so poll what is queued.
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let mut avail = 0u32;
            let ok = unsafe {
                PeekNamedPipe(self.0.as_raw_handle() as _, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut())
            };
            if ok != 0 && avail > 0 {
                return true;
            }
            if ok == 0 || Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A pipe has no half-close; flushing waits until the daemon has read
    /// everything sent so far, which is the signal the stdio relay wants.
    pub fn shutdown_write(&self) {
        unsafe { FlushFileBuffers(self.0.as_raw_handle() as _) };
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

pub struct Listener {
    name: Vec<u16>,
}

pub fn listen() -> io::Result<Listener> {
    Ok(Listener { name: wide_raw(&endpoint()) })
}

impl Listener {
    /// Wait for the next client. Each call makes a fresh pipe instance, so
    /// clients never queue behind one another.
    pub fn accept(&mut self) -> io::Result<Conn> {
        let h = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // A synchronous ConnectNamedPipe returns nonzero when a client
        // connects while it waits, and zero with ERROR_PIPE_CONNECTED when one
        // was already there. Both mean "this instance has a client".
        let waited = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        let e = unsafe { GetLastError() };
        if waited != 0 || e == ERROR_PIPE_CONNECTED {
            return Ok(Conn(unsafe { std::fs::File::from_raw_handle(h as _) }));
        }
        unsafe { CloseHandle(h) };
        Err(io::Error::from_raw_os_error(e as i32))
    }
}

/// Connect to the daemon, if it is there.
pub fn connect_once() -> Option<Conn> {
    let name = wide_raw(&endpoint());
    let open = || -> Option<Conn> {
        let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        (h != INVALID_HANDLE_VALUE).then(|| Conn(unsafe { std::fs::File::from_raw_handle(h as _) }))
    };
    if let Some(c) = open() {
        return Some(c);
    }
    if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
        return None;
    }
    unsafe { WaitNamedPipeW(name.as_ptr(), 2000) };
    open()
}

/// Start the daemon detached and windowless, logging to `log`. Prefers
/// `fsearchd.exe`, which is built without a console so nothing ever flashes.
pub fn spawn_daemon(log: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let me = std::env::current_exe()?;
    let d = me.with_file_name("fsearchd.exe");
    let exe = d.is_file().then_some(d).unwrap_or(me);
    let f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let o = f.try_clone()?;
    std::process::Command::new(exe)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(f))
        .stderr(std::process::Stdio::from(o))
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn()?;
    Ok(())
}

/// Add or remove the logon entry that starts the daemon at sign-in.
pub fn set_login(exe: &Path, on: bool) -> io::Result<()> {
    let sub = wide_raw(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let mut hk: HKEY = std::ptr::null_mut();
    let r = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut hk) };
    if r != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let value = wide_raw("FSearch");
    let res = if on {
        let cmd = wide_raw(&format!("\"{}\" serve", exe.display()));
        let bytes = unsafe { std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2) };
        unsafe { RegSetValueExW(hk, value.as_ptr(), 0, REG_SZ, bytes.as_ptr(), bytes.len() as u32) }
    } else {
        let r = unsafe { RegDeleteValueW(hk, value.as_ptr()) };
        // A value that is not there is already uninstalled.
        if r == 2 { ERROR_SUCCESS } else { r }
    };
    unsafe { RegCloseKey(hk) };
    if res != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(res as i32));
    }
    Ok(())
}

/// Send and read UTF-8 on the console, so non-ASCII paths survive a redirect
/// or a pipe. Without a console (the daemon) this is a no-op that fails harmlessly.
pub fn utf8_console() {
    const CP_UTF8: u32 = 65001;
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(CP_UTF8);
        windows_sys::Win32::System::Console::SetConsoleCP(CP_UTF8);
    }
}

// ------------------------------------------------------------------ naming

/// Turn an index path into the wide, `\\?\`-prefixed form Win32 wants: no
/// MAX_PATH, no drive-relative surprises, and reparse points left alone.
/// With `FSEARCH_ROOT` set, the `C:` volume is that folder instead.
pub fn wide(path: &[u8]) -> Vec<u16> {
    wide_of(&mapped(path))
}

/// The index path as a normal (non-verbatim) Windows path.
fn mapped(path: &[u8]) -> String {
    let s = String::from_utf8_lossy(path).into_owned();
    let Some(r) = crate::os::redirect() else { return s };
    let mut m = String::from_utf8_lossy(&r).trim_end_matches('\\').to_string();
    if path != ROOT && !crate::os::is_drive(path) {
        m.push_str(s.get(2..).unwrap_or(""));
    }
    m
}

/// Plain UTF-16 + NUL, for names that are already exactly what Win32 wants
/// (a pipe name, a registry key, a drive root).
pub fn wide_raw(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `wide_raw` of `s` with the verbatim path prefix applied.
pub fn wide_of(s: &str) -> Vec<u16> {
    let mut out = String::with_capacity(s.len() + 8);
    if s.starts_with(r"\\?\") {
        out.push_str(s);
    } else if s.starts_with(r"\\") {
        out.push_str(r"\\?\UNC\");
        out.push_str(&s[2..]);
    } else {
        out.push_str(r"\\?\");
        out.push_str(s);
    }
    // `C:` on its own means "the current directory on C"; the volume root is `C:\`.
    if out.ends_with(':') {
        out.push('\\');
    }
    wide_raw(&out)
}

/// Resolve a user-typed path to the index's spelling of it.
pub fn canonical(s: &str) -> String {
    let s = s.replace('/', "\\");
    let redir = crate::os::redirect().map(|r| String::from_utf8_lossy(&r).trim_end_matches('\\').to_string());
    let target = match &redir {
        Some(r) => {
            if s == "\\" || crate::os::is_drive(s.as_bytes()) {
                r.clone()
            } else {
                format!("{r}{}", s.get(2..).unwrap_or(""))
            }
        }
        None => s.clone(),
    };
    let c = std::fs::canonicalize(&target).map(|p| crate::os::strip_verbatim(&p.to_string_lossy())).unwrap_or(target);
    match &redir {
        Some(r) if c.starts_with(r.as_str()) => match &c[r.len()..] {
            "" => "C:".to_string(),
            rest => format!("C:{rest}"),
        },
        _ => c,
    }
}
