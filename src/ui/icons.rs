//! File-type icons for the explorer view.
//!
//! Real shell icons where the platform gives them (`SHGetFileInfo` on Windows),
//! and a generated extension badge everywhere else — including whenever shell
//! extraction fails, so the list is never left with holes in it.

use egui::{ColorImage, ImageData, TextureHandle};
use std::collections::HashMap;
use std::sync::Mutex;

pub const ICON_PX: usize = 16;

/// One rendered icon, plus the fallback badge for extensions with no shell icon.
pub struct Icons {
    by_ext: Mutex<HashMap<String, TextureHandle>>,
    /// Kept so textures outlive the handles handed to egui.
    ctx: egui::Context,
}

impl Icons {
    pub fn new(ctx: &egui::Context) -> Icons {
        Icons { by_ext: Mutex::new(HashMap::new()), ctx: ctx.clone() }
    }

    /// The icon for a file, cached by extension. `is_dir` wins over the name,
    /// because a folder called `notes.txt` is still a folder.
    pub fn get(&self, name: &str, is_dir: bool, path: Option<&std::path::Path>) -> TextureHandle {
        let key = if is_dir { "<dir>".to_string() } else { crate::ui::ops::ext_of(name).to_ascii_lowercase() };
        if let Some(h) = self.by_ext.lock().unwrap().get(&key) {
            return h.clone();
        }
        let img = system_icon(path, is_dir).unwrap_or_else(|| badge(if is_dir { "" } else { &key }));
        let handle = self.ctx.load_texture(format!("icon-{key}"), ImageData::Color(std::sync::Arc::new(img)), Default::default());
        self.by_ext.lock().unwrap().insert(key, handle.clone());
        handle
    }
}

/// The shell's own icon for a path, rendered into RGBA.
#[cfg(windows)]
fn system_icon(path: Option<&std::path::Path>, is_dir: bool) -> Option<ColorImage> {
    use windows_sys::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, SelectObject,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, DrawIconEx};

    // One buffer that outlives the call. With no path we ask for the generic
    // folder or document icon by attribute instead.
    let wide: Vec<u16> = match path {
        Some(p) => p.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect(),
        None => "folder\0".encode_utf16().collect(),
    };
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    let flags = SHGFI_ICON | SHGFI_SMALLICON | if path.is_none() { SHGFI_USEFILEATTRIBUTES } else { 0 };
    let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let ok = unsafe { SHGetFileInfoW(wide.as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, flags) };
    if ok == 0 || sfi.hIcon.is_null() {
        return None;
    }

    // Draw it into a 32-bpp DIB section and read the pixels back.
    let img = unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        if dc.is_null() {
            DestroyIcon(sfi.hIcon);
            return None;
        }
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = ICON_PX as i32;
        // Negative height gives top-down rows, so row 0 is the top of the icon.
        bi.bmiHeader.biHeight = -(ICON_PX as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = 0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            DeleteDC(dc);
            DestroyIcon(sfi.hIcon);
            return None;
        }
        let old = SelectObject(dc, dib);
        std::ptr::write_bytes(bits as *mut u8, 0, ICON_PX * ICON_PX * 4);
        // 3 == DI_NORMAL
        DrawIconEx(dc, 0, 0, sfi.hIcon, ICON_PX as i32, ICON_PX as i32, 0, std::ptr::null_mut(), 3);
        SelectObject(dc, old);
        let raw = std::slice::from_raw_parts(bits as *const u8, ICON_PX * ICON_PX * 4);
        let mut px = Vec::with_capacity(ICON_PX * ICON_PX);
        for c in raw.chunks_exact(4) {
            // The DIB is premultiplied BGRA; egui wants premultiplied RGBA.
            px.push(egui::Color32::from_rgba_premultiplied(c[2], c[1], c[0], c[3]));
        }
        DeleteObject(dib);
        DeleteDC(dc);
        DestroyIcon(sfi.hIcon);
        ColorImage { size: [ICON_PX, ICON_PX], pixels: px }
    };
    // An all-transparent result means the shell had nothing for it.
    if img.pixels.iter().all(|p| *p == egui::Color32::TRANSPARENT) { None } else { Some(img) }
}

#[cfg(not(windows))]
fn system_icon(_path: Option<&std::path::Path>, _is_dir: bool) -> Option<ColorImage> {
    None
}

/// A deterministic, dependency-free fallback: a rounded tile in a colour derived
/// from the extension, with its first letters on it. Folders get a tab shape.
pub fn badge(ext: &str) -> ColorImage {
    const S: usize = ICON_PX;
    let mut px = vec![egui::Color32::TRANSPARENT; S * S];
    let (base, accent) = palette(ext);
    if ext.is_empty() {
        // Folder: a tabbed rectangle.
        for y in 3..S - 2 {
            for x in 1..S - 1 {
                let tab = y < 6 && x > 6;
                if !tab && y < 6 && x >= 5 {
                    continue;
                }
                let edge = y == 3 || y == S - 3 || x == 1 || x == S - 2;
                px[y * S + x] = if edge { accent } else { base };
            }
        }
        for x in 1..6 {
            px[4 * S + x] = accent;
            px[5 * S + x] = base;
        }
    } else {
        // Document: a page with a folded corner.
        for y in 1..S - 1 {
            for x in 3..S - 3 {
                let fold = x + y > S + 3 && y < 6;
                if fold {
                    continue;
                }
                let edge = y == 1 || y == S - 2 || x == 3 || x == S - 4 || (y == 5 && x + y > S + 2);
                px[y * S + x] = if edge { accent } else { base };
            }
        }
        // Extension letters as two dark bars, so different types differ at a glance.
        let h = ext.as_bytes()[0] as usize;
        for i in 0..2 {
            let y = 7 + i * 3;
            let w = 2 + ((h >> i) % 4);
            for x in 5..(5 + w).min(S - 4) {
                px[y * S + x] = accent;
            }
        }
    }
    ColorImage { size: [S, S], pixels: px }
}

/// A stable colour per extension: same type, same colour, every run.
fn palette(ext: &str) -> (egui::Color32, egui::Color32) {
    if ext.is_empty() {
        return (egui::Color32::from_rgb(250, 200, 90), egui::Color32::from_rgb(150, 105, 25));
    }
    // Well-known families first, so the list reads sensibly.
    let known: &[(&[&str], (u8, u8, u8))] = &[
        (&["rs", "c", "h", "cpp", "cc", "hpp", "go", "py", "js", "ts", "jsx", "tsx", "java", "cs", "rb", "php", "swift", "kt"], (70, 130, 220)),
        (&["json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf"], (190, 160, 225)),
        (&["md", "txt", "rst", "org", "doc", "docx", "pdf", "rtf"], (110, 175, 110)),
        (&["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "psd"], (225, 110, 90)),
        (&["mp4", "mkv", "mov", "avi", "webm"], (235, 140, 175)),
        (&["mp3", "wav", "flac", "ogg", "m4a"], (245, 195, 120)),
        (&["zip", "rar", "7z", "tar", "gz", "iso"], (185, 150, 70)),
        (&["exe", "msi", "dll", "bat", "cmd", "ps1", "lnk"], (175, 195, 215)),
    ];
    for (exts, rgb) in known {
        if exts.contains(&ext) {
            let (r, g, b) = *rgb;
            return (egui::Color32::from_rgb(r, g, b), egui::Color32::from_rgb(r / 2, g / 2, b / 2));
        }
    }
    let h = ext.bytes().fold(5381u32, |a, c| a.wrapping_mul(33) ^ c as u32);
    let hue = (h % 360) as f32;
    let base = hsv(hue, 0.35, 0.90);
    let edge = hsv(hue, 0.55, 0.55);
    (base, edge)
}

fn hsv(h: f32, s: f32, v: f32) -> egui::Color32 {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    egui::Color32::from_rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}
