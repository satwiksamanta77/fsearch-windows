//! The CLI. A console program, so it prints to the terminal that ran it and
//! the shell waits for it.

use fsearch::os::MmapAlloc;
/// Big buffers come straight from the OS and go straight back (see
/// `os::MmapAlloc`), so an index build's transient memory is returned instead
/// of staying in the heap as the daemon's footprint.
#[global_allocator]
static GLOBAL: MmapAlloc = MmapAlloc;

fn main() {
    fsearch::cli::run()
}
