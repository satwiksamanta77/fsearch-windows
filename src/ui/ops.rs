//! The file operations the explorer offers, done the way Windows does them.
//!
//! Delete goes through `SHFileOperation` so it lands in the Recycle Bin and can
//! be undone; opening goes through `ShellExecute` so file associations, UAC
//! prompts and "run as administrator" all behave as they do in Explorer. Copies
//! and moves are ours, so they can be reported and cancelled.

use std::path::{Path, PathBuf};

/// Open with the registered handler, exactly like double-clicking in Explorer.
pub fn open(p: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        let w: Vec<u16> = p.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        let h = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), w.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
        // Anything <= 32 is an error code, not a handle.
        if (h as usize) <= 32 {
            return Err(format!("Windows refused to open it (code {})", h as usize));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(cmd).arg(p).status().map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Select the file in a real Explorer window.
pub fn reveal(p: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer").arg("/select,").arg(p).spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        open(p.parent().unwrap_or(p))
    }
}

/// Show the shell's own properties sheet.
pub fn properties(p: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        let w: Vec<u16> = p.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let verb: Vec<u16> = "properties\0".encode_utf16().collect();
        let h = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), w.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
        if (h as usize) <= 32 {
            return Err(format!("could not open properties (code {})", h as usize));
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let _ = p;
        Err("the properties sheet is Windows-only".into())
    }
}

/// Send paths to the Recycle Bin, so a mistake is undoable.
pub fn delete(paths: &[PathBuf]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW};
        // pFrom is a list of NUL-terminated paths with an extra NUL at the end.
        let mut from: Vec<u16> = Vec::new();
        for p in paths {
            from.extend(p.to_string_lossy().encode_utf16());
            from.push(0);
        }
        from.push(0);
        let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
        op.wFunc = FO_DELETE;
        op.pFrom = from.as_ptr();
        op.fFlags = (FOF_ALLOWUNDO | FOF_NOERRORUI | FOF_SILENT) as u16;
        let rc = unsafe { SHFileOperationW(&mut op) };
        if rc != 0 {
            return Err(format!("Windows could not delete it (code {rc})"));
        }
        if op.fAnyOperationsAborted != 0 {
            return Err("the delete was cancelled".into());
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        let mut errs = Vec::new();
        for p in paths {
            let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
            if let Err(e) = r {
                errs.push(format!("{}: {e}", p.display()));
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
    }
}

pub fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("rename: {e}"))
}

pub fn new_folder(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).map_err(|e| format!("new folder: {e}"))?;
    Ok(p)
}

/// Copy, recursing into directories. Returns the number of files copied.
pub fn copy(from: &Path, to_dir: &Path) -> Result<usize, String> {
    let name = from.file_name().map(|n| n.to_os_string()).ok_or("no file name")?;
    let to = to_dir.join(name);
    let md = std::fs::symlink_metadata(from).map_err(|e| e.to_string())?;
    if md.is_dir() {
        std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
        let mut n = 0;
        for e in std::fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
            n += copy(&e.path(), &to)?;
        }
        Ok(n)
    } else {
        std::fs::copy(from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        Ok(1)
    }
}

/// Move: a rename when the destination is on the same volume, otherwise copy
/// then delete (which is what Explorer does too, across drives).
pub fn move_to(from: &Path, to_dir: &Path) -> Result<usize, String> {
    let name = from.file_name().map(|n| n.to_os_string()).ok_or("no file name")?;
    let to = to_dir.join(name);
    match std::fs::rename(from, &to) {
        Ok(()) => Ok(1),
        Err(_) => {
            let n = copy(from, to_dir)?;
            delete(&[from.to_path_buf()])?;
            Ok(n)
        }
    }
}

/// "Text Document", "PNG File" and friends — read straight from the registry
/// the way Explorer does, with the extension as a fallback.
pub fn type_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "File folder".to_string();
    }
    let ext = ext_of(name);
    if ext.is_empty() {
        return "File".to_string();
    }
    #[cfg(windows)]
    {
        if let Some(s) = registry_type(ext) {
            return s;
        }
    }
    format!("{} File", ext.to_ascii_uppercase())
}

#[cfg(windows)]
fn registry_type(ext: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};
    // HKCR\.png -> "pngfile", then HKCR\pngfile -> "PNG File".
    let probe = |key: &str| -> Option<String> {
        let mut sub: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hk: windows_sys::Win32::System::Registry::HKEY = std::ptr::null_mut();
        if unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, sub.as_mut_ptr(), 0, KEY_READ, &mut hk) } != 0 {
            return None;
        }
        let mut kind = 0u32;
        let mut n = 0u32;
        unsafe { RegQueryValueExW(hk, std::ptr::null(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut n) };
        if n == 0 || n > 1024 {
            unsafe { RegCloseKey(hk) };
            return None;
        }
        let mut buf = vec![0u8; n as usize + 2];
        let r = unsafe { RegQueryValueExW(hk, std::ptr::null(), std::ptr::null(), &mut kind, buf.as_mut_ptr(), &mut n) };
        unsafe { RegCloseKey(hk) };
        if r != 0 {
            return None;
        }
        let w: Vec<u16> = buf[..n as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let s = String::from_utf16_lossy(&w);
        let s = s.trim_end_matches('\0').trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let prog = probe(&format!(".{ext}"))?;
    probe(&prog).or(Some(prog))
}

/// The extension of a file name, lowercased by callers.
pub fn ext_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => &name[i + 1..],
        _ => "",
    }
}

pub fn human_size(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", U[i])
    } else {
        format!("{v:.0} {}", U[i])
    }
}

/// Seconds since the epoch to a short local stamp, as Explorer shows it.
pub fn human_time(secs: u64) -> String {
    if secs == 0 {
        return String::new();
    }
    // No chrono dependency: shift to local time, then civil-from-days.
    let local = (secs as i64 + local_offset_secs()).max(0) as u64;
    let days = (local / 86400) as i64;
    let rem = (local % 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Minutes west of UTC from the OS, cached: asking per row would cost a syscall
/// each. Zero (UTC) where the platform gives us nothing.
fn local_offset_secs() -> i64 {
    static O: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *O.get_or_init(|| {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
            let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
            // TIME_ZONE_ID_INVALID
            if unsafe { GetTimeZoneInformation(&mut tz) } != 0xFFFF_FFFF {
                return -(tz.Bias as i64) * 60;
            }
        }
        0
    })
}
