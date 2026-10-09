//! The graphical explorer. A window of its own (`fsearchui.exe`), talking to
//! the same daemon as the CLI, so both share one index and one live view of the
//! disk.

#[cfg(windows)]
pub mod app;
#[cfg(windows)]
pub mod icons;
pub mod ops;
pub mod search;

#[cfg(windows)]
use app::ExplorerApp;

pub const TITLE: &str = "FSearch";

/// Open the window. Blocks until it closes.
#[cfg(windows)]
pub fn run() -> Result<(), String> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 720.0]).with_min_inner_size([620.0, 380.0]).with_title(TITLE),
        ..Default::default()
    };
    eframe::run_native(TITLE, opts, Box::new(|cc| Ok(Box::new(ExplorerApp::new(cc))))).map_err(|e| format!("could not open a window: {e}"))
}

/// The window is a Windows feature; anywhere else the CLI is the interface.
#[cfg(not(windows))]
pub fn run() -> Result<(), String> {
    Err("the explorer window is only available on Windows".into())
}

/// Start the windowed explorer, preferring the dedicated no-console binary so
/// nothing flashes. Used by `fsearch ui`.
pub fn launch_detached() -> std::io::Result<()> {
    let me = std::env::current_exe()?;
    let ui = me.with_file_name(ui_exe_name());
    let ui = ui.is_file().then_some(ui).unwrap_or(me);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(ui).arg("ui").creation_flags(CREATE_NO_WINDOW).spawn()?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(ui).arg("ui").spawn()?;
        Ok(())
    }
}

pub fn ui_exe_name() -> &'static str {
    if cfg!(windows) { "fsearchui.exe" } else { "fsearchui" }
}
