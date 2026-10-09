//! The explorer window: tabs, a folder tree, a sortable file list, and the
//! fsearch query bar wired into the same daemon the CLI uses.
//!
//! Two things shape the code. Rows are virtualised (`show_rows`), so a folder
//! with a hundred thousand entries scrolls as cheaply as one with ten. And
//! every interaction is recorded as an action and applied *after* the draw
//! closure returns, because that closure borrows the row data while the
//! actions need `&mut self`.

use crate::ui::icons::Icons;
use crate::ui::ops;
use crate::ui::search::Search;
use egui::{Align, Color32, Key, Layout, RichText, ScrollArea, TextEdit};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ROW_H: f32 = 22.0;
const DEBOUNCE: Duration = Duration::from_millis(160);
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
enum SortCol {
    Name,
    Size,
    Modified,
    Type,
}

#[derive(Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: u64,
    ty: String,
}

struct Tab {
    path: PathBuf,
    entries: Vec<Entry>,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    sel: BTreeSet<usize>,
    err: Option<String>,
    loaded_at: Instant,
}

impl Tab {
    fn new(path: PathBuf) -> Tab {
        Tab {
            path,
            entries: Vec::new(),
            back: Vec::new(),
            forward: Vec::new(),
            sel: BTreeSet::new(),
            err: None,
            loaded_at: Instant::now() - Duration::from_secs(3600),
        }
    }
    fn title(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| self.path.display().to_string())
    }
}

#[derive(Clone)]
enum Clip {
    Empty,
    Copy(Vec<PathBuf>),
    Cut(Vec<PathBuf>),
}

/// Something the list or the tree asked for, applied once the frame is drawn.
#[derive(Clone)]
enum Act {
    Go(PathBuf),
    Open(Vec<PathBuf>),
    Rename(PathBuf),
    Delete(Vec<PathBuf>),
    Cut(Vec<PathBuf>),
    CopyPaths(Vec<PathBuf>),
    NewTab(PathBuf),
    NewFolder,
    Paste,
    Reveal(PathBuf),
    Props(PathBuf),
}

pub struct ExplorerApp {
    tabs: Vec<Tab>,
    active: usize,
    icons: Icons,
    search: Search,
    query: String,
    /// What we last sent, so a query goes out once rather than every frame.
    sent: String,
    last_key: Instant,
    sort: (SortCol, bool),
    clip: Clip,
    renaming: Option<(PathBuf, String)>,
    confirm_delete: Option<Vec<PathBuf>>,
    flash: Option<(String, Instant)>,
    show_tree: bool,
    home: String,
    pending_clip: Option<String>,
}

impl ExplorerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> ExplorerApp {
        let home = crate::cli::home();
        let data_dir = crate::cli::data_dir();
        let start = std::env::var_os("FSEARCH_UI_START").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&home));
        let start = if start.is_dir() { start } else { roots().first().cloned().unwrap_or_else(|| PathBuf::from("C:\\")) };
        let mut app = ExplorerApp {
            tabs: vec![Tab::new(start)],
            active: 0,
            icons: Icons::new(&cc.egui_ctx),
            search: Search::start(data_dir, home.clone()),
            query: std::env::var("FSEARCH_UI_QUERY").unwrap_or_default(),
            sent: String::new(),
            last_key: Instant::now() - DEBOUNCE,
            sort: (SortCol::Name, true),
            clip: Clip::Empty,
            renaming: None,
            confirm_delete: None,
            flash: None,
            show_tree: true,
            home,
            pending_clip: None,
        };
        if !app.query.is_empty() {
            app.last_key = Instant::now();
        }
        app.reload(true);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing.y = 3.0;
        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        cc.egui_ctx.set_style(style);
        app
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), Instant::now()));
    }

    fn idx(&self) -> usize {
        self.active.min(self.tabs.len() - 1)
    }
    fn tab(&self) -> &Tab {
        &self.tabs[self.idx()]
    }
    fn tab_mut(&mut self) -> &mut Tab {
        let i = self.idx();
        &mut self.tabs[i]
    }

    /// Read the current folder from disk. `force` skips the freshness check.
    fn reload(&mut self, force: bool) {
        let sort = self.sort;
        let t = self.tab_mut();
        if !force && t.loaded_at.elapsed() < Duration::from_millis(700) {
            return;
        }
        let path = t.path.clone();
        t.sel.clear();
        match list_dir(&path) {
            Ok(mut ents) => {
                sort_entries(&mut ents, sort);
                t.entries = ents;
                t.err = None;
            }
            Err(e) => {
                t.entries.clear();
                t.err = Some(e);
            }
        }
        t.loaded_at = Instant::now();
    }

    fn navigate(&mut self, to: PathBuf) {
        if !to.is_dir() {
            return;
        }
        let t = self.tab_mut();
        if t.path == to {
            self.reload(true);
            return;
        }
        t.back.push(t.path.clone());
        if t.back.len() > 60 {
            t.back.remove(0);
        }
        t.forward.clear();
        t.path = to;
        self.reload(true);
    }

    fn back(&mut self) {
        let Some(prev) = self.tab_mut().back.pop() else { return };
        let cur = self.tab().path.clone();
        self.tab_mut().path = prev;
        self.tab_mut().forward.push(cur);
        self.reload(true);
    }

    fn forward(&mut self) {
        let Some(next) = self.tab_mut().forward.pop() else { return };
        let cur = self.tab().path.clone();
        self.tab_mut().path = next;
        self.tab_mut().back.push(cur);
        self.reload(true);
    }

    fn up(&mut self) {
        if let Some(p) = self.tab().parent_path() {
            self.navigate(p);
        }
    }

    fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn row_count(&self) -> usize {
        if self.searching() { self.search.rows.len() } else { self.tab().entries.len() }
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        let sel = &self.tab().sel;
        if self.searching() {
            self.search.rows.iter().enumerate().filter(|(i, _)| sel.contains(i)).map(|(_, r)| r.path.clone()).collect()
        } else {
            self.tab().entries.iter().enumerate().filter(|(i, _)| sel.contains(i)).map(|(_, e)| e.path.clone()).collect()
        }
    }

    fn clear_search(&mut self) {
        self.query.clear();
        self.sent.clear();
        self.search.rows.clear();
        self.search.error = None;
        self.search.busy = false;
    }

    fn open_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        // A single folder: go into it rather than opening a new window.
        if paths.len() == 1 && paths[0].is_dir() {
            let d = paths[0].clone();
            if self.searching() {
                self.clear_search();
            }
            self.navigate(d);
            return;
        }
        for p in paths.iter().take(12) {
            if let Err(e) = ops::open(p) {
                self.say(e);
                return;
            }
        }
    }

    fn paste(&mut self) {
        let dest = self.tab().path.clone();
        match self.clip.clone() {
            Clip::Empty => self.say("nothing on the clipboard"),
            Clip::Copy(srcs) => {
                let mut n = 0;
                for s in &srcs {
                    match ops::copy(s, &dest) {
                        Ok(c) => n += c,
                        Err(e) => {
                            self.say(e);
                            break;
                        }
                    }
                }
                self.say(format!("copied {n} file(s) to {}", dest.display()));
            }
            Clip::Cut(srcs) => {
                let mut n = 0;
                for s in &srcs {
                    match ops::move_to(s, &dest) {
                        Ok(c) => n += c,
                        Err(e) => {
                            self.say(e);
                            break;
                        }
                    }
                }
                self.clip = Clip::Empty;
                self.say(format!("moved {n} item(s) to {}", dest.display()));
            }
        }
        self.reload(true);
    }

    fn do_delete(&mut self, paths: Vec<PathBuf>) {
        match ops::delete(&paths) {
            Ok(()) => {
                self.say(format!("sent {} item(s) to the Recycle Bin", paths.len()));
                self.reload(true);
            }
            Err(e) => self.say(e),
        }
    }

    fn finish_rename(&mut self) {
        let Some((from, to)) = self.renaming.take() else { return };
        let to = to.trim().to_string();
        if to.is_empty() {
            return;
        }
        let target = from.parent().unwrap_or(Path::new(".")).join(&to);
        match ops::rename(&from, &target) {
            Ok(()) => {
                self.say(format!("renamed to {to}"));
                self.reload(true);
            }
            Err(e) => self.say(e),
        }
    }

    fn apply(&mut self, a: Act) {
        match a {
            Act::Go(p) => {
                if self.searching() {
                    self.clear_search();
                }
                self.navigate(p);
            }
            Act::Open(ps) => self.open_paths(ps),
            Act::Rename(p) => {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.renaming = Some((p, name));
            }
            Act::Delete(ps) => self.confirm_delete = Some(ps),
            Act::Cut(ps) => {
                self.clip = Clip::Cut(ps.clone());
                self.pending_clip = Some(ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
                self.say(format!("{} item(s) ready to move", ps.len()));
            }
            Act::CopyPaths(ps) => {
                self.pending_clip = Some(ps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n"));
                self.say(format!("copied {} path(s) to the clipboard", ps.len()));
            }
            Act::NewTab(p) => {
                self.tabs.push(Tab::new(p));
                self.active = self.tabs.len() - 1;
                self.reload(true);
            }
            Act::NewFolder => {
                let dir = self.tab().path.clone();
                // Explorer's behaviour: pick the first unused "New folder".
                let mut n = 1;
                let mut name = "New folder".to_string();
                while dir.join(&name).exists() {
                    n += 1;
                    name = format!("New folder ({n})");
                }
                match ops::new_folder(&dir, &name) {
                    Ok(p) => {
                        self.reload(true);
                        self.renaming = Some((p, name));
                    }
                    Err(e) => self.say(e),
                }
            }
            Act::Paste => self.paste(),
            Act::Reveal(p) => {
                if let Err(e) = ops::reveal(&p) {
                    self.say(e);
                }
            }
            Act::Props(p) => {
                if let Err(e) = ops::properties(&p) {
                    self.say(e);
                }
            }
        }
    }
}

impl Tab {
    fn parent_path(&self) -> Option<PathBuf> {
        self.path.parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty())
    }
}

/// Volumes for the tree, and the fallback start folder.
fn roots() -> Vec<PathBuf> {
    crate::os::volumes().iter().map(|v| crate::os::path_from_bytes(&[v.as_slice(), b"\\"].concat())).collect()
}

fn list_dir(p: &Path) -> Result<Vec<Entry>, String> {
    let rd = std::fs::read_dir(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let md = e.metadata().ok();
        let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
        let mtime = md.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let name = e.file_name().to_string_lossy().into_owned();
        let ty = ops::type_label(&name, is_dir);
        out.push(Entry { name, path, is_dir, size: md.as_ref().map_or(0, |m| m.len()), mtime, ty });
    }
    Ok(out)
}

fn sort_entries(v: &mut [Entry], (col, asc): (SortCol, bool)) {
    v.sort_by(|a, b| {
        // Folders first, like Explorer.
        let d = b.is_dir.cmp(&a.is_dir);
        if d != std::cmp::Ordering::Equal {
            return d;
        }
        let o = match col {
            SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortCol::Size => a.size.cmp(&b.size),
            SortCol::Modified => a.mtime.cmp(&b.mtime),
            SortCol::Type => a.ty.to_lowercase().cmp(&b.ty.to_lowercase()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        };
        if asc { o } else { o.reverse() }
    });
}

impl eframe::App for ExplorerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.search.poll();
        self.search.maybe_status();
        if let Some(text) = self.pending_clip.take() {
            ctx.copy_text(text);
        }

        // Debounced live search: send once typing pauses, not per keystroke.
        let q = self.query.trim().to_string();
        if q != self.sent && self.last_key.elapsed() >= DEBOUNCE {
            self.sent = q.clone();
            if q.is_empty() {
                self.search.rows.clear();
                self.search.error = None;
                self.search.busy = false;
            } else {
                self.search.submit(&q, 500);
            }
            self.tab_mut().sel.clear();
        }
        if self.search.busy {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        // Folders change under us; refresh gently while the window is open.
        if !self.searching() && self.tab().loaded_at.elapsed() > REFRESH {
            self.reload(true);
            ctx.request_repaint_after(REFRESH);
        }

        let mut act: Option<Act> = None;
        self.top_bar(ctx, &mut act);
        self.tree_panel(ctx, &mut act);
        self.central(ctx, &mut act);
        self.status_bar(ctx);
        self.modals(ctx);
        self.keys(ctx, &mut act);
        if let Some(a) = act {
            self.apply(a);
        }
    }
}

impl ExplorerApp {
    fn top_bar(&mut self, ctx: &egui::Context, act: &mut Option<Act>) {
        egui::TopBottomPanel::top("tabs").exact_height(28.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let mut close: Option<usize> = None;
                for i in 0..self.tabs.len() {
                    let title = self.tabs[i].title();
                    let r = ui.selectable_label(i == self.active, RichText::new(title).size(12.5));
                    if r.clicked() {
                        self.active = i;
                        self.reload(true);
                    }
                    if r.middle_clicked() && self.tabs.len() > 1 {
                        close = Some(i);
                    }
                    let path = self.tabs[i].path.clone();
                    r.context_menu(|m| {
                        if m.button("New tab here").clicked() {
                            *act = Some(Act::NewTab(path.clone()));
                        }
                        if m.button("Open in Explorer").clicked() {
                            *act = Some(Act::Reveal(path.clone()));
                        }
                    });
                }
                if let Some(i) = close {
                    self.tabs.remove(i.min(self.tabs.len() - 1));
                    self.active = self.idx();
                }
                if ui.button(RichText::new("+").size(14.0)).on_hover_text("New tab (Ctrl+T)").clicked() {
                    let p = self.tab().path.clone();
                    *act = Some(Act::NewTab(p));
                }
            });
        });

        egui::TopBottomPanel::top("nav").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if ui.button("←").on_hover_text("Back (Alt+Left)").clicked() {
                    self.back();
                }
                if ui.button("→").on_hover_text("Forward (Alt+Right)").clicked() {
                    self.forward();
                }
                if ui.button("↑").on_hover_text("Up one folder (Alt+Up)").clicked() {
                    self.up();
                }
                if ui.button("⟳").on_hover_text("Refresh (F5)").clicked() {
                    self.reload(true);
                }
                if ui.button(if self.show_tree { "▤" } else { "▣" }).on_hover_text("Show or hide the folder tree").clicked() {
                    self.show_tree = !self.show_tree;
                }
                ui.separator();
                let avail = ui.available_width();
                let mut addr = self.tab().path.display().to_string();
                let r = ui.add(TextEdit::singleline(&mut addr).desired_width(avail * 0.44).hint_text("folder or drive, then Enter"));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    let p = PathBuf::from(addr.trim().trim_matches('"'));
                    if p.is_dir() {
                        *act = Some(Act::Go(p));
                    } else {
                        self.say(format!("not a folder: {}", addr.trim()));
                    }
                }
                ui.separator();
                let sr = ui.add(
                    TextEdit::singleline(&mut self.query)
                        .desired_width((avail * 0.38).max(220.0))
                        .hint_text("search — fuzzy names · ext:rs · grep:todo · sym:main")
                        .id_salt("searchbox"),
                );
                if sr.changed() {
                    self.last_key = Instant::now();
                }
                if !self.query.is_empty() && ui.button("✕").on_hover_text("Clear the search (Esc)").clicked() {
                    self.clear_search();
                }
            });
            ui.add_space(2.0);
        });
    }

    fn tree_panel(&mut self, ctx: &egui::Context, act: &mut Option<Act>) {
        if !self.show_tree {
            return;
        }
        let mut goto: Option<PathBuf> = None;
        let current = self.tab().path.clone();
        let home = PathBuf::from(&self.home);
        egui::SidePanel::left("tree").resizable(true).default_width(230.0).min_width(150.0).show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("This PC").strong());
                if ui.small_button("⌂").on_hover_text("Home folder").clicked() {
                    goto = Some(home.clone());
                }
            });
            ui.separator();
            ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                for r in roots() {
                    tree_node(ui, &r, &current, &self.icons, &mut goto);
                }
            });
        });
        if let Some(p) = goto {
            *act = Some(Act::Go(p));
        }
    }

    fn central(&mut self, ctx: &egui::Context, act: &mut Option<Act>) {
        // Take the rows out of `self` for the frame: the draw closure reads
        // them while clicks need to write to selection and queue actions.
        let searching = self.searching();
        let i = self.idx();
        let entries = if searching { Vec::new() } else { std::mem::take(&mut self.tabs[i].entries) };
        let rows = if searching { std::mem::take(&mut self.search.rows) } else { Vec::new() };
        let n = if searching { rows.len() } else { entries.len() };
        let mut sel = std::mem::take(&mut self.tabs[i].sel);
        let err = self.tabs[i].err.clone();
        let s_err = self.search.error.clone();
        let s_busy = self.search.busy;
        let query = self.query.trim().to_string();

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(e) = &err {
                ui.colored_label(Color32::from_rgb(225, 120, 120), e);
                ui.separator();
            }
            column_header(ui, self.sort, searching, &mut |c| {
                self.sort = if self.sort.0 == c { (c, !self.sort.1) } else { (c, true) };
            });
            ui.separator();

            if n == 0 {
                ui.add_space(14.0);
                if searching {
                    if let Some(e) = &s_err {
                        ui.colored_label(Color32::from_rgb(225, 160, 120), format!("search: {e}"));
                        if e.contains("indexing") || e.contains("cannot reach") {
                            ui.label(RichText::new("The daemon is still building its index. Leave this window open and it will fill in.").weak());
                        }
                    } else if s_busy {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("searching…");
                        });
                    } else {
                        ui.label(RichText::new(format!("Nothing matches “{query}”.")).weak());
                    }
                } else {
                    ui.label(RichText::new("This folder is empty. Right-click for New folder.").weak());
                }
                return;
            }

            let icons = &self.icons;
            let w_all = ui.available_width();
            ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, n, |ui, range| {
                let w = w_all;
                let c_name = (w - 340.0).max(140.0);
                let visuals = ui.visuals().clone();
                for k in range {
                    let (name, path, is_dir, size, mtime, ty, sub) = if searching {
                        let r = &rows[k];
                        (
                            r.name.clone(),
                            r.path.clone(),
                            r.is_dir(),
                            r.size,
                            r.mtime as u64,
                            r.folder.clone(),
                            r.lines.first().map(|(l, t)| format!(":{l} {}", t.trim().chars().take(80).collect::<String>())),
                        )
                    } else {
                        let e = &entries[k];
                        (e.name.clone(), e.path.clone(), e.is_dir, e.size, e.mtime, e.ty.clone(), None)
                    };
                    let selected = sel.contains(&k);
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 2.0, visuals.selection.bg_fill);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 2.0, visuals.faint_bg_color);
                    }
                    let tex = icons.get(&name, is_dir, Some(&path));
                    let mut x = rect.min.x + 4.0;
                    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter().image(
                        tex.id(),
                        egui::Rect::from_min_size(egui::pos2(x, rect.center().y - 8.0), egui::vec2(16.0, 16.0)),
                        uv,
                        Color32::WHITE,
                    );
                    x += 20.0;
                    ui.painter().text(
                        egui::pos2(x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        &name,
                        egui::FontId::proportional(13.0),
                        if is_dir { visuals.strong_text_color() } else { visuals.text_color() },
                    );
                    // Truncate to the column's pixel width so long paths can
                    // never spill into their neighbour, the way they did when
                    // a deep folder met a wide date.
                    let rest = w - c_name;
                    let paint = |frac: f32, txt: &str, align: egui::Align2, px: f32| {
                        let cx = rect.min.x + c_name + rest * frac;
                        let n = (px / 6.4).max(4.0) as usize;
                        let shown: String = if txt.chars().count() <= n {
                            txt.to_string()
                        } else {
                            txt.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
                        };
                        ui.painter().text(egui::pos2(cx, rect.center().y), align, shown, egui::FontId::proportional(12.0), visuals.weak_text_color());
                    };
                    if let Some(s) = &sub {
                        ui.painter().text(
                            egui::pos2(x + 210.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            s,
                            egui::FontId::proportional(11.0),
                            Color32::from_rgb(120, 190, 150),
                        );
                    } else {
                        let sz = ops::human_size(size);
                        paint(0.295, if is_dir { "" } else { &sz }, egui::Align2::RIGHT_CENTER, rest * 0.27);
                    }
                    let when = ops::human_time(mtime);
                    paint(0.305, &when, egui::Align2::LEFT_CENTER, rest * 0.30);
                    paint(0.625, &ty, egui::Align2::LEFT_CENTER, rest * 0.365);

                    if resp.clicked() {
                        let mods = ui.input(|inp| inp.modifiers);
                        if mods.shift {
                            let anchor = sel.iter().next().copied().unwrap_or(k);
                            let (a, b) = if anchor <= k { (anchor, k) } else { (k, anchor) };
                            sel = (a..=b).collect();
                        } else if mods.ctrl || mods.command {
                            if !sel.remove(&k) {
                                sel.insert(k);
                            }
                        } else {
                            sel.clear();
                            sel.insert(k);
                        }
                    }
                    if resp.double_clicked() {
                        sel = [k].into_iter().collect();
                        let dirs: Vec<PathBuf> = std::iter::once(path.clone()).filter(|p| p.is_dir()).collect();
                        *act = Some(if dirs.len() == 1 { Act::Go(path.clone()) } else { Act::Open(vec![path.clone()]) });
                    }
                    if resp.secondary_clicked() {
                        if !sel.contains(&k) {
                            sel.clear();
                            sel.insert(k);
                        }
                        let sel_paths: Vec<PathBuf> = {
                            let it: Box<dyn Iterator<Item = &PathBuf>> = if searching {
                                Box::new(rows.iter().enumerate().filter(|(j, _)| sel.contains(j)).map(|(_, r)| &r.path))
                            } else {
                                Box::new(entries.iter().enumerate().filter(|(j, _)| sel.contains(j)).map(|(_, e)| &e.path))
                            };
                            it.cloned().collect()
                        };
                        let p2 = path.clone();
                        resp.context_menu(|m| {
                            if m.button("Open").clicked() {
                                *act = Some(if p2.is_dir() { Act::Go(p2.clone()) } else { Act::Open(sel_paths.clone()) });
                            }
                            if is_dir && m.button("Open in a new tab").clicked() {
                                *act = Some(Act::NewTab(p2.clone()));
                            }
                            if m.button("Show in Explorer").clicked() {
                                *act = Some(Act::Reveal(p2.clone()));
                            }
                            m.separator();
                            if m.button("Cut        Ctrl+X").clicked() {
                                *act = Some(Act::Cut(sel_paths.clone()));
                            }
                            if m.button("Copy       Ctrl+C").clicked() {
                                *act = Some(Act::CopyPaths(sel_paths.clone()));
                            }
                            if m.button("Paste      Ctrl+V").clicked() {
                                *act = Some(Act::Paste);
                            }
                            if m.button("New folder").clicked() {
                                *act = Some(Act::NewFolder);
                            }
                            m.separator();
                            if m.button("Rename     F2").clicked() {
                                *act = Some(Act::Rename(p2.clone()));
                            }
                            if m.button("Delete     Del").clicked() {
                                *act = Some(Act::Delete(sel_paths.clone()));
                            }
                            if m.button("Copy path").clicked() {
                                *act = Some(Act::CopyPaths(sel_paths.clone()));
                            }
                            if m.button("Properties").clicked() {
                                *act = Some(Act::Props(p2.clone()));
                            }
                        });
                    }
                }
            });
        });

        // Put everything back.
        self.tabs[i].sel = sel;
        if searching {
            self.search.rows = rows;
        } else {
            self.tabs[i].entries = entries;
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let n = self.row_count();
        let s = self.tab().sel.len();
        let searching = self.searching();
        let q = self.query.trim().to_string();
        let busy = self.search.busy;
        let took = self.search.took_ms;
        let index = self.search.index.clone();
        let flash = self.flash.clone().filter(|(_, at)| at.elapsed() < Duration::from_secs(6));
        egui::TopBottomPanel::bottom("status").exact_height(24.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let head = if searching { format!("{n} result(s) for “{q}”") } else { format!("{n} item(s)") };
                ui.label(RichText::new(head).size(12.0));
                if s > 0 {
                    ui.label(RichText::new(format!("· {s} selected")).weak().size(12.0));
                }
                if searching {
                    if busy {
                        ui.spinner();
                    } else if took > 0.0 {
                        ui.label(RichText::new(format!("· {took:.1} ms")).weak().size(12.0));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(index).weak().size(12.0));
                    if let Some((msg, _)) = flash {
                        ui.separator();
                        ui.label(RichText::new(msg).size(12.0).color(Color32::from_rgb(120, 195, 140)));
                    }
                });
            });
        });
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if let Some(paths) = self.confirm_delete.clone() {
            let mut done = None;
            egui::Window::new("Delete").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("Send {} item(s) to the Recycle Bin?", paths.len()));
                for p in paths.iter().take(6) {
                    ui.label(RichText::new(p.display().to_string()).weak().size(12.0));
                }
                if paths.len() > 6 {
                    ui.label(RichText::new(format!("…and {} more", paths.len() - 6)).weak().size(12.0));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Delete").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => {
                    self.confirm_delete = None;
                    self.do_delete(paths);
                }
                Some(false) => self.confirm_delete = None,
                None => {}
            }
        }

        if let Some((path, text)) = self.renaming.clone() {
            let mut done = None;
            let mut edited = text.clone();
            egui::Window::new("Rename").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(RichText::new(path.display().to_string()).weak().size(12.0));
                let r = ui.add(TextEdit::singleline(&mut edited).desired_width(340.0).id_salt("renamebox"));
                r.request_focus();
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || enter {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => {
                    self.renaming = Some((path, edited));
                    self.finish_rename();
                }
                Some(false) => self.renaming = None,
                None => self.renaming = Some((path, edited)),
            }
        }
    }

    fn keys(&mut self, ctx: &egui::Context, act: &mut Option<Act>) {
        // Editing a field? Then only navigation and the tab shortcuts apply.
        let editing = ctx.memory(|m| m.focused().is_some()) && self.renaming.is_none();
        let (ctrl, alt) = ctx.input(|i| (i.modifiers.ctrl || i.modifiers.command, i.modifiers.alt));
        let mut pressed: Vec<Key> = Vec::new();
        ctx.input(|i| {
            for k in [
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::ArrowUp,
                Key::T,
                Key::W,
                Key::L,
                Key::F5,
                Key::Escape,
                Key::Enter,
                Key::F2,
                Key::Delete,
                Key::C,
                Key::X,
                Key::V,
                Key::A,
                Key::N,
            ] {
                if i.key_pressed(k) {
                    pressed.push(k);
                }
            }
        });
        for k in pressed {
            match k {
                Key::ArrowLeft if alt => self.back(),
                Key::ArrowRight if alt => self.forward(),
                Key::ArrowUp if alt => self.up(),
                Key::T if ctrl => {
                    let p = self.tab().path.clone();
                    *act = Some(Act::NewTab(p));
                }
                Key::W if ctrl => {
                    if self.tabs.len() > 1 {
                        self.tabs.remove(self.idx());
                        self.active = self.idx();
                    }
                }
                Key::L if ctrl => self.clear_search(),
                Key::F5 => self.reload(true),
                Key::Escape if !self.query.is_empty() && !editing => self.clear_search(),
                Key::Enter if !editing => {
                    let ps = self.selected_paths();
                    if ps.len() == 1 && ps[0].is_dir() {
                        *act = Some(Act::Go(ps[0].clone()));
                    } else if !ps.is_empty() {
                        *act = Some(Act::Open(ps));
                    }
                }
                Key::F2 if !editing => {
                    let ps = self.selected_paths();
                    if ps.len() == 1 {
                        *act = Some(Act::Rename(ps[0].clone()));
                    }
                }
                Key::Delete if !editing => {
                    let ps = self.selected_paths();
                    if !ps.is_empty() {
                        *act = Some(Act::Delete(ps));
                    }
                }
                Key::C if ctrl && !editing => {
                    let ps = self.selected_paths();
                    if !ps.is_empty() {
                        self.clip = Clip::Copy(ps.clone());
                        *act = Some(Act::CopyPaths(ps));
                    }
                }
                Key::X if ctrl && !editing => {
                    let ps = self.selected_paths();
                    if !ps.is_empty() {
                        *act = Some(Act::Cut(ps));
                    }
                }
                Key::V if ctrl && !editing => *act = Some(Act::Paste),
                Key::A if ctrl && !editing => {
                    let n = self.row_count();
                    self.tab_mut().sel = (0..n).collect();
                }
                Key::N if ctrl && !editing => *act = Some(Act::NewFolder),
                _ => {}
            }
        }
    }
}

fn column_header(ui: &mut egui::Ui, sort: (SortCol, bool), searching: bool, flip: &mut impl FnMut(SortCol)) {
    let w = ui.available_width();
    let c_name = (w - 340.0).max(140.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let rest = w - c_name;
        let head = |ui: &mut egui::Ui, label: &str, col: SortCol, width: f32, align: Align, sort: (SortCol, bool), flip: &mut dyn FnMut(SortCol)| {
            let arrow = if sort.0 == col { if sort.1 { " ▲" } else { " ▼" } } else { "" };
            let r = ui.allocate_ui_with_layout(egui::vec2(width, 18.0), Layout::left_to_right(align), |ui| {
                ui.selectable_label(sort.0 == col, RichText::new(format!("{label}{arrow}")).strong().size(12.0))
            });
            if r.inner.clicked() {
                flip(col);
            }
        };
        head(ui, "Name", SortCol::Name, c_name, Align::Min, sort, flip);
        head(ui, "Size", SortCol::Size, rest * 0.30, Align::Max, sort, flip);
        head(ui, "Date modified", SortCol::Modified, rest * 0.32, Align::Min, sort, flip);
        head(ui, if searching { "In folder" } else { "Type" }, SortCol::Type, rest * 0.38, Align::Min, sort, flip);
    });
}

/// A lazily-expanded folder node. Children are read only once it is open.
fn tree_node(ui: &mut egui::Ui, path: &Path, current: &Path, icons: &Icons, goto: &mut Option<PathBuf>) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let is_cur = *path == *current;
    let id = ui.make_persistent_id(path);
    let open: bool = ui.memory_mut(|m| m.data.get_persisted::<bool>(id).unwrap_or(false));
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(120.0), 20.0), egui::Sense::click());
    if is_cur {
        ui.painter().rect_filled(rect, 2.0, ui.visuals().selection.bg_fill);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 2.0, ui.visuals().faint_bg_color);
    }
    let has_kids = std::fs::read_dir(path).map(|mut d| d.next().is_some()).unwrap_or(false);
    let mut x = rect.min.x + 2.0;
    if has_kids {
        ui.painter().text(
            egui::pos2(x + 5.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            if open { "▾" } else { "▸" },
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
    }
    x += 14.0;
    let tex = icons.get(&name, true, Some(path));
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    ui.painter().image(tex.id(), egui::Rect::from_min_size(egui::pos2(x, rect.center().y - 8.0), egui::vec2(16.0, 16.0)), uv, Color32::WHITE);
    ui.painter().text(
        egui::pos2(x + 20.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.0),
        ui.visuals().text_color(),
    );
    if resp.clicked() {
        // Clicking the row both navigates and toggles the node, which is what
        // Explorer does when you click a folder in its tree.
        if has_kids {
            let next = !open;
            ui.memory_mut(|m| m.data.insert_persisted(id, next));
        }
        *goto = Some(path.to_path_buf());
    }
    if ui.memory_mut(|m| m.data.get_persisted::<bool>(id).unwrap_or(false)) {
        ui.indent(id, |ui| {
            if let Ok(rd) = std::fs::read_dir(path) {
                let mut kids: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.path()).collect();
                kids.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default());
                for k in kids.into_iter().take(500) {
                    tree_node(ui, &k, current, icons, goto);
                }
            }
        });
    }
}
