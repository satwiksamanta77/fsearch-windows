//! The same program, built without a console: the daemon started at sign-in
//! (or spawned by the CLI) never flashes a window this way.

#![windows_subsystem = "windows"]

use fsearch::os::MmapAlloc;

#[global_allocator]
static GLOBAL: MmapAlloc = MmapAlloc;

fn main() {
    fsearch::cli::run()
}
