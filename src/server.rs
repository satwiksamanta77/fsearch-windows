//! The daemon: one `Engine`, answering JSON lines over a named pipe.
//! `fsearch stdio` and the CLI are thin clients.

use crate::os::{self, Conn};
use crate::walk::{KIND_DIR, KIND_FILE, KIND_LINK};
use crate::{Engine, GrepMode, Options, Query};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where clients meet the daemon. On Windows this is a named pipe, so the data
/// dir only holds the lock that keeps one daemon per session.
pub fn socket_path(_dir: &Path) -> String {
    os::endpoint()
}

pub fn serve(dir: PathBuf, home: String) {
    crate::diag::trace(&format!("serve: data dir {}, home {home}", dir.display()));
    // One daemon per pipe. (The engine's own lock decides who writes the index:
    // an app embedding fsearch may own it while the daemon follows.)
    std::fs::create_dir_all(&dir).ok();
    let Ok(lock) = std::fs::File::create(dir.join("socket.lock")) else { return };
    if !os::try_lock(&lock) {
        eprintln!("{} another fsearch daemon is running", crate::query::now_secs());
        return;
    }
    let engine = match Engine::start(Options { dir: dir.clone(), home, skip: None }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{} {e}", crate::query::now_secs());
            return;
        }
    };
    crate::diag::trace("serve: engine started, binding the pipe");
    let mut listener = os::listen().expect("bind pipe");
    eprintln!("{} listening on {}", crate::query::now_secs(), socket_path(&dir));
    crate::diag::trace(&format!("serve: listening on {}", socket_path(&dir)));
    let mut last_err = Instant::now() - Duration::from_secs(60);
    loop {
        match listener.accept() {
            Ok(conn) => {
                let e = engine.clone();
                std::thread::spawn(move || handle(conn, &e));
            }
            Err(err) => {
                // Rate-limited: a pipe that cannot be created would otherwise
                // fill the log while spinning.
                if last_err.elapsed() > Duration::from_secs(10) {
                    last_err = Instant::now();
                    eprintln!("{} accept: {err}", crate::query::now_secs());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn handle(conn: Conn, engine: &Engine) {
    let Ok(r) = conn.try_clone() else { return };
    let mut w = std::io::BufWriter::new(conn);
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let resp = respond(&line, engine);
        if writeln!(w, "{resp}").and_then(|_| w.flush()).is_err() {
            return;
        }
    }
}

fn respond(line: &str, engine: &Engine) -> Value {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let mut out = match run(&v, engine) {
        Ok(r) => r,
        Err(e) => json!({"ok": false, "error": e}),
    };
    out["id"] = id;
    out
}

fn run(v: &Value, engine: &Engine) -> Result<Value, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
    let is_grep = op == "grep"
        || (op == "search"
            && v.get("q").and_then(Value::as_str).is_some_and(|q| ["grep:", "regex:", "sym:", "content:", "symbol:"].iter().any(|k| q.contains(k))));
    match op {
        "ping" => Ok(json!({"ok": true})),
        "save" => {
            engine.save();
            Ok(json!({"ok": true, "scheduled": true}))
        }
        _ if is_grep => grep(v, engine),
        "status" => {
            let s = engine.status();
            if !s.ready {
                return Err(INDEXING.into());
            }
            let mut v = serde_json::to_value(s).map_err(|e| e.to_string())?;
            v["ok"] = true.into();
            Ok(v)
        }
        "search" => {
            let q = parse_request(v, engine.home())?;
            let t = Instant::now();
            let found = engine.search(&q)?;
            let took = t.elapsed().as_micros() as u64;
            let hits: Vec<Value> = found
                .iter()
                .map(|f| {
                    json!({
                        "path": f.path.to_string_lossy(),
                        "kind": kind_name(f.kind),
                        "size": f.size,
                        "mtime": f.mtime,
                        "score": f.score,
                    })
                })
                .collect();
            Ok(json!({"ok": true, "took_us": took, "hits": hits}))
        }
        _ => Err(format!("unknown op {op}")),
    }
}

const INDEXING: &str = "indexing (the first run scans the whole disk; try again in a minute)";

/// Content search. The pattern comes from `pattern` (+ `mode`) or from a
/// `grep:`/`regex:`/`sym:` filter in `q`; the rest of the query narrows which
/// files are read.
fn grep(v: &Value, engine: &Engine) -> Result<Value, String> {
    let mut q = parse_request(v, engine.home())?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("regex") => GrepMode::Regex,
        Some("symbol") => GrepMode::Symbol,
        Some("literal") => GrepMode::Literal,
        Some(m) => return Err(format!("unknown mode {m}")),
        None => q.grep_mode,
    };
    let pattern = v.get("pattern").and_then(Value::as_str).map(str::to_string).or(q.grep.take()).ok_or("grep needs a pattern")?;
    let mut g = crate::Grep::new(&pattern, mode)?;
    if let Some(n) = v.get("per_file").and_then(Value::as_u64) {
        g.max_per_file = n as usize;
    }
    if let Some(ms) = v.get("budget_ms").and_then(Value::as_u64) {
        g.budget = (ms > 0).then(|| Duration::from_millis(ms));
    }
    let t = Instant::now();
    let (r, indexed) = engine.grep(&q, &g)?;
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "path": String::from_utf8_lossy(&f.path),
                "matches": f.lines.iter().map(|(n, t)| json!({"line": n, "text": t})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "took_us": t.elapsed().as_micros() as u64,
        "source": if indexed { "index" } else { "scan" },
        "candidates": r.candidates,
        "read": r.read,
        "complete": r.complete,
        "indexing": engine.status().content_pending,
        "files": files,
    }))
}

/// `q` is the query language; any filter key may also be given as its own JSON
/// field (`{"q": "main", "ext": "rs", "in": "~/dev"}`).
fn parse_request(v: &Value, home: &str) -> Result<Query, String> {
    let mut q = Query::parse(v.get("q").and_then(Value::as_str).unwrap_or(""), home)?;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "limit" {
                q.limit = val.as_u64().ok_or("limit must be a number")? as usize;
            } else {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                q.filter(k, &s, home)?;
            }
        }
    }
    Ok(q)
}

fn kind_name(k: u8) -> &'static str {
    match k & 3 {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        _ => "other",
    }
}

/// Connect to the daemon, starting it if it isn't running.
///
/// A pipe connection is only handed back once it has answered a `ping`: while
/// the daemon is still starting, a client can connect to an instance that is
/// about to be closed under it, and the first real request would then fail.
pub fn connect(dir: &Path) -> std::io::Result<Conn> {
    std::fs::create_dir_all(dir).ok();
    // Bounded by a deadline, not a retry count: each attempt can spend the
    // whole ping timeout, so a count alone could mean minutes of waiting.
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut spawned = false;
    while Instant::now() < deadline {
        if let Some(mut c) = os::connect_once() {
            if ping(&mut c) {
                return Ok(c);
            }
        } else if !spawned {
            spawned = true;
            crate::diag::trace("daemon not running; starting it");
            os::spawn_daemon(&dir.join("daemon.log"))?;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("cannot reach {}", os::endpoint())))
}

/// One round trip that proves the daemon is really there. The response is read
/// back, so the connection is clean for the caller's own request.
fn ping(c: &mut Conn) -> bool {
    if writeln!(c, "{{\"op\":\"ping\"}}").and_then(|_| c.flush()).is_err() {
        return false;
    }
    if !c.wait_readable(Duration::from_millis(1500)) {
        return false;
    }
    let mut line = String::new();
    BufReader::new(&mut *c).read_line(&mut line).is_ok_and(|n| n > 0 && line.contains("\"ok\":true"))
}
