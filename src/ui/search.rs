//! The explorer's link to the fsearch daemon.
//!
//! Searching has to stay off the UI thread: the first query may have to start
//! the daemon and wait for it, and a cold crawl can take a minute. So a worker
//! thread owns the pipe connection, queries arrive over a channel, and the UI
//! polls for whatever the latest one produced — which also means a slow query
//! can never be shown over the results of a newer one.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One result row. `lines` is filled for content (`grep:`) matches.
#[derive(Clone)]
pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub folder: String,
    pub size: u64,
    pub mtime: u32,
    pub kind: String,
    pub score: i64,
    pub lines: Vec<(usize, String)>,
}

impl Row {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

pub struct Search {
    req: Sender<(u64, String, usize)>,
    resp: Arc<Mutex<Receiver<(u64, Answer)>>>,
    seq: u64,
    /// Results on screen, and the query they belong to.
    pub rows: Vec<Row>,
    pub shown_for: String,
    pub busy: bool,
    pub error: Option<String>,
    pub took_ms: f64,
    pub index: String,
    last_status: Instant,
}

#[derive(Clone)]
pub enum Answer {
    Hits { rows: Vec<Row>, took_ms: f64 },
    Err(String),
    Status(String),
}

impl Search {
    pub fn start(data_dir: PathBuf, home: String) -> Search {
        let (req_tx, req_rx) = channel::<(u64, String, usize)>();
        let (resp_tx, resp_rx) = channel::<(u64, Answer)>();
        let resp = Arc::new(Mutex::new(resp_rx));
        let r2 = resp.clone();
        std::thread::Builder::new().name("fsearch-ui-search".into()).spawn(move || worker(data_dir, home, req_rx, resp_tx, r2)).ok();
        Search {
            req: req_tx,
            resp,
            seq: 0,
            rows: Vec::new(),
            shown_for: String::new(),
            busy: false,
            error: None,
            took_ms: 0.0,
            index: "connecting…".into(),
            last_status: Instant::now() - Duration::from_secs(10),
        }
    }

    /// Queue a query. Results are dropped on the floor if a newer one has
    /// already been asked for.
    pub fn submit(&mut self, q: &str, limit: usize) {
        self.seq += 1;
        self.busy = true;
        let _ = self.req.send((self.seq, q.to_string(), limit));
    }

    /// Take whatever the worker has produced. Call once per frame.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let rx = self.resp.clone();
        let g = rx.lock().unwrap();
        loop {
            match g.recv_timeout(Duration::ZERO) {
                Ok((seq, ans)) => {
                    changed = true;
                    match ans {
                        Answer::Status(s) => self.index = s,
                        Answer::Hits { rows, took_ms } => {
                            if seq >= self.seq {
                                self.rows = rows;
                                self.took_ms = took_ms;
                                self.error = None;
                                self.busy = false;
                            }
                        }
                        Answer::Err(e) => {
                            if seq >= self.seq {
                                self.error = Some(e);
                                self.rows.clear();
                                self.busy = false;
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    self.index = "daemon gone".into();
                    self.busy = false;
                    break;
                }
            }
        }
        changed
    }

    /// Ask for index progress, but only occasionally — the status bar does not
    /// need a round trip per frame.
    pub fn maybe_status(&mut self) -> bool {
        if self.last_status.elapsed() < Duration::from_secs(2) {
            return false;
        }
        self.last_status = Instant::now();
        self.seq += 1;
        let _ = self.req.send((self.seq, "\u{0}status".to_string(), 0));
        true
    }
}

fn worker(
    data_dir: PathBuf,
    home: String,
    req: Receiver<(u64, String, usize)>,
    resp: Sender<(u64, Answer)>,
    _r: Arc<Mutex<Receiver<(u64, Answer)>>>,
) {
    let mut conn: Option<crate::os::Conn> = None;
    let mut last_err_at = Instant::now() - Duration::from_secs(60);
    for (seq, q, limit) in req.iter() {
        let is_status = q.starts_with('\u{0}');
        // (Re)connect as needed; a daemon that restarts invalidates the pipe.
        if conn.is_none() && last_err_at.elapsed() > Duration::from_secs(3) {
            match crate::server::connect(&data_dir) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    last_err_at = Instant::now();
                    let _ = resp.send((seq, Answer::Err(format!("cannot reach the daemon: {e}"))));
                    continue;
                }
            }
        }
        let Some(c) = conn.as_mut() else {
            let _ = resp.send((seq, Answer::Err("no connection to the daemon".into())));
            continue;
        };
        let req_json = if is_status { serde_json::json!({"op": "status"}) } else { serde_json::json!({"q": q, "limit": limit}) };
        let line = match roundtrip(c, &req_json) {
            Ok(l) => l,
            Err(e) => {
                conn = None; // stale pipe: reconnect next time
                let _ = resp.send((seq, Answer::Err(e)));
                continue;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = resp.send((seq, Answer::Err(format!("bad reply: {e}"))));
                continue;
            }
        };
        if is_status {
            let _ = resp.send((seq, Answer::Status(describe_status(&v, &home))));
            continue;
        }
        if v["ok"] != true {
            let _ = resp.send((seq, Answer::Err(v["error"].as_str().unwrap_or("error").to_string())));
            continue;
        }
        let took = v["took_us"].as_f64().unwrap_or(0.0) / 1000.0;
        let mut rows = Vec::new();
        for h in v["hits"].as_array().into_iter().flatten() {
            let p = h["path"].as_str().unwrap_or("");
            rows.push(Row {
                name: file_name(p).to_string(),
                folder: parent_of(p, &home),
                path: PathBuf::from(p),
                size: h["size"].as_u64().unwrap_or(0),
                mtime: h["mtime"].as_u64().unwrap_or(0) as u32,
                kind: h["kind"].as_str().unwrap_or("file").to_string(),
                score: h["score"].as_i64().unwrap_or(0),
                lines: Vec::new(),
            });
        }
        for f in v["files"].as_array().into_iter().flatten() {
            let p = f["path"].as_str().unwrap_or("");
            let lines: Vec<(usize, String)> = f["matches"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| (m["line"].as_u64().unwrap_or(0) as usize, m["text"].as_str().unwrap_or("").to_string()))
                .collect();
            rows.push(Row {
                name: file_name(p).to_string(),
                folder: parent_of(p, &home),
                path: PathBuf::from(p),
                size: 0,
                mtime: 0,
                kind: "file".into(),
                score: 0,
                lines,
            });
        }
        let _ = resp.send((seq, Answer::Hits { rows, took_ms: took }));
    }
}

fn roundtrip(c: &mut crate::os::Conn, req: &serde_json::Value) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    writeln!(c, "{req}").and_then(|_| c.flush()).map_err(|e| format!("write to daemon: {e}"))?;
    let mut line = String::new();
    let n = BufReader::new(&mut *c).read_line(&mut line).map_err(|e| format!("read from daemon: {e}"))?;
    if n == 0 {
        return Err("the daemon closed the connection".into());
    }
    Ok(line)
}

fn describe_status(v: &serde_json::Value, _home: &str) -> String {
    if v["ok"] != true {
        return v["error"].as_str().unwrap_or("indexing").to_string();
    }
    let e = v["entries"].as_u64().unwrap_or(0);
    let d = v["content_docs"].as_u64().unwrap_or(0);
    let pending = v["content_pending"].as_u64().unwrap_or(0);
    let owner = if v["owner"] == true { "" } else { " (following)" };
    if pending > 0 {
        format!("{e} entries · indexing contents {pending} left{owner}")
    } else {
        format!("{e} entries · {d} files content-indexed{owner}")
    }
}

fn file_name(p: &str) -> &str {
    match p.rfind(['\\', '/']) {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// The folder shown in the results list, shortened against home.
fn parent_of(p: &str, home: &str) -> String {
    let cut = p.rfind(['\\', '/']).unwrap_or(p.len());
    let dir = &p[..cut];
    if !home.is_empty() && dir.eq_ignore_ascii_case(home) {
        return "~".to_string();
    }
    if let Some(rest) = dir.get(home.len()..)
        && !home.is_empty()
        && dir[..home.len()].eq_ignore_ascii_case(home)
        && (rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/'))
    {
        return format!("~{rest}");
    }
    dir.to_string()
}

/// Open a path with whatever Windows would open it with.
pub fn open_path(p: &Path) -> Result<(), String> {
    crate::ui::ops::open(p)
}
