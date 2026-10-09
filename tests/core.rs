//! End-to-end tests of the ported engine, run against the host backend: a temp
//! folder stands in for the `C:` volume, index paths still look like
//! `C:\...`, so everything but the Win32 calls is the real code path.
//!
//! `FSEARCH_ROOT` is read once per process, so all tests share one root and
//! each gets its own subtree. Run with `--test-threads=1` is not required, but
//! the root is created by whichever test gets there first.

use fsearch::content::{self, Content, Grep};
use fsearch::index::Index;
use fsearch::live::{Applied, Live};
use fsearch::query::{GrepMode, Query, Searcher};
use fsearch::{Engine, Options};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

static SETUP: Once = Once::new();

/// The folder standing in for `C:\`.
fn root() -> PathBuf {
    let p = std::env::temp_dir().join("fsearch-test-root");
    SETUP.call_once(|| {
        // Safe in the test binary: it is the only thing that reads this, and
        // it is read once, before any indexing.
        unsafe { std::env::set_var("FSEARCH_ROOT", &p) };
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
    });
    // The OnceLock in `os::redirect` may already have been read by another
    // test in this process; the value is the same either way.
    p
}

/// A subtree of the test root, cleared first.
fn area(name: &str) -> PathBuf {
    root();
    let p = std::env::temp_dir().join("fsearch-test-root").join(name);
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn write(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// Index path of a file inside `area`: `<area>` is `C:\<name>`.
fn logical(area: &str, rel: &str) -> String {
    format!("C:\\{area}\\{}", rel.replace('/', "\\"))
}

fn build(area: &str) -> Live {
    let ls = fsearch::walk::scan_volumes(4);
    let idx = Index::build(ls, 1, fsearch::query::now_secs(), logical_home(area).as_bytes());
    Live::new(idx)
}

fn logical_home(area: &str) -> String {
    format!("C:\\{area}")
}

fn paths(live: &Live, q: &str) -> Vec<String> {
    let query = Query::parse(q, &logical_home("home")).unwrap();
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for h in (Searcher { live }).search(&query) {
        match &h.over {
            Some(p) => out.push(String::from_utf8_lossy(p).into_owned()),
            None => {
                live.base.path(h.idx as usize, &mut buf);
                out.push(String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }
    out
}

/// A small tree with the properties the ranking and query code care about.
fn make_tree() -> PathBuf {
    let a = area("home");
    write(&a.join("dev/fsearch/src/main.rs"), "fn apply_dir(p: &Path) { walk(p) }\nfn main() {}\n");
    write(&a.join("dev/fsearch/src/engine.rs"), "pub struct Engine;\nfn rebuild_all() {}\n");
    write(&a.join("dev/fsearch/README.md"), "# FSearch\n\nwhole disk search\n");
    write(&a.join("dev/fsearch/Cargo.toml"), "[package]\nname = \"fsearch\"\n");
    write(&a.join("dev/notes/mian.txt"), "a typo'd filename that should still be found\n");
    write(&a.join("dev/notes/main.txt"), "the correctly spelled one\n");
    write(&a.join("dev/notes/My Report 2024.docx"), "binary-ish");
    write(&a.join("Pictures/holiday.png"), "not really a png");
    write(&a.join("Pictures/holiday.jpg"), "not really a jpg");
    write(&a.join("big/archive.zip"), &"z".repeat(2048));
    write(&a.join(".config/secret.ini"), "hidden config\n");
    write(&a.join("dev/node_modules/junk/index.js"), "module.exports = 1\n");
    a
}

#[test]
fn indexes_a_tree_and_finds_files_by_name() {
    make_tree();
    let live = build("home");
    assert!(live.base.n > 10, "index has {} entries", live.base.n);

    // The whole tree hangs off one virtual root with `C:` under it.
    let found = paths(&live, "readme");
    assert!(found.iter().any(|p| p.ends_with("README.md")), "{found:?}");

    // A path renders as a Windows path, drive token and all.
    let p = found.iter().find(|p| p.ends_with("README.md")).unwrap();
    assert!(p.starts_with("C:\\home\\dev\\fsearch\\"), "{p}");
    assert!(!p.contains('/'), "no forward slashes in {p}");

    // Lookup by exact path, and case-insensitively (NTFS is).
    let e = live.base.lookup(logical("home", "dev/fsearch/README.md").as_bytes());
    assert!(e.is_some(), "lookup failed");
    let upper = logical("home", "dev/fsearch/readme.MD");
    assert_eq!(live.base.lookup(upper.as_bytes()), e);
}

#[test]
fn fuzzy_and_typo_tolerance() {
    make_tree();
    let live = build("home");
    // 5+ letter words forgive one typo: "mian" is "main".
    let hits = paths(&live, "mian.rs");
    assert!(hits.iter().any(|p| p.ends_with("main.rs")), "{hits:?}");
    // ... and the clean spelling still ranks first.
    assert!(hits[0].ends_with("main.rs"), "{hits:?}");
    // A transposition is one edit: "fesarch" finds "fsearch".
    assert!(paths(&live, "fesarch").iter().any(|p| p.contains("fsearch")), "{:?}", paths(&live, "fesarch"));
    // Plain subsequence matching: "engn" finds "engine.rs".
    assert!(paths(&live, "engn").iter().any(|p| p.ends_with("engine.rs")));
}

#[test]
fn filters_scope_kind_ext_and_negation() {
    make_tree();
    let live = build("home");

    // ext:
    let rs = paths(&live, "ext:rs");
    assert_eq!(rs.len(), 2, "{rs:?}");
    assert!(rs.iter().all(|p| p.ends_with(".rs")), "{rs:?}");

    // in: scopes to a subtree (and takes a forward-slash path).
    let scoped = paths(&live, "in:C:/home/dev/notes main");
    assert!(!scoped.is_empty(), "no results in scope");
    assert!(scoped.iter().all(|p| p.starts_with("C:\\home\\dev\\notes\\")), "{scoped:?}");
    assert!(scoped.iter().any(|p| p.ends_with("main.txt")), "{scoped:?}");

    // kind:
    let dirs = paths(&live, "kind:dir fsearch");
    assert!(!dirs.is_empty() && dirs.iter().any(|p| p.ends_with("\\fsearch")), "{dirs:?}");

    // type:image
    let imgs = paths(&live, "type:image");
    assert!(imgs.iter().any(|p| p.ends_with(".png")) && imgs.iter().any(|p| p.ends_with(".jpg")), "{imgs:?}");

    // !exclude
    let no_notes = paths(&live, "main !notes");
    assert!(no_notes.iter().any(|p| p.ends_with("main.rs")), "{no_notes:?}");
    assert!(no_notes.iter().all(|p| !p.contains("\\notes")), "{no_notes:?}");

    // 'exact, ^prefix, suffix$
    assert!(paths(&live, "'README").iter().any(|p| p.ends_with("README.md")));
    assert!(paths(&live, "^READ").iter().any(|p| p.ends_with("README.md")));
    assert!(paths(&live, "docx$").iter().any(|p| p.ends_with(".docx")));

    // size:
    let big = paths(&live, "size:>1kb archive");
    assert!(big.iter().any(|p| p.ends_with("archive.zip")), "{big:?}");
}

#[test]
fn ranking_prefers_home_and_demotes_generated_dirs() {
    make_tree();
    // Put a same-named file in a good place and a bad one.
    let a = std::env::temp_dir().join("fsearch-test-root").join("home");
    write(&a.join("dev/thingumajig.txt"), "good");
    write(&a.join("dev/node_modules/thingumajig.txt"), "generated");
    let live = build("home");
    let hits = paths(&live, "thingumajig");
    assert!(hits.len() >= 1, "{hits:?}");
    // The copy outside node_modules ranks first.
    let first = hits.iter().find(|p| p.ends_with("thingumajig.txt")).unwrap();
    assert!(!first.contains("node_modules"), "node_modules ranked first: {hits:?}");
}

#[test]
fn live_updates_follow_the_disk() {
    make_tree();
    let a = std::env::temp_dir().join("fsearch-test-root").join("home");
    let mut live = build("home");

    // A new file shows up after its folder is relisted.
    write(&a.join("dev/brandnewfile.txt"), "hello");
    let p = logical("home", "dev");
    assert!(matches!(live.apply_dir(p.as_bytes(), false), Applied::Done));
    assert!(paths(&live, "brandnewfile").iter().any(|x| x.ends_with("brandnewfile.txt")));

    // Deleting it removes it again.
    fs::remove_file(a.join("dev/brandnewfile.txt")).unwrap();
    live.apply_dir(p.as_bytes(), false);
    assert!(paths(&live, "brandnewfile").is_empty(), "deleted file still found");

    // A new folder is scanned as a subtree.
    write(&a.join("dev/newtree/deep/leaf.txt"), "x");
    live.apply_dir(p.as_bytes(), false);
    assert!(paths(&live, "leaf.txt").iter().any(|x| x.ends_with("deep\\leaf.txt")), "subtree not indexed");

    // Renaming a folder moves the whole subtree.
    fs::rename(a.join("dev/newtree"), a.join("dev/renamed")).unwrap();
    live.apply_dir(p.as_bytes(), false);
    assert!(paths(&live, "leaf.txt").iter().any(|x| x.contains("\\renamed\\deep\\")), "rename lost");
    assert!(!paths(&live, "leaf.txt").iter().any(|x| x.contains("\\newtree\\")), "old name survived");
}

#[test]
fn compaction_round_trips_the_overlay() {
    make_tree();
    let a = std::env::temp_dir().join("fsearch-test-root").join("home");
    let mut live = build("home");
    write(&a.join("dev/compacted.txt"), "x");
    live.apply_dir(logical("home", "dev").as_bytes(), false);
    assert!(!live.over.is_empty(), "nothing in the overlay");

    // Everything alive, rebuilt from scratch, must find the same things.
    let before = paths(&live, "compacted");
    let ls = live.to_listings();
    let idx = Index::build(ls, 2, fsearch::query::now_secs(), logical_home("home").as_bytes());
    let live2 = Live::new(idx);
    assert!(live2.over.is_empty(), "overlay should be folded in");
    assert_eq!(paths(&live2, "compacted"), before);
    assert_eq!(paths(&live2, "readme"), paths(&live, "readme"));
}

#[test]
fn index_saves_and_loads() {
    make_tree();
    let live = build("home");
    let dir = area("data");
    let path = dir.join("index.bin");
    live.base.save(&path).unwrap();
    let loaded = Index::load(&path).expect("reload");
    assert_eq!(loaded.n, live.base.n);
    assert_eq!(loaded.d, live.base.d);
    assert_eq!(Index::saved_event_id(&path), Some(live.base.event_id));
    let live2 = Live::new(loaded);
    assert_eq!(paths(&live2, "readme"), paths(&live, "readme"));
}

#[test]
fn content_index_finds_text_inside_files() {
    make_tree();
    let live = build("home");
    let home = logical_home("home");
    let dir = area("content");

    // What the name index says should be indexed under home, recursively.
    let mut want = content::wanted(&live, home.as_bytes(), home.as_bytes(), true);
    want.sort();
    assert!(!want.is_empty(), "no candidate text files");
    // node_modules is out of scope, source files are in.
    let mut paths: Vec<String> = (0..want.len()).map(|i| String::from_utf8_lossy(want.path(i)).into_owned()).collect();
    paths.sort();
    assert!(paths.iter().any(|p| p.ends_with("main.rs")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("node_modules")), "{paths:?}");

    let mut c = Content::open(dir.clone());
    let todo = c.diff(content::wants(&live, home.as_bytes(), &[], &[home.as_bytes().to_vec()]));
    assert!(!todo.is_empty());
    for batch in todo.batches() {
        let id = c.alloc_id();
        if let Some(seg) = content::build_segment(&dir, id, &todo, batch) {
            c.push(seg);
        }
    }
    assert!(c.docs() > 0, "no docs indexed");

    // Literal grep through the trigram index.
    let filt = Query::parse("", &home).unwrap();
    let g = Grep::new("apply_dir", GrepMode::Literal).unwrap();
    let r = c.search(&g, &filt);
    assert!(r.files.iter().any(|f| f.path.ends_with(b"main.rs")), "grep missed main.rs: {:?}", r.files.len());
    assert!(r.files.iter().any(|f| f.lines.iter().any(|(_, t)| t.contains("fn apply_dir"))));

    // Symbol search finds where it is defined.
    let g = Grep::new("rebuild_all", GrepMode::Symbol).unwrap();
    let r = c.search(&g, &filt);
    assert!(r.files.iter().any(|f| f.path.ends_with(b"engine.rs")), "sym: missed engine.rs");

    // Regex search.
    let g = Grep::new(r"fn\s+\w+_dir", GrepMode::Regex).unwrap();
    let r = c.search(&g, &filt);
    assert!(r.files.iter().any(|f| f.path.ends_with(b"main.rs")), "regex missed main.rs");

    // Something not in any file finds nothing.
    let g = Grep::new("zzzqqqxxxnotpresent", GrepMode::Literal).unwrap();
    assert!(c.search(&g, &filt).files.is_empty());

    // Reopening from the manifest gives the same answers.
    let c2 = Content::open_shared(dir);
    assert_eq!(c2.docs(), c.docs());
    let g = Grep::new("apply_dir", GrepMode::Literal).unwrap();
    assert!(!c2.search(&g, &filt).files.is_empty());
}

#[test]
fn engine_answers_searches_and_greps() {
    make_tree();
    let data = area("engine-data");
    let home = logical_home("home");
    let engine = Engine::start(Options { dir: data, home: home.clone(), skip: None }).unwrap();

    // Until the first crawl finishes, searches say so rather than guessing.
    let mut ready = false;
    for _ in 0..300 {
        match engine.search(&Query::parse("readme", &home).unwrap()) {
            Ok(hits) if !hits.is_empty() => {
                assert!(hits[0].path.to_string_lossy().ends_with("README.md"), "{:?}", hits[0].path);
                assert_eq!(hits[0].kind & 3, fsearch::walk::KIND_FILE);
                ready = true;
                break;
            }
            Ok(_) => {}
            Err(e) => assert!(e.contains("indexing"), "{e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "engine never became ready");

    let s = engine.status();
    assert!(s.ready && s.entries > 10 && s.owner, "{:?}", s.index_bytes);

    // Content search through the engine, once the content worker has caught up.
    let mut grepped = false;
    for _ in 0..400 {
        let g = Grep::new("apply_dir", GrepMode::Literal).unwrap();
        let q = Query::parse("ext:rs", &home).unwrap();
        if let Ok((r, _)) = engine.grep(&q, &g) {
            if r.files.iter().any(|f| f.path.ends_with(b"main.rs")) {
                grepped = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(grepped, "engine never indexed content");

    // A new file is found through the live index without a rebuild.
    let a = std::env::temp_dir().join("fsearch-test-root").join("home");
    write(&a.join("dev/livething.txt"), "x");
    let p = logical("home", "dev");
    let mut found = false;
    for _ in 0..100 {
        {
            // Drive the same code path the daemon's apply loop uses.
            let q = Query::parse("livething", &home).unwrap();
            if let Ok(h) = engine.search(&q) {
                if !h.is_empty() {
                    found = true;
                }
            }
        }
        if found {
            break;
        }
        engine.apply_dir(p.as_bytes(), false);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(found, "a new file never showed up");
}

#[test]
fn query_parse_errors_are_reported() {
    let home = logical_home("home");
    assert!(Query::parse("type:nonsense", &home).is_err());
    assert!(Query::parse("kind:nonsense", &home).is_err());
    assert!(Query::parse("re:[unclosed", &home).is_err());
    assert!(Query::parse("limit:abc", &home).is_err());
    assert!(Query::parse("size:>", &home).is_err());
    assert!(Query::parse("ext:rs limit:5 'main", &home).is_ok());
}

/// Read one line without a BufReader, so nothing is read ahead and lost.
fn read_line<R: std::io::Read>(r: &mut R) -> String {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while r.read(&mut b).unwrap_or(0) == 1 {
        if b[0] == b'\n' {
            break;
        }
        out.push(b[0]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn daemon_answers_json_lines() {
    use std::io::Write;
    make_tree();
    let dir = area("server-data");
    let home = logical_home("home");
    let (d, h) = (dir.clone(), home.clone());
    std::thread::spawn(move || fsearch::server::serve(d, h));

    let mut c = None;
    for _ in 0..400 {
        if let Some(x) = fsearch::os::connect_once() {
            c = Some(x);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut c = c.expect("the daemon never listened");
    let mut ask = |req: &str| -> serde_json::Value {
        writeln!(c, "{req}").unwrap();
        c.flush().unwrap();
        if !c.wait_readable(std::time::Duration::from_secs(30)) {
            return serde_json::json!({"ok": false, "error": "timed out"});
        }
        serde_json::from_str(&read_line(&mut c)).unwrap_or_default()
    };

    // Handshake and errors.
    assert_eq!(ask(r#"{"op":"ping"}"#)["ok"], true);
    assert_eq!(ask("not json")["ok"], false);
    assert!(ask(r#"{"op":"nonsense"}"#)["error"].as_str().unwrap().contains("unknown op"));
    // A caller's id comes back untouched, so requests can be matched up.
    assert_eq!(ask(r#"{"op":"ping","id":7}"#)["id"], 7);

    // Search: wait out the first crawl.
    let mut hits = serde_json::Value::Null;
    for _ in 0..400 {
        let r = ask(r#"{"q":"readme","limit":5}"#);
        if r["ok"] == true {
            hits = r["hits"].clone();
            break;
        }
        assert!(r["error"].as_str().unwrap_or("").contains("indexing"), "{r}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let arr = hits.as_array().expect("no hits");
    assert!(!arr.is_empty(), "{hits}");
    let p = arr[0]["path"].as_str().unwrap();
    assert!(p.ends_with("README.md") && p.starts_with("C:\\home\\"), "{p}");
    assert_eq!(arr[0]["kind"], "file");
    assert!(arr[0]["size"].as_u64().unwrap() > 0);
    assert!(arr[0]["score"].as_i64().is_some());

    // Filters arrive as their own JSON fields, not just inside `q`.
    let r = ask(r#"{"q":"","ext":"rs","limit":10}"#);
    assert_eq!(r["ok"], true);
    let got: Vec<&str> = r["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap()).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|p| p.ends_with(".rs")), "{got:?}");

    // Status.
    let s = ask(r#"{"op":"status"}"#);
    assert_eq!(s["ok"], true);
    assert!(s["entries"].as_u64().unwrap() > 10 && s["owner"] == true, "{s}");

    // Content search, once the content worker has caught up.
    let mut files = serde_json::Value::Null;
    for _ in 0..400 {
        let r = ask(r#"{"op":"grep","pattern":"apply_dir","ext":"rs"}"#);
        if r["ok"] == true && r["files"].as_array().is_some_and(|f| !f.is_empty()) {
            files = r.clone();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let f = files.as_object().expect("grep never answered");
    let first = &f["files"].as_array().unwrap()[0];
    assert!(first["path"].as_str().unwrap().ends_with("main.rs"), "{first}");
    assert!(first["matches"].as_array().unwrap()[0]["text"].as_str().unwrap().contains("fn apply_dir"));
    assert!(matches!(f["source"].as_str(), Some("index") | Some("scan")), "{:?}", f["source"]);

    // `save` schedules a compaction rather than doing it inline.
    assert_eq!(ask(r#"{"op":"save"}"#)["scheduled"], true);
}
