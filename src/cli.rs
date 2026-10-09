//! The command line, shared by `fsearch.exe` (a console program) and
//! `fsearchd.exe` (the same code built without a console, so starting the
//! daemon at sign-in never flashes a window).

use crate::{index, live, os, query, server};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

const USAGE: &str = "usage:
  fsearch <query...> [--json]   search (starts the daemon if needed)
  fsearch stdio                 JSON lines on stdin/stdout
  fsearch serve                 run the daemon in the foreground
  fsearch status
  fsearch ui                    open the explorer window
  fsearch doctor                check each subsystem and report which one fails
  fsearch -i                    interactive prompt (what double-clicking gives you)
  fsearch install [--login]      copy to %LOCALAPPDATA%\\Programs\\FSearch; --login also starts
                                the daemon at sign-in (run as administrator to index everything)
  fsearch uninstall             remove the sign-in entry (keeps the index)
  fsearch bench <query...>      time a query in-process against the saved index";

/// The sign-in entry's name, and the installed program's folder name.
const LABEL: &str = "FSearch";

pub fn home() -> String {
    std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| "C:\\".into())
}

pub fn data_dir() -> PathBuf {
    let d = crate::default_dir(&home());
    std::fs::create_dir_all(&d).ok();
    d
}

pub fn run() {
    // Before anything else: a crash we cannot see is a crash we cannot fix.
    crate::diag::init();
    os::enable_backup_privileges();
    // Paths are UTF-16 on Windows and a console defaults to the OEM code page,
    // which would mangle non-ASCII names on the way out.
    os::utf8_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    crate::diag::trace(&format!("args: {}", args.join(" ")));
    let interactive_launch = (args.is_empty() && os::console_is_ours()) || matches!(args.first().map(String::as_str), Some("-i" | "--interactive"));
    let pause =
        !interactive_launch && !matches!(args.first().map(String::as_str), Some("serve") | Some("stdio") | Some("__touch") | Some("__delete"));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(&args)));
    if pause {
        hold_console();
    }
    match r {
        Ok(()) => {}
        Err(_) => {
            // diag's panic hook already wrote crash.log; say so where the user
            // will actually see it.
            eprintln!("fsearch: internal error — details in {}", crate::diag::crash_path().display());
            std::process::exit(1);
        }
    }
}

fn dispatch(args: &[String]) {
    match args.first().map(String::as_str) {
        None => {
            if os::console_is_ours() {
                interactive();
            } else {
                eprintln!("{USAGE}");
            }
        }
        Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("-i" | "--interactive") => interactive(),
        Some("serve") => server::serve(data_dir(), home()),
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
        Some("doctor") => std::process::exit(crate::doctor::run()),
        Some("ui") => {
            // In-process when this IS the windowed binary; otherwise hand off
            // to it so no console window comes along.
            if std::env::current_exe().is_ok_and(|e| e.file_name().is_some_and(|n| n == crate::ui::ui_exe_name())) {
                if let Err(e) = crate::ui::run() {
                    die(&e);
                }
            } else if let Err(e) = crate::ui::launch_detached() {
                die(&format!("could not start the explorer window: {e}"));
            }
        }
        // Hidden helpers for `doctor`: make a change from a *second* process,
        // which is what a search daemon actually observes. Undocumented on
        // purpose; there is no reason to type these by hand.
        Some("__touch") => {
            if let Some(p) = args.get(1) {
                let _ = std::fs::create_dir_all(std::path::Path::new(p).parent().unwrap_or(std::path::Path::new(".")));
                let _ = std::fs::write(p, b"probe");
            }
        }
        Some("__delete") => {
            if let Some(p) = args.get(1) {
                let _ = std::fs::remove_file(p);
            }
        }
        Some("bench") => bench(&args[1..].join(" ")),
        Some("install") => install(args.iter().any(|a| a == "--login")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
            print_one(&serde_json::json!({"q": q.join(" ")}), json);
        }
    }
}

/// Double-clicking `fsearch.exe` opens a console that closes the instant we
/// return, which looks exactly like a crash. If we are the only process on that
/// console, wait for a keypress instead.
fn hold_console() {
    let _ = std::io::stdout().flush();
    if os::console_is_ours() {
        eprint!("\nPress Enter to close...");
        let _ = std::io::stderr().flush();
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
}

fn print_one(req: &serde_json::Value, raw: bool) {
    if let Err(e) = try_request(req, raw) {
        die(&e);
    }
}

/// One request/response. Errors come back instead of ending the process, so the
/// interactive prompt can report them and keep going.
fn try_request(req: &serde_json::Value, raw: bool) -> Result<(), String> {
    crate::diag::trace(&format!("request: {req}"));
    let mut s = server::connect(&data_dir()).map_err(|e| format!("cannot reach daemon: {e}"))?;
    crate::diag::trace("connected to the daemon");
    writeln!(s, "{req}").and_then(|_| s.flush()).map_err(|_| "lost the connection to the daemon")?;
    let mut line = String::new();
    BufReader::new(&mut s).read_line(&mut line).unwrap_or(0);
    if line.trim().is_empty() {
        return Err("daemon closed the connection".into());
    }
    if raw {
        print!("{line}");
        let _ = std::io::stdout().flush();
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        return Err(v["error"].as_str().unwrap_or("error").to_string());
    }
    let mut out = std::io::stdout().lock();
    let mut n = 0;
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
        n += 1;
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
            n += 1;
        }
    }
    let _ = out.flush();
    if n == 0 {
        let _ = writeln!(out, "(no matches)");
    }
    Ok(())
}

/// What double-clicking gets you: the usage, then a prompt. A CLI with no
/// arguments has nothing to do, and a window that prints help and closes is
/// indistinguishable from a crash, so stay and take queries instead.
fn interactive() {
    println!("{USAGE}");
    println!();
    println!("Interactive mode — type a query and press Enter.");
    println!("  readme            fuzzy name search (typos forgiven)");
    println!("  ext:rs main       filters: ext: type: kind: in: size: mtime: re: path:");
    println!("  grep:todo         search inside files   |   sym:name   where it is defined");
    println!("  status            index progress        |   doctor     self-test");
    println!("  install           copy beside itself    |   help       this text");
    println!("First run crawls every disk, which can take a minute; `status` shows progress.");
    println!("Blank line or `exit` to quit.");
    let stdin = std::io::stdin();
    loop {
        print!("\nfsearch> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line == "exit" || line == "quit" {
            break;
        }
        match line {
            "help" | "?" | "-h" | "--help" => println!("{USAGE}"),
            "status" => {
                if let Err(e) = try_request(&serde_json::json!({"op": "status"}), false) {
                    println!("fsearch: {e}");
                } else {
                    let _ = try_request(&serde_json::json!({"op": "status"}), true);
                }
            }
            "doctor" => {
                crate::doctor::run();
            }
            "install" => install(false),
            "uninstall" => uninstall(),
            _ => {
                let raw = line.split_whitespace().any(|a| a == "--json");
                let q: Vec<&str> = line.split_whitespace().filter(|a| *a != "--json").collect();
                let t = std::time::Instant::now();
                match try_request(&serde_json::json!({"q": q.join(" ")}), raw) {
                    Ok(()) => println!("({:.0} ms)", t.elapsed().as_secs_f64() * 1000.0),
                    Err(e) if e.contains("indexing") => {
                        println!("fsearch: {e}");
                        println!("       the first crawl is still running — try `status`, then ask again.");
                    }
                    Err(e) => println!("fsearch: {e}"),
                }
            }
        }
    }
    println!("bye");
}

fn stdio() {
    let s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut up = s.try_clone().unwrap();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if writeln!(up, "{line}").and_then(|_| up.flush()).is_err() {
                break;
            }
        }
        up.shutdown_write();
    });
    let mut out = std::io::stdout().lock();
    let mut r = BufReader::new(s);
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn bench(qs: &str) {
    let idx = index::Index::load(&data_dir().join("index.bin")).unwrap_or_else(|| die("no index yet; run fsearch serve"));
    let live = live::Live::new(idx);
    let q = query::Query::parse(qs, &home()).unwrap_or_else(|e| die(&e));
    let s = query::Searcher { live: &live };
    let mut times = Vec::new();
    let mut hits = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        hits = s.search(&q);
        times.push(t.elapsed());
    }
    let mut p = Vec::new();
    for h in hits.iter().take(10) {
        match &h.over {
            Some(path) => println!("{:5} {}", h.score, String::from_utf8_lossy(path)),
            None => {
                live.base.path(h.idx as usize, &mut p);
                println!("{:5} {}", h.score, String::from_utf8_lossy(&p));
            }
        }
    }
    times.sort();
    eprintln!("first {:.2?}  median {:.2?}  min {:.2?}", times[0].max(times[times.len() - 1]), times[times.len() / 2], times[0]);
}

fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home()).join("AppData").join("Local"))
        .join("Programs")
        .join(LABEL)
}

fn install(login: bool) {
    let dir = install_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(&format!("mkdir {}: {e}", dir.display())));
    let mut installed = Vec::new();
    for name in ["fsearch.exe", "fsearchd.exe", "fsearchui.exe"] {
        let src = std::env::current_exe().unwrap().with_file_name(name);
        if !src.is_file() {
            continue;
        }
        let dst = dir.join(name);
        // Replace, never overwrite in place: the daemon may be running from it.
        let _ = std::fs::remove_file(&dst);
        if let Err(e) = std::fs::copy(&src, &dst) {
            if name == "fsearch.exe" {
                die(&format!("copy {}: {e}", dst.display()));
            }
            continue;
        }
        installed.push(dst);
    }
    if installed.is_empty() {
        // Nothing to copy from (e.g. already installed): put ourselves there.
        let dst = dir.join("fsearch.exe");
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(std::env::current_exe().unwrap(), &dst).unwrap_or_else(|e| die(&format!("copy: {e}")));
        installed.push(dst);
    }
    let bin = installed.first().unwrap().clone();
    if !login {
        println!("installed {}; the daemon starts on first use", bin.display());
        return;
    }
    let daemon = installed.iter().find(|p| p.file_name().is_some_and(|n| n == "fsearchd.exe")).unwrap_or(&bin);
    if let Err(e) = os::set_login(daemon, true) {
        die(&format!("could not add the sign-in entry: {e}"));
    }
    println!("installed {} (starts at sign-in)", bin.display());
}

fn uninstall() {
    if let Err(e) = os::set_login(std::path::Path::new("fsearchd.exe"), false) {
        eprintln!("fsearch: could not remove the sign-in entry: {e}");
    }
    println!("removed the sign-in entry; index kept in {}", data_dir().display());
}

fn die(msg: &str) -> ! {
    eprintln!("fsearch: {msg}");
    crate::diag::trace(&format!("fatal: {msg}"));
    hold_console();
    std::process::exit(1)
}
