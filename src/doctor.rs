//! `fsearch doctor`: run each subsystem on its own and say which one fails.
//!
//! A search daemon has a lot of moving parts that can each fail independently —
//! volumes, the bulk directory class, reparse-point handling, memory maps, the
//! change stream, the pipe, the registry — and a crash report that just says
//! "it died" does not tell you which. This walks them in the order startup does,
//! with the real OS error next to each failure, and never lets one panic take
//! the rest down with it.

use crate::diag;
use crate::os;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Report {
    fails: Vec<String>,
    warns: Vec<String>,
}

impl Report {
    /// Run `f` as one named check. A panic is a failure, not a crash.
    fn check(&mut self, name: &str, f: impl FnOnce(&mut Vec<String>) -> Result<(), String>) {
        let mut out = Vec::new();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut out)));
        match r {
            Ok(Ok(())) => {
                println!("  ok    {name}");
                for l in &out {
                    println!("          {l}");
                }
                self.warns.extend(out.iter().filter(|l| l.starts_with("WARNING")).map(|l| format!("{name}: {l}")));
            }
            Ok(Err(e)) => {
                println!("  FAIL  {name}: {e}");
                for l in &out {
                    println!("          {l}");
                }
                self.fails.push(format!("{name}: {e}"));
            }
            Err(_) => {
                println!("  PANIC {name} (see crash.log for the backtrace)");
                self.fails.push(format!("{name}: panicked"));
            }
        }
        let _ = std::io::stdout().flush();
    }
}

pub fn run() -> i32 {
    diag::init();
    println!("fsearch {} doctor — {} {}", diag::VERSION, std::env::consts::OS, std::env::consts::ARCH);
    println!("logs: {}  (trace.log, crash.log)", diag::log_dir_path().display());
    let mut r = Report { fails: Vec::new(), warns: Vec::new() };

    r.check("environment", |o| {
        for k in ["USERPROFILE", "HOME", "LOCALAPPDATA", "APPDATA", "FSEARCH_ROOT", "FSEARCH_TRACE", "SystemDrive"] {
            o.push(format!("{k} = {}", std::env::var_os(k).map(|v| v.to_string_lossy().into_owned()).unwrap_or_else(|| "(unset)".into())));
        }
        o.push(format!("home() = {}", crate::cli::home()));
        o.push(format!("data dir = {}", crate::cli::data_dir().display()));
        o.push(format!("pipe = {}", os::endpoint()));
        o.push(format!("elevated = {}", os::has_full_disk_access()));
        Ok(())
    });

    r.check("data dir writable", |o| {
        let d = crate::cli::data_dir();
        std::fs::create_dir_all(&d).map_err(|e| format!("create_dir_all {}: {e}", d.display()))?;
        let p = d.join(".doctor-probe");
        std::fs::write(&p, b"x").map_err(|e| format!("write {}: {e}", p.display()))?;
        std::fs::remove_file(&p).ok();
        o.push(d.display().to_string());
        Ok(())
    });

    // Big allocations go to VirtualAlloc and back; if that is wrong nothing
    // else can work, because the index build allocates through it.
    r.check("allocator (large + small)", |o| {
        use std::alloc::{GlobalAlloc, Layout};
        let a = os::MmapAlloc;
        for size in [1usize << 20, 8 << 20, 64 << 20] {
            let l = Layout::from_size_align(size, 8).unwrap();
            let p = unsafe { a.alloc(l) };
            if p.is_null() {
                return Err(format!("alloc of {size} bytes returned null"));
            }
            // Touch the first and last byte: a committed-range bug shows here.
            unsafe {
                *p = 1;
                *p.add(size - 1) = 2;
            }
            unsafe { a.dealloc(p, l) };
        }
        let l = Layout::from_size_align(64, 8).unwrap();
        let p = unsafe { a.alloc(l) };
        if p.is_null() {
            return Err("small alloc returned null".into());
        }
        unsafe { a.dealloc(p, l) };
        // Vec growth exercises realloc across the 1 MiB boundary.
        let mut v: Vec<u8> = Vec::new();
        for _ in 0..3_000_000 {
            v.push(7);
        }
        o.push(format!("grew a Vec to {} bytes", v.len()));
        Ok(())
    });

    r.check("volumes", |o| {
        let v = os::volumes();
        if v.is_empty() {
            return Err("no volumes to index".into());
        }
        for x in &v {
            o.push(format!("{} (stattrs {})", String::from_utf8_lossy(x), os::lstat(x).is_some()));
        }
        Ok(())
    });

    // The bulk directory class is the fast path; the FindFirstFileExW fallback
    // matters on filesystems without file ids, so report which one ran.
    r.check("directory listing", |o| {
        let v = os::volumes();
        let root = v.first().ok_or("no volume")?;
        let h = os::open_dir(root).ok_or_else(|| diag::os_err(&format!("open_dir {}", String::from_utf8_lossy(root))))?;
        let mut l = crate::walk::Listing::new(0);
        os::read_dir_batch(&h, &mut l);
        o.push(format!("{} entries in {}", l.ents.len(), String::from_utf8_lossy(root)));
        if l.ents.is_empty() {
            return Err("listed nothing — the bulk class and the fallback both came up empty".into());
        }
        let mut dirs = 0;
        for e in l.ents.iter().take(400) {
            let nm = String::from_utf8_lossy(l.name(e)).into_owned();
            let kind = match e.kind & 3 {
                crate::walk::KIND_DIR => "dir",
                crate::walk::KIND_FILE => "file",
                crate::walk::KIND_LINK => "link",
                _ => "other",
            };
            o.push(format!(
                "  {nm}  {kind}{}{}{} size={} mtime={}",
                if e.kind & crate::walk::FLAG_HIDDEN != 0 { " hidden" } else { "" },
                if e.kind & crate::walk::FLAG_MOUNT != 0 { " mount/nofollow" } else { "" },
                if e.kind & crate::walk::FLAG_CLOUD != 0 { " cloud" } else { "" },
                e.size,
                e.mtime
            ));
            if e.kind & 3 == crate::walk::KIND_DIR {
                dirs += 1;
            }
        }
        o.push(format!("{} of the first {} are directories", dirs, l.ents.len().min(400)));
        // Descend one level, which is where a path-building bug would show.
        if let Some(e) = l.ents.iter().find(|e| e.kind & 3 == crate::walk::KIND_DIR && e.kind & crate::walk::FLAG_MOUNT == 0) {
            let child = os::join(root, l.name(e));
            match os::open_dir(&child) {
                Some(h2) => {
                    let mut l2 = crate::walk::Listing::new(1);
                    os::read_dir_batch(&h2, &mut l2);
                    o.push(format!("  descended into {}: {} entries", String::from_utf8_lossy(&child), l2.ents.len()));
                }
                None => o.push(format!("  could not open {}: {}", String::from_utf8_lossy(&child), diag::os_err("open_dir"))),
            }
        }
        Ok(())
    });

    r.check("attributes (lstat)", |o| {
        let root = os::volumes().into_iter().next().ok_or("no volume")?;
        let s = os::lstat(&root).ok_or_else(|| diag::os_err(&format!("lstat {}", String::from_utf8_lossy(&root))))?;
        o.push(format!("{} kind={} size={} mtime={}", String::from_utf8_lossy(&root), s.kind, s.size, s.mtime));
        if s.kind & 3 != crate::walk::KIND_DIR {
            return Err("the volume root did not come back as a directory".into());
        }
        if s.mtime == 0 {
            o.push("WARNING: mtime came back 0 — FILETIME conversion may be wrong".into());
        }
        Ok(())
    });

    r.check("memory map", |o| {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("fsearch-doctor-{}.bin", std::process::id()));
        let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).map_err(|e| format!("write probe: {e}"))?;
        let f = std::fs::File::open(&p).map_err(|e| format!("open probe: {e}"))?;
        let m = unsafe { memmap2::Mmap::map(&f) }.map_err(|e| format!("mmap probe: {e}"))?;
        if m.len() != bytes.len() || m[1234] != bytes[1234] {
            return Err("mapped contents do not match what was written".into());
        }
        let anon = memmap2::MmapMut::map_anon(1 << 20).map_err(|e| format!("anonymous mmap: {e}"))?;
        if anon[999] != 0 {
            return Err("anonymous map was not zero-filled".into());
        }
        drop(anon);
        drop(m);
        std::fs::remove_file(&p).ok();
        o.push(format!("mapped {} bytes and read one back", bytes.len()));
        Ok(())
    });

    r.check("index build + path/lookup", |_o| {
        // A hand-made tree, so this tests the layout without touching the disk.
        use crate::walk::{KIND_DIR, KIND_FILE, Listing};
        let mut ls = vec![Listing::new(0), Listing::new(1), Listing::new(2)];
        ls[0].push(b"C:", KIND_DIR, 0, 100);
        ls[0].ents[0].child = 1;
        ls[1].push(b"Users", KIND_DIR, 0, 100);
        ls[1].ents[0].child = 2;
        ls[2].push(b"main.rs", KIND_FILE, 42, 12345);
        ls[2].push(b"zzz.txt", KIND_FILE, 7, 999);
        let idx = crate::index::Index::build(ls, 1, crate::query::now_secs(), b"C:\\Users");
        let mut p = Vec::new();
        let e = idx.lookup(b"C:\\Users\\main.rs").ok_or("lookup of C:\\Users\\main.rs failed")?;
        idx.path(e as usize, &mut p);
        if p != b"C:\\Users\\main.rs" {
            return Err(format!("path() rendered {:?}", String::from_utf8_lossy(&p)));
        }
        if idx.size_of(e as usize) != 42 || idx.mtime()[e as usize] != 12345 {
            return Err("size/mtime did not survive the round trip".into());
        }
        if idx.lookup(b"C:\\USERS\\MAIN.RS") != Some(e) {
            return Err("case-insensitive lookup failed".into());
        }
        if idx.lookup(b"C:\\nope").is_some() {
            return Err("lookup of a missing path succeeded".into());
        }
        Ok(())
    });

    r.check("pipe round trip", |o| {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let mut l = match os::listen() {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Err(format!("listen: {e}")));
                    return;
                }
            };
            match l.accept() {
                Ok(mut c) => {
                    let _ = tx.send(Ok(()));
                    use std::io::{BufRead, BufReader};
                    let r = c.try_clone();
                    if let Ok(r) = r {
                        if let Some(line) = BufReader::new(r).lines().next().and_then(|l| l.ok()) {
                            let _ = writeln!(c, "echo:{line}");
                            let _ = c.flush();
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("accept: {e}")));
                }
            }
        });
        // The instance only exists once accept() has been called.
        let mut c = None;
        for _ in 0..100 {
            if let Some(x) = os::connect_once() {
                c = Some(x);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut c = c.ok_or_else(|| "could not connect to our own pipe in 2s".to_string())?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|e| format!("{e}"))??;
        writeln!(c, "hello").map_err(|e| format!("write: {e}"))?;
        c.flush().ok();
        if !c.wait_readable(Duration::from_secs(5)) {
            return Err("wrote to the pipe but nothing came back".into());
        }
        // Read a byte at a time: no BufReader, so nothing is read ahead and
        // lost, and Conn only has to be Read.
        let mut line: Vec<u8> = Vec::new();
        let mut b = [0u8; 1];
        loop {
            match std::io::Read::read(&mut c, &mut b) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if b[0] == b'\n' {
                        break;
                    }
                    line.push(b[0]);
                }
            }
        }
        let line = String::from_utf8_lossy(&line).into_owned();
        if !line.contains("echo:hello") {
            return Err(format!("unexpected reply {line:?}"));
        }
        o.push(os::endpoint());
        let _ = t.join();
        Ok(())
    });

    r.check("change stream (create + delete)", |o| {
        let dir = watch_probe_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create probe dir: {e}"))?;
        let f = dir.join(format!("fsearch-probe-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&f);
        let (tx, rx) = std::sync::mpsc::channel();
        let stream = os::watch(os::current_pos(), 0.1, tx);
        // Let a recursive watch finish registering before touching the disk.
        std::thread::sleep(Duration::from_secs(3));

        // Takes its arguments rather than capturing them, so `o` stays usable
        // between calls.
        let drain = |rx: &std::sync::mpsc::Receiver<Vec<os::Event>>, secs: u64, o: &mut Vec<String>| -> usize {
            let mut n = 0;
            let end = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < end {
                if let Ok(batch) = rx.recv_timeout(Duration::from_millis(200)) {
                    for e in &batch {
                        if e.flags & os::HISTORY_DONE != 0 {
                            continue;
                        }
                        n += 1;
                        if n <= 4 {
                            o.push(format!("event: {} flags={:#x}", String::from_utf8_lossy(&e.path), e.flags));
                        }
                    }
                    if n > 0 {
                        break;
                    }
                }
            }
            n
        };

        // First from this process, then from a second one. Some change
        // notification implementations ignore a process's own writes; a search
        // daemon only ever cares about other programs, so knowing which of the
        // two works is the useful answer.
        std::fs::write(&f, b"probe").map_err(|e| format!("write probe file: {e}"))?;
        let t = Instant::now();
        let self_events = drain(&rx, 6, o);
        o.push(format!("write from this process: {self_events} event(s) in {:.1?}", t.elapsed()));

        let _ = std::fs::remove_file(&f);
        let child_events = if spawn_helper("__touch", &f) {
            drain(&rx, 6, o)
        } else {
            o.push("could not spawn a helper process".into());
            0
        };
        o.push(format!("write from a second process: {child_events} event(s)"));

        if child_events > 0 {
            if spawn_helper("__delete", &f) {
                let deletes = drain(&rx, 5, o);
                o.push(format!("delete from a second process: {deletes} event(s)"));
                if deletes == 0 {
                    o.push("WARNING: the delete produced no event; removals may lag until that folder is touched again".into());
                }
            }
        }
        let _ = std::fs::remove_file(&f);

        let fails = stream.failures();
        os::record_watch_failures(&fails);
        for x in &fails {
            o.push(format!("watch problem: {x}"));
        }
        drop(stream);
        if self_events == 0 && child_events == 0 {
            // Could not even arm the watch: that is actionable (access denied,
            // an unsupported filesystem, no volume to watch).
            if !fails.is_empty() {
                return Err(format!("the change stream could not be started{}", os::watch_failures_note()));
            }
            // Armed and silent. On Windows this means live updates are broken.
            // Under Wine it also happens in a short-lived process, where the
            // recursive watch never finishes registering, while a long-lived
            // daemon watching the same folder works fine — so say both.
            o.push("WARNING: the watch armed but reported nothing for 12s. Live updates will not work.".into());
            o.push("         If this is Wine, a short-lived process is expected to show this;".into());
            o.push("         re-check with `fsearchd.exe serve` running and a file created from outside.".into());
        }
        if child_events == 0 && self_events > 0 {
            o.push("WARNING: only this process's own writes were reported".into());
        }
        Ok(())
    });

    r.check("content index (build + grep)", |o| {
        let dir = std::env::temp_dir().join(format!("fsearch-doctor-content-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        let home = os::bytes_from_path(&dir);
        let src = dir.join("probe.rs");
        std::fs::write(&src, "fn doctor_probe_symbol() {}\n").map_err(|e| format!("write: {e}"))?;
        let mut docs = crate::content::Docs::default();
        docs.push(&os::join(&home, b"probe.rs"), 28, crate::query::now_secs());
        docs.sort();
        let mut c = crate::content::Content::open(dir.join("content"));
        let id = c.alloc_id();
        let seg = crate::content::build_segment(&c.dir, id, &docs, 0..docs.len()).ok_or("build_segment returned nothing")?;
        c.push(seg);
        let filt = crate::query::Query::parse("", &String::from_utf8_lossy(&home)).unwrap();
        let g = crate::content::Grep::new("doctor_probe_symbol", crate::query::GrepMode::Literal).unwrap();
        let res = c.search(&g, &filt);
        if res.files.is_empty() {
            return Err(format!("indexed {} doc(s) but grep found nothing", c.docs()));
        }
        let g2 = crate::content::Grep::new("doctor_probe_symbol", crate::query::GrepMode::Symbol).unwrap();
        if c.search(&g2, &filt).files.is_empty() {
            return Err("literal grep worked but sym: did not".into());
        }
        o.push(format!("{} doc(s), literal and sym: both matched", c.docs()));
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    });

    r.check("registry (read-only)", |o| {
        o.push(format!("sign-in entry: {}", os::login_state()));
        Ok(())
    });

    // Only meaningful when indexing one folder rather than the machine.
    if os::redirect().is_some() {
        r.check("full crawl of FSEARCH_ROOT", |o| {
            let t = Instant::now();
            let ls = crate::walk::scan_volumes(4);
            let n: usize = ls.iter().map(|l| l.ents.len()).sum();
            o.push(format!("{} listings, {} entries in {:.2?}", ls.len(), n, t.elapsed()));
            if ls.is_empty() || n == 0 {
                return Err("the crawl produced nothing".into());
            }
            Ok(())
        });
    }

    println!();
    if !r.warns.is_empty() {
        println!("{} warning(s):", r.warns.len());
        for w in &r.warns {
            println!("  - {w}");
        }
        println!();
    }
    if r.fails.is_empty() {
        println!("doctor: no check failed. If fsearch still dies, the trace is in");
        println!("  {}", diag::trace_path().display());
        println!("and any fault is in");
        println!("  {}", diag::crash_path().display());
        println!("Send both, plus the output of:  fsearch status");
        0
    } else {
        println!("doctor: {} check(s) FAILED", r.fails.len());
        for f in &r.fails {
            println!("  - {f}");
        }
        println!("\nlogs: {}\n      {}", diag::trace_path().display(), diag::crash_path().display());
        1
    }
}

/// Somewhere a probe file can be written that the change stream is definitely
/// watching. The data dir, not TEMP: TEMP can sit outside the watched volume
/// (a redirected or junctioned temp folder), which would look like a dead
/// stream when nothing is wrong with it.
fn watch_probe_dir() -> PathBuf {
    // A directory that already exists, on a volume that is definitely watched:
    // the data dir. Not TEMP (it can live on another volume, or behind a
    // junction) and not a brand-new folder (a lazy watch registration may not
    // have picked it up yet, which would look like a dead stream).
    match os::redirect() {
        Some(r) => os::path_from_bytes(&r),
        None => crate::cli::data_dir(),
    }
}

/// Run our own executable with a hidden helper subcommand, so the change comes
/// from a second process. No shell, so no quoting rules to get wrong.
fn spawn_helper(helper: &str, f: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe)
        .args([helper, &f.to_string_lossy()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
