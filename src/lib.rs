//! Whole-disk file search for Windows: a fuzzy name index over every entry on
//! disk, kept live from `ReadDirectoryChangesW`, plus a trigram content index
//! for text files. `Engine` runs it all in-process; the `fsearch` binary wraps
//! one in a daemon with a JSON-lines named pipe.
//!
//! A port of [noahdunnagan/fsearch](https://github.com/noahdunnagan/fsearch)
//! (MIT), which does the same thing on macOS with `getattrlistbulk` and
//! FSEvents. The engine, the index layout, the query language, the ranking and
//! the daemon protocol are the original's; everything that talked to Darwin
//! talks to Win32 here instead. See `PORTING.md`.

pub mod content;
pub mod diag;
pub mod doctor;
mod engine;
pub mod index;
pub mod live;
pub mod os;
pub mod query;
pub mod server;
pub mod walk;

pub mod cli;

pub use content::{FileMatches, Grep, GrepResult};
pub use engine::{Engine, Found, Options, Status, default_dir};
pub use os::{gated, has_full_disk_access, no_materialize};
pub use query::{GrepMode, Query};
