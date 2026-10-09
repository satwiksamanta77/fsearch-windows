//! The explorer window. Built without a console, so opening it never flashes
//! one, and so it can be the thing a shortcut or the sign-in entry points at.

#![windows_subsystem = "windows"]

use fsearch::os::MmapAlloc;

#[global_allocator]
static GLOBAL: MmapAlloc = MmapAlloc;

fn main() {
    // A GUI has nowhere to print, so a failure to start has to be a dialog and
    // a log entry rather than a message nobody sees.
    fsearch::diag::init();
    #[cfg(not(windows))]
    {
        eprintln!("fsearchui: the explorer window is only available on Windows.");
        return;
    }
    fsearch::diag::trace("ui: starting");
    if let Err(e) = fsearch::ui::run() {
        fsearch::diag::trace(&format!("ui: {e}"));
        fsearch::diag::crash_message(&format!("FSearch could not open its window.\n\n{e}\n\nDetails: {}", fsearch::diag::crash_path().display()));
        std::process::exit(1);
    }
}
