//! Crash and progress diagnostics.
//!
//! A search tool that dies silently is undebuggable, and the two ways it can die
//! silently are exactly the ones that matter here: a Rust panic inside the
//! windowless daemon (whose stderr goes to a log nobody has opened), and a
//! Win32 access violation, which is not a panic at all and takes the process
//! down without running any Rust cleanup.
//!
//! So: every stage of startup writes a timestamped line to `trace.log`, panics
//! are caught and written to `crash.log` with a backtrace, and a structured
//! exception handler catches faults below Rust and records the exception code
//! and address. All three land next to the index in `%LOCALAPPDATA%\FSearch`.
//!
//! `fsearch doctor` exercises each subsystem in turn and says which one fails.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn start() -> &'static Instant {
    static S: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    S.get_or_init(Instant::now)
}

/// Where the logs go: the data dir if it is writable, else next to the
/// executable, else the temp dir. Deliberately never fails — a diagnostics
/// layer that can panic is worse than none.
fn log_dir() -> &'static Path {
    static D: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let mut cands: Vec<PathBuf> = Vec::new();
        #[cfg(windows)]
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            cands.push(PathBuf::from(d).join("FSearch"));
        }
        if let Ok(e) = std::env::current_exe() {
            if let Some(p) = e.parent() {
                cands.push(p.join("fsearch-logs"));
            }
        }
        cands.push(std::env::temp_dir().join("fsearch-logs"));
        cands.push(PathBuf::from("."));
        for c in cands {
            if std::fs::create_dir_all(&c).is_ok() && probe_writable(&c) {
                return c;
            }
        }
        PathBuf::from(".")
    })
}

fn probe_writable(dir: &Path) -> bool {
    let p = dir.join(format!(".w{}", std::process::id()));
    std::fs::write(&p, b"x").is_ok() && std::fs::remove_file(&p).is_ok()
}

pub fn log_dir_path() -> PathBuf {
    log_dir().to_path_buf()
}
pub fn trace_path() -> PathBuf {
    log_dir().join("trace.log")
}
pub fn crash_path() -> PathBuf {
    log_dir().join("crash.log")
}

fn append(path: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
        let _ = f.flush();
    }
}

/// One timestamped line, flushed immediately: if the next stage is the one that
/// dies, the line describing it has to already be on disk.
pub fn trace(msg: &str) {
    let mut line = String::with_capacity(msg.len() + 24);
    let _ = write!(line, "[{:>10.3}s pid {}] {}\n", start().elapsed().as_secs_f64(), std::process::id(), msg);
    append(&trace_path(), &line);
}

/// `trace`, but only when `FSEARCH_TRACE` is set. For the per-directory chatter
/// a startup trace does not want.
pub fn trace_verbose(msg: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("FSEARCH_TRACE").is_some_and(|v| v != "0")) {
        trace(msg);
    }
}

/// Format and record a crash. Shared by the panic hook and the fault handler.
fn record_crash(headline: &str, detail: &str) {
    let mut s = String::new();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ =
        write!(s, "\n=== fsearch {VERSION} crashed at unix {now}, uptime {:.3}s, pid {} ===\n", start().elapsed().as_secs_f64(), std::process::id());
    let _ = write!(s, "{headline}\n");
    if !detail.is_empty() {
        let _ = write!(s, "{detail}\n");
    }
    let _ = write!(s, "args: {}\n", std::env::args().skip(1).collect::<Vec<_>>().join(" "));
    let _ = write!(s, "cwd: {}\n", std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default());
    for (k, v) in [("USERPROFILE", "home"), ("LOCALAPPDATA", "localappdata"), ("FSEARCH_ROOT", "root"), ("FSEARCH_TRACE", "trace")] {
        let _ = write!(s, "env {v}: {}\n", std::env::var_os(k).map(|x| x.to_string_lossy().into_owned()).unwrap_or_default());
    }
    append(&crash_path(), &s);
    // Also to stderr, for whoever is watching a console.
    eprint!("{s}");
}

/// Install the panic hook and (on Windows) the fault handler. Call this first
/// thing in `main`, before anything that can fail.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    ONCE.call_once(|| first = true);
    if !first {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        record_crash(&format!("PANIC at {loc}: {info}"), &format!("backtrace:\n{bt}"));
    }));
    #[cfg(windows)]
    imp::install_fault_handler();
    trace(&format!("fsearch {VERSION} starting; arch {}, logs in {}", std::env::consts::ARCH, log_dir().display()));
}

/// A human-readable line for `doctor` and for stderr when something fails.
pub fn os_err(what: &str) -> String {
    let e = std::io::Error::last_os_error();
    format!("{what}: {e} (code {})", e.raw_os_error().unwrap_or(0))
}

#[cfg(windows)]
mod imp {
    use super::{crash_path, trace};
    use std::fmt::Write as _;
    use windows_sys::Win32::Foundation::{
        CloseHandle, EXCEPTION_ACCESS_VIOLATION, EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_STACK_OVERFLOW, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_END, FILE_SHARE_READ, OPEN_ALWAYS, SetFilePointerEx, WriteFile,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SEM_NOGPFAULTERRORBOX, SetErrorMode, SetUnhandledExceptionFilter};

    /// The crash-log path as UTF-16, captured before any thread starts so the
    /// fault handler never has to allocate: a corrupted heap is one of the
    /// things it is there to survive.
    const PATH_CAP: usize = 1024;
    static mut PATH_W: [u16; PATH_CAP] = [0; PATH_CAP];
    static mut PATH_LEN: usize = 0;

    pub fn install_fault_handler() {
        let p = crash_path();
        let w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().collect();
        unsafe {
            // addr_of! rather than a reference: a shared reference to a mutable
            // static is UB, and this is written once before any thread starts.
            let dst = std::ptr::addr_of_mut!(PATH_W) as *mut u16;
            let n = w.len().min(PATH_CAP - 1);
            std::ptr::copy_nonoverlapping(w.as_ptr(), dst, n);
            *dst.add(n) = 0;
            PATH_LEN = n;
            // Die quietly: our log is the record, not a Windows Error Reporting dialog.
            SetErrorMode(SEM_NOGPFAULTERRORBOX);
            SetUnhandledExceptionFilter(Some(unhandled));
        }
        trace(&format!("fault handler armed, crash log {}", p.display()));
    }

    unsafe extern "system" fn unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
        let (code, addr) = if info.is_null() {
            (0u32, 0usize)
        } else {
            let rec = unsafe { (*info).ExceptionRecord };
            if rec.is_null() { (0u32, 0usize) } else { unsafe { ((*rec).ExceptionCode as u32, (*rec).ExceptionAddress as usize) } }
        };
        let mut buf = [0u8; 512];
        let n = fmt(&mut buf, code, addr);
        raw_write(&buf[..n]);
        // 1 == EXCEPTION_EXECUTE_HANDLER: stop here rather than letting a
        // second-chance handler or the debugger take over.
        1
    }

    /// No allocator, no formatting machinery: hex and decimal by hand.
    fn fmt(buf: &mut [u8; 512], code: u32, addr: usize) -> usize {
        let mut s = String::new();
        let name = match code {
            c if c == EXCEPTION_ACCESS_VIOLATION as u32 => "ACCESS_VIOLATION",
            c if c == EXCEPTION_STACK_OVERFLOW as u32 => "STACK_OVERFLOW",
            c if c == EXCEPTION_INT_DIVIDE_BY_ZERO as u32 => "INT_DIVIDE_BY_ZERO",
            _ => "",
        };
        let _ = write!(s, "\nFAULT: exception code 0x{code:08X} {name} at address 0x{addr:016X}\n");
        let _ = write!(
            s,
            "This is a crash below Rust (a bad pointer, a stack overflow, a Win32 misuse),\nnot a panic, so there is no backtrace. Run `fsearch doctor` to find the stage.\n"
        );
        let b = s.as_bytes();
        let n = b.len().min(buf.len());
        buf[..n].copy_from_slice(&b[..n]);
        n
    }

    fn raw_write(bytes: &[u8]) {
        unsafe {
            if PATH_LEN == 0 {
                return;
            }
            let h: HANDLE = CreateFileW(
                std::ptr::addr_of!(PATH_W) as *const u16,
                GENERIC_WRITE,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return;
            }
            // Append by seeking to the end; OPEN_ALWAYS leaves what is there.
            SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END);
            let mut written = 0u32;
            WriteFile(h, bytes.as_ptr(), bytes.len() as u32, &mut written, std::ptr::null_mut());
            CloseHandle(h);
        }
    }
}
