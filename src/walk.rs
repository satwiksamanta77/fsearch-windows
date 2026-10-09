//! Parallel whole-disk enumeration.
//!
//! One call per directory returns hundreds of entries with name, type, size
//! and mtime already attached, so there is no per-file stat. Directories fan
//! out over a rayon pool. Junctions, symlinks and mount points are not
//! descended into — that is what stops the same subtree being indexed twice,
//! or a link to an ancestor looping forever — while cloud placeholders (a
//! reparse point whose contents list without downloading) are.

use crate::os::{self, DirHandle, MAX_DEPTH, ROOT, join};
use rayon::Scope;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

pub const NONE: u32 = u32::MAX;

/// Folders never to open: set when running without elevation, where the
/// folders that need it are known in advance and cost an access-denied each.
pub static SKIP: OnceLock<Vec<Vec<u8>>> = OnceLock::new();

pub fn blocked(path: &[u8]) -> bool {
    SKIP.get().is_some_and(|v| v.iter().any(|s| path.starts_with(s) && (path.len() == s.len() || path[s.len()] == os::SEP)))
}

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;

/// Entry flag bit: the hidden attribute.
pub const FLAG_HIDDEN: u8 = 1 << 2;
/// A directory that is a reparse point or mount we did not descend into.
pub const FLAG_MOUNT: u8 = 1 << 3;
/// Entry flag bit: a cloud-files placeholder. Listing one is free; reading it
/// would download it, so the content index leaves those files alone.
pub const FLAG_CLOUD: u8 = 1 << 4;

#[derive(Clone, Copy)]
pub struct RawEnt {
    pub name_off: u32,
    pub name_len: u16,
    /// kind in the low 2 bits, FLAG_* above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// Temp id of the listing for this directory, or NONE.
    pub child: u32,
}

pub struct Listing {
    pub id: u32,
    pub names: Vec<u8>,
    pub ents: Vec<RawEnt>,
}

impl Listing {
    pub fn new(id: u32) -> Listing {
        Listing { id, names: Vec::new(), ents: Vec::new() }
    }
    pub fn push(&mut self, name: &[u8], kind: u8, size: u64, mtime: u32) {
        if name.is_empty() || name.len() > u16::MAX as usize {
            return;
        }
        self.ents.push(RawEnt { name_off: self.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child: NONE });
        self.names.extend_from_slice(name);
    }
    pub fn name(&self, r: &RawEnt) -> &[u8] {
        &self.names[r.name_off as usize..][..r.name_len as usize]
    }
}

struct Ctx {
    next_id: AtomicU32,
    out: Vec<Mutex<Vec<Listing>>>,
}

/// Scan every volume, as one tree under the virtual root (listing id 0).
pub fn scan_volumes(threads: usize) -> Vec<Listing> {
    let vols = os::volumes();
    let mut root = Listing::new(0);
    let mut roots = Vec::with_capacity(vols.len());
    for (i, v) in vols.iter().enumerate() {
        let id = i as u32 + 1;
        let st = os::lstat(v);
        root.push(v, st.as_ref().map_or(KIND_DIR, |s| s.kind & 3), 0, st.map_or(0, |s| s.mtime));
        root.ents.last_mut().unwrap().child = id;
        roots.push((v.clone(), id));
    }
    scan_from(roots, vols.len() as u32 + 1, threads, Some(root))
}

/// Scan one folder recursively. Listing id 0 is `root` itself.
pub fn scan(root: &[u8], threads: usize) -> Vec<Listing> {
    scan_from(vec![(root.to_vec(), 0)], 1, threads, None)
}

fn scan_from(roots: Vec<(Vec<u8>, u32)>, first_id: u32, threads: usize, head: Option<Listing>) -> Vec<Listing> {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).start_handler(|_| os::no_materialize()).build().unwrap();
    let ctx = Ctx { next_id: AtomicU32::new(first_id), out: (0..threads + 1).map(|_| Mutex::new(Vec::new())).collect() };
    // A reference, so each spawned task copies it rather than moving the ctx.
    let c = &ctx;
    pool.scope(move |s| {
        if let Some(l) = head {
            push(l, c);
        }
        for (p, id) in roots {
            s.spawn(move |s| finish_dir(s, p, id, 0, c));
        }
    });
    ctx.out.into_iter().flat_map(|m| m.into_inner().unwrap()).collect()
}

/// List one directory and fan its subdirectories out over the pool. Paths are
/// rebuilt per child (Windows has no `openat`), which is cheap next to the
/// open itself.
fn finish_dir<'s>(s: &Scope<'s>, path: Vec<u8>, id: u32, depth: u32, ctx: &'s Ctx) {
    let mut l = Listing::new(id);
    if !blocked(&path)
        && depth < MAX_DEPTH
        && let Some(h) = os::open_dir(&path)
    {
        os::read_dir_batch(&h, &mut l);
    }
    let mut kids = Vec::new();
    for e in l.ents.iter_mut() {
        if e.kind & 3 != KIND_DIR || e.kind & FLAG_MOUNT != 0 || depth + 1 >= MAX_DEPTH {
            continue;
        }
        let child = {
            let name = &l.names[e.name_off as usize..][..e.name_len as usize];
            join(&path, name)
        };
        if blocked(&child) {
            continue;
        }
        e.child = ctx.next_id.fetch_add(1, Ordering::Relaxed);
        kids.push((child, e.child));
    }
    push(l, ctx);
    for (p, cid) in kids {
        s.spawn(move |s| finish_dir(s, p, cid, depth + 1, ctx));
    }
}

fn push(l: Listing, ctx: &Ctx) {
    let slot = rayon::current_thread_index().unwrap_or(ctx.out.len() - 1);
    ctx.out[slot].lock().unwrap().push(l);
}

/// List a single directory (no recursion). Used by the live updater.
pub fn list_one(path: &[u8]) -> Option<Listing> {
    if blocked(path) || path == ROOT {
        return None;
    }
    let h: DirHandle = os::open_dir(path)?;
    let mut l = Listing::new(0);
    os::read_dir_batch(&h, &mut l);
    Some(l)
}
