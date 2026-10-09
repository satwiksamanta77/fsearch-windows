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
    os::enable_backup_privileges();
    // Paths are UTF-16 on Windows and a console defaults to the OEM code page,
    // which would mangle non-ASCII names on the way out.
    os::utf8_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("serve") => server::serve(data_dir(), home()),
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
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

fn print_one(req: &serde_json::Value, raw: bool) {
    let mut s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    if writeln!(s, "{req}").and_then(|_| s.flush()).is_err() {
        die("lost the connection to the daemon");
    }
    let mut line = String::new();
    BufReader::new(&mut s).read_line(&mut line).unwrap_or(0);
    if line.trim().is_empty() {
        die("daemon closed the connection");
    }
    if raw {
        print!("{line}");
        let _ = std::io::stdout().flush();
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        die(v["error"].as_str().unwrap_or("error"));
    }
    let mut out = std::io::stdout().lock();
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
        }
    }
    let _ = out.flush();
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
    for name in ["fsearch.exe", "fsearchd.exe"] {
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
    std::process::exit(1)
}
