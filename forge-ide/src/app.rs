use crate::agent_panel::{ApprovalState, ChatItem};
use crate::buffer::Buffer;
use crate::filetree::{FileTree, TreeAction};
use crate::terminal::Terminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

const DIVIDER_H: f32 = 5.0;

/// Default cap on how many of a conversation's trailing chat items get
/// rendered — see the doc comment at its use site in `draw_agent_view` for
/// why an uncapped, uncached full rebuild every frame doesn't scale.
const AGENT_HISTORY_RENDER_LIMIT: usize = 150;

// ── Quick Open ────────────────────────────────────────────────────────────────

/// Quick Open lists exactly one directory at a time and you descend into
/// folders to reach a file, rather than indexing the tree up front. The cost
/// of opening it is therefore one `read_dir` — a function of how crowded the
/// current folder is, not of how large the tree below it is. That is what
/// makes `Ctrl+P` in `$HOME` or `/` cheap instead of unbounded.
///
/// Cap on entries kept for a single directory. Only pathological folders
/// (cache/maildir style, hundreds of thousands of siblings) reach it.
const QUICK_OPEN_MAX_ENTRIES: usize = 20_000;

/// How many rows the list shows. Must bound *every* path that assigns
/// `filtered` — the ScrollArea builds one widget per entry per frame, so an
/// unbounded list is its own freeze independent of how fast listing was.
const QUICK_OPEN_MAX_RESULTS: usize = 50;

struct QuickOpenEntry {
    path:       PathBuf,
    /// Bare file name, with a trailing `/` on directories so the list makes
    /// clear which rows descend and which rows open.
    name:       String,
    /// `name` pre-lowercased, so filtering allocates nothing per keystroke.
    name_lower: String,
    is_dir:     bool,
}

struct QuickOpen {
    /// Directory being listed. Starts at `root` and moves as you navigate.
    dir:      PathBuf,
    /// Project root. `..` stops here, so navigation can't wander off into the
    /// filesystem above the open folder.
    root:     PathBuf,
    query:    String,
    entries:  Vec<QuickOpenEntry>,
    filtered: Vec<usize>,
    cursor:   usize,
    /// Set when this directory's listing stopped at `QUICK_OPEN_MAX_ENTRIES`.
    truncated: bool,
    /// `Some` while a directory listing is in flight.
    rx:       Option<mpsc::Receiver<(PathBuf, Vec<QuickOpenEntry>, bool)>>,
    /// Tells an in-flight listing its result is unwanted; tripped by `Drop`
    /// and on every navigation, so a stalled network mount can't pile up.
    cancel:   Arc<AtomicBool>,
}

impl Drop for QuickOpen {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); }
}

impl QuickOpen {
    fn new(root: &Path) -> Self {
        let mut qo = Self {
            dir: root.to_path_buf(), root: root.to_path_buf(),
            query: String::new(), entries: Vec::new(), filtered: Vec::new(),
            cursor: 0, truncated: false, rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        qo.load(root.to_path_buf());
        qo
    }

    /// Kick off a listing of `dir`. Off-thread even though it is a single
    /// `read_dir`: on a stalled network mount that one call can block for
    /// seconds, and blocking the render thread is what froze the window.
    fn load(&mut self, dir: PathBuf) {
        // Abandon any listing already running — its result is stale now.
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&self.cancel);

        self.dir       = dir.clone();
        self.query.clear();
        self.entries.clear();
        self.filtered.clear();
        self.cursor    = 0;
        self.truncated = false;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let (entries, truncated) = list_dir(&dir, &flag);
            if !flag.load(Ordering::Relaxed) {
                let _ = tx.send((dir, entries, truncated));
            }
        });
    }

    fn listing(&self) -> bool { self.rx.is_some() }

    /// Path of the listed directory relative to the root, for the header.
    fn breadcrumb(&self) -> String {
        match self.dir.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => {
                self.root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.root.to_string_lossy().into_owned())
            }
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_)  => self.dir.to_string_lossy().into_owned(),
        }
    }

    fn at_root(&self) -> bool { self.dir == self.root }

    /// Descend into `dir`, listing it fresh.
    fn enter(&mut self, dir: PathBuf) { self.load(dir); }

    /// Go up one level, stopping at the project root.
    fn go_up(&mut self) {
        if self.at_root() { return; }
        let Some(parent) = self.dir.parent().map(|p| p.to_path_buf()) else { return };
        self.load(parent);
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        match rx.try_recv() {
            Ok((dir, entries, truncated)) => {
                self.rx = None;
                // Guard against a listing that finished after we navigated on.
                if dir != self.dir { return; }
                self.entries   = entries;
                self.truncated = truncated;
                self.update_filter();
            }
            Err(mpsc::TryRecvError::Empty)        => {}
            Err(mpsc::TryRecvError::Disconnected) => self.rx = None,
        }
    }

    fn update_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self.entries.iter().enumerate()
            .filter(|(_, e)| fuzzy_match(&e.name_lower, &q))
            .map(|(i, _)| i)
            .take(QUICK_OPEN_MAX_RESULTS)
            .collect();
        // Highlight the first real entry rather than the `../` row that sits at
        // slot 0 below the root: Enter in a folder you just opened should act on
        // its contents, not bounce straight back out. Up-arrow still reaches
        // `../`. With nothing to show, `../` is the only actionable row.
        self.cursor = if self.at_root() || self.filtered.is_empty() { 0 } else { 1 };
    }
}

/// One Quick Open row. Allocates the full available width so the whole row is
/// a click target, not just the text glyphs.
fn draw_quick_open_row(
    ui:       &mut egui::Ui,
    label:    &str,
    selected: bool,
    is_dir:   bool,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(0, 120, 212));
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(55));
    }
    let fg = if selected      { egui::Color32::WHITE }
             else if is_dir   { egui::Color32::from_rgb(176, 200, 230) }
             else             { egui::Color32::from_gray(200) };
    ui.painter().text(
        rect.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        fg,
    );
    resp
}

/// `dir/name`, or `dir/name (2)`, `(3)`… if that already exists. Never returns
/// a path that exists, so an import cannot clobber.
fn unique_dest(dir: &Path, name: &Path) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() { return first; }
    let stem = name.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext  = name.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..1000 {
        let candidate = match &ext {
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None    => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() { return candidate; }
    }
    first
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() { return true; }
    let mut hi = haystack.chars();
    'outer: for nc in needle.chars() {
        loop {
            match hi.next() {
                None    => return false,
                Some(h) => if h == nc { continue 'outer; }
            }
        }
    }
    true
}

/// List one directory — no recursion, so this is bounded by that directory's
/// own entry count no matter how deep the tree below it goes. Returns the
/// entries plus whether the cap truncated them.
///
/// Hidden entries and `node_modules` are skipped, matching what the file tree
/// shows (see `filetree::FileTree::walk`) so the two views agree.
fn list_dir(dir: &Path, cancel: &AtomicBool) -> (Vec<QuickOpenEntry>, bool) {
    let Ok(iter) = std::fs::read_dir(dir) else { return (Vec::new(), false) };
    let mut out = Vec::new();
    let mut truncated = false;
    for entry in iter.flatten() {
        if cancel.load(Ordering::Relaxed) { return (out, truncated); }
        if out.len() >= QUICK_OPEN_MAX_ENTRIES { truncated = true; break; }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') || name == "node_modules" { continue; }
        // Resolve through symlinks so a linked directory is still navigable.
        // Safe to follow here precisely because nothing recurses: a link cycle
        // costs one extra `read_dir` when the user walks into it, not an
        // unbounded descent.
        let path = entry.path();
        let is_dir = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => path.is_dir(),
            Ok(ft)                    => ft.is_dir(),
            Err(_)                    => continue,
        };
        let display = if is_dir { format!("{name}/") } else { name.to_string() };
        out.push(QuickOpenEntry {
            path,
            name_lower: display.to_lowercase(),
            name: display,
            is_dir,
        });
    }
    // Directories first, then case-insensitive by name — the file tree's order.
    out.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| a.name_lower.cmp(&b.name_lower))
    });
    (out, truncated)
}

// ── Command palette ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Cmd {
    NewFile, SaveFile, OpenFolder, NewWindow,
    ToggleTerminal, ToggleFileTree,
    QuickOpen,
    /// Index into PluginHost::commands.
    Plugin(usize),
}

const COMMANDS: &[(&str, &str, Cmd)] = &[
    ("New File",          "Ctrl+N",       Cmd::NewFile),
    ("Save File",         "Ctrl+S",       Cmd::SaveFile),
    ("Open Folder…",      "",             Cmd::OpenFolder),
    ("New Window",        "",             Cmd::NewWindow),
    ("Toggle Terminal",   "",             Cmd::ToggleTerminal),
    ("Toggle File Tree",  "",             Cmd::ToggleFileTree),
    ("Go to File…",       "Ctrl+P",       Cmd::QuickOpen),
];

struct CmdPalette {
    query:    String,
    filtered: Vec<usize>,
    cursor:   usize,
    /// Plugin command titles, appended after the built-in COMMANDS.
    extra:    Vec<String>,
}

impl CmdPalette {
    fn new(extra: Vec<String>) -> Self {
        let filtered = (0..COMMANDS.len() + extra.len()).collect();
        Self { query: String::new(), filtered, cursor: 0, extra }
    }

    fn entry(&self, i: usize) -> (&str, &str, Cmd) {
        if i < COMMANDS.len() {
            COMMANDS[i]
        } else {
            (self.extra[i - COMMANDS.len()].as_str(), "", Cmd::Plugin(i - COMMANDS.len()))
        }
    }

    fn update_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = (0..COMMANDS.len() + self.extra.len())
            .filter(|&i| {
                let name = if i < COMMANDS.len() { COMMANDS[i].0 }
                           else { self.extra[i - COMMANDS.len()].as_str() };
                fuzzy_match(&name.to_lowercase(), &q)
            })
            .collect();
        self.cursor = 0;
    }
}

// ── Multi-file search ─────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
enum SidebarView { Explorer, Search, SourceControl, Ssh, Outline }

/// Which step of the two-step SSH connect flow is shown.
#[derive(PartialEq, Clone, Copy)]
enum SshOverlayStep {
    /// Asking: "Connect to Host..." vs "Connect Current Window to Host..."
    ChooseWindow,
    /// Host picker — in a new window
    PickHostNewWindow,
    /// Host picker — in the current window
    PickHostCurrentWindow,
}

/// Spec for a window to create. Passed from the app to the winit loop.
///
/// `Debug` so a failing startup-planning test can print what it planned rather
/// than only how many windows it came up with.
#[derive(Debug, Default)]
pub struct NewWindowSpec {
    /// Working directory for the new window (None = inherit current).
    pub cwd:      Option<std::path::PathBuf>,
    /// SSH host to connect immediately on open.
    pub ssh_host: Option<crate::ssh::SshHost>,
    /// Where to put the window, when reopening one that was open before.
    /// `None` lets the platform place it, which is what a genuinely new window
    /// wants.
    pub frame: Option<crate::session::WindowFrame>,
    /// Reopen zoomed.
    pub maximized: bool,
    /// Which stored session this window continues. `0` for a genuinely new
    /// window, which is given a fresh id.
    pub window_id: u64,
    /// True when this process was just launched by "Reload Window", not a
    /// genuinely fresh start. Reload always restores the session it just
    /// saved — the same instance continuing, matching VS Code's Reload
    /// Window — regardless of the `restore_session` *setting*, which only
    /// governs restoring across a real quit-and-relaunch later.
    pub is_reload: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum BottomTab { Terminal, Output }

#[derive(Clone)]
pub enum OutputLevel { Info, Warn, Error, Success }

impl OutputLevel {
    fn color(&self) -> egui::Color32 {
        match self {
            OutputLevel::Info    => egui::Color32::from_gray(190),
            OutputLevel::Warn    => egui::Color32::from_rgb(220, 180, 80),
            OutputLevel::Error   => egui::Color32::from_rgb(240, 100, 100),
            OutputLevel::Success => egui::Color32::from_rgb(100, 200, 120),
        }
    }
}

#[derive(Clone)]
struct SearchHit {
    file: PathBuf,
    line: usize, // 0-indexed
    text: String,
}

struct SearchState {
    query:          String,
    case_sensitive: bool,
    results:        Vec<SearchHit>,
    searching:      bool,
    rx:             Option<mpsc::Receiver<Vec<SearchHit>>>,
    request_focus:  bool,
    last_query:     String,
    /// Tells the in-flight walk to stop. Tripped whenever a new search starts,
    /// so retyping doesn't leave the previous walk churning through the tree.
    cancel:         Arc<AtomicBool>,
}

impl Drop for SearchState {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); }
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(), case_sensitive: false,
            results: Vec::new(), searching: false, rx: None,
            request_focus: false, last_query: String::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(&mut self, root: PathBuf) {
        if self.query.is_empty() {
            self.cancel.store(true, Ordering::Relaxed);
            self.rx = None;
            self.searching = false;
            self.results.clear();
            self.last_query.clear();
            return;
        }
        if self.query == self.last_query && !self.results.is_empty() { return; }
        self.last_query = self.query.clone();
        self.results.clear();
        self.searching = true;
        // Abandon whatever the previous keystroke started.
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let q  = self.query.clone();
        let cs = self.case_sensitive;
        std::thread::spawn(move || {
            let mut hits = Vec::new();
            walk_search(&root, &q, cs, &mut hits, 2000, 0, &flag);
            if !flag.load(Ordering::Relaxed) { let _ = tx.send(hits); }
        });
    }

    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            if let Ok(hits) = rx.try_recv() {
                self.results = hits;
                self.searching = false;
                self.rx = None;
            }
        }
    }
}

/// Lines retained by the Output panel. The panel carries task/git/SSH status,
/// not full build output (that lives in the terminal), so this is generous —
/// and every retained line is also a widget laid out each frame it's visible.
const MAX_OUTPUT_LINES: usize = 2_000;

/// Longest single Output line kept, in characters.
const MAX_OUTPUT_LINE_CHARS: usize = 4_000;

/// How long to let file-watch events settle before re-walking the file tree.
/// Long enough to swallow a build's worth of churn, short enough that a file
/// an agent just wrote shows up without feeling laggy.
const TREE_REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// A finished external-formatter run, sent back from its worker thread.
struct PendingFormat {
    /// Buffer this was started for; `None` means an untitled buffer.
    path:   Option<PathBuf>,
    /// The text handed to the formatter. The result is only applied if the
    /// buffer still matches, so edits typed while it ran aren't clobbered.
    sent:   String,
    result: Result<String, String>,
}

/// Depth cap for the project-wide search walk.
const SEARCH_MAX_DEPTH: usize = 24;

/// Largest file the search will read. Search is a background thread, so an
/// unbounded read doesn't freeze the window — it just pins a core and can
/// exhaust memory on a single huge file.
const SEARCH_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn walk_search(
    dir:            &Path,
    query:          &str,
    case_sensitive: bool,
    hits:           &mut Vec<SearchHit>,
    max:            usize,
    depth:          usize,
    cancel:         &AtomicBool,
) {
    if hits.len() >= max || depth > SEARCH_MAX_DEPTH || cancel.load(Ordering::Relaxed) { return; }
    let Ok(iter) = std::fs::read_dir(dir) else { return };
    let q_lower = if case_sensitive { String::new() } else { query.to_lowercase() };
    for entry in iter.flatten() {
        if hits.len() >= max || cancel.load(Ordering::Relaxed) { return; }
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') { continue; }
        if matches!(name, "target" | "node_modules" | "dist" | "build" | "__pycache__") { continue; }
        // `file_type()` does not resolve symlinks, unlike `Path::is_dir()`.
        // Following them here meant a single cycle (`~/link -> ~`) recursed
        // until the thread died — the same defect Quick Open's walker had.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_search(&path, query, case_sensitive, hits, max, depth + 1, cancel);
        } else if ft.is_file() {
            // Check the size before reading: `read_to_string` on a multi-GB
            // file pulls all of it into memory before the binary sniff below
            // ever gets to reject it.
            match entry.metadata() {
                Ok(m) if m.len() > SEARCH_MAX_FILE_BYTES => continue,
                Ok(_)  => {}
                Err(_) => continue,
            }
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            // skip binary-looking files
            if content.bytes().take(4096).any(|b| b == 0) { continue; }
            for (line_idx, line) in content.lines().enumerate() {
                let m = if case_sensitive {
                    line.contains(query)
                } else {
                    line.to_lowercase().contains(&q_lower)
                };
                if m {
                    hits.push(SearchHit {
                        file: path.clone(),
                        line: line_idx,
                        text: line.trim().chars().take(140).collect(),
                    });
                    if hits.len() >= max { return; }
                }
            }
        }
    }
}

// ── Activity bar icons (painter-drawn, font-independent) ──────────────────────

fn paint_explorer_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Two stacked document/file outlines
    let s   = egui::Stroke::new(1.5_f32, color);
    let w   = 10.0;
    let h   = 12.0;
    let fold = 3.0;
    // Front document
    let tl = c + egui::vec2(-w * 0.5, -h * 0.5);
    let tr = tl + egui::vec2(w - fold, 0.0);
    let cr = tl + egui::vec2(w, fold);
    let br = tl + egui::vec2(w, h);
    let bl = tl + egui::vec2(0.0, h);
    let fc = tl + egui::vec2(w - fold, fold);
    p.line_segment([tl, tr], s);
    p.line_segment([tr, fc], s);
    p.line_segment([fc, cr], s);
    p.line_segment([cr, br], s);
    p.line_segment([br, bl], s);
    p.line_segment([bl, tl], s);
    p.line_segment([tr, cr], s);
}

fn paint_outline_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    let s = egui::Stroke::new(1.4_f32, color);
    // Bulleted list with indented sub-items
    for (i, (indent, w)) in [(0.0, 12.0), (5.0, 8.0), (5.0, 8.0), (0.0, 12.0)].iter().enumerate() {
        let y = c.y - 7.5 + i as f32 * 5.0;
        let x = c.x - 8.0 + indent;
        p.circle_filled(egui::pos2(x, y), 1.2, color);
        p.line_segment([egui::pos2(x + 4.0, y), egui::pos2(x + 4.0 + w, y)], s);
    }
}

/// How long ago the *currently running* binary was last rebuilt — lets you
/// tell at a glance whether "Reload Window" actually picked up a fresh
/// build, since reload restarts the process using whatever's already on
/// disk rather than triggering a rebuild itself.
fn running_binary_age() -> String {
    let Ok(exe) = std::env::current_exe() else { return "unknown".into() };
    let Ok(meta) = std::fs::metadata(&exe) else { return "unknown".into() };
    let Ok(modified) = meta.modified() else { return "unknown".into() };
    let Ok(elapsed) = modified.elapsed() else { return "just now".into() };
    let secs = elapsed.as_secs();
    if secs < 5           { "just now".into() }
    else if secs < 60     { format!("{secs}s ago") }
    else if secs < 3600   { format!("{}m ago", secs / 60) }
    else if secs < 86400  { format!("{}h ago", secs / 3600) }
    else                  { format!("{}d ago", secs / 86400) }
}

fn paint_gear_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    let r = 6.0;
    p.circle_stroke(c, r * 0.5, egui::Stroke::new(1.4_f32, color));
    // Eight teeth around the rim.
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::TAU / 8.0;
        let (sin, cos) = a.sin_cos();
        let inner = c + egui::vec2(cos, sin) * (r * 0.8);
        let outer = c + egui::vec2(cos, sin) * r;
        p.line_segment([inner, outer], egui::Stroke::new(1.6_f32, color));
    }
}

/// Tiny wireframe mockup of the window layout — file tree (left), editor
/// (center), agent panel (right), terminal (bottom) — used by the Settings
/// layout picker so each option is a diagram, not just a label. Whichever
/// of the two side columns is "full height" is drawn spanning the whole
/// diagram; the terminal strip is drawn only across whatever's left.
fn paint_layout_diagram(
    p: &egui::Painter, rect: egui::Rect,
    file_tree_full: bool, agent_full: bool, selected: bool,
) {
    let border = if selected { egui::Color32::from_rgb(0, 120, 212) } else { egui::Color32::from_gray(75) };
    p.rect_filled(rect, 3.0, egui::Color32::from_rgb(30, 30, 30));
    p.rect_stroke(rect, 3.0, egui::Stroke::new(if selected { 2.0_f32 } else { 1.0_f32 }, border));

    let inner   = rect.shrink(4.0);
    let side_w  = inner.width() * 0.22;
    let term_h  = inner.height() * 0.34;

    let left_col = egui::Rect::from_min_size(
        inner.min,
        egui::vec2(side_w, if file_tree_full { inner.height() } else { inner.height() - term_h }),
    );
    let right_col = egui::Rect::from_min_max(
        egui::pos2(inner.max.x - side_w, inner.min.y),
        egui::pos2(inner.max.x, if agent_full { inner.max.y } else { inner.max.y - term_h }),
    );
    let term_rect = egui::Rect::from_min_max(
        egui::pos2(if file_tree_full { left_col.max.x } else { inner.min.x }, inner.max.y - term_h),
        egui::pos2(if agent_full { right_col.min.x } else { inner.max.x }, inner.max.y),
    );
    let editor_rect = egui::Rect::from_min_max(
        egui::pos2(left_col.max.x, inner.min.y),
        egui::pos2(right_col.min.x, term_rect.min.y),
    );

    p.rect_filled(left_col,   1.0, egui::Color32::from_gray(65));
    p.rect_filled(right_col,  1.0, egui::Color32::from_gray(65));
    p.rect_filled(editor_rect,1.0, egui::Color32::from_gray(48));
    p.rect_filled(term_rect,  1.0, egui::Color32::from_rgb(20, 20, 20));
}

fn paint_search_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Magnifying glass: circle + handle
    let center = c + egui::vec2(-2.0, -2.0);
    let r = 5.5;
    p.circle_stroke(center, r, egui::Stroke::new(1.6_f32, color));
    let a = center + egui::vec2(r * 0.70, r * 0.70);
    let b = c + egui::vec2(5.0, 5.0);
    p.line_segment([a, b], egui::Stroke::new(1.8_f32, color));
}

/// Maps anvil heat (0.0 cold → 1.0 white-hot) to a blacksmith-glow color,
/// interpolating across dim gray → dull red → orange → pale yellow-white.
fn anvil_heat_color(t: f32) -> egui::Color32 {
    const STOPS: [(f32, egui::Color32); 5] = [
        (0.0,  egui::Color32::from_rgb(48, 48, 48)),
        (0.35, egui::Color32::from_rgb(100, 35, 20)),
        (0.6,  egui::Color32::from_rgb(210, 70, 20)),
        (0.85, egui::Color32::from_rgb(255, 150, 40)),
        (1.0,  egui::Color32::from_rgb(255, 230, 150)),
    ];
    let t = t.clamp(0.0, 1.0);
    let lerp_u8 = |a: u8, b: u8, f: f32| (a as f32 + (b as f32 - a as f32) * f).round() as u8;
    for pair in STOPS.windows(2) {
        let (t0, c0) = pair[0];
        let (t1, c1) = pair[1];
        if t <= t1 {
            let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return egui::Color32::from_rgb(
                lerp_u8(c0.r(), c1.r(), f),
                lerp_u8(c0.g(), c1.g(), f),
                lerp_u8(c0.b(), c1.b(), f),
            );
        }
    }
    STOPS[STOPS.len() - 1].1
}

fn paint_anvil(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Minimalist anvil silhouette — matches the bundled Forge.png aesthetic.
    // Built from 4 simple primitives so it reads cleanly at icon size.

    // Work surface (top rectangle)
    p.rect_filled(
        egui::Rect::from_min_max(
            c + egui::vec2(-9.0, -3.0),
            c + egui::vec2( 9.0,  0.0),
        ),
        1.0, color,
    );
    // Horn (triangle off the left edge of the work surface)
    p.add(egui::Shape::convex_polygon(
        vec![
            c + egui::vec2(-9.0, -2.5),
            c + egui::vec2(-13.0, -1.3),
            c + egui::vec2(-9.0,  0.0),
        ],
        color, egui::Stroke::NONE,
    ));
    // Waist (trapezoid narrowing toward the base)
    p.add(egui::Shape::convex_polygon(
        vec![
            c + egui::vec2(-7.0, 0.0),
            c + egui::vec2( 7.0, 0.0),
            c + egui::vec2( 5.5, 4.0),
            c + egui::vec2(-5.5, 4.0),
        ],
        color, egui::Stroke::NONE,
    ));
    // Base (wide foot)
    p.rect_filled(
        egui::Rect::from_min_max(
            c + egui::vec2(-9.0, 4.0),
            c + egui::vec2( 9.0, 6.5),
        ),
        1.0, color,
    );
}

fn paint_check_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Simple checkmark: short-left, long-right strokes meeting at the bottom.
    let s = egui::Stroke::new(2.0_f32, color);
    p.line_segment([c + egui::vec2(-5.0, 0.5), c + egui::vec2(-1.5, 4.0)], s);
    p.line_segment([c + egui::vec2(-1.5, 4.0), c + egui::vec2( 5.0, -3.5)], s);
}

/// Uniform height for every Source Control list row (file rows *and* section
/// headers).  Required so the list can be virtualized with `show_rows`.
const SC_ROW_H: f32 = 22.0;

struct ScRowAction {
    open:   bool,
    action: bool, // stage if !is_staged, unstage if is_staged
}

/// Section header rendered as a single fixed-height row (so it can live inside
/// the virtualized `show_rows` list alongside file rows).
fn sc_section_header(ui: &mut egui::Ui, label: &str, count: usize) {
    let avail_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(avail_w, SC_ROW_H), egui::Sense::hover());
    let cy = rect.center().y;
    let p  = ui.painter();
    p.text(egui::pos2(rect.left() + 10.0, cy), egui::Align2::LEFT_CENTER,
        label, egui::FontId::proportional(11.0), egui::Color32::from_gray(200));

    // Count rendered in a small rounded badge, right-aligned (VSCode style).
    let txt = format!("{count}");
    let badge_w = (txt.chars().count() as f32 * 6.5 + 12.0).max(18.0);
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.right() - badge_w - 10.0, cy - 7.5),
        egui::vec2(badge_w, 15.0));
    p.rect_filled(badge, 7.5, egui::Color32::from_rgb(14, 99, 156));
    p.text(badge.center(), egui::Align2::CENTER_CENTER, &txt,
        egui::FontId::proportional(10.0), egui::Color32::WHITE);
}

fn sc_row(
    ui: &mut egui::Ui,
    workdir: &std::path::Path,
    path:    &std::path::Path,
    status:  crate::git::FileStatus,
    is_staged: bool,
) -> ScRowAction {
    let mut out = ScRowAction { open: false, action: false };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let dir  = path.parent()
        .and_then(|p| p.strip_prefix(workdir).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let avail_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(avail_w, SC_ROW_H), egui::Sense::click(),
    );

    // Hover highlight
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(
            rect, 0.0,
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 12),
        );
    }

    let p   = ui.painter();
    let cy  = rect.center().y;
    let pad = 10.0;

    // File-type icon (matches the file tree / VSCode's leading glyph)
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + pad, cy - 8.0),
        egui::vec2(16.0, 16.0),
    );
    crate::icons::paint(p, icon_rect, path, false);

    // Filename
    let name_x = rect.left() + pad + 20.0;
    p.text(
        egui::pos2(name_x, cy),
        egui::Align2::LEFT_CENTER,
        &name,
        egui::FontId::proportional(13.0),
        egui::Color32::from_gray(220),
    );

    // Dir (right after the name in a dim color), truncated with an ellipsis so
    // long paths don't run into the action button / status letter on the right.
    if !dir.is_empty() {
        let name_w = name.chars().count() as f32 * 7.5;
        let dir_x  = name_x + name_w + 6.0;
        // Leave room for the +/− button and the status letter on the right edge.
        let content_right = rect.right() - 46.0;
        let max_w = content_right - dir_x;
        if max_w > 10.0 {
            let mut job = egui::text::LayoutJob::single_section(
                dir.clone(),
                egui::TextFormat {
                    font_id: egui::FontId::proportional(10.5),
                    color:   egui::Color32::from_gray(110),
                    ..Default::default()
                },
            );
            job.wrap = egui::text::TextWrapping {
                max_width: max_w,
                max_rows:  1,
                overflow_character: Some('…'),
                ..Default::default()
            };
            let galley = ui.ctx().fonts(|f| f.layout_job(job));
            let gy = cy - galley.size().y * 0.5;
            p.galley(egui::pos2(dir_x, gy), galley, egui::Color32::PLACEHOLDER);
        }
    }

    // Status letter (M / A / U / D / R / !) on the far right, VSCode-style.
    p.text(
        egui::pos2(rect.right() - 12.0, cy),
        egui::Align2::CENTER_CENTER,
        status.letter(),
        egui::FontId::proportional(11.0),
        status.color(),
    );

    // Action button (+ stage / − unstage) just left of the status letter,
    // revealed on row hover like VSCode.
    let btn_size = 18.0;
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 24.0 - btn_size, cy - btn_size * 0.5),
        egui::vec2(btn_size, btn_size),
    );
    let btn_id   = ui.id().with(("sc_btn", path));
    let btn_resp = ui.interact(btn_rect, btn_id, egui::Sense::click());
    if resp.hovered() || btn_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if btn_resp.hovered() {
            ui.painter().circle_filled(btn_rect.center(), btn_size * 0.5,
                egui::Color32::from_rgba_premultiplied(255, 255, 255, 20));
        }
        let btn_color = egui::Color32::from_gray(200);
        let bc        = btn_rect.center();
        // Horizontal stroke (shared by + and −)
        ui.painter().line_segment(
            [bc + egui::vec2(-5.0, 0.0), bc + egui::vec2(5.0, 0.0)],
            egui::Stroke::new(1.8_f32, btn_color),
        );
        if !is_staged {
            // Vertical stroke turns it into a +
            ui.painter().line_segment(
                [bc + egui::vec2(0.0, -5.0), bc + egui::vec2(0.0, 5.0)],
                egui::Stroke::new(1.8_f32, btn_color),
            );
        }
    }
    let _ = btn_resp.clone().on_hover_text(
        if is_staged { "Unstage" } else { "Stage" });

    if btn_resp.clicked() { out.action = true; }
    // Row click (but NOT on the button) opens the file.
    if resp.clicked() && !btn_resp.hovered() { out.open = true; }

    out
}

fn paint_branch_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Git branch glyph: two dots connected by a Y-shaped fork.
    let stroke = egui::Stroke::new(1.4_f32, color);
    let main_x  = c.x - 2.0;
    let branch_x = c.x + 2.5;
    // Main line (vertical)
    p.line_segment(
        [egui::pos2(main_x, c.y - 5.0), egui::pos2(main_x, c.y + 5.0)], stroke,
    );
    // Branch line (forks up-right from middle of main)
    p.line_segment(
        [egui::pos2(main_x, c.y), egui::pos2(branch_x, c.y - 3.0)], stroke,
    );
    // Three small dots: top of main, end of branch, bottom of main.
    p.circle_filled(egui::pos2(main_x,   c.y - 5.0), 1.6, color);
    p.circle_filled(egui::pos2(branch_x, c.y - 3.0), 1.6, color);
    p.circle_filled(egui::pos2(main_x,   c.y + 5.0), 1.6, color);
}

/// Wavy underline squiggle between two x-coordinates at height y.
fn paint_squiggle(p: &egui::Painter, x0: f32, x1: f32, y: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.2_f32, color);
    let amp    = 1.5;
    let step   = 4.0;
    let mut x  = x0;
    let mut up = true;
    while x < x1 {
        let nx = (x + step).min(x1);
        let ny = if up { y - amp } else { y + amp };
        p.line_segment([egui::pos2(x, if up { y + amp } else { y - amp }),
                        egui::pos2(nx, ny)], stroke);
        x  = nx;
        up = !up;
    }
}

/// Convert a 0-based (line, col) position to a char offset in `text`.
/// One tab in the Forge Agent panel — its own agent process + conversation.
struct AgentTab {
    session: crate::agent_panel::AgentSession,
    conv_id: String,
    /// How much this specific tab is allowed to do without asking first.
    /// Independent per tab — one conversation can be cautious while another
    /// is auto-approving, and switching modes on this tab never affects the
    /// others (see `settings::AgentPermissionMode`).
    permission_mode: crate::settings::AgentPermissionMode,
    /// Session-scoped password for auto-answering detected password prompts
    /// (sudo, ssh, etc.) from commands this tab's agent runs. Set only by
    /// explicit user action — typing "ALLOW" in a password card — and never
    /// implied by `permission_mode`, including Dangerously Skip All. Held in
    /// memory and mirrored into `password_tmp_path`, never regular settings
    /// or the saved-conversation store; both are cleared when the tab
    /// closes or the user explicitly forgets it. Never sent to the model —
    /// only ever written directly to the blocked process's stdin.
    session_password:     Option<String>,
    /// Whether `session_password` should be auto-injected into detected
    /// password prompts without asking each time.
    password_auto_inject: bool,
    /// This tab's own secret tmp file, created lazily the first time a
    /// password is remembered — the only place it's ever allowed to touch
    /// disk. Deleted on tab close (see the close-tab call site) and best-
    /// effort on drop; not guaranteed to survive a Reload Window, whose
    /// `exec` skips destructors entirely — same limitation as this app's own
    /// GPU-resource teardown, and equally acceptable here since a restored
    /// tab gets a brand-new subprocess anyway, never the old one.
    password_tmp_path:    Option<std::path::PathBuf>,
    /// Renders every item when true; otherwise only the most recent
    /// `AGENT_HISTORY_RENDER_LIMIT` items, behind a "show earlier messages"
    /// banner. Every item — even a `User`/`Status` one that's cheap on its
    /// own — goes through per-frame markdown parsing and a handful of egui
    /// widget calls with no caching (unlike the editor/terminal panels,
    /// which already cache their shaped output), so a long conversation's
    /// full item list rebuilding from scratch on *every single frame*
    /// scales directly with conversation length: measured ~85ms/frame for
    /// a real 1442-item conversation, easily enough to make typing itself
    /// feel delayed, since every keystroke's repaint has to pay that same
    /// cost before the new character can even appear. Capped rendering
    /// keeps a long-running conversation's per-frame cost roughly constant
    /// regardless of how long it's gotten.
    show_full_history: bool,
    /// `session.items.len()` as of the last auto-save — see the auto-save
    /// call site in `draw_agent_view` for why this exists (skips a
    /// clone+JSON-serialize+disk-write of the whole conversation on every
    /// single frame when nothing's actually changed since the last one).
    last_saved_item_count: usize,
}

impl Drop for AgentTab {
    fn drop(&mut self) {
        if let Some(path) = self.password_tmp_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One local terminal session. Multiple can be open at once (VS Code-style),
/// switched between via a tab strip in the TERMINAL bottom-panel view.
///
/// `title` is just the shell name — the leading "N:" shown in the tab strip
/// is computed from each tab's *current* position in `terminal_tabs` at
/// render time, not stored here. Baking a fixed number in at creation meant
/// closing an earlier tab left the survivors' numbers stale (a tab created
/// as "2: zsh" stayed "2: zsh" forever, even once it became the only tab).
struct TerminalTab {
    terminal: Terminal,
    title:    String,
}

impl TerminalTab {
    fn new(cwd: &std::path::Path) -> Self {
        Self { terminal: Terminal::new(cwd), title: shell_name() }
    }

    fn reattach(
        client: std::sync::Arc<crate::ptyhost::PtyHostClient>,
        info: &forge_proto::PtyInfo,
        snapshot: Option<crate::terminal::GridSnapshot>,
    ) -> Self {
        Self {
            terminal: Terminal::reattach(
                client, info.id, std::path::Path::new(&info.cwd), info.cols, info.rows, snapshot),
            title: shell_name(),
        }
    }
}

fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| std::path::Path::new(&s).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "shell".into())
}

impl AgentTab {
    fn new(cwd: &std::path::Path, mode: crate::settings::AgentPermissionMode) -> Self {
        Self {
            session: crate::agent_panel::AgentSession::spawn(cwd, mode, None),
            conv_id: new_conv_id(),
            permission_mode: mode,
            session_password: None,
            password_auto_inject: false,
            password_tmp_path: None,
            show_full_history: false,
            last_saved_item_count: 0,
        }
    }

    /// Reopens a previously saved conversation — from the history list, or
    /// from session restore on reboot. Unlike `new`, this resumes
    /// forge-agent's *own* process state via `--resume-session` (when the
    /// save has one — older saves predating that field fall back to a fresh,
    /// contextless subprocess, same as before) so the model actually
    /// remembers the earlier turns, not just a UI transcript that looks like
    /// it does. `items`/`model` are still applied on top so the chat view
    /// has something to show immediately, before the resumed process's own
    /// (context-free) `init` message would otherwise arrive.
    fn reopen(cwd: &std::path::Path, mode: crate::settings::AgentPermissionMode,
              conv: &crate::agent_panel::SavedConversation) -> Self {
        let resume = if conv.forge_session_id.is_empty() { None } else { Some(conv.forge_session_id.as_str()) };
        let mut tab = Self {
            session: crate::agent_panel::AgentSession::spawn(cwd, mode, resume),
            conv_id: conv.id.clone(),
            permission_mode: mode,
            session_password: None,
            password_auto_inject: false,
            password_tmp_path: None,
            show_full_history: false,
            last_saved_item_count: 0,
        };
        tab.session.items = conv.items.clone();
        tab.session.model = conv.model.clone();
        tab.session.forge_session_id = conv.forge_session_id.clone();
        tab
    }

    /// Stores `password` as this tab's session-scoped secret — in memory,
    /// and mirrored into a private, user-only-readable tmp file (the only
    /// place it's ever allowed to touch disk) — and enables auto-inject for
    /// the rest of the session. Called only from the explicit "type ALLOW"
    /// confirmation path, never implicitly.
    fn remember_session_password(&mut self, password: String) {
        let path = self.password_tmp_path.get_or_insert_with(|| {
            std::env::temp_dir().join(format!("forge-ide-secret-{}.tmp", self.conv_id))
        }).clone();
        if std::fs::write(&path, &password).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        self.session_password = Some(password);
        self.password_auto_inject = true;
    }

    /// Clears the stored session password and deletes its tmp file, if any.
    fn forget_session_password(&mut self) {
        if let Some(path) = self.password_tmp_path.take() {
            let _ = std::fs::remove_file(path);
        }
        self.session_password = None;
        self.password_auto_inject = false;
    }

    /// Switches this tab's permission mode. `AlwaysAsk` <-> `AutoApprove` is
    /// a live toggle (no process restart). Any transition into or out of
    /// `DangerouslySkipAll` requires a fresh subprocess — it's only settable
    /// via a CLI flag at spawn time — so this respawns the session in place,
    /// replaying the current transcript into it exactly like reopening a
    /// saved conversation from history already does. Call only when the
    /// current turn isn't active — respawning mid-turn would drop it.
    fn set_permission_mode(&mut self, mode: crate::settings::AgentPermissionMode, cwd: &std::path::Path) {
        use crate::settings::AgentPermissionMode as Mode;
        if self.permission_mode == mode { return; }
        let needs_respawn = self.permission_mode == Mode::DangerouslySkipAll
            || mode == Mode::DangerouslySkipAll;
        if needs_respawn {
            let items = std::mem::take(&mut self.session.items);
            let model = self.session.model.clone();
            let resume_id = self.session.forge_session_id.clone();
            let resume = if resume_id.is_empty() { None } else { Some(resume_id.as_str()) };
            self.session = crate::agent_panel::AgentSession::spawn(cwd, mode, resume);
            self.session.items = items;
            self.session.model = model;
        } else {
            self.session.toggle_auto_mode();
        }
        self.permission_mode = mode;
    }
    /// Short display title for the tab strip.
    fn title(&self) -> String {
        self.session.items.iter().find_map(|i| {
            if let crate::agent_panel::ChatItem::User(t) = i {
                let t = t.trim();
                Some(if t.len() > 22 { format!("{}…", &t[..22]) } else { t.to_string() })
            } else { None }
        }).unwrap_or_else(|| "New Chat".into())
    }
}

fn new_conv_id() -> String {
    // Use SystemTime since epoch as a sortable ID (no chrono dep).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs:016}")
}

fn char_index(text: &str, line: u32, col: u32) -> usize {
    let mut chars = 0usize;
    for (i, ln) in text.split('\n').enumerate() {
        if i == line as usize {
            return chars + (col as usize).min(ln.chars().count());
        }
        chars += ln.chars().count() + 1; // +1 for the '\n'
    }
    chars
}

fn paint_ssh_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    let s = egui::Stroke::new(1.5_f32, color);
    // Terminal-prompt ">" chevron
    p.line_segment([c + egui::vec2(-4.0, -5.0), c + egui::vec2( 3.0,  0.0)], s);
    p.line_segment([c + egui::vec2( 3.0,  0.0), c + egui::vec2(-4.0,  5.0)], s);
    // Underscore cursor blink
    p.line_segment([c + egui::vec2( 2.0,  5.0), c + egui::vec2( 7.0,  5.0)], s);
}

fn comp_kind_glyph(kind: u8) -> &'static str {
    match kind {
        2 | 3  => "ƒ",  // method / function
        6      => "x",  // variable
        7      => "♦",  // class
        9      => "◉",  // interface / module
        14     => "κ",  // keyword
        _      => "·",
    }
}

fn paint_send_icon(p: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    // Up-arrow (modern chat-app style)
    let s = egui::Stroke::new(2.2_f32, color);
    p.line_segment([c + egui::vec2(0.0,  6.0), c + egui::vec2(0.0, -5.0)], s);
    p.line_segment([c + egui::vec2(0.0, -5.0), c + egui::vec2(-4.0, -1.0)], s);
    p.line_segment([c + egui::vec2(0.0, -5.0), c + egui::vec2( 4.0, -1.0)], s);
}

fn load_forge_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    // Bundled at the repo root — bytes are embedded so the binary stays standalone.
    let bytes = include_bytes!("../Forge.png");
    let img   = image::load_from_memory(bytes).ok()?;
    let rgba  = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut pixels = rgba.into_raw();

    // The PNG has a solid black background; key it out so the icon blends
    // into whatever color the activity bar uses.  Anything near-black goes
    // fully transparent; everything else keeps its alpha.
    for chunk in pixels.chunks_exact_mut(4) {
        let lum = chunk[0] as u16 + chunk[1] as u16 + chunk[2] as u16;
        if lum < 75 { chunk[3] = 0; }
    }

    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize], &pixels,
    );
    Some(ctx.load_texture("forge_icon", color_image, egui::TextureOptions::LINEAR))
}

/// Decodes raw image bytes (an open image-tab buffer's file content) and
/// uploads them as a texture for `draw_image_view` to paint.
fn load_image_texture(ctx: &egui::Context, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize], &rgba.into_raw(),
    );
    Some(ctx.load_texture("buffer_image", color_image, egui::TextureOptions::LINEAR))
}

/// "Natural" string comparison for the model picker: alphabetical, but a run
/// of digits is compared as a number rather than character-by-character —
/// so "GPT-5.6" sorts before "GPT-5.10" the way a human expects, instead of
/// a plain string sort putting "5.10" before "5.6" (ASCII '1' < '6'). Model
/// names are full of embedded version numbers (GPT-5.4, GPT-5.4-Mini,
/// Claude Sonnet 4.6, …), which is exactly where a plain alphabetical sort
/// falls apart.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        return match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let take_digits = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(&c) = it.peek() {
                        if c.is_ascii_digit() { s.push(c); it.next(); } else { break; }
                    }
                    s
                };
                let na: u64 = take_digits(&mut ai).parse().unwrap_or(0);
                let nb: u64 = take_digits(&mut bi).parse().unwrap_or(0);
                match na.cmp(&nb) {
                    std::cmp::Ordering::Equal => continue,
                    other => other,
                }
            }
            (Some(ca), Some(cb)) => {
                match ca.to_ascii_lowercase().cmp(&cb.to_ascii_lowercase()) {
                    std::cmp::Ordering::Equal => { ai.next(); bi.next(); continue; }
                    other => other,
                }
            }
        };
    }
}

/// Group an endpoint for the model picker. `endpoint_type` (sent by Forge for
/// every endpoint) reliably distinguishes Anthropic and ChatGPT Codex — those
/// are exact. Everything else comes back as the generic `open_ai`-compatible
/// type (that's true for OpenAI itself, OpenRouter, and any self-hosted local
/// server), so within that bucket this falls back to matching well-known
/// `base_url` hosts. That part is a heuristic, not something the protocol
/// tells us directly — an OpenAI-compatible proxy with an unusual URL will
/// just land in "Local / Custom".
fn classify_provider(ep: &serde_json::Value) -> &'static str {
    let endpoint_type = ep.get("endpoint_type").and_then(|v| v.as_str()).unwrap_or("");
    let base_url = ep.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
    match endpoint_type {
        "anthropic"     => "Anthropic",
        "chatgpt_codex" => "ChatGPT",
        _ => {
            if base_url.contains("openrouter.ai") { "OpenRouter" }
            else if base_url.contains("api.openai.com") { "OpenAI" }
            else if base_url.contains("api.x.ai") { "xAI" }
            else { "Local / Custom" }
        }
    }
}

/// ChatGPT Codex endpoints in `config.toml` are only ever meaningfully
/// distinguished by their `model_id` (they all share one `base_url`) — the
/// `name` field is whatever the user typed or left at its generic default
/// (e.g. a bare "chatgpt-codex" entry that's actually GPT-5.4). Rather than
/// depending on that being hand-labeled correctly, derive a clean display
/// name straight from `model_id`: split on '-', capitalize each segment
/// ("gpt" specifically becomes "GPT"), rejoin with '-'. This exactly
/// reproduces the naming already used for the correctly-labeled entries
/// ("gpt-5.4-mini" -> "GPT-5.4-Mini", "gpt-5.3-codex-spark" ->
/// "GPT-5.3-Codex-Spark") and fixes the mislabeled ones the same way
/// ("gpt-5.4" -> "GPT-5.4") with no manual config editing required.
fn pretty_codex_model_name(model_id: &str) -> String {
    model_id.split('-')
        .map(|seg| {
            if seg.eq_ignore_ascii_case("gpt") { "GPT".to_string() }
            else {
                let mut chars = seg.chars();
                match chars.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Display label for an endpoint's `name` — auto-derived from `model_id` for
/// ChatGPT Codex endpoints (see `pretty_codex_model_name`), left as-is for
/// everything else (Anthropic/local names are already meaningful and aren't
/// reliably derivable from their model IDs — e.g. Anthropic's own
/// "claude-sonnet-4-6" would need a "4-6" -> "4.6" substitution that doesn't
/// generalize, and local endpoint names like "Vision" intentionally identify
/// the *machine*, not the model string it happens to be running).
fn display_model_label(name: &str, endpoints: &[serde_json::Value]) -> String {
    let Some(ep) = endpoints.iter().find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
        else { return name.to_string() };
    if classify_provider(ep) == "ChatGPT" {
        if let Some(model_id) = ep.get("model_id").and_then(|v| v.as_str()) {
            return pretty_codex_model_name(model_id);
        }
    }
    name.to_string()
}

/// Which sub-object of an endpoint's `reasoning` field actually applies to
/// it, keyed by provider — "ChatGPT" uses `chatgpt_codex.effort`, Anthropic
/// uses `anthropic.thinking`/`budget_tokens`, everything else (local/OpenAI-
/// compatible servers) uses `open_ai_compatible.thinking`/`preserve_thinking`.
fn reasoning_provider_key(ep: &serde_json::Value) -> &'static str {
    match classify_provider(ep) {
        "ChatGPT"    => "chatgpt_codex",
        "Anthropic"  => "anthropic",
        _            => "open_ai_compatible",
    }
}

fn reasoning_str_field<'a>(ep: &'a serde_json::Value, provider_key: &str, field: &str) -> Option<&'a str> {
    ep.get("reasoning")?.get(provider_key)?.get(field)?.as_str()
}

fn effort_label(s: &str) -> &'static str {
    match s {
        "none"    => "None",
        "minimal" => "Minimal",
        "low"     => "Low",
        "medium"  => "Medium",
        "high"    => "High",
        "xhigh"   => "X-High",
        _         => "Default",
    }
}

fn toggle_label(s: &str) -> &'static str {
    match s {
        "on"  => "On",
        "off" => "Off",
        _     => "Default",
    }
}

/// Short "Thinking: X" label for the status-bar badge, reading whichever
/// field actually applies to this endpoint's provider.
// Labeled "Reasoning" rather than "Thinking" to avoid reading as the same
// thing as the existing "· thinking" live-activity indicator elsewhere in
// the status bar (whether the model is actively generating right now) —
// this badge is the configured effort/thinking-budget level instead.
fn reasoning_badge_label(ep: &serde_json::Value) -> String {
    let key = reasoning_provider_key(ep);
    let current = reasoning_str_field(ep, key, if key == "chatgpt_codex" { "effort" } else { "thinking" })
        .unwrap_or("provider_default");
    let label = if key == "chatgpt_codex" { effort_label(current) } else { toggle_label(current) };
    format!("Reasoning: {label}")
}

/// Draw one row of the model picker — a hand-drawn hover-highlighted row (same
/// convention as the conversation-history list above), rather than a default
/// `ui.button()`, to match the rest of this panel's look. The current model
/// gets a hand-drawn checkmark rather than a "✓" character — that glyph isn't
/// in every font (same issue the disclosure triangles had) and shows as a
/// missing-glyph box.
/// Hand-drawn checkmark at a given center point, font-independent (see
/// `paint_disclosure_triangle` for why: not every glyph, including "✓", is
/// guaranteed to exist in the loaded font).
fn paint_checkmark_at(painter: &egui::Painter, c: egui::Pos2, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.4_f32, color);
    painter.line_segment([c + egui::vec2(-3.5, 0.0), c + egui::vec2(-1.0, 2.8)], stroke);
    painter.line_segment([c + egui::vec2(-1.0, 2.8), c + egui::vec2(4.0, -3.2)], stroke);
}

/// Same, but allocates its own small cell — for use inline in a `ui.horizontal`
/// row alongside other widgets.
fn paint_checkmark(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    paint_checkmark_at(ui.painter(), rect.center(), color);
}

/// Small filled dot, font-independent — used as an "in progress" indicator
/// instead of a bullet/circle glyph.
fn paint_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

/// Unfilled ring — "awaiting approval" (distinguishes from the filled
/// `paint_dot` "running" state at a glance without relying on color alone).
fn paint_ring(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().circle_stroke(rect.center(), 3.2, egui::Stroke::new(1.3_f32, color));
}

/// Small X — "denied"/"error", font-independent (see `paint_checkmark_at`).
fn paint_cross(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    let stroke = egui::Stroke::new(1.4_f32, color);
    ui.painter().line_segment([c + egui::vec2(-3.2, -3.2), c + egui::vec2(3.2, 3.2)], stroke);
    ui.painter().line_segment([c + egui::vec2(-3.2, 3.2), c + egui::vec2(3.2, -3.2)], stroke);
}

/// Formats a token count for compact display: 372000 -> "372k", 1000000 -> "1M".
fn format_ctx_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        if m.fract() == 0.0 { format!("{}M", m as u64) } else { format!("{:.1}M", m) }
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

fn context_strategy_label(strategy: &str) -> &'static str {
    match strategy {
        "rolling_window" => "Rolling Window",
        _                => "Compaction",
    }
}

fn draw_model_row(ui: &mut egui::Ui, name: &str, ctx_tokens: u64, is_current: bool) -> egui::Response {
    let avail = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(avail, 26.0), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(58, 58, 62));
    } else if is_current {
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_rgb(38, 48, 60));
    }
    let text_x = rect.left() + 22.0;
    if is_current {
        paint_checkmark_at(ui.painter(), egui::pos2(rect.left() + 11.0, rect.center().y),
            egui::Color32::from_rgb(120, 190, 255));
    }
    ui.painter().text(
        egui::pos2(text_x, rect.center().y), egui::Align2::LEFT_CENTER, name,
        egui::FontId::proportional(12.0),
        if is_current { egui::Color32::from_rgb(200, 225, 255) } else { egui::Color32::from_gray(200) });
    if ctx_tokens > 0 {
        ui.painter().text(
            egui::pos2(rect.right() - 8.0, rect.center().y), egui::Align2::RIGHT_CENTER,
            format_ctx_tokens(ctx_tokens),
            egui::FontId::proportional(10.5),
            egui::Color32::from_gray(120));
    }
    resp
}

/// A clickable label + disclosure-triangle in the chat status bar (the
/// model / permission-mode / reasoning-effort pickers). These used to be a
/// bare `ui.label` + triangle with the click sense tightened to their own
/// unpadded text bounds, crammed edge-to-edge against neighboring badges
/// with only a `·` between them — a thin, easy-to-miss target with no
/// visual sign it was clickable until the cursor happened to land exactly
/// on the text. This gives each one its own padded, full-bar-height hit
/// area with a hover highlight, so the clickable region is obvious and
/// comfortably larger than the label itself.
fn draw_status_badge(
    ui: &mut egui::Ui,
    salt: &str,
    text: &str,
    color: egui::Color32,
    expanded: bool,
    height: f32,
) -> egui::Response {
    let bg = ui.painter().add(egui::Shape::Noop);
    let inner = ui.allocate_ui(egui::vec2(0.0, height), |ui| {
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.add_space(6.0);
            ui.label(egui::RichText::new(text).size(10.5).color(color));
            ui.add_space(2.0);
            paint_disclosure_triangle(ui, expanded, color);
            ui.add_space(6.0);
        });
    });
    let rect = inner.response.rect;
    let id = ui.id().with("status_badge").with(salt);
    let resp = ui.interact(rect, id, egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().set(bg, egui::Shape::rect_filled(rect, 4.0, egui::Color32::from_rgb(52, 52, 56)));
    }
    resp
}

/// One selectable option in the permission-mode dropdown. Unlike a bare
/// `ui.vertical(..).response.interact(click)`, whose hit-rect was only ever
/// as tight as its unpadded label+description text (the "slim click space"
/// this replaces), this reserves a full-width padded card and highlights it
/// on hover/current — the description's wrapped height isn't known until
/// after it's laid out, so the background is painted into a placeholder
/// shape slot reserved *before* the content, then filled in once the real
/// rect is known.
fn draw_perm_mode_row(
    ui: &mut egui::Ui,
    salt: usize,
    label: &str,
    description: &str,
    color: egui::Color32,
    is_current: bool,
) -> egui::Response {
    let bg = ui.painter().add(egui::Shape::Noop);
    let content = ui.allocate_ui(egui::vec2(ui.available_width(), 0.0), |ui| {
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - 8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(label).size(12.5).color(color).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if is_current { paint_checkmark(ui, color); }
                    });
                });
                ui.add_space(3.0);
                ui.label(egui::RichText::new(description).size(9.5)
                    .color(egui::Color32::from_gray(150)));
            });
        });
        ui.add_space(5.0);
    });
    let rect = content.response.rect;
    let id = ui.id().with("perm_mode_row").with(salt);
    let resp = ui.interact(rect, id, egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().set(bg, egui::Shape::rect_filled(rect, 5.0, egui::Color32::from_rgb(55, 55, 60)));
    } else if is_current {
        ui.painter().set(bg, egui::Shape::rect_filled(rect, 5.0, egui::Color32::from_rgb(40, 45, 54)));
    }
    resp
}

/// Paint a small disclosure triangle (pointing right when collapsed, down when
/// expanded) at the cursor position, font-independent — matches the convention
/// already used for the file tree's expand arrows and the diff prev/next
/// buttons, since not every glyph (e.g. ▾/▸) is guaranteed to exist in the
/// loaded font and shows as a missing-glyph box otherwise.
fn paint_disclosure_triangle(ui: &mut egui::Ui, expanded: bool, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    let pts = if expanded {
        vec![egui::pos2(c.x - 4.0, c.y - 2.0), egui::pos2(c.x + 4.0, c.y - 2.0), egui::pos2(c.x, c.y + 3.0)]
    } else {
        vec![egui::pos2(c.x - 2.5, c.y - 4.0), egui::pos2(c.x + 3.5, c.y), egui::pos2(c.x - 2.5, c.y + 4.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

/// egui's word-wrap only breaks at whitespace — a single long unbroken run
/// (a path, hash, minified line, long URL) overflows its container instead of
/// wrapping. Insert a zero-width space every `max_run` characters into any
/// whitespace-free run longer than that, giving egui's existing wrap logic
/// somewhere to break without changing what's visually displayed.
/// How many characters `soft_wrap` should let run before it offers a break, for
/// the width actually available here.
///
/// The break opportunities it inserts are only that — opportunities — so
/// offering them too often costs nothing, while offering them too rarely means
/// a line that cannot be broken where it needs to be. The constants these
/// callers used (40, 44, 60) were each right for one panel width: 44 characters
/// of monospace is about 280pt, so in a panel narrowed past that, every one of
/// these lines ran off the edge instead of wrapping.
///
/// Measured against the widest glyph in the font, so the answer holds whatever
/// the text turns out to be.
/// Width of `text` at `size`, for reserving space before it is drawn.
fn text_width(ui: &egui::Ui, text: &str, size: f32, monospace: bool) -> f32 {
    let font = if monospace {
        egui::FontId::monospace(size)
    } else {
        egui::FontId::proportional(size)
    };
    ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, egui::Color32::WHITE).size().x)
}

/// Width a small button carrying `text` will take, including its padding.
fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    text_width(ui, text, 12.0, false) + ui.spacing().button_padding.x * 2.0
}

fn wrap_run(ui: &egui::Ui, size: f32, monospace: bool) -> usize {
    let font = if monospace {
        egui::FontId::monospace(size)
    } else {
        egui::FontId::proportional(size)
    };
    let widest = ui.fonts(|f| f.glyph_width(&font, 'W')).max(1.0);
    run_for_width(ui.available_width(), widest)
}

/// The arithmetic of [`wrap_run`], separated from the font it measures.
///
/// The floor of 8 matters: a panel dragged to its narrowest, or a deeply
/// indented card, can leave almost no room, and a run of 0 would put a break
/// between every character.
fn run_for_width(avail: f32, widest_glyph: f32) -> usize {
    if widest_glyph <= 0.0 {
        return 8;
    }
    ((avail / widest_glyph).floor() as usize).max(8)
}

pub(crate) fn soft_wrap(text: &str, max_run: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for ch in text.chars() {
        if ch.is_whitespace() {
            run = 0;
        } else {
            run += 1;
            if run > max_run {
                out.push('\u{200B}');
                run = 1;
            }
        }
        out.push(ch);
    }
    out
}

/// One row of a parsed diff — see `parse_diff_result`.
struct DiffLine {
    marker:  char,        // '+' / '-' / ' '
    line_no: Option<u32>, // absent for apply_patch-derived diffs (no line numbers)
    text:    String,
}

/// A tool result parsed from forge-agent's `DIFF:{path}\n+{added} -{removed}\n<lines>`
/// format — emitted by `write_file`/`edit_file`/`apply_patch` on a successful
/// edit (see that project's `tools/executor.rs::format_edit_diff`/`format_patch_diff`).
struct ParsedDiff {
    path:    String,
    added:   u32,
    removed: u32,
    lines:   Vec<DiffLine>,
}

/// Parses a `DIFF:`-formatted tool result into structured rows for the
/// diffstat badge and diff popup. Returns `None` for anything else (a
/// fresh-file `WRITE:` result, an error/mismatch message, or any non-edit
/// tool's result) — those just render as plain text.
///
/// `edit_file`/`write_file` diffs prefix each line with a line number
/// (`"- {:>4} {text}"`); `apply_patch` diffs don't (`"- {text}"`) since a raw
/// unified diff hunk doesn't carry one per line without re-deriving it from
/// the `@@` header. The line-number token is therefore optional, not assumed.
fn parse_diff_result(content: &str) -> Option<ParsedDiff> {
    let mut lines = content.lines();
    let path = lines.next()?.strip_prefix("DIFF:")?.to_string();
    let mut stat_parts = lines.next()?.split_whitespace();
    let added: u32 = stat_parts.next()?.strip_prefix('+')?.parse().ok()?;
    let removed: u32 = stat_parts.next()?.strip_prefix('-')?.parse().ok()?;

    let rows = lines.filter(|l| !l.is_empty()).map(|line| {
        let marker = line.chars().next().unwrap_or(' ');
        let rest = line.get(1..).unwrap_or("").trim_start();
        match rest.split_once(' ') {
            Some((n, t)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                DiffLine { marker, line_no: n.parse().ok(), text: t.to_string() }
            }
            _ => DiffLine { marker, line_no: None, text: rest.to_string() },
        }
    }).collect();

    Some(ParsedDiff { path, added, removed, lines: rows })
}

/// Strips ANSI/VT100 escape sequences and most control characters from tool
/// output before displaying it as plain text. `shell_exec` runs commands
/// through a real PTY (see forge's `run_shell_streaming`/`spawn_command`),
/// so its captured output can legitimately contain raw terminal control
/// sequences — cursor/keypad mode switches, charset designators, etc. —
/// exactly like a real terminal session would receive. This card is a
/// plain-text preview, not a terminal emulator, so those sequences need to
/// be dropped rather than shown as literal garbage (`□[?1h□=`) where a
/// command's actual output should have been.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => { // CSI — consume through the final letter (or '~')
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == '~' { break; }
                    }
                }
                Some(']') => { // OSC — consume through BEL or ESC
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || c2 == '\x1b' { break; }
                    }
                }
                Some('(') | Some(')') => { // charset designation — one more byte
                    chars.next();
                    chars.next();
                }
                Some(_) => { chars.next(); } // simple 2-byte sequence (ESC=, ESC>, ESC M, ...)
                None => {}
            }
            continue;
        }
        if c == '\r' { continue; }
        if (c as u32) < 0x20 && c != '\n' && c != '\t' { continue; }
        out.push(c);
    }
    out
}

/// One-line plain-English description of a tool call, for the compact card
/// styles — e.g. "Read session.rs (1–140)", "Searched code for `pub fn`".
/// Field names are tool-specific (`forge`'s own `tools/definitions.rs`
/// schemas don't agree on `path` vs `query` vs `command`), so this is a
/// small per-tool dispatch rather than a generic key-scan.
fn describe_tool_call(name: &str, args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let n = |k: &str| v.get(k).and_then(|x| x.as_i64());

    match name {
        "read_file" => match (n("start_line"), n("end_line")) {
            (Some(a), Some(b)) => format!("Read {} ({a}–{b})", s("path")),
            (Some(a), None)    => format!("Read {} (from {a})", s("path")),
            _                  => format!("Read {}", s("path")),
        },
        "list_directory" => {
            let p = s("path");
            format!("Listed {}", if p.is_empty() || p == "." { "project root" } else { &p })
        }
        "search_code"  => format!("Searched code for {}", s("query")),
        "glob_files"   => format!("Searched files matching {}", s("pattern")),
        "todo_write"   => "Updated the task list".to_string(),
        "web_search"   => format!("Searched the web for {}", s("query")),
        "web_fetch"    => format!("Fetched {}", s("url")),
        "write_file"   => format!("Wrote {}", s("path")),
        "edit_file"    => format!("Edited {}", s("path")),
        "apply_patch"  => "Applied a patch".to_string(),
        "shell_exec"   => s("command"),
        "enter_plan_mode" => "Entered plan mode".to_string(),
        "ask_question" => "Asked a question".to_string(),
        other => other.replace('_', " "),
    }
}

/// Trailing result count for a grouped read-only tool call — "12 results"
/// for the tools where that's meaningful, `None` otherwise (e.g. `read_file`,
/// where showing a line/char count reads as noise, not signal).
fn read_group_meta(name: &str, result: Option<&str>) -> Option<String> {
    let content = result?.trim();
    if content.is_empty() { return None; }
    match name {
        "search_code" | "glob_files" | "list_directory" => {
            let n = content.lines().filter(|l| !l.trim().is_empty()).count();
            Some(format!("{n} result{}", if n == 1 { "" } else { "s" }))
        }
        _ => None,
    }
}

/// A hover-revealed "Copy" button under a message, so a message can be taken
/// whole without dragging a selection across it — selection is fiddly for
/// anything taller than the viewport, and it also picks up the soft-wrap
/// breaks rather than the text as it was written.
///
/// The 16pt strip is allocated whether or not the button is showing, so
/// moving the pointer down the conversation doesn't reflow it.
fn copy_message_button(ui: &mut egui::Ui, body: egui::Rect, id: egui::Id, text: &str, pad_l: f32) {
    /// How long the button stays on "Copied" — long enough to read, short
    /// enough that it's back to normal by the next time you look at it.
    const ACK: f64 = 1.2;

    let (strip, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 16.0),
        egui::Sense::hover(),
    );
    let now = ui.input(|i| i.time);
    let copied_at = ui.data(|d| d.get_temp::<f64>(id));
    let acking = copied_at.is_some_and(|t| now - t < ACK);
    if !acking && !ui.rect_contains_pointer(body) && !ui.rect_contains_pointer(strip) {
        return;
    }

    let rect = egui::Rect::from_min_size(
        egui::pos2(strip.left() + pad_l, strip.top()),
        egui::vec2(if acking { 52.0 } else { 44.0 }, 15.0),
    );
    let label = if acking { "Copied" } else { "Copy" };
    let resp = ui.put(
        rect,
        egui::Button::new(egui::RichText::new(label).size(10.0).color(egui::Color32::from_gray(170)))
            .fill(egui::Color32::from_gray(38))
            .small(),
    );
    if resp.clicked() {
        ui.output_mut(|o| o.copied_text = text.to_string());
        ui.data_mut(|d| d.insert_temp(id, now));
    }
    if acking {
        // Nothing else asks for a frame once the pointer stops, so the label
        // would sit on "Copied" until something unrelated redrew.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(ACK));
    }
}

/// Points to scroll a list this frame while a selection drag is being held
/// past its edge, or `None` to stay put. `overshoot` is how far beyond the
/// edge the pointer is, negative above and positive below.
///
/// Speed ramps with the overshoot so a small overrun creeps and a big one
/// moves, and is capped so a pointer parked far outside the window can't
/// slingshot past everything before the user lets go.
fn edge_scroll_step(overshoot: f32, dt: f32) -> Option<f32> {
    /// Overshoot at which the ramp reaches full speed.
    const RAMP: f32 = 60.0;
    /// Points per second at full speed — roughly four lines' worth.
    const MAX_SPEED: f32 = 900.0;
    /// Below this the pointer is effectively still on the edge.
    const DEAD: f32 = 1.0;

    if overshoot.abs() < DEAD || dt <= 0.0 {
        return None;
    }
    let ramp = (overshoot.abs() / RAMP).min(1.0);
    Some(overshoot.signum() * ramp * MAX_SPEED * dt.min(0.1))
}

/// `edge_scroll_step` for the ui's own viewport, applied only while the
/// primary button is held down on a drag that began inside it — so dragging
/// the scrollbar, or holding the button down over some other panel, doesn't
/// drag the conversation along with it.
fn selection_autoscroll(ui: &egui::Ui) -> Option<f32> {
    let view = ui.clip_rect();
    let (down, origin, pos, dt) = ui.input(|i| {
        (
            i.pointer.primary_down(),
            i.pointer.press_origin(),
            i.pointer.interact_pos(),
            i.stable_dt,
        )
    });
    if !down {
        return None;
    }
    let origin = origin?;
    if !view.contains(origin) {
        return None;
    }
    let y = pos?.y;
    let overshoot = if y < view.top() {
        y - view.top()
    } else if y > view.bottom() {
        y - view.bottom()
    } else {
        return None;
    };
    edge_scroll_step(overshoot, dt)
}

/// Whether `items[i]` (a `ToolRequest`) is immediately followed by its
/// `ToolResult` — forge's top-level agent runs one tool call at a time, so
/// request/result always arrive as an adjacent pair once the call finishes;
/// while it's still running, only the request has arrived yet.
fn tool_call_has_result(items: &[ChatItem], i: usize) -> bool {
    i + 1 < items.len() && matches!(items[i + 1], ChatItem::ToolResult { .. })
}

/// Renders one tool-call "run" starting at `items[start]` (always a
/// `ToolRequest`) using whichever card style fits, and returns the index
/// just past everything it consumed — 1 or 2 items for a single call, more
/// for a grouped run of read-only calls.
/// Fit a tool summary — `Edited /a/long/path/file.md` — into `avail` points by
/// dropping leading path components, so what survives is the end.
///
/// egui truncates from the tail, which on a path removes the only part that
/// identifies which file it is; "Edited /Users/someone/Projects/some-pro…"
/// tells you nothing. Dropping from the front instead gives ".../paper/note.md",
/// which is how every editor renders a path it cannot fit.
///
/// Returns the text unchanged when it already fits, or when there is no path in
/// it to shorten — the caller's `truncate()` is the backstop for that case.
fn elide_path_head(ui: &egui::Ui, text: &str, font: &egui::FontId, avail: f32) -> String {
    elide_path_head_by(text, avail, |s| {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE)
                .size()
                .x
        })
    })
}

/// `elide_path_head` with the text measurement supplied, so the rule can be
/// tested without a live `Ui` and its fonts.
fn elide_path_head_by(text: &str, avail: f32, width: impl Fn(&str) -> f32) -> String {
    if avail <= 0.0 || width(text) <= avail {
        return text.to_owned();
    }
    let Some(slash) = text.find('/') else { return text.to_owned() };
    let (head, path) = text.split_at(slash);
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for skip in 1..parts.len() {
        let candidate = format!("{head}…/{}", parts[skip..].join("/"));
        if width(&candidate) <= avail {
            return candidate;
        }
    }
    // Not even the file name on its own fits; hand the caller something whose
    // tail is still the name, and let the label ellipsize what is left.
    match parts.last() {
        Some(name) => format!("{head}…/{name}"),
        None => text.to_owned(),
    }
}

fn draw_tool_run(
    ui: &mut egui::Ui,
    items: &[ChatItem],
    start: usize,
    pad_l: f32,
    pad_r: f32,
    pending_action: &mut Option<(usize, bool)>,
    toggle_expand: &mut Option<usize>,
) -> usize {
    // Consecutive read-only calls (search/read/list/glob) never need
    // individual attention — they fold into one compact checklist so a
    // multi-step lookup doesn't produce a wall of identical-looking cards.
    // Anything else (a write, a shell command, or a call still awaiting
    // approval) gets its own card and breaks the run.
    let is_groupable = |i: usize| matches!(&items[i],
        ChatItem::ToolRequest { kind, approval, .. }
            if kind == "read" && !matches!(approval, ApprovalState::Pending));

    if is_groupable(start) {
        let mut end = start;
        while end < items.len() && is_groupable(end) {
            end += if tool_call_has_result(items, end) { 2 } else { 1 };
        }
        draw_read_group(ui, items, start, end, pad_l, pad_r);
        return end;
    }

    let has_result = tool_call_has_result(items, start);
    draw_single_tool_card(ui, items, start, has_result, pad_l, pad_r, pending_action, toggle_expand);
    start + if has_result { 2 } else { 1 }
}

fn draw_read_group(ui: &mut egui::Ui, items: &[ChatItem], start: usize, end: usize, pad_l: f32, pad_r: f32) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(27, 29, 32))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(44, 47, 52)))
            .inner_margin(egui::Margin::symmetric(10.0, 7.0))
            .rounding(6.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    let mut i = start;
                    let mut first = true;
                    while i < end {
                        let ChatItem::ToolRequest { name, args, approval, .. } = &items[i] else { break };
                        let has_result = tool_call_has_result(items, i);
                        let result = if has_result { Some(&items[i + 1]) } else { None };

                        if !first { ui.add_space(3.0); }
                        first = false;

                        // Right to left, so the row's trailing detail claims
                        // its space first and the path gives way instead of
                        // running off the edge of a narrowed panel.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let meta = result.and_then(|r| match r {
                                ChatItem::ToolResult { content, .. } => read_group_meta(name, Some(content)),
                                _ => None,
                            });
                            if let Some(meta) = meta {
                                ui.label(egui::RichText::new(meta).size(11.0).color(egui::Color32::from_gray(130)));
                            }
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                match (approval, &result) {
                                    (ApprovalState::Denied, _) => paint_cross(ui, egui::Color32::from_rgb(224, 110, 95)),
                                    (_, None) => paint_dot(ui, egui::Color32::from_rgb(110, 150, 220)),
                                    (_, Some(ChatItem::ToolResult { success: false, .. })) =>
                                        paint_cross(ui, egui::Color32::from_rgb(224, 110, 95)),
                                    _ => paint_checkmark(ui, egui::Color32::from_rgb(120, 190, 120)),
                                }
                                let avail = ui.available_width();
                                let font = egui::FontId::proportional(12.0);
                                let desc = elide_path_head(
                                    ui, &describe_tool_call(name, args), &font, avail,
                                );
                                ui.add_sized(
                                    egui::vec2(avail, 16.0),
                                    egui::Label::new(
                                        egui::RichText::new(desc).size(12.0)
                                            .color(egui::Color32::from_gray(205)),
                                    )
                                    .truncate(),
                                );
                            });
                        });

                        i += if has_result { 2 } else { 1 };
                    }
                });
            });
    });
}

fn draw_single_tool_card(
    ui: &mut egui::Ui,
    items: &[ChatItem],
    idx: usize,
    has_result: bool,
    pad_l: f32,
    pad_r: f32,
    pending_action: &mut Option<(usize, bool)>,
    toggle_expand: &mut Option<usize>,
) {
    let ChatItem::ToolRequest { name, args, approval, expanded, .. } = &items[idx] else { return };
    let result = if has_result { Some(&items[idx + 1]) } else { None };

    if name == "shell_exec" {
        draw_terminal_card(ui, idx, name, args, approval, result, pad_l, pad_r, pending_action);
    } else {
        draw_write_card(ui, idx, name, args, approval, *expanded, result, pad_l, pad_r, pending_action, toggle_expand);
    }
}

/// Status-rail card for a write-kind call (`edit_file`/`write_file`/
/// `apply_patch`) or anything else that isn't `shell_exec` — the rail color
/// carries the state (git-gutter-style) so it reads at a glance without
/// needing to parse the text. Shows a diffstat + "Open diff" popup when the
/// result is a parseable `DIFF:` (see `parse_diff_result`); otherwise falls
/// back to a plain one-line result preview.
#[allow(clippy::too_many_arguments)]
fn draw_write_card(
    ui: &mut egui::Ui,
    idx: usize,
    name: &str,
    args: &str,
    approval: &ApprovalState,
    expanded: bool,
    result: Option<&ChatItem>,
    pad_l: f32,
    pad_r: f32,
    pending_action: &mut Option<(usize, bool)>,
    toggle_expand: &mut Option<usize>,
) {
    let content = result.and_then(|r| match r {
        ChatItem::ToolResult { content, .. } => Some(content.as_str()),
        _ => None,
    });
    let success = result.map_or(true, |r| matches!(r, ChatItem::ToolResult { success: true, .. }));
    let diff = content.and_then(parse_diff_result);

    let rail = match (approval, result.is_some(), success) {
        (ApprovalState::Denied, ..)      => egui::Color32::from_rgb(224, 110, 95),
        (ApprovalState::Pending, ..)     => egui::Color32::from_rgb(224, 158, 90),
        (_, false, _)                    => egui::Color32::from_rgb(110, 150, 220), // running
        (_, true, false)                 => egui::Color32::from_rgb(224, 110, 95),  // errored
        (_, true, true)                  => egui::Color32::from_rgb(110, 190, 110), // done
    };

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        let frame_resp = egui::Frame::none()
            .fill(egui::Color32::from_rgb(26, 28, 31))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(44, 47, 52)))
            .rounding(6.0)
            .inner_margin(egui::Margin { left: 12.0, right: 10.0, top: 7.0, bottom: 7.0 })
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    // Right to left, so the diffstat and its button claim their
                    // space before the path does: they are controls, and a
                    // narrowed panel used to push them off the edge entirely
                    // while the path it could not fit ran on past them.
                    let header = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(d) = &diff {
                            ui.label(egui::RichText::new(format!("+{} \u{2212}{}", d.added, d.removed))
                                .size(11.0).monospace().color(egui::Color32::from_gray(150)));
                            ui.add_space(6.0);
                            let popup_id = ui.make_persistent_id(("tool_diff_popup", idx));
                            let btn = ui.add(egui::Button::new(egui::RichText::new("Open diff").size(11.0))
                                .fill(egui::Color32::from_rgb(36, 39, 44)));
                            if btn.clicked() { ui.memory_mut(|m| m.toggle_popup(popup_id)); }
                            egui::popup_below_widget(
                                ui, popup_id, &btn, egui::PopupCloseBehavior::CloseOnClickOutside,
                                |ui| draw_diff_popup(ui, d),
                            );
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            match (approval, result.is_some(), success) {
                                (ApprovalState::Denied, ..)  => paint_cross(ui, rail),
                                (ApprovalState::Pending, ..) => paint_ring(ui, rail),
                                (_, false, _)                => paint_dot(ui, rail),
                                (_, true, false)              => paint_cross(ui, rail),
                                (_, true, true)                => paint_checkmark(ui, rail),
                            }
                            let avail = ui.available_width();
                            let font = egui::FontId::proportional(12.5);
                            let desc = elide_path_head(
                                ui, &describe_tool_call(name, args), &font, avail,
                            );
                            ui.add_sized(
                                egui::vec2(avail, 16.0),
                                egui::Label::new(
                                    egui::RichText::new(desc).size(12.5)
                                        .color(egui::Color32::from_gray(220)),
                                )
                                .truncate(),
                            );
                        });
                    });

                    let mut click_target = header.response.clone();
                    if diff.is_none() {
                        if !expanded {
                            if let Some(c) = content {
                                let first_line: String = strip_ansi(c.trim()).lines().next().unwrap_or("").chars().take(70).collect();
                                if !first_line.is_empty() {
                                    let resp = ui.label(egui::RichText::new(soft_wrap(&first_line, wrap_run(ui, 10.5, true)))
                                        .monospace().size(10.5).color(egui::Color32::from_gray(140)));
                                    click_target = click_target.union(resp);
                                }
                            }
                        } else if let Some(c) = content {
                            let full = strip_ansi(c.trim());
                            if !full.is_empty() {
                                // Full content, not a truncated preview — scrollable
                                // instead of cut off, since a tool result (a big
                                // read, a long search) can run well past any fixed
                                // character cap.
                                // See the plan card's identical fix — this can land as the
                                // newest chat item too, where the auto-scrolled-to-bottom
                                // outer chat area leaves `available_height()` near zero,
                                // and `ScrollArea` would otherwise fall back to its tiny
                                // 64px default floor instead of this 220px cap.
                                // See the main chat ScrollArea's `drag_to_scroll(false)` —
                                // same reasoning: this holds selectable text too.
                                egui::ScrollArea::vertical().id_salt(("tool_card_body", idx))
                                    .max_height(220.0).min_scrolled_height(180.0).drag_to_scroll(false)
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new(soft_wrap(&full, wrap_run(ui, 10.5, true)))
                                            .monospace().size(10.5).color(egui::Color32::from_gray(150)));
                                    });
                            }
                        }
                        if !matches!(approval, ApprovalState::Pending)
                            && click_target.interact(egui::Sense::click()).clicked() {
                            *toggle_expand = Some(idx);
                        }
                    }

                    if matches!(approval, ApprovalState::Pending) {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let approve = ui.add(egui::Button::new(egui::RichText::new("Approve").size(11.0))
                                .fill(egui::Color32::from_rgb(40, 80, 40)));
                            if approve.clicked() { *pending_action = Some((idx, true)); }
                            let deny = ui.add(egui::Button::new(egui::RichText::new("Deny").size(11.0))
                                .fill(egui::Color32::from_rgb(80, 40, 40)));
                            if deny.clicked() { *pending_action = Some((idx, false)); }
                        });
                    }
                });
            });
        // Painted after layout so it can use the frame's real measured
        // height — a colored left rail, git-gutter style, instead of relying
        // on text color alone to convey state.
        let r = frame_resp.response.rect;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(r.min, egui::vec2(3.0, r.height())),
            egui::Rounding { nw: 6.0, sw: 6.0, ne: 0.0, se: 0.0 },
            rail,
        );
    });
}

fn draw_diff_popup(ui: &mut egui::Ui, diff: &ParsedDiff) {
    ui.set_min_width(320.0);
    ui.set_max_width(440.0);
    ui.label(egui::RichText::new(&diff.path).monospace().size(11.0).color(egui::Color32::from_gray(170)));
    ui.add_space(4.0);
    egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
        for line in &diff.lines {
            let (bg, fg) = match line.marker {
                '+' => (egui::Color32::from_rgba_unmultiplied(111, 191, 120, 24), egui::Color32::from_rgb(140, 210, 140)),
                '-' => (egui::Color32::from_rgba_unmultiplied(224, 112, 95, 24), egui::Color32::from_rgb(230, 140, 125)),
                _   => (egui::Color32::TRANSPARENT, egui::Color32::from_gray(140)),
            };
            egui::Frame::none().fill(bg).inner_margin(egui::Margin::symmetric(4.0, 1.0)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let num = line.line_no.map(|n| n.to_string()).unwrap_or_default();
                    ui.label(egui::RichText::new(format!("{:>5}", num)).monospace().size(10.0).color(egui::Color32::from_gray(90)));
                    ui.label(egui::RichText::new(format!("{} {}", line.marker, line.text)).monospace().size(10.5).color(fg));
                });
            });
        }
    });
}

/// Dark, monospace terminal-style card for `shell_exec` specifically — a
/// command reads better as a terminal than as prose, and it's the one tool
/// whose result *is* naturally a stream of raw text.
#[allow(clippy::too_many_arguments)]
fn draw_terminal_card(
    ui: &mut egui::Ui,
    idx: usize,
    name: &str,
    args: &str,
    approval: &ApprovalState,
    result: Option<&ChatItem>,
    pad_l: f32,
    pad_r: f32,
    pending_action: &mut Option<(usize, bool)>,
) {
    let content = result.and_then(|r| match r {
        ChatItem::ToolResult { content, .. } => Some(content.as_str()),
        _ => None,
    });
    let success = result.map_or(true, |r| matches!(r, ChatItem::ToolResult { success: true, .. }));
    let status_color = match (approval, result.is_some(), success) {
        (ApprovalState::Denied, ..)  => egui::Color32::from_rgb(224, 110, 95),
        (ApprovalState::Pending, ..) => egui::Color32::from_rgb(224, 158, 90),
        (_, false, _)                => egui::Color32::from_rgb(110, 150, 220),
        (_, true, false)              => egui::Color32::from_rgb(224, 110, 95),
        (_, true, true)                => egui::Color32::from_rgb(110, 190, 110),
    };

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(14, 15, 17))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(44, 47, 52)))
            .rounding(6.0)
            .inner_margin(egui::Margin::symmetric(11.0, 8.0))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        match (approval, result.is_some(), success) {
                            (ApprovalState::Denied, ..)  => paint_cross(ui, status_color),
                            (ApprovalState::Pending, ..) => paint_ring(ui, status_color),
                            (_, false, _)                => paint_dot(ui, status_color),
                            (_, true, false)              => paint_cross(ui, status_color),
                            (_, true, true)                => paint_checkmark(ui, status_color),
                        }
                        ui.label(egui::RichText::new("shell").monospace().size(10.5).color(egui::Color32::from_gray(120)));
                    });
                    ui.add_space(2.0);
                    let cmd = describe_tool_call(name, args);
                    ui.label(egui::RichText::new(format!("$ {}", soft_wrap(&cmd, wrap_run(ui, 11.5, true))))
                        .monospace().size(11.5).color(egui::Color32::from_gray(225)));
                    if let Some(c) = content {
                        let trimmed = strip_ansi(c.trim());
                        if !trimmed.is_empty() {
                            ui.add_space(3.0);
                            // Full output, scrollable — a build/test run's
                            // output can easily run past any fixed cap.
                            // Same fix as the plan card and tool-result body above.
                            egui::ScrollArea::vertical().id_salt(("term_card_body", idx))
                                .max_height(220.0).min_scrolled_height(180.0).drag_to_scroll(false)
                                .show(ui, |ui| {
                                    ui.label(egui::RichText::new(soft_wrap(&trimmed, wrap_run(ui, 11.0, true)))
                                        .monospace().size(11.0).color(egui::Color32::from_gray(165)));
                                });
                        }
                    }
                    if matches!(approval, ApprovalState::Pending) {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let approve = ui.add(egui::Button::new(egui::RichText::new("Approve").size(11.0))
                                .fill(egui::Color32::from_rgb(40, 80, 40)));
                            if approve.clicked() { *pending_action = Some((idx, true)); }
                            let deny = ui.add(egui::Button::new(egui::RichText::new("Deny").size(11.0))
                                .fill(egui::Color32::from_rgb(80, 40, 40)));
                            if deny.clicked() { *pending_action = Some((idx, false)); }
                        });
                    }
                });
            });
    });
}

/// One entry in the docked subagent strip — a minimized chip that expands in
/// place to that subagent's own tool-call history (reusing `draw_tool_run`
/// et al. exactly as the main chat does), so its approvals stay reachable
/// without blocking whatever the main conversation is doing above.
#[allow(clippy::too_many_arguments)]
/// Navigates `path` through nested `ChatItem::Subagent.items`, requiring
/// every entry to be a Subagent, and returns the item living at the full
/// path (which may itself be a Subagent — used for expand-toggling — or a
/// `ToolRequest`/`ToolResult` inside one — not currently used directly, see
/// `subagent_container_items_mut` for the approve/deny case instead).
fn subagent_at_path_mut<'a>(items: &'a mut Vec<ChatItem>, path: &[usize]) -> Option<&'a mut ChatItem> {
    match path {
        [] => None,
        [only] => items.get_mut(*only),
        [head, rest @ ..] => {
            let item = items.get_mut(*head)?;
            let ChatItem::Subagent { items: nested, .. } = item else { return None };
            subagent_at_path_mut(nested, rest)
        }
    }
}

/// Navigates `container_path` through nested `ChatItem::Subagent.items`
/// (each entry must be a Subagent), returning the *items list* living at
/// that subagent — i.e. the container a following index addresses into.
/// An empty path returns `items` itself (the top level).
fn subagent_container_items_mut<'a>(items: &'a mut Vec<ChatItem>, container_path: &[usize]) -> Option<&'a mut Vec<ChatItem>> {
    match container_path {
        [] => Some(items),
        [head, rest @ ..] => {
            let item = items.get_mut(*head)?;
            let ChatItem::Subagent { items: nested, .. } = item else { return None };
            subagent_container_items_mut(nested, rest)
        }
    }
}

/// One interaction with a pending `ChatItem::Question` card this frame —
/// collected during (read-only) rendering and applied afterward via a fresh
/// `&mut` borrow, same pattern as `pending_action`/`toggle_expand` above.
enum QuestionEdit {
    /// Single-select replaces the whole selection; multi-select toggles
    /// membership — `draw_question_card` doesn't know which, only the
    /// application site (which has the real `QuestionItem::multi_select`) does.
    ToggleOption { item_idx: usize, question_idx: usize, option_idx: usize },
    /// Free-text override for whichever question had "Other" picked.
    OtherText { item_idx: usize, question_idx: usize, text: String },
    /// Free-text reply box content (only for a plain question with no items).
    FreeText { item_idx: usize, text: String },
    /// Finalize and send the combined answer.
    Submit { item_idx: usize },
}

impl QuestionEdit {
    fn item_idx(&self) -> usize {
        match self {
            QuestionEdit::ToggleOption { item_idx, .. }
            | QuestionEdit::OtherText { item_idx, .. }
            | QuestionEdit::FreeText { item_idx, .. }
            | QuestionEdit::Submit { item_idx } => *item_idx,
        }
    }
}

/// Renders a pending (or already-answered) `ask_question` call — either a
/// plain free-text question (`items` empty) or one-to-several structured
/// multi-choice questions matching Anthropic's own AskUserQuestion tool
/// shape. Styled after the permission-mode picker's option-list pattern.
#[allow(clippy::too_many_arguments)]
fn draw_question_card(
    ui: &mut egui::Ui,
    item_idx: usize,
    question: &str,
    items: &[crate::agent_panel::QuestionItem],
    selected: &[Vec<usize>],
    other_text: &[String],
    free_text: &str,
    answered: bool,
    pad_l: f32,
    pad_r: f32,
    edit: &mut Option<QuestionEdit>,
) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(26, 32, 42))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 105, 145)))
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .rounding(6.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if answered {
                            paint_checkmark(ui, egui::Color32::from_rgb(140, 180, 220));
                        } else {
                            paint_ring(ui, egui::Color32::from_rgb(224, 158, 90));
                        }
                        ui.label(egui::RichText::new("question").size(11.5).color(egui::Color32::from_rgb(140, 180, 220)));
                    });
                    ui.add_space(3.0);

                    if items.is_empty() {
                        ui.label(egui::RichText::new(soft_wrap(question, wrap_run(ui, 12.5, false))).size(12.5).color(egui::Color32::from_gray(220)));
                        if answered {
                            if !free_text.trim().is_empty() {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(soft_wrap(free_text, wrap_run(ui, 11.0, false))).italics().size(11.0).color(egui::Color32::from_gray(160)));
                            }
                        } else {
                            ui.add_space(4.0);
                            let mut text = free_text.to_string();
                            let resp = ui.add(egui::TextEdit::multiline(&mut text).desired_rows(2).hint_text("Type your answer…"));
                            if resp.changed() {
                                *edit = Some(QuestionEdit::FreeText { item_idx, text });
                            }
                            ui.add_space(4.0);
                            if ui.button("Submit").clicked() {
                                *edit = Some(QuestionEdit::Submit { item_idx });
                            }
                        }
                    } else {
                        for (qi, q) in items.iter().enumerate() {
                            if qi > 0 { ui.add_space(8.0); }
                            if !q.header.is_empty() {
                                ui.label(egui::RichText::new(&q.header).strong().size(11.5).color(egui::Color32::from_rgb(140, 180, 220)));
                            }
                            ui.label(egui::RichText::new(soft_wrap(&q.question, wrap_run(ui, 12.0, false))).size(12.0).color(egui::Color32::from_gray(220)));
                            ui.add_space(3.0);
                            let sel = selected.get(qi).cloned().unwrap_or_default();
                            if answered {
                                let labels: Vec<&str> = sel.iter()
                                    .filter_map(|&oi| q.options.get(oi)).map(|o| o.label.as_str()).collect();
                                let summary = if labels.is_empty() { "(no answer)".to_string() } else { labels.join(", ") };
                                ui.label(egui::RichText::new(summary).italics().size(11.0).color(egui::Color32::from_gray(160)));
                            } else {
                                for (oi, opt) in q.options.iter().enumerate() {
                                    let is_sel = sel.contains(&oi);
                                    let row = ui.horizontal(|ui| {
                                        ui.add_space(4.0);
                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                                        if is_sel {
                                            ui.painter().circle_filled(rect.center(), 4.0, egui::Color32::from_rgb(140, 180, 220));
                                        }
                                        ui.painter().circle_stroke(rect.center(), 5.0, egui::Stroke::new(1.0_f32, egui::Color32::from_gray(140)));
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(&opt.label).size(11.5)
                                                .color(if is_sel { egui::Color32::WHITE } else { egui::Color32::from_gray(200) }));
                                            if !opt.description.is_empty() {
                                                ui.label(egui::RichText::new(soft_wrap(&opt.description, wrap_run(ui, 10.0, false))).size(10.0).color(egui::Color32::from_gray(140)));
                                            }
                                        });
                                    });
                                    if row.response.interact(egui::Sense::click()).clicked() {
                                        *edit = Some(QuestionEdit::ToggleOption { item_idx, question_idx: qi, option_idx: oi });
                                    }
                                    if is_sel && opt.label.eq_ignore_ascii_case("other") {
                                        let mut text = other_text.get(qi).cloned().unwrap_or_default();
                                        ui.add_space(2.0);
                                        let inner = ui.horizontal(|ui| {
                                            ui.add_space(20.0);
                                            ui.add(egui::TextEdit::singleline(&mut text).hint_text("Your answer…"))
                                        }).inner;
                                        if inner.changed() {
                                            *edit = Some(QuestionEdit::OtherText { item_idx, question_idx: qi, text });
                                        }
                                    }
                                }
                            }
                        }
                        if !answered {
                            ui.add_space(6.0);
                            if ui.button("Submit").clicked() {
                                *edit = Some(QuestionEdit::Submit { item_idx });
                            }
                        }
                    }
                });
            });
    });
}

enum PlanEdit {
    FeedbackText { item_idx: usize, text: String },
    ToggleExpand { item_idx: usize },
    Approve      { item_idx: usize },
    ApproveClear { item_idx: usize },
    Reject       { item_idx: usize },
    Discuss      { item_idx: usize },
}

impl PlanEdit {
    fn item_idx(&self) -> usize {
        match self {
            PlanEdit::FeedbackText { item_idx, .. }
            | PlanEdit::ToggleExpand { item_idx }
            | PlanEdit::Approve { item_idx }
            | PlanEdit::ApproveClear { item_idx }
            | PlanEdit::Reject { item_idx }
            | PlanEdit::Discuss { item_idx } => *item_idx,
        }
    }
}

/// What to actually send once a `PlanEdit` action is applied — computed
/// while `tab.session.items` is still borrowed, then acted on afterward
/// since sending needs `&mut tab.session` itself, not just its `items`.
enum PlanResolution {
    Approve,
    ApproveClear,
    Reject(String),
    Discuss,
}

/// Renders a plan submitted via `exit_plan_mode` — either awaiting the
/// user's decision (full content, review actions) or already resolved (a
/// compact, collapsible summary), matching the `ask_question` card's
/// unresolved/resolved styling convention.
#[allow(clippy::too_many_arguments)]
fn draw_plan_card(
    ui: &mut egui::Ui,
    item_idx: usize,
    plan_path: &str,
    content: &str,
    resolved: bool,
    resolution: &str,
    reject_feedback: &str,
    expanded: bool,
    pad_l: f32,
    pad_r: f32,
    edit: &mut Option<PlanEdit>,
) {
    let accent = if !resolved {
        egui::Color32::from_rgb(140, 180, 220)
    } else if resolution.starts_with("Approved") {
        egui::Color32::from_rgb(130, 200, 130)
    } else if resolution.starts_with("Rejected") {
        egui::Color32::from_rgb(230, 140, 110)
    } else {
        egui::Color32::from_rgb(140, 180, 220)
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(26, 32, 42))
            .stroke(egui::Stroke::new(1.0_f32, accent.gamma_multiply(0.55)))
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .rounding(6.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    let header = ui.horizontal(|ui| {
                        if resolved {
                            paint_checkmark(ui, accent);
                        } else {
                            paint_ring(ui, accent);
                        }
                        ui.label(egui::RichText::new("plan").size(11.5).color(accent));
                        if !plan_path.is_empty() {
                            // Shortened from the front, as on the tool cards, and
                            // sized so the disclosure triangle keeps its place.
                            let avail = (ui.available_width() - 18.0).max(24.0);
                            let font = egui::FontId::monospace(10.0);
                            let shown = elide_path_head(ui, plan_path, &font, avail);
                            ui.add_sized(
                                egui::vec2(avail, 16.0),
                                egui::Label::new(egui::RichText::new(shown).monospace().size(10.0)
                                    .color(egui::Color32::from_gray(140))).truncate(),
                            );
                        }
                        if resolved {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                paint_disclosure_triangle(ui, expanded, egui::Color32::from_gray(140));
                            });
                        }
                    });
                    if resolved && header.response.interact(egui::Sense::click()).clicked() {
                        *edit = Some(PlanEdit::ToggleExpand { item_idx });
                    }
                    if resolved {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(resolution).italics().size(11.0).color(accent));
                    }

                    if expanded {
                        ui.add_space(6.0);
                        // A plan card can land anywhere in the chat scroll area, including
                        // right at the bottom edge of the current viewport (this is the
                        // common case — the chat auto-scrolls to the newest item, which a
                        // freshly-arrived plan always is). `ui.available_height()` at that
                        // point reflects how much of the *viewport* is left below the
                        // cursor, not the plan's own reasonable content height — often next
                        // to nothing. `ScrollArea` clamps its own height up to only
                        // `min_scrolled_height` (a small 64px default) in that case, so the
                        // plan rendered in a near-invisible sliver instead of its intended
                        // up-to-280px box. `min_scrolled_height` raises that floor so the
                        // plan is actually visible regardless of scroll position.
                        egui::ScrollArea::vertical()
                            .id_salt(("plan_content", item_idx))
                            .max_height(280.0)
                            .min_scrolled_height(220.0)
                            .show(ui, |ui| {
                                crate::markdown::render(ui, content, wrap_run(ui, 12.5, false));
                            });
                    }

                    if !resolved {
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Approve").clicked() {
                                *edit = Some(PlanEdit::Approve { item_idx });
                            }
                            if ui.button("Approve & Clear Context").clicked() {
                                *edit = Some(PlanEdit::ApproveClear { item_idx });
                            }
                            if ui.button("Discuss").clicked() {
                                *edit = Some(PlanEdit::Discuss { item_idx });
                            }
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            let mut text = reject_feedback.to_string();
                            let resp = ui.add(egui::TextEdit::singleline(&mut text)
                                .hint_text("Optional feedback for revision…"));
                            if resp.changed() {
                                *edit = Some(PlanEdit::FeedbackText { item_idx, text });
                            }
                            if ui.button("Reject").clicked() {
                                *edit = Some(PlanEdit::Reject { item_idx });
                            }
                        });
                    }
                });
            });
    });
}

enum InputNeededEdit {
    /// Generic reply text, or the one-time password field.
    Text { item_idx: usize, text: String },
    /// The "type ALLOW to remember" confirmation box (password prompts only).
    RememberConfirm { item_idx: usize, text: String },
    Send { item_idx: usize },
    Remember { item_idx: usize },
    Reject { item_idx: usize },
    /// Clicked on an already auto-resolved card — forgets the saved session
    /// password entirely (not just a pause), matching "reject the password
    /// entirely" as the only other alternative to auto-approve.
    ForgetSaved { item_idx: usize },
}

impl InputNeededEdit {
    fn item_idx(&self) -> usize {
        match self {
            InputNeededEdit::Text { item_idx, .. }
            | InputNeededEdit::RememberConfirm { item_idx, .. }
            | InputNeededEdit::Send { item_idx }
            | InputNeededEdit::Remember { item_idx }
            | InputNeededEdit::Reject { item_idx }
            | InputNeededEdit::ForgetSaved { item_idx } => *item_idx,
        }
    }
}

/// Renders a shell command blocked on stdin — a plain text-reply card for a
/// generic prompt, or (when the prompt text contains "password") a masked
/// password card offering three explicit choices every time a *new* prompt
/// needs deciding: use once (never stored), remember for the rest of the
/// session (requires literally typing ALLOW, not just entering a password —
/// a separate, deliberate gesture), or reject. An auto-resolved card (a
/// previously remembered password answering this one automatically) instead
/// shows a one-click way to forget that saved password outright.
#[allow(clippy::too_many_arguments)]
fn draw_input_needed_card(
    ui: &mut egui::Ui,
    item_idx: usize,
    command: &str,
    prompt: &str,
    is_password: bool,
    resolved: bool,
    resolution: &str,
    text: &str,
    remember_confirm: &str,
    pad_l: f32,
    pad_r: f32,
    edit: &mut Option<InputNeededEdit>,
) {
    let accent = if !resolved {
        egui::Color32::from_rgb(224, 158, 90)
    } else if resolution.starts_with("Rejected") {
        egui::Color32::from_rgb(230, 140, 110)
    } else {
        egui::Color32::from_rgb(150, 190, 150)
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(32, 28, 24))
            .stroke(egui::Stroke::new(1.0_f32, accent.gamma_multiply(0.55)))
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .rounding(6.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        if resolved { paint_checkmark(ui, accent); } else { paint_ring(ui, accent); }
                        let label = if is_password { "password needed" } else { "input needed" };
                        ui.label(egui::RichText::new(label).size(11.5).color(accent));
                        if !command.is_empty() {
                            ui.label(egui::RichText::new(soft_wrap(command, wrap_run(ui, 10.0, true))).monospace().size(10.0)
                                .color(egui::Color32::from_gray(140)));
                        }
                    });
                    ui.add_space(3.0);
                    ui.label(egui::RichText::new(soft_wrap(prompt, wrap_run(ui, 11.5, false))).size(11.5).color(egui::Color32::from_gray(210)));

                    if resolved {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(resolution).italics().size(11.0).color(accent));
                        if resolution.starts_with("Auto-supplied") {
                            let link = ui.label(egui::RichText::new("forget saved password")
                                .size(10.0).underline().color(egui::Color32::from_gray(150)));
                            if link.interact(egui::Sense::click()).clicked() {
                                *edit = Some(InputNeededEdit::ForgetSaved { item_idx });
                            }
                        }
                        return;
                    }

                    ui.add_space(6.0);
                    if is_password {
                        let mut pw = text.to_string();
                        let resp = ui.add(egui::TextEdit::singleline(&mut pw).password(true)
                            .hint_text("Password…"));
                        if resp.changed() {
                            *edit = Some(InputNeededEdit::Text { item_idx, text: pw.clone() });
                        }
                        let has_text = !pw.trim().is_empty();
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.add_enabled(has_text, egui::Button::new("Use Once")).clicked() {
                                *edit = Some(InputNeededEdit::Send { item_idx });
                            }
                            if ui.button("Reject").clicked() {
                                *edit = Some(InputNeededEdit::Reject { item_idx });
                            }
                        });
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(
                            "To let the agent auto-answer this password for the rest of the \
                             session (never stored anywhere but a temp file, never shown to \
                             the model), type ALLOW below.")
                            .size(9.5).color(egui::Color32::from_gray(140)));
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            let mut confirm = remember_confirm.to_string();
                            let resp = ui.add(egui::TextEdit::singleline(&mut confirm)
                                .hint_text("Type ALLOW to remember"));
                            if resp.changed() {
                                *edit = Some(InputNeededEdit::RememberConfirm { item_idx, text: confirm.clone() });
                            }
                            let armed = has_text && confirm.trim().eq_ignore_ascii_case("allow");
                            if ui.add_enabled(armed, egui::Button::new("Remember & Use")).clicked() {
                                *edit = Some(InputNeededEdit::Remember { item_idx });
                            }
                        });
                    } else {
                        let mut reply = text.to_string();
                        let resp = ui.add(egui::TextEdit::singleline(&mut reply).hint_text("Type a reply…"));
                        if resp.changed() {
                            *edit = Some(InputNeededEdit::Text { item_idx, text: reply.clone() });
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Send").clicked() {
                                *edit = Some(InputNeededEdit::Send { item_idx });
                            }
                            if ui.button("Reject").clicked() {
                                *edit = Some(InputNeededEdit::Reject { item_idx });
                            }
                        });
                    }
                });
            });
    });
}

enum RewindEdit {
    Arm { item_idx: usize },
    Cancel { item_idx: usize },
    Confirm { item_idx: usize },
    Preview { item_idx: usize },
}

impl RewindEdit {
    fn item_idx(&self) -> usize {
        match self {
            RewindEdit::Arm { item_idx }
            | RewindEdit::Cancel { item_idx }
            | RewindEdit::Confirm { item_idx }
            | RewindEdit::Preview { item_idx } => *item_idx,
        }
    }
}

/// Renders an automatic rewind checkpoint as a slim inline marker — not a
/// big card, since these show up once per turn unprompted and would
/// otherwise dominate the scrollback. "Rewind here" requires a second,
/// explicit confirm click before it actually sends anything, since it
/// restores file/git state and truncates the agent's own memory back to
/// this point — a real, git-adjacent action deserving of a little friction.
fn draw_checkpoint_card(
    ui: &mut egui::Ui,
    item_idx: usize,
    preview: &str,
    message_count: usize,
    confirming: bool,
    preview_loading: bool,
    preview_result: &Option<String>,
    pad_l: f32,
    pad_r: f32,
    edit: &mut Option<RewindEdit>,
) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(28, 28, 26))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
            .inner_margin(egui::Margin::symmetric(8.0, 5.0))
            .rounding(4.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    // Below this the preview and the controls cannot share a
                    // row without one of them losing: reserving space by
                    // measuring buttons is never exact enough, and being a few
                    // points short puts the text underneath them. Stacking is
                    // the honest answer — the controls keep their full size and
                    // move to their own line.
                    let narrow = ui.available_width() < 300.0;
                    ui.horizontal(|ui| {
                        paint_dot(ui, egui::Color32::from_gray(140));
                        ui.label(egui::RichText::new("checkpoint").size(10.5).color(egui::Color32::from_gray(160)));
                        // The controls are the only route to a rewind, so their
                        // width is reserved and the preview takes what is left.
                        // Added ahead of them it claimed the whole row and pushed
                        // them off the edge of a narrow panel, unreachable.
                        let preview_short: String = preview.chars().take(60).collect();
                        ui.add_sized(
                            egui::vec2(ui.available_width(), 16.0),
                            egui::Label::new(egui::RichText::new(preview_short).size(10.5)
                                .color(egui::Color32::from_gray(190))).truncate(),
                        );
                        if narrow { return; }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !confirming {
                                if ui.small_button("Rewind here").clicked() {
                                    *edit = Some(RewindEdit::Arm { item_idx });
                                }
                                if !preview_loading && preview_result.is_none() {
                                    if ui.small_button("Preview").clicked() {
                                        *edit = Some(RewindEdit::Preview { item_idx });
                                    }
                                }
                            }
                            ui.label(egui::RichText::new(format!("{message_count} msgs")).size(9.5)
                                .color(egui::Color32::from_gray(120)));
                        });
                    });
                    if narrow {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{message_count} msgs")).size(9.5)
                                .color(egui::Color32::from_gray(120)));
                            if !confirming {
                                if !preview_loading && preview_result.is_none() {
                                    if ui.small_button("Preview").clicked() {
                                        *edit = Some(RewindEdit::Preview { item_idx });
                                    }
                                }
                                if ui.small_button("Rewind here").clicked() {
                                    *edit = Some(RewindEdit::Arm { item_idx });
                                }
                            }
                        });
                    }
                    if preview_loading {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new("Loading preview…")
                            .size(10.0).color(egui::Color32::from_gray(140)));
                    }
                    if let Some(result) = preview_result {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(soft_wrap(result, wrap_run(ui, 10.0, false)))
                            .size(10.0).color(egui::Color32::from_gray(190)));
                    }
                    if confirming {
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(
                            "Restore files and conversation to this point? Messages after it \
                             will still show here, but no longer apply.")
                            .size(10.0).color(egui::Color32::from_rgb(230, 170, 110)));
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            if ui.button("Confirm Rewind").clicked() {
                                *edit = Some(RewindEdit::Confirm { item_idx });
                            }
                            if ui.button("Cancel").clicked() {
                                *edit = Some(RewindEdit::Cancel { item_idx });
                            }
                        });
                    }
                });
            });
    });
}

enum ProviderBusyEdit {
    SwitchPriority { item_idx: usize },
    Dismiss { item_idx: usize },
}

impl ProviderBusyEdit {
    fn item_idx(&self) -> usize {
        match self {
            ProviderBusyEdit::SwitchPriority { item_idx }
            | ProviderBusyEdit::Dismiss { item_idx } => *item_idx,
        }
    }
}

/// Renders a provider-at-capacity error. Same visual weight as a plain
/// `ChatItem::Error` (this *is* one, effectively — the request failed and
/// nothing happened), but offers switching the affected endpoint to its
/// paid priority tier right from the card when one exists, instead of
/// making the user go find the toggle in the reasoning picker after the
/// fact. `endpoints` is `AgentSession::endpoints` — looked up fresh each
/// frame rather than cached on the item, so the offered action (and whether
/// it's offered at all) always reflects the endpoint's *current* state.
fn draw_provider_busy_card(
    ui: &mut egui::Ui,
    item_idx: usize,
    message: &str,
    endpoint_name: &str,
    resolved: bool,
    resolution: &str,
    endpoints: &[serde_json::Value],
    pad_l: f32,
    pad_r: f32,
    edit: &mut Option<ProviderBusyEdit>,
) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width() - pad_r);
            ui.label(egui::RichText::new(soft_wrap(&format!("  {message}"), wrap_run(ui, 12.5, false)))
                .color(egui::Color32::from_rgb(255, 110, 100)));
            if resolved {
                ui.label(egui::RichText::new(resolution).size(10.0).color(egui::Color32::from_gray(130)));
                return;
            }
            let ep = endpoints.iter()
                .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(endpoint_name));
            let is_xai = ep.map_or(false, |e| classify_provider(e) == "xAI");
            let already_priority = ep
                .and_then(|e| e.get("xai_priority_tier"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                if is_xai && !already_priority {
                    if ui.small_button("Switch to Priority Tier (2x cost)").clicked() {
                        *edit = Some(ProviderBusyEdit::SwitchPriority { item_idx });
                    }
                }
                if ui.small_button("Dismiss").clicked() {
                    *edit = Some(ProviderBusyEdit::Dismiss { item_idx });
                }
            });
        });
    });
}

/// One entry in the docked subagent strip — always an *active* subagent
/// (finished ones drop out of the strip entirely; see the call site).
/// `path` is this subagent's own address from the top of `tab.session.items`
/// (e.g. `[2]` for a top-level one, `[2, 0]` for one it nested itself) —
/// threaded through so a click deep inside a recursively-nested subagent's
/// panel can still be applied back at the right spot.
fn draw_subagent_strip_entry(
    ui: &mut egui::Ui,
    path: &[usize],
    agent_type: &str,
    prompt: &str,
    expanded: bool,
    items: &[ChatItem],
    pending_action: &mut Option<(Vec<usize>, bool)>,
    toggle_expand: &mut Option<Vec<usize>>,
) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(28, 26, 40))
        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(70, 60, 100)))
        .inner_margin(egui::Margin::symmetric(8.0, 5.0))
        .rounding(5.0)
        .show(ui, |ui| {
            ui.vertical(|ui| {
                let header = ui.horizontal(|ui| {
                    paint_disclosure_triangle(ui, expanded, egui::Color32::from_gray(150));
                    paint_dot(ui, egui::Color32::from_rgb(150, 170, 255));
                    ui.label(egui::RichText::new(agent_type).monospace().size(11.5).strong()
                        .color(egui::Color32::from_rgb(190, 180, 240)));
                    // Same reservation as the checkpoint row: an approval count
                    // must not be pushed off by the prompt it belongs to.
                    let pending_now = items.iter()
                        .filter(|i| matches!(i, ChatItem::ToolRequest { approval: ApprovalState::Pending, .. }))
                        .count();
                    let reserved = if pending_now > 0 {
                        text_width(ui, &format!("{pending_now} awaiting approval"), 10.0, false)
                            + ui.spacing().item_spacing.x * 2.0
                    } else {
                        0.0
                    };
                    let preview: String = prompt.chars().take(50).collect();
                    ui.add_sized(
                        egui::vec2((ui.available_width() - reserved).max(16.0), 16.0),
                        egui::Label::new(egui::RichText::new(preview).size(10.5)
                            .color(egui::Color32::from_gray(150))).truncate(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pending_count = items.iter()
                            .filter(|i| matches!(i, ChatItem::ToolRequest { approval: ApprovalState::Pending, .. }))
                            .count();
                        if pending_count > 0 {
                            ui.label(egui::RichText::new(format!("{pending_count} awaiting approval")).size(10.0)
                                .color(egui::Color32::from_rgb(224, 158, 90)));
                        }
                    });
                });
                if header.response.interact(egui::Sense::click()).clicked() {
                    *toggle_expand = Some(path.to_vec());
                }

                if expanded {
                    ui.add_space(3.0);
                    egui::ScrollArea::vertical().id_salt(("subagent_items", path.to_vec())).max_height(130.0)
                        .show(ui, |ui| {
                            if items.is_empty() {
                                ui.label(egui::RichText::new("No tool calls yet").italics().size(10.5)
                                    .color(egui::Color32::from_gray(120)));
                                return;
                            }
                            let mut i = 0;
                            while i < items.len() {
                                match &items[i] {
                                    ChatItem::ToolRequest { .. } => {
                                        let mut local_pending: Option<(usize, bool)> = None;
                                        let mut local_toggle: Option<usize> = None;
                                        i = draw_tool_run(ui, items, i, 0.0, 0.0, &mut local_pending, &mut local_toggle);
                                        if let Some((li, approve)) = local_pending {
                                            let mut p = path.to_vec(); p.push(li);
                                            *pending_action = Some((p, approve));
                                        }
                                        // A nested tool card's own expand/collapse
                                        // (full args/diff) isn't wired through the
                                        // strip yet — Approve/Deny and the diff
                                        // popup both still work regardless.
                                    }
                                    // A subagent this subagent itself nested —
                                    // it's genuinely paused waiting on this, so
                                    // render it as its own recursive panel
                                    // rather than a flat sibling.
                                    ChatItem::Subagent { agent_type: cat, prompt: cprompt, finished: false, expanded: cexp, items: nested, .. } => {
                                        let mut child_path = path.to_vec();
                                        child_path.push(i);
                                        draw_subagent_strip_entry(
                                            ui, &child_path, cat, cprompt, *cexp, nested,
                                            pending_action, toggle_expand,
                                        );
                                        i += 1;
                                    }
                                    // Finished nested subagents have no separate
                                    // main-chat conversation to surface their
                                    // summary into (unlike a top-level one), so
                                    // they get a compact "done" line here instead
                                    // of just vanishing.
                                    ChatItem::Subagent { agent_type: cat, summary, finished: true, .. } => {
                                        ui.horizontal(|ui| {
                                            paint_checkmark(ui, egui::Color32::from_rgb(150, 170, 255));
                                            ui.label(egui::RichText::new(cat.as_str()).monospace().size(10.5)
                                                .color(egui::Color32::from_rgb(190, 180, 240)));
                                            let s: String = summary.trim().chars().take(60).collect();
                                            ui.label(egui::RichText::new(s).size(10.0).color(egui::Color32::from_gray(150)));
                                        });
                                        i += 1;
                                    }
                                    _ => { i += 1; }
                                }
                            }
                        });
                }
            });
        });
    ui.add_space(4.0);
}

// ── Find bar ──────────────────────────────────────────────────────────────────

struct FindBar {
    query:         String,
    replace:       String,
    show_replace:  bool,
    matches:       Vec<(usize, usize)>, // byte offsets in flat text
    current:       usize,
    case_sensitive: bool,
    dirty:         bool,
    request_focus: bool,
    scroll_to_cur: bool,
}

impl FindBar {
    fn new(show_replace: bool) -> Self {
        Self {
            query: String::new(), replace: String::new(),
            show_replace, matches: Vec::new(), current: 0,
            case_sensitive: false, dirty: true,
            request_focus: true, scroll_to_cur: false,
        }
    }

    fn recompute(&mut self, text: &str) {
        self.dirty = false;
        self.matches.clear();
        if self.query.is_empty() { return; }
        let (hay, needle) = if self.case_sensitive {
            (text.to_string(), self.query.clone())
        } else {
            (text.to_lowercase(), self.query.to_lowercase())
        };
        let qlen = needle.len();
        let mut pos = 0usize;
        while pos + qlen <= hay.len() {
            if hay[pos..].starts_with(needle.as_str()) {
                self.matches.push((pos, pos + qlen));
                pos += 1;
            } else {
                pos += hay[pos..].chars().next().map_or(1, |c| c.len_utf8());
            }
        }
        if !self.matches.is_empty() {
            self.current = self.current.min(self.matches.len() - 1);
        } else {
            self.current = 0;
        }
    }

    fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
            self.scroll_to_cur = true;
        }
    }

    fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = self.current.checked_sub(1).unwrap_or(self.matches.len() - 1);
            self.scroll_to_cur = true;
        }
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct IdeApp {
    file_tree:       FileTree,
    buffers:         Vec<Buffer>,
    active:          usize,
    terminal_tabs:   Vec<TerminalTab>,
    terminal_active: usize,
    show_tree:       bool,
    show_term:       bool,
    bottom_tab:      BottomTab,
    output_log:      Vec<(String, OutputLevel)>,  // (message, level)
    status:          String,
    /// The directory terminals, the agent, DAP, LSP and tasks are rooted in.
    /// Always a real path, even with no workspace open (`$HOME` in that case),
    /// because those subsystems all need *somewhere* to run.
    cwd:             PathBuf,
    /// Whether a workspace folder is actually open.
    ///
    /// A window created with no folder — "New Window", including from the Dock
    /// menu — starts `false`: the Explorer shows an empty state rather than
    /// rendering `cwd`, and nothing scans it. This matches VS Code, where a new
    /// window has no folder but its terminal still opens somewhere sensible.
    /// Flipped by "Open Folder", which is the only thing that establishes a
    /// workspace. Guard anything workspace-shaped (tree, git, watcher, Quick
    /// Open, search, session, tasks) on this rather than on `cwd`.
    has_folder:      bool,
    /// What the last "Add to Dock" attempt did, shown under the button.
    dock_status: Option<String>,
    /// Identifies this window's stored session; see `session::load_for_window`.
    pub window_id:   u64,
    file_watcher:    Option<crate::filewatch::FileWatcher>,
    terminal_height: f32,
    pub settings:    crate::settings::Settings,
    settings_open:   bool,
    /// One-time "want update checks?" prompt — shown once when
    /// `settings.update_check_prompted` is still false.
    show_update_prompt: bool,
    update_check_rx: Option<std::sync::mpsc::Receiver<Option<crate::update_check::UpdateAvailable>>>,
    update_available: Option<crate::update_check::UpdateAvailable>,
    update_banner_dismissed: bool,
    /// AI-provider setup wizard — `Some` while open, regardless of which
    /// step it's on. `Some(ProviderPicker)` at startup when forge-agent has
    /// no real endpoint configured yet and the user hasn't dismissed it
    /// before; otherwise opened manually (e.g. from Settings).
    onboarding: Option<crate::onboarding::OnboardingStep>,
    palette:         crate::theme::Palette,
    theme_picker:    Option<usize>,   // Some(selected idx) while picker is open
    /// Theme active before the picker opened (restored on Escape).
    theme_prev:      Option<String>,
    ctrl_k_chord:    bool,            // true after Ctrl+K, waiting for second key
    goto_line:       Option<String>,  // Some(input) while Ctrl+G overlay is open
    // Task runner (.forge/tasks.toml)
    task_picker:     Option<usize>,   // Some(selected idx) while picker is open
    task_rx:         Option<mpsc::Receiver<(String, OutputLevel)>>,
    /// Split editor: buffer index shown in the right pane (None = no split).
    split:           Option<usize>,
    /// Multi-cursor: extra cursor positions (char indices) in the active buffer.
    extra_cursors:   Vec<usize>,
    /// Word length highlighted at each extra cursor (from Ctrl+D).
    mc_sel_len:      usize,
    /// TextEdit widget id of the main editor (from the previous frame).
    editor_te_id:    Option<egui::Id>,
    /// Word wrap: y offset of each buffer line's first visual row (galley-relative,
    /// from the previous frame). Empty when wrap is off (rows are uniform).
    wrap_line_ys:    Vec<f32>,
    /// Loaded plugins (~/.config/forge-ide/plugins/*.so).
    plugins:         crate::plugin::PluginHost,
    // Debugging (DAP)
    dap:             Option<crate::dap::DapClient>,
    breakpoints:     std::collections::HashMap<PathBuf, std::collections::BTreeSet<usize>>,
    dap_stack:       Vec<crate::dap::StackFrame>,
    dap_vars:        Vec<crate::dap::Variable>,
    /// File + 0-based line where the debuggee is currently stopped.
    dap_stopped:     Option<(PathBuf, usize)>,
    dap_running:     bool,
    fonts_loaded:    bool,
    // Overlays
    quick_open:      Option<QuickOpen>,
    cmd_palette:     Option<CmdPalette>,
    find_bar:        Option<FindBar>,
    // Sidebar + search
    sidebar_view:    SidebarView,
    search:          SearchState,
    pending_scroll:  Option<usize>, // scroll editor to this line on next draw
    // Right-side Forge agent panel
    agent_visible:   bool,
    agent_tabs:      Vec<AgentTab>,
    agent_active:    usize,
    agent_show_list: bool,
    agent_saved:     Vec<crate::agent_panel::SavedConversation>,
    agent_model_picker_open: bool,
    agent_model_picker_frame: u8,
    agent_pending_switch:    Option<serde_json::Value>,
    agent_perm_picker_open:  bool,
    agent_perm_picker_frame: u8,
    /// A permission-mode change awaiting confirmation because applying it
    /// requires respawning the tab's subprocess (any transition into or out
    /// of `DangerouslySkipAll`) — `AlwaysAsk` <-> `AutoApprove` applies
    /// immediately with no confirmation needed.
    agent_pending_perm_mode: Option<crate::settings::AgentPermissionMode>,
    agent_thinking_picker_open:  bool,
    agent_thinking_picker_frame: u8,
    agent_context_picker_open:  bool,
    agent_context_picker_frame: u8,
    forge_icon:      Option<egui::TextureHandle>,
    /// 0.0 (cold) to 1.0 (white-hot) — climbs while any Forge Agent tab has a
    /// turn in flight, cools back down when idle. Drives the welcome-screen
    /// anvil watermark's tint, like a real forge fire. This is the *displayed*
    /// value — see `anvil_heat_target` for why it's kept separate.
    anvil_heat:      f32,
    /// Where `anvil_heat` is chasing. Each tool-call pulse jumps this
    /// instantly (a real hammer strike is sudden), but `anvil_heat` itself
    /// eases toward it every frame instead of snapping — jumping the
    /// *displayed* value straight to the target on every pulse read as a
    /// stiff, discrete color change rather than a fire actually climbing.
    anvil_heat_target: f32,
    /// Memoized syntax-highlight `LayoutJob` for the active buffer's editor.
    syntax_cache:    Option<SyntaxCache>,
    /// Same, for the split-view secondary editor (kept separate so the two
    /// panes don't thrash each other's cache when showing different buffers).
    syntax_cache_split: Option<SyntaxCache>,
    // Git
    git:             Option<crate::git::GitState>,
    commit_msg:      String,
    git_error:       Option<String>,
    // SSH remote
    ssh:             Option<crate::ssh::SshConnection>,
    ssh_hosts:       Vec<crate::ssh::SshHost>,
    ssh_form:        crate::ssh::SshHost,   // new-connection form state
    ssh_password:    String,
    ssh_error:       Option<String>,
    ssh_connecting:  bool,
    /// Remote directory tree: stack of (path, entries) — last = current dir.
    ssh_tree:        Vec<(String, Vec<crate::ssh::RemoteEntry>)>,
    /// Shell I/O for the remote PTY.
    ssh_shell:       Option<crate::ssh::ShellChannel>,
    /// Dedicated terminal grid for the remote shell (separate from local PTY).
    ssh_term:        std::sync::Arc<std::sync::Mutex<crate::terminal::Grid>>,
    ssh_term_focused: bool,
    /// Cached *viewport* Galley for `draw_ssh_terminal`, keyed by
    /// `Grid::version` — see the equivalent field on `Terminal` for why this
    /// matters (skips rebuilding + re-shaping on every frame).
    ssh_term_cached_galley: Option<(u64, egui::FontId, std::sync::Arc<egui::Galley>)>,
    /// Cached *scrollback* Galley, keyed by `Grid::scrollback_version` — see
    /// `Terminal`'s equivalent field for why this is cached separately from
    /// the viewport instead of as one combined job.
    ssh_term_cached_scrollback_galley: Option<(u64, egui::FontId, std::sync::Arc<egui::Galley>)>,
    /// When the SSH remote terminal's grid last received new bytes — used
    /// to decide whether it's worth a fast repaint cadence right now (see
    /// `Terminal::recently_active` for the equivalent on local terminals).
    ssh_term_last_output: Option<std::time::Instant>,
    /// When the user last actually interacted (moved the mouse, scrolled,
    /// clicked, typed) — used to render at full display refresh rate for a
    /// short window after real input, rather than the slower background-
    /// polling cadence below. See the "Repaint scheduling" block in `draw`.
    last_interaction_at: Option<std::time::Instant>,
    /// In-flight SSH connect result channel (carries fully-initialized ready bundle).
    ssh_connect_rx:  Option<mpsc::Receiver<Result<crate::ssh::SshReady, String>>>,
    ssh_pty_rx:      Option<mpsc::Receiver<Result<crate::ssh::ShellChannel, String>>>,
    ssh_log_rx:      Option<mpsc::Receiver<(String, OutputLevel)>>,
    /// In-flight directory listing (drilling into subdirs from the file tree).
    ssh_nav_rx:      Option<mpsc::Receiver<Result<(String, Vec<crate::ssh::RemoteEntry>), String>>>,
    /// In-flight remote file read.
    ssh_open_rx:     Option<mpsc::Receiver<Result<(String, String), String>>>, // (path, text)
    /// In-flight remote uploads (dropped files); one result per file.
    ssh_upload_rx:   Option<mpsc::Receiver<Result<String, String>>>,
    /// Directory those uploads are landing in, so the view can be refreshed
    /// once they finish.
    ssh_upload_dir:  Option<String>,
    /// Whether the SSH quick-pick overlay is open.
    ssh_overlay:       bool,
    ssh_overlay_query: String,
    ssh_overlay_frame: u8,
    /// Which step of the two-step connect flow we're on.
    ssh_overlay_step:  SshOverlayStep,
    /// "add" = showing the "enter user@host" sub-prompt.
    ssh_add_input:   Option<String>,

    // Diff gutter cache for the active buffer (recomputed when dirty / on switch)
    gutter_marks:    std::collections::HashMap<usize, crate::git::GutterMark>,
    gutter_path:     Option<PathBuf>,
    gutter_dirty:    bool,
    /// Per-line blame for the active buffer (for hover tooltips).
    blame_lines:     Vec<crate::git::BlameLine>,
    blame_path:      Option<PathBuf>,
    /// In-flight background blame; see `crate::git::spawn_blame`.
    blame_rx:        Option<mpsc::Receiver<(PathBuf, Vec<crate::git::BlameLine>)>>,
    /// In-flight external formatter run.
    fmt_rx:          Option<mpsc::Receiver<PendingFormat>>,
    /// When a coalesced file-tree refresh is due; see the file-watch drain.
    tree_refresh_due: Option<std::time::Instant>,
    /// In-flight fetch/pull/push result channel (None when idle).
    git_task:        Option<mpsc::Receiver<Result<String, String>>>,
    /// Language server (rust-analyzer) + latest diagnostics per file.
    lsp:             Option<crate::lsp::LspClient>,
    diagnostics:     std::collections::HashMap<PathBuf, Vec<crate::lsp::Diagnostic>>,
    // In-flight LSP request ids (0 = none pending)
    hover_req:       i64,
    hover_text:      Option<String>,    // last resolved hover
    hover_pos:       Option<egui::Pos2>,
    comp_req:        i64,
    comp_items:      Vec<crate::lsp::CompletionItem>,
    comp_cursor:     usize,             // selected item index
    goto_req:        i64,
    refs_req:        i64,
    refs_results:    Vec<crate::lsp::Location>,
    refs_visible:    bool,
    rename_req:      i64,
    rename_input:    Option<String>,  // Some while prompting, None otherwise
    action_req:      i64,
    action_items:    Vec<crate::lsp::CodeAction>,
    sig_req:         i64,
    sig_help:        Option<crate::lsp::SignatureHelp>,
    fmt_req:         i64,
    outline_req:     i64,
    outline:         Vec<crate::lsp::DocSymbol>,
    outline_path:    Option<PathBuf>,
    /// Text-field state for the "add origin remote" row.
    remote_url_input: String,
    /// Whether `gh` is installed+authed (None = not yet checked).
    gh_ready:        Option<bool>,
    gh_check:        Option<mpsc::Receiver<bool>>,
    /// GitHub owner/org to publish under (blank = active github.com account).
    publish_owner:   String,
    // Open-folder channel (non-blocking native dialog)
    folder_rx:       Option<mpsc::Receiver<Option<PathBuf>>>,
    /// If true, the pending folder dialog adds a workspace root instead of
    /// replacing the current one.
    folder_add_root: bool,
    /// Set by the app when the user requests a new window.
    /// Consumed by the winit event loop (main.rs) to create the window.
    pub pending_new_window:  Option<NewWindowSpec>,
    /// Set by `reload_window` once state is saved; consumed by the winit
    /// event loop (main.rs), which explicitly tears down *every* window's
    /// Vulkan/Metal/window-server state *before* exec'ing the fresh
    /// process — `exec` replaces the process image without running Rust
    /// destructors, so doing the actual exec from here (instead of
    /// synchronously inside `draw()`, mid-frame) is what makes that
    /// teardown happen at all instead of leaving the old GPU/window state
    /// orphaned for the OS to notice and reclaim on its own time. `exec`
    /// replaces the *whole* process regardless of which window asked, so
    /// main.rs re-launches with every currently open window's own folder —
    /// not just this one's — which is why this only needs to be a flag: the
    /// full cwd list is gathered from `Ide::windows` at the point of exec.
    pub pending_reload: bool,
    /// SSH host to connect on the first draw (set by new_with_spec).
    pending_ssh_connect: Option<crate::ssh::SshHost>,
}

impl IdeApp {
    /// The workspace folder, or `None` for a window with no folder open.
    ///
    /// Deliberately not `cwd`, which is always a real path — a folderless window
    /// roots its terminal at `$HOME`, and recording that would reopen a
    /// workspace the user never chose.
    pub fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.has_folder.then(|| self.cwd.clone())
    }

    pub fn new_with_spec(spec: NewWindowSpec) -> Self {
        let is_reload = spec.is_reload;
        // `None` means "no workspace", not "inherit the process working
        // directory". That old behavior meant a window opened from the Dock —
        // where the process cwd is `/` — rendered the entire filesystem as the
        // workspace root.
        // A record written before windows had identity carries no id; give it one
        // so its state is kept from here on.
        let window_id = if spec.window_id != 0 { spec.window_id } else { crate::session::new_window_id() };
        let has_folder = spec.cwd.is_some();
        let cwd = spec.cwd.unwrap_or_else(|| {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        });
        // Resolves a relative arg (`forge-ide .`) or symlink to a stable
        // absolute path — best-effort, since this is now also used as a
        // per-workspace identity key (conversation history scoping; see
        // `agent_panel::SavedConversation::workspace`), where two different
        // spellings of the same folder must compare equal. Falls back to
        // the raw path if canonicalization fails (doesn't exist yet, no
        // permission, etc.) rather than erroring out.
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        // With no workspace open there is nothing to walk, watch or diff. The
        // tree is still constructed (rooted at `cwd`) so "Open Folder" has an
        // object to retarget, but `has_folder` keeps it from being drawn.
        let tree = FileTree::new(cwd.clone());
        let git  = if has_folder { crate::git::GitState::open(&cwd) } else { None };
        let file_watcher = if has_folder {
            crate::filewatch::FileWatcher::new(&cwd)
        } else {
            None
        };
        let pending_ssh = spec.ssh_host;
        let agent_saved = crate::agent_panel::load_conversations(&cwd);
        let mut app = Self {
            dock_status: None,
            window_id,
            file_tree:       tree,
            buffers:         vec![],   // no untitled tab on startup
            active:          0,
            terminal_tabs:   vec![TerminalTab::new(&cwd)],
            terminal_active: 0,
            show_tree:       true,
            show_term:       true,
            bottom_tab:      BottomTab::Terminal,
            output_log:      Vec::new(),
            status:          String::new(),
            cwd,
            has_folder,
            file_watcher,
            terminal_height: 260.0,
            settings:        crate::settings::load(),
            settings_open:   false,
            palette:         crate::theme::Palette::default(),
            theme_picker:    None,
            theme_prev:      None,
            ctrl_k_chord:    false,
            goto_line:       None,
            task_picker:     None,
            task_rx:         None,
            split:           None,
            extra_cursors:   Vec::new(),
            mc_sel_len:      0,
            editor_te_id:    None,
            wrap_line_ys:    Vec::new(),
            plugins:         crate::plugin::PluginHost::load(),
            dap:             None,
            breakpoints:     std::collections::HashMap::new(),
            dap_stack:       Vec::new(),
            dap_vars:        Vec::new(),
            dap_stopped:     None,
            dap_running:     false,
            fonts_loaded:    false,
            quick_open:      None,
            cmd_palette:     None,
            find_bar:        None,
            sidebar_view:    SidebarView::Explorer,
            search:          SearchState::new(),
            pending_scroll:  None,
            agent_visible:   false,
            agent_tabs:      vec![],
            agent_active:    0,
            agent_show_list: false,
            agent_saved,
            agent_model_picker_open: false,
            agent_model_picker_frame: 0,
            agent_pending_switch:    None,
            agent_perm_picker_open:  false,
            agent_perm_picker_frame: 0,
            agent_pending_perm_mode: None,
            agent_thinking_picker_open:  false,
            agent_thinking_picker_frame: 0,
            agent_context_picker_open:  false,
            agent_context_picker_frame: 0,
            forge_icon:      None,
            anvil_heat:      0.0,
            anvil_heat_target: 0.0,
            syntax_cache:    None,
            syntax_cache_split: None,
            git,
            commit_msg:      String::new(),
            git_error:       None,
            gutter_marks:    std::collections::HashMap::new(),
            gutter_path:     None,
            gutter_dirty:    true,
            blame_lines:     Vec::new(),
            blame_path:      None,
            blame_rx:        None,
            fmt_rx:          None,
            tree_refresh_due: None,
            git_task:        None,
            ssh:             None,
            ssh_hosts:       crate::ssh::load_hosts(),
            ssh_form:        crate::ssh::SshHost::default(),
            ssh_password:    String::new(),
            ssh_error:       None,
            ssh_connecting:  false,
            ssh_tree:        Vec::new(),
            ssh_shell:        None,
            ssh_term:         std::sync::Arc::new(std::sync::Mutex::new(
                                   crate::terminal::Grid::with_size(50, 220))),
            ssh_term_focused: false,
            ssh_term_cached_galley: None,
            ssh_term_cached_scrollback_galley: None,
            ssh_term_last_output: None,
            last_interaction_at: None,
            ssh_connect_rx:   None,
            ssh_pty_rx:       None,
            ssh_log_rx:       None,
            ssh_nav_rx:      None,
            ssh_open_rx:     None,
            ssh_upload_rx:   None,
            ssh_upload_dir:  None,
            ssh_overlay:       false,
            ssh_overlay_query: String::new(),
            ssh_overlay_frame: 0,
            ssh_overlay_step:  SshOverlayStep::ChooseWindow,
            ssh_add_input:     None,
            lsp:             None,
            diagnostics:     std::collections::HashMap::new(),
            hover_req:       0,
            hover_text:      None,
            hover_pos:       None,
            comp_req:        0,
            comp_items:      Vec::new(),
            comp_cursor:     0,
            goto_req:        0,
            refs_req:        0,
            refs_results:    Vec::new(),
            refs_visible:    false,
            rename_req:      0,
            rename_input:    None,
            action_req:      0,
            action_items:    Vec::new(),
            sig_req:         0,
            sig_help:        None,
            fmt_req:         0,
            outline_req:     0,
            outline:         Vec::new(),
            outline_path:    None,
            remote_url_input: String::new(),
            gh_ready:        None,
            gh_check:        None,
            publish_owner:   String::new(),
            folder_rx:           None,
            folder_add_root:     false,
            pending_new_window:  None,
            pending_reload:      false,
            pending_ssh_connect: pending_ssh,
            show_update_prompt: false,
            update_check_rx: None,
            update_available: None,
            update_banner_dismissed: false,
            onboarding: None,
        };

        app.show_update_prompt = !app.settings.update_check_prompted;
        if app.settings.check_for_updates {
            app.update_check_rx = Some(crate::update_check::spawn_check());
        }
        if !app.settings.onboarding_skipped && crate::onboarding::needs_setup() {
            app.onboarding = Some(crate::onboarding::OnboardingStep::ProviderPicker);
        }

        // Restore the previous session for this workspace: either the
        // `restore_session` setting is on (opt-in across a real quit and
        // relaunch), or this process was just launched by Reload Window,
        // which always continues the same session it just saved —
        // matching VS Code's Reload Window regardless of that setting.
        // Either way, skip it if we're opening a remote (SSH) workspace
        // instead.
        if (app.settings.restore_session || is_reload)
            && app.pending_ssh_connect.is_none()
        {
            let root = app.workspace_root();
            if let Some(state) = crate::session::load_for_window(app.window_id, root.as_deref()) {
                let restored: Vec<Buffer> = state.open_files.iter()
                    .filter_map(|p| Buffer::from_file(p.clone()).ok())
                    .collect();
                if !restored.is_empty() {
                    app.active  = state.active_file.min(restored.len() - 1);
                    app.buffers = restored;
                }
                if !state.terminals.is_empty() {
                    // Reattach to sessions the pty-host daemon still has
                    // alive (it survives Forge IDE's own process
                    // restarting) rather than assuming they're gone —
                    // only open a fresh shell for ones that really are.
                    let client = crate::ptyhost::shared();
                    let live: Vec<forge_proto::PtyInfo> = client.as_ref()
                        .and_then(|c| c.pty_list().ok())
                        .unwrap_or_default();
                    app.terminal_tabs = state.terminals.iter()
                        .map(|t| {
                            let existing = t.pty_id.and_then(|id| live.iter().find(|s| s.id == id));
                            match (existing, &client) {
                                (Some(info), Some(client)) =>
                                    TerminalTab::reattach(client.clone(), info, t.viewport.clone()),
                                _ => TerminalTab::new(&t.cwd),
                            }
                        })
                        .collect();
                    app.terminal_active = 0;
                }
                // Whether the panel itself was open is independent of
                // whether any tab had messages worth restoring (e.g. an
                // untouched "New Chat" tab has nothing to save).
                app.agent_visible = state.agent_visible;
                if !state.agent_tabs.is_empty() {
                    // Reopen each saved conversation exactly like manually
                    // reopening it from the history list does — resumes
                    // forge-agent's own process state (see `AgentTab::reopen`)
                    // when the save has a session id, not just a UI replay.
                    let saved = crate::agent_panel::load_conversations(&app.cwd);
                    app.agent_tabs = state.agent_tabs.iter()
                        .filter_map(|id| saved.iter().find(|c| &c.id == id))
                        .map(|conv| AgentTab::reopen(&app.cwd, app.settings.default_agent_permission_mode, conv))
                        .collect();
                    if !app.agent_tabs.is_empty() {
                        app.agent_active = state.agent_active.min(app.agent_tabs.len() - 1);
                    }
                }
            }
        }
        app
    }

    /// Build a snapshot of open files/terminal directories for this
    /// workspace. Shared by `save_session` (gated by the opt-in setting)
    /// and `reload_window` (always saves — see its doc comment).
    fn build_session_state(&self) -> crate::session::SessionState {
        // Diff/image tabs are derived/transient views, not real editable
        // files — skip them, and remap `active` onto the filtered list
        // (it was an index into the *unfiltered* buffer list).
        let restorable: Vec<(usize, PathBuf)> = self.buffers.iter().enumerate()
            .filter(|(_, b)| b.diff.is_none() && b.image_bytes.is_none())
            .filter_map(|(i, b)| b.path.clone().map(|p| (i, p)))
            .collect();
        let active_file = restorable.iter().position(|&(i, _)| i == self.active).unwrap_or(0);

        // Agent tabs with no messages yet have nothing worth restoring —
        // skip them, and remap `agent_active` onto the filtered list, same
        // as `active_file` above. Re-save explicitly rather than trusting
        // the per-frame auto-save to already be current, since this can run
        // right after a message lands and before the agent view's own next
        // frame would have re-saved it.
        let restorable_agents: Vec<(usize, &str)> = self.agent_tabs.iter().enumerate()
            .filter(|(_, t)| !t.session.items.is_empty())
            .map(|(i, t)| {
                crate::agent_panel::save_conversation(&t.session, &t.conv_id, &self.cwd);
                (i, t.conv_id.as_str())
            })
            .collect();
        let agent_active = restorable_agents.iter()
            .position(|&(i, _)| i == self.agent_active).unwrap_or(0);

        crate::session::SessionState {
            open_files: restorable.into_iter().map(|(_, p)| p).collect(),
            active_file,
            terminals: self.terminal_tabs.iter()
                .map(|t| crate::session::TerminalState {
                    pty_id:    t.terminal.pty_id(),
                    cwd:       t.terminal.cwd().to_path_buf(),
                    viewport:  t.terminal.snapshot_viewport(),
                })
                .collect(),
            agent_visible: self.agent_visible,
            agent_tabs:    restorable_agents.into_iter().map(|(_, id)| id.to_string()).collect(),
            agent_active,
        }
    }

    /// Snapshot open files/terminal directories for this workspace so the
    /// next launch can restore them (if the user has that setting on).
    /// Called on window close — session state isn't worth persisting on
    /// every keystroke/tab switch the way settings changes are.
    pub fn save_session(&self) {
        if !self.settings.restore_session || self.pending_ssh_connect.is_some() {
            return;
        }
        let state = self.build_session_state();
        crate::session::save_for_window(self.window_id, self.workspace_root().as_deref(), &state);
    }

    /// Restart in-place — same PID, no full quit/relaunch through the OS.
    /// `exec` replaces the running process image with a fresh invocation
    /// of the on-disk binary, so a rebuild since this process started
    /// takes effect immediately. The workspace path is passed explicitly
    /// (rather than relying on the OS-level cwd, which `open_folder`
    /// changing `self.cwd` doesn't necessarily keep in sync with) so the
    /// fresh process reopens the same folder.
    ///
    /// Reload always saves and restores state, regardless of the
    /// `restore_session` setting: this is the same session continuing,
    /// not a fresh start, matching VS Code's Reload Window. The opt-in
    /// setting only governs restoring across a genuine quit-and-relaunch
    /// later — so the state is saved unconditionally here, and the `
    /// --reload` flag tells the freshly exec'd process to restore it
    /// unconditionally too.
    ///
    /// The actual `exec` doesn't happen here — it's deferred to the winit
    /// event loop via `pending_reload` (main.rs), which owns this window's
    /// `GfxContext`/`EguiPass`/`Window` and can drop them explicitly first.
    /// `exec` replaces the process image without running destructors, so
    /// calling it synchronously from here (mid-`draw()`, with the GPU/window
    /// state still very much alive further up the call stack) would leave
    /// all of it orphaned instead of cleanly torn down.
    pub fn reload_window(&mut self) {
        self.save_session_for_reload();
        self.pending_reload = true;
    }

    /// Unconditional version of `save_session` — ignores the
    /// `restore_session` setting, since a reload is the same session
    /// continuing, not a fresh start. `exec`/`spawn` replaces the *whole*
    /// process regardless of which window's `reload_window()` was actually
    /// called, so main.rs calls this on every open window before tearing
    /// any of them down — otherwise only the one window that triggered the
    /// reload ever got its conversation/open-files state saved, and every
    /// other open window came back empty on the other end.
    pub fn save_session_for_reload(&self) {
        if self.pending_ssh_connect.is_some() {
            return;
        }
        let state = self.build_session_state();
        crate::session::save_for_window(self.window_id, self.workspace_root().as_deref(), &state);
    }


    fn draw_terminal_panels(&mut self, ctx: &egui::Context) {
        if self.show_term {
            // Bottom-up order: content → tabs → divider.
            // Divider declared last = highest = sits between editor and terminal tabs,
            // matching VSCode where you drag the top edge of the terminal panel.

            // ── Content ──────────────────────────────────────────────────────
            egui::TopBottomPanel::bottom("terminal_content")
                .exact_height(self.terminal_height.max(40.0))
                .show(ctx, |ui| {
                    match self.bottom_tab {
                        BottomTab::Terminal => {
                            if self.ssh.is_some() {
                                if self.ssh_shell.is_none() {
                                    // Lazy PTY open: first time user opens terminal tab
                                    // while SSH-connected. Non-blocking — spawns in background.
                                    self.open_ssh_pty_background();
                                }
                                self.draw_ssh_terminal(ui);
                            } else {
                                // Terminal-instance tab strip (VS Code-style: multiple
                                // concurrent local terminal sessions, switchable). Mirrors
                                // the agent-tabs bar pattern elsewhere in this file.
                                //
                                // `Terminal::draw_sized` fills its background using
                                // `ui.max_rect()` — the *fixed* bound baked into a `Ui` when
                                // it's created, which a `TopBottomPanel::show_inside` does NOT
                                // shrink (it only advances the parent `Ui`'s cursor/available
                                // rect, which `max_rect()` ignores). So drawing the strip via
                                // `show_inside` and then calling `tab.terminal.draw(ui)` on
                                // the same outer `ui` still hands the terminal the *whole*
                                // panel rect, and it paints over the strip. The fix is to
                                // split `ui.max_rect()` ourselves and hand the terminal an
                                // explicit child `Ui` whose `max_rect` is already shrunk.
                                let strip_h = 24.0;
                                let full_rect = ui.max_rect();
                                let strip_rect = egui::Rect::from_min_size(
                                    full_rect.min, egui::vec2(full_rect.width(), strip_h));
                                let term_rect = egui::Rect::from_min_max(
                                    egui::pos2(full_rect.min.x, full_rect.min.y + strip_h),
                                    full_rect.max);

                                let mut close_idx: Option<usize> = None;
                                let mut new_terminal = false;
                                let mut strip_ui = ui.new_child(
                                    egui::UiBuilder::new().max_rect(strip_rect));
                                strip_ui.painter().rect_filled(
                                    strip_rect, 0.0, egui::Color32::from_rgb(30,30,30));
                                strip_ui.horizontal(|ui| {
                                    ui.set_height(strip_h);
                                    ui.spacing_mut().item_spacing.x = 2.0;
                                    for (i, tab) in self.terminal_tabs.iter().enumerate() {
                                        let active = i == self.terminal_active;
                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(120.0, strip_h), egui::Sense::click());
                                        if active {
                                            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(37,37,38));
                                        } else if resp.hovered() {
                                            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(45,45,46));
                                        }
                                        ui.painter().text(
                                            egui::pos2(rect.left()+8.0, rect.center().y), egui::Align2::LEFT_CENTER,
                                            format!("{}: {}", i + 1, tab.title), egui::FontId::proportional(11.0),
                                            if active { egui::Color32::from_gray(220) } else { egui::Color32::from_gray(140) });
                                        let xr = egui::Rect::from_min_size(
                                            egui::pos2(rect.right()-18.0, rect.center().y-7.0), egui::vec2(14.0,14.0));
                                        let xresp = ui.interact(xr, ui.id().with(("term_close", i)), egui::Sense::click());
                                        let xcol = egui::Color32::from_gray(if xresp.hovered() { 220 } else { 110 });
                                        let xs = egui::Stroke::new(1.2_f32, xcol);
                                        let xc = xr.center();
                                        ui.painter().line_segment([xc-egui::vec2(4.0,4.0), xc+egui::vec2(4.0,4.0)], xs);
                                        ui.painter().line_segment([xc+egui::vec2(-4.0,4.0), xc+egui::vec2(4.0,-4.0)], xs);
                                        if xresp.clicked() { close_idx = Some(i); }
                                        else if resp.clicked() { self.terminal_active = i; }
                                        if resp.hovered() || xresp.hovered() {
                                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        }
                                    }
                                    // "+" new terminal
                                    let (pr, presp) = ui.allocate_exact_size(
                                        egui::vec2(strip_h, strip_h), egui::Sense::click());
                                    if presp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                                    let pc = pr.center();
                                    let pcol = egui::Color32::from_gray(if presp.hovered() { 220 } else { 150 });
                                    let ps = egui::Stroke::new(1.4_f32, pcol);
                                    ui.painter().line_segment([pc-egui::vec2(5.0,0.0), pc+egui::vec2(5.0,0.0)], ps);
                                    ui.painter().line_segment([pc-egui::vec2(0.0,5.0), pc+egui::vec2(0.0,5.0)], ps);
                                    let _ = presp.clone().on_hover_text("New Terminal");
                                    if presp.clicked() { new_terminal = true; }
                                });
                                if let Some(idx) = close_idx {
                                    // Dropping the local `Terminal` no longer kills the
                                    // shell — it's owned by the pty-host daemon now, by
                                    // design, so it survives a Reload Window. An explicit
                                    // tab close needs to explicitly tell the daemon to
                                    // end the session, or it'd leak forever.
                                    if let (Some(id), Some(client)) = (
                                        self.terminal_tabs[idx].terminal.pty_id(),
                                        crate::ptyhost::shared(),
                                    ) {
                                        let _ = client.pty_close(id);
                                    }
                                    self.terminal_tabs.remove(idx);
                                    if self.terminal_active >= self.terminal_tabs.len() {
                                        self.terminal_active = self.terminal_tabs.len().saturating_sub(1);
                                    }
                                }
                                if new_terminal {
                                    self.terminal_tabs.push(TerminalTab::new(&self.cwd));
                                    self.terminal_active = self.terminal_tabs.len() - 1;
                                }
                                let mut term_ui = ui.new_child(
                                    egui::UiBuilder::new().max_rect(term_rect));
                                if let Some(tab) = self.terminal_tabs.get_mut(self.terminal_active) {
                                    tab.terminal.draw(&mut term_ui);
                                } else {
                                    term_ui.centered_and_justified(|ui| {
                                        ui.label(egui::RichText::new("No terminal — click + to start one")
                                            .color(egui::Color32::from_gray(120)));
                                    });
                                }
                            }
                        }
                        BottomTab::Output => {
                            let bg = egui::Color32::from_rgb(14, 14, 14);
                            ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
                            // Move the log out for the duration of the render
                            // instead of deep-copying it. This was a full
                            // `clone()` — every retained line's `String`
                            // reallocated on every repaint the panel is
                            // visible, which with the 300ms baseline is
                            // several times a second and up to 250/sec while
                            // interacting. Restored below, keeping anything
                            // appended while it was out.
                            let log = std::mem::take(&mut self.output_log);
                            egui::ScrollArea::vertical()
                                .id_salt("output_scroll")
                                .stick_to_bottom(true)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.add_space(4.0);
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    for (msg, level) in &log {
                                        ui.label(egui::RichText::new(msg)
                                            .monospace().size(12.5)
                                            .color(level.color()));
                    }
                                    if log.is_empty() {
                                        ui.label(egui::RichText::new("No output yet.")
                                            .size(12.0)
                                            .color(egui::Color32::from_gray(80)));
                    }
                                });
                            // Put it back, appending anything that arrived
                            // while it was moved out so no line is dropped.
                            let appended = std::mem::replace(&mut self.output_log, log);
                            self.output_log.extend(appended);
        }
    }
                });

            // ── Tab bar (declared last = highest, sits at top of panel) ──────
            let tab_h   = 28.0;
            let tab_bg  = egui::Color32::from_rgb(37, 37, 38);
            let act_fg  = egui::Color32::from_gray(220);
            let idle_fg = egui::Color32::from_gray(110);
            egui::TopBottomPanel::bottom("terminal_tabs")
                .exact_height(tab_h)
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().rect_filled(rect, 0.0, tab_bg);
                    ui.horizontal(|ui| {
                        ui.set_height(tab_h);
                        for (label, tab) in [
                            ("TERMINAL", BottomTab::Terminal),
                            ("OUTPUT",   BottomTab::Output),
                        ] {
                            let active = self.bottom_tab == tab;
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(80.0, tab_h), egui::Sense::click());
                            if active {
                                ui.painter().rect_filled(rect, 0.0,
                                    egui::Color32::from_rgb(30, 30, 30));
                                // blue underline accent
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(rect.left(), rect.bottom() - 2.0),
                                        rect.right_bottom()),
                                    0.0, egui::Color32::from_rgb(0, 120, 212));
            }
                            ui.painter().text(
                                rect.center(), egui::Align2::CENTER_CENTER, label,
                                egui::FontId::proportional(11.0),
                                if active { act_fg } else { idle_fg });
                            if resp.clicked() { self.bottom_tab = tab; }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
                    });
                });

            // ── Drag divider (declared last = sits at the very top of the
            //    terminal section, matching VSCode's resize handle position) ──
            egui::TopBottomPanel::bottom("terminal_divider")
                .exact_height(DIVIDER_H)
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    let resp = ui.interact(rect, ui.id().with("drag"), egui::Sense::drag());
                    let color = if resp.hovered() || resp.dragged() {
                        ctx.set_cursor_icon(egui::CursorIcon::ResizeVertical);
                        egui::Color32::from_rgb(0, 120, 212)
                    } else {
                        egui::Color32::from_rgb(40, 40, 40)
                    };
                    ui.painter().rect_filled(rect, 0.0, color);
                    if resp.dragged() {
                        self.terminal_height -= resp.drag_delta().y;
                        self.terminal_height  = self.terminal_height.clamp(40.0, 900.0);
                    }
                });
        }
    }

    fn draw_agent_side_panels(&mut self, ctx: &egui::Context) {
        // ── Right activity bar (Forge agent) ────────────────────────────────
        // Lazy-load the brand icon once.
        if self.forge_icon.is_none() {
            self.forge_icon = load_forge_icon(ctx);
        }
        egui::SidePanel::right("right_activity_bar")
            .exact_width(48.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(egui::Color32::from_rgb(24, 24, 24))
                .inner_margin(egui::Margin::ZERO))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                let active = self.agent_visible;
                let (icon_rect, resp) = ui.allocate_exact_size(
                    egui::vec2(48.0, 44.0), egui::Sense::click(),
                );
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
}
                if active {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(icon_rect.right() - 2.0, icon_rect.top() + 4.0),
                            egui::pos2(icon_rect.right(),       icon_rect.bottom() - 4.0),
                        ),
                        0.0, egui::Color32::WHITE,
                    );
}
                let tint = if active || resp.hovered() {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(140)
                };

                if let Some(tex) = &self.forge_icon {
                    // The PNG canvas is square but the anvil silhouette is
                    // visually wider than tall — give the icon some breathing
                    // room and center it on the slot's midpoint.
                    let s = 30.0;
                    let img_rect = egui::Rect::from_center_size(
                        icon_rect.center(), egui::vec2(s, s),
                    );
                    ui.painter().image(
                        tex.id(), img_rect,
                        egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0),
                        ),
                        tint,
                    );
                } else {
                    paint_anvil(ui.painter(), icon_rect.center(), tint);
}
                let resp = resp.on_hover_text("Forge agent (Ctrl+Shift+A)");
                if resp.clicked() { self.agent_visible = !self.agent_visible; }
            });

        if self.agent_visible {
            egui::SidePanel::right("agent_panel")
                .resizable(true)
                .default_width(360.0)
                .width_range(240.0..=800.0)
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(21, 21, 21));
                    // Drawn into a child clipped to the panel, and the panel
                    // then claims exactly its own rect. egui sizes a side panel
                    // from what its contents ended up occupying, and the default
                    // wrap mode for a label in a horizontal layout is `Extend` —
                    // it grows the parent rather than clipping (there is a note
                    // about this on the queued-message row too). One long path,
                    // checkpoint preview, or a status bar with a few badges too
                    // many therefore became a floor under the panel width that
                    // dragging could not get past: it could be widened and
                    // brought back, never made narrower than its widest line.
                    let mut inner = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    inner.set_clip_rect(rect);
                    self.draw_agent_view(&mut inner);
                    ui.advance_cursor_after_rect(rect);
                });
        }
    }

    fn draw_left_panels(&mut self, ctx: &egui::Context) {
        // ── Activity Bar (always visible, thin vertical icon strip) ────────
        egui::SidePanel::left("activity_bar")
            .exact_width(48.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(egui::Color32::from_rgb(24, 24, 24))
                .inner_margin(egui::Margin::ZERO))
            .show(ctx, |ui| {
                let icons: &[(SidebarView, &str, fn(&egui::Painter, egui::Pos2, egui::Color32))] = &[
                    (SidebarView::Explorer,      "Explorer (Ctrl+Shift+E)",       paint_explorer_icon),
                    (SidebarView::Search,        "Search (Ctrl+Shift+F)",         paint_search_icon),
                    (SidebarView::SourceControl, "Source Control (Ctrl+Shift+G)", paint_branch_icon),
                    (SidebarView::Ssh,           "SSH Remote (Ctrl+Shift+S)",     paint_ssh_icon),
                    (SidebarView::Outline,       "Outline (Ctrl+Shift+O)",        paint_outline_icon),
                ];

                // Total distinct changed files (staged ∪ unstaged), for the Source
                // Control icon's badge — matches VS Code's activity-bar change count.
                let scm_count = self.git.as_ref().map(|g| {
                    let mut paths = std::collections::HashSet::new();
                    paths.extend(g.staged.iter().map(|(p, _)| p.as_path()));
                    paths.extend(g.unstaged.iter().map(|(p, _)| p.as_path()));
                    paths.len()
                }).unwrap_or(0);

                ui.add_space(6.0);
                for &(view, tooltip, paint_fn) in icons {
                    let active = self.sidebar_view == view && self.show_tree;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(48.0, 44.0), egui::Sense::click(),
                    );
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
                    // Active left-border accent (VS Code blue/white bar)
                    if active {
                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(rect.left(), rect.top() + 4.0),
                                egui::pos2(rect.left() + 2.0, rect.bottom() - 4.0),
                            ),
                            0.0, egui::Color32::WHITE,
                        );
    }
                    let color = if active || resp.hovered() {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_gray(140)
                    };
                    paint_fn(ui.painter(), rect.center(), color);

                    // VS Code-style change-count badge on the Source Control icon.
                    if matches!(view, SidebarView::SourceControl) && scm_count > 0 {
                        let badge_c = egui::pos2(rect.right() - 11.0, rect.bottom() - 11.0);
                        ui.painter().circle_filled(badge_c, 8.0, egui::Color32::from_rgb(0, 122, 204));
                        ui.painter().text(
                            badge_c, egui::Align2::CENTER_CENTER,
                            scm_count.to_string(),
                            egui::FontId::proportional(9.5),
                            egui::Color32::WHITE,
                        );
                    }

                    let resp = resp.on_hover_text(tooltip);
                    if resp.clicked() {
                        if active {
                            self.show_tree = false; // collapse if clicking active icon
                        } else {
                            self.show_tree = true;
                            self.sidebar_view = view;
                            if matches!(view, SidebarView::Search) {
                                self.search.request_focus = true;
            }
        }
    }
}

                // Settings gear — pinned to the bottom of the activity bar.
                // There was previously no visible way to reach Settings at
                // all (Ctrl+, only), which isn't discoverable.
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(48.0, 44.0), egui::Sense::click(),
                    );
                    if resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                    let color = if self.settings_open || resp.hovered() {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_gray(140)
                    };
                    paint_gear_icon(ui.painter(), rect.center(), color);
                    let resp = resp.on_hover_text("Settings (Ctrl+,)");
                    if resp.clicked() { self.settings_open = !self.settings_open; }
                });
            });

        // Refresh the outline when it's visible and the active file changed.
        if self.show_tree && self.sidebar_view == SidebarView::Outline {
            let active_path = self.buffers.get(self.active).and_then(|b| b.path.clone());
            if active_path != self.outline_path {
                self.outline.clear();
                self.outline_path = active_path.clone();
                if let (Some(lsp), Some(p)) = (&mut self.lsp, &active_path) {
                    self.outline_req = lsp.document_symbols(p);
                }
            }
        }

        if self.show_tree {
            egui::SidePanel::left("sidebar_content")
                .default_width(260.0)
                .min_width(160.0)
                .max_width(500.0)
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(21, 21, 21));

                    // Panel title bar (matches VS Code style)
                    let title = match self.sidebar_view {
                        SidebarView::Explorer => if self.ssh.is_some() { "REMOTE EXPLORER" } else { "EXPLORER" },
                        SidebarView::Search        => "SEARCH",
                        SidebarView::SourceControl => "SOURCE CONTROL",
                        SidebarView::Ssh           => "SSH REMOTE",
                        SidebarView::Outline       => "OUTLINE",
                    };
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new(title).size(10.5)
                            .color(egui::Color32::from_gray(200)));
                    });
                    ui.add_space(6.0);

                    match self.sidebar_view {
                        SidebarView::Explorer => {
                            if self.ssh.is_some() {
                                // Remote workspace: show the SSH file tree in Explorer
                                self.draw_remote_explorer(ui);
                            } else if !self.has_folder {
                                // No workspace open. Don't render `cwd` as if it
                                // were one — offer to pick a folder instead.
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.label(egui::RichText::new("No folder opened")
                                        .size(12.0).color(egui::Color32::from_gray(150)));
                                });
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    if ui.button("Open Folder…").clicked() {
                                        self.open_folder_dialog();
                                    }
                                });
                            } else {
                                match self.file_tree.draw(ui, self.git.as_ref()) {
                                    Some(TreeAction::Open(path))          => self.open_file(path),
                                    Some(TreeAction::OpenInTerminal(dir)) => {
                                        self.terminal_tabs.push(TerminalTab::new(&dir));
                                        self.terminal_active = self.terminal_tabs.len() - 1;
                                        self.show_term  = true;
                                        self.bottom_tab = BottomTab::Terminal;
                    }
                                    Some(TreeAction::OpenFolderDialog)    => self.open_folder_dialog(),
                                    Some(TreeAction::AddFolderDialog)     => {
                                        self.folder_add_root = true;
                                        self.open_folder_dialog();
                                    }
                                    Some(TreeAction::DropFiles { dir, paths }) => {
                                        self.import_dropped_files(&dir, &paths);
                                    }
                                    None => {}
                }
            }
        }
                        SidebarView::Search => {
                            self.draw_search_panel(ui);
        }
                        SidebarView::SourceControl => {
                            self.draw_source_control_panel(ui);
        }
                        SidebarView::Ssh => {
                            self.draw_ssh_panel(ui);
        }
                        SidebarView::Outline => {
                            self.draw_outline_panel(ui);
        }
    }
                });
        }
    }

    pub fn draw(&mut self, ctx: &egui::Context) {
        // Load fonts on first frame and skip rendering — egui processes
        // set_fonts() asynchronously so the named family isn't ready until
        // the following frame.
        if !self.fonts_loaded {
            self.fonts_loaded = true;
            setup_fonts(ctx);
            ctx.request_repaint();
            return;
        }

        // ── Per-frame trace (remove after debugging) ──────────────────────────
        if self.ssh_overlay {
        }


        // Auto-connect SSH when this window was opened with a host spec
        if let Some(host) = self.pending_ssh_connect.take() {
            self.ssh_form    = host;
            self.ssh_connect();
        }

        // Anvil watermark heat: each tool call is a hammer strike that jumps
        // the heat up, like a real anvil. Between strikes it cools a little
        // while the agent is still working — obviously still hot, just
        // waiting on the next tool call — and cools fully once idle.
        let agent_working = self.agent_tabs.iter().any(|t| t.session.is_active());
        let tool_pulses: u32 = self.agent_tabs.iter_mut()
            .map(|t| t.session.drain_tool_pulses()).sum();
        let dt = ctx.input(|i| i.stable_dt);
        self.anvil_heat_target = (self.anvil_heat_target + tool_pulses as f32 * 0.10).min(1.0);
        let cool_rate = if agent_working { dt / 150.0 } else { dt / 40.0 };
        self.anvil_heat_target = (self.anvil_heat_target - cool_rate).max(0.0);
        // The *displayed* heat eases toward the target instead of jumping
        // straight to it on every pulse — a real forge fire climbs, it
        // doesn't snap to a new color the instant more coal goes on.
        // Frame-rate-independent exponential approach (~0.35s to close most
        // of the gap to a new target).
        let rise = 1.0 - (-dt / 0.35).exp();
        self.anvil_heat += (self.anvil_heat_target - self.anvil_heat) * rise;

        // ── Repaint scheduling ──────────────────────────────────────────────
        // The event loop (main.rs) sleeps between frames now instead of
        // spinning unconditionally (that used to be the dominant cause of
        // this app's CPU/energy usage — see `about_to_wait`'s doc comment).
        // Real input always wakes it immediately regardless; what needs
        // explicit scheduling here is everything that updates *without*
        // direct user input this frame.
        //
        // While the user is actively touching the mouse/trackpad/keyboard —
        // scrolling, dragging, clicking, typing — render at (essentially)
        // full display refresh rate instead of the background-polling
        // cadence below. Winit already wakes the loop immediately on each
        // real input event regardless of sleep state, but relying on that
        // alone during a fast continuous gesture (a scroll) reads as rough:
        // any OS-level wake-latency that a busy-spinning loop would have
        // hidden becomes visible once the loop actually sleeps between
        // frames. Scheduling the next frame proactively for a short window
        // after the last bit of input closes that gap. 200ms of "still
        // interacting" covers pauses mid-gesture (e.g. a scroll that
        // briefly stops) without keeping this tier alive indefinitely.
        // `any_down` matters as much as events or velocity here. A button held
        // still — mid drag-select, mid scrollbar drag — generates no events and
        // has zero velocity, so on those two alone the loop went to sleep and
        // the gesture froze until the pointer moved again. That was invisible
        // while an unconditional 300ms repaint existed to paper over it; now
        // that idle really is idle, a held button has to count as interaction.
        let had_input_this_frame = ctx.input(|i| {
            !i.events.is_empty()
                || i.pointer.velocity() != egui::Vec2::ZERO
                || i.pointer.any_down()
        });
        if had_input_this_frame {
            self.last_interaction_at = Some(std::time::Instant::now());
        }
        let interacting_recently = self.last_interaction_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(200));
        if interacting_recently {
            // ~240Hz ceiling — effectively "as fast as the display can show"
            // for any real panel refresh rate, without being a literal
            // zero-delay busy loop.
            ctx.request_repaint_after(std::time::Duration::from_millis(4));
        }

        // There is deliberately no unconditional baseline repaint here.
        //
        // This used to be `request_repaint_after(300ms)`, always — a blanket
        // 3.3 Hz poll so that anything arriving on any channel would be picked
        // up without having to enumerate every background source. The cost was
        // that the app never idled: ~4% of a core sitting still, each tick a
        // full egui layout plus a GPU submit (~8.7ms measured).
        //
        // Producers now wake the loop themselves via `crate::wake::wake()` —
        // the terminal PTY readers, the LSP reader, the file watcher, the git
        // status/blame scans, and the pty-host client all call it after sending.
        // Anything that pushes without user input therefore gets a frame
        // promptly, and a genuinely idle window schedules nothing at all.
        //
        // The tiers below still apply: they cover work that is *in flight and
        // polled* (where a steady tick is simpler than threading a waker
        // through), and the interactive tier keeps gestures smooth.
        // While something is actively streaming/in-flight, poll fast enough
        // that it still feels live: agent tokens, terminal output, a
        // background git/SSH/task/dialog result, or the anvil cooldown
        // animation still playing out. Terminal "activity" is judged by
        // actual recent output, not just the panel being visible — the
        // Terminal panel is on by default, so gating on visibility alone
        // made this tier effectively permanent (a shell sitting idle at
        // its prompt was still polled at 20Hz forever).
        const TERMINAL_ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
        let terminal_active = self.show_term && self.bottom_tab == BottomTab::Terminal && (
            if self.ssh.is_some() {
                self.ssh_term_last_output.is_some_and(|at| at.elapsed() < TERMINAL_ACTIVE_WINDOW)
            } else {
                self.terminal_tabs.get(self.terminal_active)
                    .is_some_and(|t| t.terminal.recently_active(TERMINAL_ACTIVE_WINDOW))
            }
        );
        let anything_streaming =
            (self.agent_visible && agent_working)
            || terminal_active
            || self.task_rx.is_some()
            || self.ssh_log_rx.is_some()
            || self.ssh_pty_rx.is_some()
            || self.ssh_open_rx.is_some()
            || self.ssh_upload_rx.is_some()
            || self.ssh_nav_rx.is_some()
            || self.ssh_connect_rx.is_some()
            || self.gh_check.is_some()
            || self.git_task.is_some()
            || self.git.as_ref().is_some_and(|g| g.scanning())
            || self.blame_rx.is_some()
            || self.fmt_rx.is_some()
            || self.tree_refresh_due.is_some()
            || self.folder_rx.is_some()
            || self.dap_running
            || self.search.searching
            || self.anvil_heat > 0.001;
        if anything_streaming {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Poll any in-flight git fetch/pull/push
        self.poll_git_task();

        // Apply a finished background status scan. Decorations and the SC
        // panel are empty until this lands, which is the tradeoff for not
        // blocking window creation on a working-tree walk.
        if let Some(g) = &mut self.git {
            if g.poll() { self.gutter_dirty = true; }
        }

        // Fire a coalesced file-tree refresh once the event burst has settled.
        if self.tree_refresh_due.is_some_and(|t| std::time::Instant::now() >= t) {
            self.tree_refresh_due = None;
            self.file_tree.refresh();
        }

        // Apply a finished external-formatter run.
        if let Some(rx) = &self.fmt_rx {
            match rx.try_recv() {
                Ok(pf) => { self.fmt_rx = None; self.apply_format(pf); }
                Err(mpsc::TryRecvError::Empty)        => {}
                Err(mpsc::TryRecvError::Disconnected) => self.fmt_rx = None,
            }
        }

        // Drain external file-change events (git checkout, a pull, another
        // process, or an agent creating/editing files) and reload matching
        // open buffers — same "reload if unmodified, warn if not" policy as
        // the agent's own live-reload. Also refreshes the file tree once if
        // anything changed at all: a brand-new file (or a deleted one)
        // never matches an open buffer, so without this the tree just sat
        // frozen at whatever it looked like when the folder was first
        // opened — most visible in a fresh workspace where an agent is
        // creating files that were never open in an editor tab to begin with.
        if let Some(fw) = &self.file_watcher {
            let mut logs: Vec<(String, OutputLevel)> = Vec::new();
            let mut any_change = false;
            while let Ok(ev) = fw.rx.try_recv() {
                any_change = true;
                let (path, removed) = match ev {
                    crate::filewatch::WatchEvent::Changed(p) => (p, false),
                    crate::filewatch::WatchEvent::Removed(p) => (p, true),
                };
                if let Some(buf) = self.buffers.iter_mut().find(|b| b.path.as_deref() == Some(path.as_path())) {
                    let rel = path.strip_prefix(&self.cwd).unwrap_or(&path).display().to_string();
                    if removed {
                        logs.push((format!("{rel} was deleted outside the editor — buffer kept in memory."), OutputLevel::Warn));
                        buf.modified = true;
                    } else if buf.modified {
                        logs.push((format!("{rel} changed on disk, but has unsaved changes here — not auto-reloaded."), OutputLevel::Warn));
                    } else {
                        match buf.reload() {
                            Ok(()) => logs.push((format!("Reloaded {rel} (changed externally)"), OutputLevel::Info)),
                            Err(e) => logs.push((format!("Failed to reload {rel}: {e}"), OutputLevel::Warn)),
                        }
                        self.gutter_dirty = true;
                    }
                }
            }
            // Coalesce instead of refreshing per batch of events. The watcher is
            // recursive over the whole workspace, so one `cargo build` or `git
            // checkout` produces a burst of events — and each refresh re-walks
            // every expanded directory on this thread. Collapse a burst into a
            // single refresh shortly after it goes quiet.
            if any_change {
                self.tree_refresh_due =
                    Some(std::time::Instant::now() + TREE_REFRESH_DEBOUNCE);
            }
            for (msg, level) in logs {
                self.push_output(msg, level);
            }
        }

        // Drain running-task output into the Output panel. Collected first so
        // the receiver borrow ends before `push_output` takes `&mut self`.
        if let Some(rx) = &self.task_rx {
            let mut drained = Vec::new();
            let mut done = false;
            loop {
                match rx.try_recv() {
                    Ok(line) => drained.push(line),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => { done = true; break; }
                }
            }
            if done { self.task_rx = None; }
            for (msg, level) in drained { self.push_output(msg, level); }
        }

        // Drain SSH connection log messages into the output panel
        if let Some(rx) = &self.ssh_log_rx {
            let drained: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
            for (msg, level) in drained { self.push_output(msg, level); }
        }

        // Drain remote upload results.
        if let Some(rx) = &self.ssh_upload_rx {
            let mut done = false;
            let mut msgs: Vec<(String, OutputLevel)> = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(Ok(path))  => msgs.push((format!("Uploaded {path}"), OutputLevel::Success)),
                    Ok(Err(e))    => msgs.push((format!("Upload failed: {e}"), OutputLevel::Error)),
                    Err(mpsc::TryRecvError::Empty) => break,
                    // Sender dropped = every file has been reported.
                    Err(mpsc::TryRecvError::Disconnected) => { done = true; break; }
                }
            }
            for (m, l) in msgs { self.output_log(m, l); }
            if done {
                self.ssh_upload_rx = None;
                self.status = "Upload complete".into();
                // Show what landed.
                if let Some(dir) = self.ssh_upload_dir.take() {
                    if self.ssh_tree.last().map(|(p, _)| p.as_str()) == Some(dir.as_str()) {
                        self.ssh_tree.pop();
                        self.ssh_navigate(dir);
                    }
                }
            }
        }

        // Poll lazy PTY open result
        if let Some(rx) = &self.ssh_pty_rx {
            match rx.try_recv() {
                Ok(Ok(shell)) => {
                    self.ssh_shell = Some(shell);
                    self.ssh_pty_rx = None;
                    self.output_log("Remote terminal connected".to_string(), OutputLevel::Success);
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    self.ssh_pty_rx = None;
                    self.output_log(format!("Terminal unavailable: {e}"), OutputLevel::Warn);
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty)        => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.ssh_pty_rx = None; }
            }
        }

        // Poll remote file open result
        if let Some(rx) = &self.ssh_open_rx {
            match rx.try_recv() {
                Ok(Ok((path, text))) => {
                    self.ssh_open_rx = None;
                    let fake_path = std::path::PathBuf::from(&path);
                    if let Some(i) = self.buffers.iter().position(|b| {
                        b.diff.is_none() && b.path.as_ref().map(|p| *p == fake_path).unwrap_or(false)
                    }) {
                        self.active = i;
                    } else {
                        let lines = if text.is_empty() { vec![String::new()] }
                                    else { text.lines().map(String::from).collect() };
                        let mut buf = crate::buffer::Buffer::new();
                        buf.path     = Some(fake_path);
                        buf.lines    = lines;
                        buf.modified = false;
                        self.buffers.push(buf);
                        self.active       = self.buffers.len() - 1;
                        self.gutter_dirty = true;
                        self.status       = format!("Opened {path}");
                    }
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    self.ssh_open_rx = None;
                    self.ssh_error   = Some(e);
                    ctx.request_repaint();
                }
                Err(mpsc::TryRecvError::Empty)        => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.ssh_open_rx = None; }
            }
        }

        // Poll SSH directory navigation result
        if let Some(rx) = &self.ssh_nav_rx {
            match rx.try_recv() {
                Ok(Ok((path, entries))) => {
                    self.ssh_tree.push((path, entries));
                    self.ssh_nav_rx = None;
                    ctx.request_repaint();
}
                Ok(Err(e)) => { self.ssh_error = Some(e); self.ssh_nav_rx = None; }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.ssh_nav_rx = None; }
            }
        }

        // Poll SSH connect result
        if let Some(rx) = &self.ssh_connect_rx {
            match rx.try_recv() {
                Ok(Ok(ready)) => {
                    self.ssh_connecting = false;
                    self.ssh_connect_rx = None;
                    self.ssh_log_rx     = None;
                    self.ssh_error      = ready.shell_err.clone();
                    let host_str = ready.conn.host.host.clone();
                    self.output_log(format!("Connected to {host_str}"), OutputLevel::Success);
                    self.status       = format!("Connected to {host_str}");
                    // Switch to Explorer so user immediately sees the remote tree
                    self.sidebar_view = SidebarView::Explorer;
                    self.show_tree    = true;
                    // Populate file tree with the pre-fetched directory listing.
                    self.ssh_tree = vec![(ready.root_path.clone(), ready.entries)];
                    self.ssh_shell = ready.shell;
                    // Clear the SSH terminal grid for the new connection
                    if let Ok(mut g) = self.ssh_term.lock() { *g = crate::terminal::Grid::with_size(50, 220); }
                    self.ssh_term_focused = false;
                    self.bottom_tab = BottomTab::Terminal; // switch to terminal tab
                    self.ssh = Some(ready.conn);
                    ctx.request_repaint();
}
                Ok(Err(e)) => {
                    self.ssh_connecting = false;
                    self.ssh_connect_rx = None;
                    self.ssh_log_rx     = None;
                    self.output_log(format!("Error: {e}"), OutputLevel::Error);
                    self.status    = format!("SSH error: {e}");
                    self.ssh_error = Some(e);
                    ctx.request_repaint();
}
                Err(mpsc::TryRecvError::Empty)        => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.ssh_connecting = false;
                    self.ssh_connect_rx = None;
}
            }
        }

        // Poll update-checker result (best-effort — no error path, a failed
        // check just looks identical to "already up to date")
        if let Some(rx) = &self.update_check_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.update_available = result;
                    self.update_check_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.update_check_rx = None; }
            }
        }

        // Drain language-server messages — collect all responses first, then
        // apply mutations so we don't hold the lsp borrow across self.open_file etc.
        #[derive(Default)]
        struct LspResults {
            diags_changed: bool,
            hover:         Option<Option<String>>,
            completions:   Option<Vec<crate::lsp::CompletionItem>>,
            goto_loc:      Option<crate::lsp::Location>,
            refs:          Option<Vec<crate::lsp::Location>>,
            rename_edits:  Option<(Vec<crate::lsp::TextEdit>, String)>,
            actions:       Option<Vec<crate::lsp::CodeAction>>,
            sig:           Option<Option<crate::lsp::SignatureHelp>>,
            fmt_edits:     Option<(Vec<crate::lsp::TextEdit>, String)>,
        }
        let mut lr = LspResults::default();
        if let Some(lsp) = self.lsp.as_mut() {
            lr.diags_changed = lsp.poll(&mut self.diagnostics);
            if self.hover_req > 0 {
                if let Some(r) = lsp.take_response(self.hover_req) {
                    self.hover_req = 0;
                    lr.hover = Some(crate::lsp::parse_hover(&r));
}
            }
            if self.comp_req > 0 {
                if let Some(r) = lsp.take_response(self.comp_req) {
                    self.comp_req = 0;
                    lr.completions = Some(crate::lsp::parse_completions(&r));
}
            }
            if self.goto_req > 0 {
                if let Some(r) = lsp.take_response(self.goto_req) {
                    self.goto_req = 0;
                    lr.goto_loc = crate::lsp::parse_goto(&r);
}
            }
            if self.refs_req > 0 {
                if let Some(r) = lsp.take_response(self.refs_req) {
                    self.refs_req = 0;
                    lr.refs = Some(crate::lsp::parse_references(&r));
}
            }
            if self.rename_req > 0 {
                if let Some(r) = lsp.take_response(self.rename_req) {
                    self.rename_req = 0;
                    let active_uri = self.buffers.get(self.active)
                        .and_then(|b| b.path.as_ref())
                        .map(|p| crate::lsp::path_to_uri(p))
                        .unwrap_or_default();
                    let edits = crate::lsp::parse_text_edits(&r, Some(&active_uri));
                    lr.rename_edits = Some((edits, active_uri));
}
            }
            if self.action_req > 0 {
                if let Some(r) = lsp.take_response(self.action_req) {
                    self.action_req = 0;
                    lr.actions = Some(crate::lsp::parse_code_actions(&r));
}
            }
            if self.sig_req > 0 {
                if let Some(r) = lsp.take_response(self.sig_req) {
                    self.sig_req = 0;
                    lr.sig = Some(crate::lsp::parse_signature_help(&r));
}
            }
            if self.outline_req > 0 {
                if let Some(r) = lsp.take_response(self.outline_req) {
                    self.outline_req = 0;
                    self.outline = crate::lsp::parse_document_symbols(&r);
                }
            }
            if self.fmt_req > 0 {
                if let Some(r) = lsp.take_response(self.fmt_req) {
                    self.fmt_req = 0;
                    let active_uri = self.buffers.get(self.active)
                        .and_then(|b| b.path.as_ref())
                        .map(|p| crate::lsp::path_to_uri(p))
                        .unwrap_or_default();
                    let edits = crate::lsp::parse_text_edits(&r, Some(&active_uri));
                    lr.fmt_edits = Some((edits, active_uri));
}
            }
        }
        // Now apply results (lsp borrow is released)
        if lr.diags_changed                { ctx.request_repaint(); }
        if let Some(h) = lr.hover          { self.hover_text = h; ctx.request_repaint(); }
        if let Some(c) = lr.completions    { self.comp_items = c; self.comp_cursor = 0; ctx.request_repaint(); }
        if let Some(loc) = lr.goto_loc {
            self.open_file(loc.path.clone());
            self.pending_scroll = Some(loc.start_line as usize);
            if let Some(buf) = self.buffers.get_mut(self.active) {
                buf.cursor = (loc.start_line as usize, loc.start_col as usize);
            }
            ctx.request_repaint();
        }
        if let Some(refs) = lr.refs {
            self.refs_visible = !refs.is_empty();
            self.refs_results = refs;
            ctx.request_repaint();
        }
        if let Some((edits, _)) = lr.rename_edits {
            if let Some(buf) = self.buffers.get_mut(self.active) {
                crate::lsp::apply_edits(&mut buf.lines, edits);
                buf.modified = true;
                self.gutter_dirty = true;
            }
            ctx.request_repaint();
        }
        if let Some(a) = lr.actions        { self.action_items = a; ctx.request_repaint(); }
        if let Some(s) = lr.sig            { self.sig_help = s; ctx.request_repaint(); }
        if let Some((edits, _)) = lr.fmt_edits {
            if let Some(buf) = self.buffers.get_mut(self.active) {
                crate::lsp::apply_edits(&mut buf.lines, edits);
                buf.modified = false;
                self.gutter_dirty = true;
                self.status = "Formatted".into();
            }
            ctx.request_repaint();
        }

        // ── Poll DAP (debug adapter) ────────────────────────────────
        if self.dap.is_some() {
            let mut terminated  = false;
            let mut stopped     = false;
            let mut status_msg: Option<String> = None;
            let mut outputs: Vec<String> = Vec::new();
            let mut goto_frame: Option<(PathBuf, usize)> = None;
            let mut new_stack:  Option<Vec<crate::dap::StackFrame>> = None;
            let mut new_vars:   Option<Vec<crate::dap::Variable>>   = None;
            if let Some(dap) = &mut self.dap {
                for ev in dap.poll() {
                    match ev {
                        crate::dap::DapEvent::Stopped { reason, .. } => {
                            stopped = true;
                            status_msg = Some(format!("Stopped ({reason})"));
                            dap.stack_trace();
                        }
                        crate::dap::DapEvent::Output(t) => outputs.push(t),
                        crate::dap::DapEvent::Terminated => terminated = true,
                    }
                }
                if dap.stack_req > 0 {
                    if let Some(r) = dap.take_response(dap.stack_req) {
                        dap.stack_req = 0;
                        let stack = crate::dap::parse_stack_trace(&r);
                        if let Some(top) = stack.first() {
                            if let Some(p) = &top.path {
                                goto_frame = Some((p.clone(), top.line.saturating_sub(1)));
                            }
                            let fid = top.id;
                            dap.scopes(fid);
                        }
                        new_stack = Some(stack);
                    }
                }
                if dap.vars_req > 0 {
                    if let Some(r) = dap.take_response(dap.vars_req) {
                        dap.vars_req = 0;
                        match r.get("command").and_then(|v| v.as_str()) {
                            Some("scopes") => {
                                if let Some(vr) = crate::dap::parse_first_scope_ref(&r) {
                                    dap.variables(vr);
                                }
                            }
                            Some("variables") => {
                                new_vars = Some(crate::dap::parse_variables(&r));
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some(m) = status_msg { self.status = m; }
            if let Some(s) = new_stack  { self.dap_stack = s; }
            if let Some(v) = new_vars   { self.dap_vars = v; }
            for t in outputs {
                self.push_output(t, OutputLevel::Info);
            }
            if let Some((path, line)) = goto_frame {
                if path.exists() {
                    self.open_file(path.clone());
                    self.pending_scroll = Some(line);
                    if let Some(buf) = self.buffers.get_mut(self.active) {
                        buf.cursor = (line, 0);
                    }
                }
                self.dap_stopped = Some((path, line));
            }
            if stopped { ctx.request_repaint(); }
            if terminated {
                self.dap = None;
                self.dap_stopped = None;
                self.dap_running = false;
                self.dap_stack.clear();
                self.dap_vars.clear();
                self.status = "Debug session ended".into();
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        // Poll the one-time `gh` availability check
        if let Some(rx) = &self.gh_check {
            match rx.try_recv() {
                Ok(v) => { self.gh_ready = Some(v); self.gh_check = None; }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.gh_check = None; }
            }
        }

        // Poll open-folder dialog result
        if let Some(rx) = &self.folder_rx {
            if let Ok(result) = rx.try_recv() {
                self.folder_rx = None;
                if self.folder_add_root {
                    self.folder_add_root = false;
                    if let Some(path) = result {
                        self.file_tree.add_root(path);
                    }
                } else if let Some(path) = result {
                    // This is the only thing that establishes a workspace.
                    self.has_folder = true;
                    self.cwd = path.clone();
                    self.file_tree.set_root(path.clone());
                    self.git = crate::git::GitState::open(&path);
                    self.file_watcher = crate::filewatch::FileWatcher::new(&path);
                    // Restart every local terminal tab's shell in the new
                    // folder instead of leaving it running in the old one —
                    // an explicit preference (VS Code instead leaves existing
                    // terminals where they were, only new ones get the new
                    // cwd, but that reads as "the terminal is stuck on the
                    // wrong folder" here). SSH terminals are unaffected —
                    // their cwd is a remote path, unrelated to the local
                    // workspace folder.
                    for tab in &mut self.terminal_tabs {
                        tab.terminal.restart_in(&path);
                    }
                }
            }
        }

        if self.palette.name != self.settings.theme {
            self.palette = crate::theme::by_name(&self.settings.theme);
        }
        ctx.set_visuals(if self.palette.dark { egui::Visuals::dark() }
                        else                 { egui::Visuals::light() });
        self.draw_menu(ctx);

        // Global keyboard shortcuts (before panels) — suppressed while a
        // terminal has keyboard focus, so shell/readline chords (Ctrl+N,
        // Ctrl+P, Ctrl+F, Ctrl+K, …) don't also fire IDE actions like New
        // File or Quick Open behind the user's back while they're typing.
        let terminal_focused = self.show_term && self.bottom_tab == BottomTab::Terminal && (
            if self.ssh.is_some() {
                self.ssh_term_focused
            } else {
                self.terminal_tabs.get(self.terminal_active)
                    .map_or(false, |t| t.terminal.is_focused())
            }
        );
        let ctrl = ctx.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
        let shift = ctx.input(|i| i.modifiers.shift);
        if !terminal_focused {
        if ctrl && !shift && ctx.input(|i| i.key_pressed(egui::Key::P)) {
            self.cmd_palette = None;
            if !self.has_folder {
                self.status = "Open a folder first".into();
            } else if self.quick_open.is_none() {
                self.quick_open = Some(QuickOpen::new(&self.cwd));
            } else {
                self.quick_open = None;
            }
        }
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::P)) {
            self.quick_open = None;
            if self.cmd_palette.is_none() {
                self.cmd_palette = Some(CmdPalette::new(self.plugins.commands.iter().map(|c| format!("Plugin: {}", c.title)).collect()));
            } else {
                self.cmd_palette = None;
            }
        }
        if ctrl && !shift && ctx.input(|i| i.key_pressed(egui::Key::N)) {
            self.buffers.push(Buffer::new());
            self.active = self.buffers.len() - 1;
        }
        // Ctrl+Shift+N: new window
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::N)) {
            self.pending_new_window = Some(NewWindowSpec::default());
        }
        // Ctrl+Shift+R: reload window (restart in-place, same workspace —
        // matches VS Code's binding for the same action).
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::R)) {
            self.reload_window();
        }
        // Ctrl+,: settings
        if ctrl && ctx.input(|i| i.key_pressed(egui::Key::Comma)) {
            self.settings_open = !self.settings_open;
        }
        if ctrl && !shift && ctx.input(|i| i.key_pressed(egui::Key::F)) {
            // Toggle find bar
            if self.find_bar.is_some() {
                self.find_bar = None;
            } else {
                self.find_bar = Some(FindBar::new(false));
            }
        }
        // Ctrl+Shift+H for Replace (avoids macOS system ⌘+H = Hide Window)
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::H)) {
            if self.find_bar.is_some() {
                self.find_bar = None;
            } else {
                self.find_bar = Some(FindBar::new(true));
            }
        }
        // Ctrl+Shift+F: open multi-file search panel in sidebar
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::F)) {
            self.show_tree = true;
            self.sidebar_view = SidebarView::Search;
            self.search.request_focus = true;
        }
        // Ctrl+Shift+E: switch sidebar back to Explorer
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::E)) {
            self.show_tree = true;
            self.sidebar_view = SidebarView::Explorer;
        }
        // Ctrl+Shift+G: source control
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::G)) {
            self.show_tree = true;
            self.sidebar_view = SidebarView::SourceControl;
        }
        // Ctrl+Shift+S: SSH remote
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::S)) {
            self.show_tree = true;
            self.sidebar_view = SidebarView::Ssh;
        }
        // Ctrl+Shift+O: outline / symbol tree
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::O)) {
            self.show_tree = true;
            self.sidebar_view = SidebarView::Outline;
        }
        // Ctrl+Shift+A: toggle Forge agent panel on the right
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::A)) {
            self.agent_visible = !self.agent_visible;
        }
        // Alt+Z: toggle word wrap
        if ctx.input(|i| i.modifiers.alt) && ctx.input(|i| i.key_pressed(egui::Key::Z)) {
            self.settings.word_wrap = !self.settings.word_wrap;
            crate::settings::save(&self.settings);
            self.status = if self.settings.word_wrap { "Word wrap: on".into() }
                          else                        { "Word wrap: off".into() };
        }
        // Ctrl+G: go to line
        if ctrl && !shift && ctx.input(|i| i.key_pressed(egui::Key::G)) {
            self.goto_line = if self.goto_line.is_some() { None } else { Some(String::new()) };
        }
        // Ctrl+Shift+B: run task (from .forge/tasks.toml)
        if ctrl && shift && ctx.input(|i| i.key_pressed(egui::Key::B)) {
            self.task_picker = if self.task_picker.is_some() { None } else { Some(0) };
        }
        // ── Debugging (DAP) ────────────────────────────────────
        // F9: toggle breakpoint on the current line
        if ctx.input(|i| i.key_pressed(egui::Key::F9)) {
            if let Some(buf) = self.buffers.get(self.active) {
                if let Some(path) = buf.path.clone() {
                    let line = buf.cursor.0;
                    let set = self.breakpoints.entry(path.clone()).or_default();
                    if !set.remove(&line) { set.insert(line); }
                    let lines: Vec<usize> = set.iter().copied().collect();
                    if let Some(dap) = &mut self.dap {
                        dap.set_breakpoints(&path, &lines);
                    }
                }
            }
        }
        // F5: start debugging / continue.  Shift+F5: stop.
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            if shift {
                self.dap = None;
                self.dap_stopped = None;
                self.dap_running = false;
                self.dap_stack.clear();
                self.dap_vars.clear();
                self.status = "Debug session stopped".into();
            } else if let Some(dap) = &mut self.dap {
                dap.continue_run();
                self.dap_stopped = None;
                self.status = "Continuing…".into();
            } else {
                let active_path = self.buffers.get(self.active).and_then(|b| b.path.clone());
                match crate::dap::launch_config(&self.cwd, active_path.as_deref()) {
                    Ok(cfg) => match crate::dap::DapClient::start(&cfg, &self.cwd) {
                        Ok(mut dap) => {
                            dap.launch(&cfg, &self.cwd);
                            for (path, set) in &self.breakpoints {
                                let lines: Vec<usize> = set.iter().copied().collect();
                                dap.set_breakpoints(path, &lines);
                            }
                            dap.configuration_done();
                            self.dap = Some(dap);
                            self.dap_running = true;
                            self.status = format!("Debugging: {}", cfg.program);
                            self.show_term  = true;
                            self.bottom_tab = BottomTab::Output;
                        }
                        Err(e) => self.status = format!("Debug: {e}"),
                    },
                    Err(e) => self.status = format!("Debug: {e}"),
                }
            }
        }
        // F10 / F11 / Shift+F11: step over / into / out
        if self.dap.is_some() {
            if ctx.input(|i| i.key_pressed(egui::Key::F10)) {
                if let Some(d) = &mut self.dap { d.step_over(); }
            }
            if !shift && ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                if let Some(d) = &mut self.dap { d.step_in(); }
            }
            if shift && ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                if let Some(d) = &mut self.dap { d.step_out(); }
            }
        }

        // Ctrl+\: split editor
        if ctrl && !shift && ctx.input(|i| i.key_pressed(egui::Key::Backslash)) {
            self.split = match self.split {
                Some(_) => None,
                None if !self.buffers.is_empty() => Some(self.active),
                None => None,
            };
        }
        // Ctrl+K … chord prefix (Ctrl+K T / Ctrl+K Ctrl+T → theme picker)
        if ctrl && ctx.input(|i| i.key_pressed(egui::Key::K)) {
            self.ctrl_k_chord = true;
        } else if self.ctrl_k_chord {
            if ctx.input(|i| i.key_pressed(egui::Key::T)) {
                self.theme_picker = Some(
                    crate::theme::theme_names().iter()
                        .position(|n| *n == self.settings.theme).unwrap_or(0));
                self.theme_prev = Some(self.settings.theme.clone());
                self.ctrl_k_chord = false;
            } else if ctx.input(|i| i.keys_down.iter().any(|k| *k != egui::Key::K))
                   || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.ctrl_k_chord = false;
            }
        }
        } // !terminal_focused

        // ── Bottom panels FIRST so they span the full window width ──────────
        // (egui assigns space in declaration order; bottom panels declared
        //  before side panels claim the full row, not just the remainder.)

        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(22.0)
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(24, 24, 24));
                let w   = egui::Color32::from_gray(160);
                let dim = egui::Color32::from_gray(90);

                ui.horizontal(|ui| {
                    // ── >< SSH Remote button (far left, VSCode-style) ──────────
                    let (ssh_rect, ssh_resp) = ui.allocate_exact_size(
                        egui::vec2(36.0, 22.0), egui::Sense::click());
                    let connected = self.ssh.is_some();
                    let ssh_bg = if connected {
                        egui::Color32::from_rgb(14, 99, 156)
                    } else {
                        egui::Color32::from_rgb(35, 35, 38)
                    };
                    ui.painter().rect_filled(ssh_rect, 0.0, ssh_bg);
                    let p  = ui.painter();
                    let cy = ssh_rect.center().y;
                    let cx = ssh_rect.center().x;
                    let fg = egui::Color32::from_gray(210);
                    if connected {
                        let name = self.ssh.as_ref().map(|s| s.host.name.clone())
                            .unwrap_or_default();
                        p.text(egui::pos2(cx, cy), egui::Align2::CENTER_CENTER,
                            format!("⏻ {name}"), egui::FontId::proportional(10.5), fg);
                    } else {
                        // ><  rendered as text — simple, readable, matches VSCode's icon
                        p.text(egui::pos2(cx, cy), egui::Align2::CENTER_CENTER,
                            "><", egui::FontId::monospace(11.5), fg);
    }
                    if ssh_resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
                    let _ = ssh_resp.clone().on_hover_text(
                        if connected { "SSH Remote — click to disconnect" }
                        else         { "Open Remote Connection" });
                    if ssh_resp.clicked() {
                        if connected {
                            self.ssh       = None;
                            self.ssh_tree  = Vec::new();
                            self.ssh_shell = None;
                            self.status    = "Disconnected".into();
                            // Explorer goes back to showing the local file tree
                            self.sidebar_view = SidebarView::Explorer;
                        } else {
                            self.ssh_overlay       = true;
                            self.ssh_overlay_query = String::new();
                            self.ssh_overlay_frame = 0;
                            self.ssh_overlay_step  = SshOverlayStep::ChooseWindow;
        }
    }

                    ui.add_space(6.0);

                    // Left: folder name · git branch · status message
                    let folder = self.cwd.file_name()
                        .and_then(|n| n.to_str()).unwrap_or("Forge IDE");
                    ui.label(egui::RichText::new(folder).size(11.0).color(w));

                    if let Some(g) = &self.git {
                        ui.label(egui::RichText::new("·").size(11.0).color(dim));
                        let (br_rect, _) = ui.allocate_exact_size(
                            egui::vec2(12.0, 14.0), egui::Sense::hover(),
                        );
                        paint_branch_icon(ui.painter(), br_rect.center(), w);

                        if let Some(ssh) = &self.ssh {
                            let lbl = format!("SSH: {}", ssh.host.host);
                            ui.label(egui::RichText::new(lbl).size(11.0)
                                .color(egui::Color32::from_rgb(80, 200, 120)));
        }

                        let mut branch_label = g.branch.clone();
                        if g.ahead > 0 || g.behind > 0 {
                            branch_label.push_str(&format!("  ↑{} ↓{}", g.ahead, g.behind));
        }
                        if g.change_count() > 0 {
                            branch_label.push_str(&format!("  ●{}", g.change_count()));
        }
                        ui.label(egui::RichText::new(branch_label).size(11.0).color(w));
    }

                    if !self.status.is_empty() {
                        ui.label(egui::RichText::new("·").size(11.0).color(dim));
                        ui.label(egui::RichText::new(&self.status).size(11.0).color(dim));
    }

                    // Right: file type · Ln/Col
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if let Some(buf) = self.buffers.get(self.active) {
                            let (row, col) = buf.cursor;
                            ui.label(egui::RichText::new(
                                format!("Ln {}, Col {}", row + 1, col + 1))
                                .size(11.0).color(w));
                            if let Some(ext) = buf.path.as_ref()
                                .and_then(|p| p.extension())
                                .and_then(|e| e.to_str())
                            {
                                ui.label(egui::RichText::new("·").size(11.0).color(dim));
                                ui.label(egui::RichText::new(ext.to_uppercase())
                                    .size(11.0).color(dim));
            }
        }
                    });
                });
            });

        // Forward terminal keystrokes to SSH shell when connected.
        if self.ssh_shell.is_some() {
            let inputs: Vec<Vec<u8>> = self.terminal_tabs.get_mut(self.terminal_active)
                .map(|t| t.terminal.pending_input.drain(..).collect())
                .unwrap_or_default();
            if let Some(shell) = &self.ssh_shell {
                for bytes in inputs {
                    let _ = shell.tx.try_send(bytes);
}
            }
        }

        // Feed SSH shell output into the dedicated SSH terminal grid.
        if let Some(shell) = &self.ssh_shell {
            while let Ok(bytes) = shell.rx.try_recv() {
                let s = String::from_utf8_lossy(&bytes).to_string();
                if let Ok(mut g) = self.ssh_term.lock() { g.process(&s); }
                self.ssh_term_last_output = Some(std::time::Instant::now());
            }
        }

        // Panel order controls which side spans the full window height:
        // whichever is declared to egui first (a side panel vs. the
        // terminal's bottom panel) claims full width/height first, and
        // whatever's declared after is inset to what's left. The file tree
        // and agent panel toggle independently — each is declared before
        // the terminal (full height) or after it (inset above the
        // terminal, terminal stays full width on that side), based on its
        // own Settings toggle (Layout section).
        if self.settings.file_tree_full_height  { self.draw_left_panels(ctx); }
        if self.settings.agent_panel_full_height { self.draw_agent_side_panels(ctx); }
        self.draw_terminal_panels(ctx);
        if !self.settings.file_tree_full_height  { self.draw_left_panels(ctx); }
        if !self.settings.agent_panel_full_height { self.draw_agent_side_panels(ctx); }


        // ── Settings overlay ─────────────────────────────────────────────────
        if self.settings_open { self.draw_settings(ctx); }

        // ── SSH quick-pick overlay ────────────────────────────────────────────
        if self.ssh_overlay {
            self.draw_ssh_overlay(ctx);
        }

        // Tab bar — hidden entirely when no files are open
        if !self.buffers.is_empty() {
            egui::TopBottomPanel::top("tabs")
                .exact_height(35.0)
                .show(ctx, |ui| {
                    let rect = ui.max_rect();
                    ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(24, 24, 24));
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        let count = self.buffers.len();
                        let mut close_idx: Option<usize> = None;
                        for i in 0..count {
                            let title    = self.buffers[i].title();
                            let selected = i == self.active;
                            let tab_bg   = if selected { egui::Color32::from_rgb(30, 30, 30) }
                                           else        { egui::Color32::from_rgb(24, 24, 24) };
                            egui::Frame::none()
                                .fill(tab_bg)
                                .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let color = if selected { egui::Color32::WHITE }
                                                    else        { egui::Color32::from_gray(140) };
                                        if ui.label(egui::RichText::new(&title).size(13.0).color(color))
                                            .clicked() { self.active = i; }
                                        let x = egui::RichText::new("×").size(13.0)
                                            .color(egui::Color32::from_gray(100));
                                        let xr = ui.add(egui::Label::new(x).sense(egui::Sense::click()));
                                        if xr.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                                        if xr.clicked() { close_idx = Some(i); }
                                    });
                                });
                            if selected {
                                let r = ui.min_rect();
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(r.left() - 10.0, r.bottom() - 2.0),
                                        egui::pos2(r.right() + 10.0, r.bottom()),
                                    ),
                                    0.0, egui::Color32::from_rgb(0, 120, 212),
                                );
            }
        }
                        if let Some(idx) = close_idx {
                            self.buffers.remove(idx);
                            if self.active >= self.buffers.len() {
                                self.active = self.buffers.len().saturating_sub(1);
            }
        }
                    });
                });
        }

        // Central panel — just the editor (terminal lives in bottom panels above)
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(30, 30, 30));
            let total_w = ui.available_width();

            // Breadcrumb — only when a file is open
            if !self.buffers.is_empty() {
                let bc_h = 22.0;
                ui.allocate_ui_with_layout(
                    egui::vec2(total_w, bc_h),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| self.draw_breadcrumb(ui),
                );
            }

            if self.split.is_some() && !self.buffers.is_empty() {
                let avail = ui.available_rect_before_wrap();
                let half  = avail.width() / 2.0;
                let left  = egui::Rect::from_min_max(
                    avail.min, egui::pos2(avail.left() + half - 1.0, avail.bottom()));
                let right = egui::Rect::from_min_max(
                    egui::pos2(avail.left() + half + 1.0, avail.top()), avail.max);
                let mut lui = ui.new_child(egui::UiBuilder::new()
                    .max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
                self.draw_editor(&mut lui);
                ui.painter().line_segment(
                    [egui::pos2(avail.left() + half, avail.top()),
                     egui::pos2(avail.left() + half, avail.bottom())],
                    egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)));
                let mut rui = ui.new_child(egui::UiBuilder::new()
                    .max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
                self.draw_split_pane(&mut rui);
            } else {
                self.draw_editor(ui);
            }
        });

        // ── Overlays (drawn last so they appear on top) ───────────────────
        self.draw_quick_open(ctx);
        self.draw_cmd_palette(ctx);
        self.draw_goto_line(ctx);
        self.draw_theme_picker(ctx);
        self.draw_task_picker(ctx);
        self.draw_debug_panel(ctx);
        self.draw_update_prompt(ctx);
        self.draw_update_banner(ctx);
        self.draw_onboarding_wizard(ctx);
    }

    /// Native "set up an AI provider" wizard — writes forge-agent's own
    /// `~/.config/forge/config.toml` directly (see `onboarding::add_endpoint`),
    /// so the result is immediately usable by this IDE's agent panel, the
    /// TUI, or a fresh `forge-agent` invocation from anywhere else. No shell
    /// script, no hand-editing TOML.
    fn draw_onboarding_wizard(&mut self, ctx: &egui::Context) {
        use crate::onboarding::{OnboardingStep, LocalForm, KeyForm, LoginEvent};
        let Some(step) = &mut self.onboarding else { return };

        let mut next: Option<OnboardingStep> = None;
        let mut close = false;

        egui::Window::new("Set Up AI Provider")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                match step {
                    OnboardingStep::ProviderPicker => {
                        ui.label("Forge IDE's agent panel needs a model provider configured \
                            before it can do anything. Pick one:");
                        ui.add_space(10.0);
                        if ui.button("Local LLM server (LM Studio, Ollama, llama.cpp, vLLM…)").clicked() {
                            next = Some(OnboardingStep::Local(LocalForm::default()));
                        }
                        if ui.button("Claude (Anthropic API key)").clicked() {
                            next = Some(OnboardingStep::Anthropic(KeyForm {
                                model_id: "claude-sonnet-4-6".into(),
                                max_context_tokens: "200000".into(),
                                ..Default::default()
                            }));
                        }
                        if ui.button("xAI (Grok API key)").clicked() {
                            next = Some(OnboardingStep::DirectApiKey(KeyForm {
                                name: "Grok".into(),
                                base_url: "https://api.x.ai/v1".into(),
                                model_id: "grok-4.5".into(),
                                max_context_tokens: "1000000".into(),
                                ..Default::default()
                            }));
                        }
                        if ui.button("ChatGPT Codex subscription (OAuth)").clicked() {
                            match crate::onboarding::CodexLogin::spawn() {
                                Ok(login) => next = Some(OnboardingStep::Codex {
                                    login, log: Vec::new(), paste_input: String::new(), done: None,
                                }),
                                Err(e) => next = Some(OnboardingStep::Error {
                                    message: format!("Couldn't start login: {e}"),
                                }),
                            }
                        }
                        if ui.button("Direct API key — any OpenAI-compatible endpoint").clicked() {
                            next = Some(OnboardingStep::DirectApiKey(KeyForm {
                                name: "OpenAI".into(),
                                base_url: "https://api.openai.com/v1".into(),
                                model_id: "auto".into(),
                                max_context_tokens: "128000".into(),
                                ..Default::default()
                            }));
                        }
                        ui.label(egui::RichText::new(
                            "Not sure which one applies to you? Use this — it works with \
                             OpenAI, OpenRouter, Groq, Together, or virtually any other \
                             OpenAI-compatible API. Just paste your key.")
                            .size(10.0).color(egui::Color32::from_gray(120)));
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);
                        if ui.link("Skip for now").clicked() {
                            self.settings.onboarding_skipped = true;
                            crate::settings::save(&self.settings);
                            close = true;
                        }
                        ui.label(egui::RichText::new(
                            "You can come back to this from Settings anytime.")
                            .size(10.0).color(egui::Color32::from_gray(120)));
                    }

                    OnboardingStep::Local(form) => {
                        ui.label("Base URL");
                        ui.text_edit_singleline(&mut form.base_url);
                        ui.label("Model ID (\"auto\" probes /v1/models for the loaded model)");
                        ui.text_edit_singleline(&mut form.model_id);
                        ui.label("Max context tokens");
                        ui.text_edit_singleline(&mut form.max_context_tokens);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() { next = Some(OnboardingStep::ProviderPicker); }
                            if ui.button("Save").clicked() {
                                let max_ctx = form.max_context_tokens.trim().parse().unwrap_or(32768);
                                let result = crate::onboarding::add_endpoint(crate::onboarding::NewEndpoint {
                                    name: "local".into(),
                                    base_url: form.base_url.trim().to_string(),
                                    api_key: None,
                                    model_id: form.model_id.trim().to_string(),
                                    max_context_tokens: max_ctx,
                                    endpoint_type: "open_ai",
                                });
                                next = Some(match result {
                                    Ok(()) => OnboardingStep::Done { message: "Local endpoint saved.".into() },
                                    Err(e) => OnboardingStep::Error { message: e },
                                });
                            }
                        });
                    }

                    OnboardingStep::Anthropic(form) => {
                        ui.label("Anthropic API key");
                        ui.add(egui::TextEdit::singleline(&mut form.api_key).password(true));
                        ui.label("Model ID");
                        ui.text_edit_singleline(&mut form.model_id);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() { next = Some(OnboardingStep::ProviderPicker); }
                            let can_save = !form.api_key.trim().is_empty();
                            if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                                let max_ctx = form.max_context_tokens.trim().parse().unwrap_or(200_000);
                                let result = crate::onboarding::add_endpoint(crate::onboarding::NewEndpoint {
                                    name: "claude".into(),
                                    base_url: "https://api.anthropic.com".into(),
                                    api_key: Some(form.api_key.trim().to_string()),
                                    model_id: form.model_id.trim().to_string(),
                                    max_context_tokens: max_ctx,
                                    endpoint_type: "anthropic",
                                });
                                next = Some(match result {
                                    Ok(()) => OnboardingStep::Done { message: "Claude endpoint saved.".into() },
                                    Err(e) => OnboardingStep::Error { message: e },
                                });
                            }
                        });
                    }

                    OnboardingStep::DirectApiKey(form) => {
                        ui.label("Provider name (for your reference)");
                        ui.text_edit_singleline(&mut form.name);
                        ui.label("Base URL");
                        ui.text_edit_singleline(&mut form.base_url);
                        ui.label(egui::RichText::new(
                            "Pre-filled for OpenAI — change it for OpenRouter, Groq, Together, \
                             or any other OpenAI-compatible provider.")
                            .size(10.0).color(egui::Color32::from_gray(120)));
                        ui.label("API key");
                        ui.add(egui::TextEdit::singleline(&mut form.api_key).password(true));
                        ui.label("Model ID (\"auto\" probes /v1/models)");
                        ui.text_edit_singleline(&mut form.model_id);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() { next = Some(OnboardingStep::ProviderPicker); }
                            let can_save = !form.name.trim().is_empty() && !form.base_url.trim().is_empty();
                            if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                                let max_ctx = form.max_context_tokens.trim().parse().unwrap_or(128_000);
                                let result = crate::onboarding::add_endpoint(crate::onboarding::NewEndpoint {
                                    name: form.name.trim().to_string(),
                                    base_url: form.base_url.trim().to_string(),
                                    api_key: Some(form.api_key.trim().to_string()),
                                    model_id: form.model_id.trim().to_string(),
                                    max_context_tokens: max_ctx,
                                    endpoint_type: "open_ai",
                                });
                                next = Some(match result {
                                    Ok(()) => OnboardingStep::Done { message: format!("{} endpoint saved.", form.name) },
                                    Err(e) => OnboardingStep::Error { message: e },
                                });
                            }
                        });
                    }

                    OnboardingStep::Codex { login, log, paste_input, done } => {
                        while let Ok(event) = login.rx.try_recv() {
                            match event { LoginEvent::Line(l) => log.push(l) }
                        }
                        if done.is_none() {
                            if let Some(success) = login.poll_exit() { *done = Some(success); }
                        }
                        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            for line in log.iter() {
                                ui.label(egui::RichText::new(line).monospace().size(11.0));
                            }
                        });
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(
                            "If a browser didn't open, or it can't reach this machine, \
                             paste the redirect URL here:")
                            .size(10.5).color(egui::Color32::from_gray(140)));
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(paste_input);
                            if ui.button("Send").clicked() && !paste_input.trim().is_empty() {
                                login.send_line(paste_input.trim());
                                paste_input.clear();
                            }
                        });
                        ui.add_space(6.0);
                        match done {
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Waiting for login…");
                                });
                            }
                            Some(true) => {
                                next = Some(OnboardingStep::Done {
                                    message: "Signed in — ChatGPT Codex endpoint saved.".into(),
                                });
                            }
                            Some(false) => {
                                next = Some(OnboardingStep::Error {
                                    message: "Login didn't complete. See the log above.".into(),
                                });
                            }
                        }
                        if ui.button("Cancel").clicked() { close = true; }
                    }

                    OnboardingStep::Done { message } => {
                        ui.label(message.as_str());
                        ui.add_space(8.0);
                        if ui.button("Close").clicked() { close = true; }
                    }

                    OnboardingStep::Error { message } => {
                        ui.label(egui::RichText::new(message.as_str())
                            .color(egui::Color32::from_rgb(230, 120, 110)));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() { next = Some(OnboardingStep::ProviderPicker); }
                            if ui.button("Close").clicked() { close = true; }
                        });
                    }
                }
            });

        if let Some(step) = next { self.onboarding = Some(step); }
        if close { self.onboarding = None; }
    }

    /// One-time "want update checks?" prompt, shown until answered once.
    fn draw_update_prompt(&mut self, ctx: &egui::Context) {
        if !self.show_update_prompt { return; }
        let mut answered = false;
        let mut enable = false;
        egui::Window::new("Check for Updates?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.label("Forge IDE can check GitHub for a newer release on startup. \
                    This is the only network call Forge IDE itself makes — nothing else \
                    is sent, and it's a plain read of the public releases page.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Enable").clicked() { enable = true; answered = true; }
                    if ui.button("Not now").clicked() { answered = true; }
                });
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Change anytime in Settings.")
                    .size(10.0).color(egui::Color32::from_gray(120)));
            });
        if answered {
            self.settings.check_for_updates = enable;
            self.settings.update_check_prompted = true;
            crate::settings::save(&self.settings);
            self.show_update_prompt = false;
            if enable {
                self.update_check_rx = Some(crate::update_check::spawn_check());
            }
        }
    }

    /// Dismissible banner once a newer release is found. Purely informational
    /// — no auto-download, just a link to the release page.
    fn draw_update_banner(&mut self, ctx: &egui::Context) {
        let Some(update) = &self.update_available else { return };
        if self.update_banner_dismissed { return; }
        let (version, url) = (update.latest_version.clone(), update.url.clone());
        let mut dismissed = false;
        egui::TopBottomPanel::top("update_banner").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(format!("Forge IDE {version} is available"))
                    .color(egui::Color32::from_gray(220)));
                if ui.button("View Release").clicked() {
                    let _ = std::process::Command::new("open").arg(&url).spawn();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    if ui.button("✕").clicked() { dismissed = true; }
                });
            });
        });
        if dismissed { self.update_banner_dismissed = true; }
    }

    // ── Debug panel (visible while a DAP session is active) ───────────
    fn draw_debug_panel(&mut self, ctx: &egui::Context) {
        if self.dap.is_none() { return; }
        let mut jump: Option<(PathBuf, usize)> = None;
        let mut frame_sel: Option<i64> = None;
        egui::Window::new("Debug")
            .anchor(egui::Align2::RIGHT_TOP, [-16.0, 48.0])
            .default_width(300.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("▶").on_hover_text("Continue (F5)").clicked() {
                        if let Some(d) = &mut self.dap { d.continue_run(); }
                        self.dap_stopped = None;
                    }
                    if ui.button("⤵").on_hover_text("Step Over (F10)").clicked() {
                        if let Some(d) = &mut self.dap { d.step_over(); }
                    }
                    if ui.button("⬇").on_hover_text("Step Into (F11)").clicked() {
                        if let Some(d) = &mut self.dap { d.step_in(); }
                    }
                    if ui.button("⬆").on_hover_text("Step Out (Shift+F11)").clicked() {
                        if let Some(d) = &mut self.dap { d.step_out(); }
                    }
                    if ui.button("⏸").on_hover_text("Pause").clicked() {
                        if let Some(d) = &mut self.dap { d.pause(); }
                    }
                    if ui.button("⏹").on_hover_text("Stop (Shift+F5)").clicked() {
                        self.dap = None;
                        self.dap_stopped = None;
                        self.dap_stack.clear();
                        self.dap_vars.clear();
                    }
                });
                if self.dap.is_none() { return; }
                ui.separator();
                ui.label(egui::RichText::new("CALL STACK").size(10.0).weak());
                egui::ScrollArea::vertical().id_salt("dap_stack").max_height(160.0)
                    .show(ui, |ui| {
                        for f in &self.dap_stack {
                            let loc = f.path.as_ref()
                                .and_then(|p| p.file_name()).and_then(|n| n.to_str())
                                .map(|n| format!("  {n}:{}", f.line)).unwrap_or_default();
                            let resp = ui.selectable_label(false,
                                egui::RichText::new(format!("{}{loc}", f.name)).size(11.5));
                            if resp.clicked() {
                                if let Some(p) = &f.path {
                                    jump = Some((p.clone(), f.line.saturating_sub(1)));
                                }
                                frame_sel = Some(f.id);
                            }
                        }
                        if self.dap_stack.is_empty() {
                            ui.label(egui::RichText::new("(running)").size(11.0).weak());
                        }
                    });
                ui.separator();
                ui.label(egui::RichText::new("VARIABLES").size(10.0).weak());
                egui::ScrollArea::vertical().id_salt("dap_vars").max_height(160.0)
                    .show(ui, |ui| {
                        for v in &self.dap_vars {
                            ui.label(egui::RichText::new(
                                format!("{} = {}", v.name, v.value))
                                .monospace().size(11.0));
                        }
                        if self.dap_vars.is_empty() {
                            ui.label(egui::RichText::new("(none)").size(11.0).weak());
                        }
                    });
            });
        if let Some(fid) = frame_sel {
            if let Some(d) = &mut self.dap { d.scopes(fid); }
        }
        if let Some((path, line)) = jump {
            if path.exists() {
                self.open_file(path.clone());
                self.pending_scroll = Some(line);
                if let Some(buf) = self.buffers.get_mut(self.active) {
                    buf.cursor = (line, 0);
                }
            }
        }
    }

    // ── Split editor: right pane with its own tab bar ─────────────────
    fn draw_split_pane(&mut self, ui: &mut egui::Ui) {
        let Some(mut idx) = self.split else { return };
        if self.buffers.is_empty() { self.split = None; return; }
        idx = idx.min(self.buffers.len() - 1);

        // Tab bar for the split pane
        let mut close_split = false;
        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal()
                .id_salt("split_tabs")
                .max_width(ui.available_width() - 28.0)
                .show(ui, |ui| {
                    for (i, b) in self.buffers.iter().enumerate() {
                        let name = b.path.as_ref()
                            .and_then(|p| p.file_name()).and_then(|n| n.to_str())
                            .unwrap_or("untitled");
                        let label = if b.modified { format!("● {name}") } else { name.to_string() };
                        if ui.selectable_label(i == idx, label).clicked() { idx = i; }
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close split").clicked() {
                    close_split = true;
                }
            });
        });
        if close_split { self.split = None; return; }
        self.split = Some(idx);

        let font_id = egui::FontId::monospace(self.settings.font_size);
        let pal = self.palette.clone();
        let ext = self.buffers[idx].path.as_ref()
            .and_then(|p| p.extension()).and_then(|e| e.to_str())
            .unwrap_or("").to_string();
        let font_size = self.settings.font_size;
        let word_wrap = self.settings.word_wrap;
        let editor_bg = self.palette.editor_bg_c();

        let buf = &mut self.buffers[idx];
        let mut text = buf.text();
        let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
            let eff_wrap = if word_wrap { wrap_width } else { f32::INFINITY };
            let hit = self.syntax_cache_split.as_ref().is_some_and(|c| {
                c.text == s && c.ext == ext && c.font_size == font_size
                    && c.palette_name == pal.name && c.wrap_width == eff_wrap
            });
            if hit {
                return self.syntax_cache_split.as_ref().unwrap().galley.clone();
            }
            let mut job = syntax_highlight(s, &ext, &[], None, font_size, &pal);
            job.wrap.max_width = eff_wrap;
            let galley = ui.fonts(|f| f.layout_job(job));
            self.syntax_cache_split = Some(SyntaxCache {
                text: s.to_string(), ext: ext.clone(), font_size,
                match_ranges: Vec::new(), current_match: None,
                palette_name: pal.name.clone(), wrap_width: eff_wrap,
                galley: galley.clone(),
            });
            galley
        };
        ui.visuals_mut().extreme_bg_color = editor_bg;
        let scroll = if word_wrap { egui::ScrollArea::vertical() }
                     else          { egui::ScrollArea::both() };
        let scroll_key = buf.path.as_ref().map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("untitled-{idx}"));
        scroll.id_salt(("split_editor_scroll", scroll_key))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let out = egui::TextEdit::multiline(&mut text)
                    .font(font_id)
                    .desired_rows(30)
                    .desired_width(if word_wrap { ui.available_width() } else { f32::INFINITY })
                    .frame(false)
                    .layouter(&mut layouter)
                    .show(ui);
                if out.response.changed() {
                    buf.lines = text.lines().map(String::from).collect();
                    if buf.lines.is_empty() { buf.lines.push(String::new()); }
                    buf.modified = true;
                }
            });
    }

    // ── Task picker overlay (Ctrl+Shift+B) ─────────────────────────────
    fn draw_task_picker(&mut self, ctx: &egui::Context) {
        let Some(mut sel) = self.task_picker else { return };
        let tasks = if self.has_folder { crate::tasks::load(&self.cwd) } else { Vec::new() };
        let mut close = false;
        let mut chosen: Option<usize> = None;
        if !tasks.is_empty() {
            sel = sel.min(tasks.len() - 1);
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) { sel = (sel + 1).min(tasks.len() - 1); }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp))   { sel = sel.saturating_sub(1); }
            if ctx.input(|i| i.key_pressed(egui::Key::Enter))     { chosen = Some(sel); }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
        egui::Window::new("task_picker")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .fixed_size([380.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Run Task").size(11.0).weak());
                ui.add_space(2.0);
                if tasks.is_empty() {
                    ui.label(egui::RichText::new(
                        "No tasks found — define them in .forge/tasks.toml:\n\n[tasks]\nbuild = \"cargo build\"")
                        .monospace().size(11.5).weak());
                }
                for (i, t) in tasks.iter().enumerate() {
                    let label = format!("{}  —  {}", t.name, t.cmd);
                    if ui.selectable_label(i == sel, label).clicked() { chosen = Some(i); }
                }
            });
        if let Some(i) = chosen {
            let t = &tasks[i];
            self.task_rx    = Some(crate::tasks::run(t, &self.cwd));
            self.show_term  = true;
            self.bottom_tab = BottomTab::Output;
            self.status     = format!("Running task: {}", t.name);
            close = true;
        }
        self.task_picker = if close { None } else { Some(sel) };
    }

    // ── Go to Line overlay (Ctrl+G) ───────────────────────────────────
    fn draw_goto_line(&mut self, ctx: &egui::Context) {
        let Some(input) = &mut self.goto_line else { return };
        let mut close = false;
        let mut goto: Option<usize> = None;
        let n_lines = self.buffers.get(self.active).map(|b| b.lines.len()).unwrap_or(0);
        egui::Window::new("goto_line")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .fixed_size([320.0, 0.0])
            .show(ctx, |ui| {
                let resp = ui.add(egui::TextEdit::singleline(input)
                    .hint_text(format!("Go to line (1–{n_lines})"))
                    .desired_width(f32::INFINITY));
                resp.request_focus();
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(n) = input.trim().parse::<usize>() {
                        if n >= 1 { goto = Some(n - 1); }
                    }
                    close = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
            });
        if let Some(line) = goto {
            let line = line.min(n_lines.saturating_sub(1));
            self.pending_scroll = Some(line);
            if let Some(buf) = self.buffers.get_mut(self.active) {
                buf.cursor = (line, 0);
            }
        }
        if close { self.goto_line = None; }
    }

    // ── Theme picker overlay (Ctrl+K T) ───────────────────────────────
    fn draw_theme_picker(&mut self, ctx: &egui::Context) {
        let Some(mut sel) = self.theme_picker else { return };
        let names = crate::theme::theme_names();
        if names.is_empty() { self.theme_picker = None; return; }
        let mut close  = false;
        let mut chosen: Option<usize> = None;
        sel = sel.min(names.len() - 1);
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) { sel = (sel + 1).min(names.len() - 1); }
        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp))   { sel = sel.saturating_sub(1); }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter))     { chosen = Some(sel); }
        let mut cancel = false;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape))    { close = true; cancel = true; }
        egui::Window::new("theme_picker")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .fixed_size([320.0, 0.0])
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Select Color Theme").size(11.0).weak());
                ui.add_space(2.0);
                for (i, name) in names.iter().enumerate() {
                    let active = i == sel;
                    let resp = ui.selectable_label(active, name);
                    if resp.clicked() { chosen = Some(i); }
                }
            });
        // Live preview: highlighted theme applies immediately.
        if names[sel] != self.settings.theme {
            self.settings.theme = names[sel].clone();
        }
        if let Some(i) = chosen {
            self.settings.theme = names[i].clone();
            crate::settings::save(&self.settings);
            close = true;
        }
        if cancel {
            if let Some(prev) = self.theme_prev.take() {
                self.settings.theme = prev;
            }
        } else if close {
            self.theme_prev = None;
        }
        self.theme_picker = if close { None } else { Some(sel) };
    }

    fn draw_breadcrumb(&self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(25, 25, 25));
        ui.add_space(4.0);
        if let Some(buf) = self.buffers.get(self.active) {
            if let Some(path) = &buf.path {
                let rel = path.strip_prefix(&self.cwd).unwrap_or(path);
                let parts: Vec<_> = rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect();
                for (i, part) in parts.iter().enumerate() {
                    let is_last = i == parts.len() - 1;
                    let color = if is_last { egui::Color32::from_gray(210) }
                                else       { egui::Color32::from_gray(130) };
                    ui.label(egui::RichText::new(part).size(12.0).color(color));
                    if !is_last {
                        ui.label(egui::RichText::new(" › ").size(12.0)
                            .color(egui::Color32::from_gray(80)));
    }
}
            }
        }
    }

    fn draw_agent_view(&mut self, ui: &mut egui::Ui) {
        // Ensure at least one tab exists
        if self.agent_tabs.is_empty() {
            self.agent_tabs.push(AgentTab::new(&self.cwd, self.settings.default_agent_permission_mode));
            self.agent_active = 0;
        }

        // ── Header: title + history + new-tab ("+") ──────────────────────────
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("FORGE AGENT").size(10.5)
                .color(egui::Color32::from_gray(200)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                // Clock = history
                let hist_active = self.agent_show_list;
                let (hr, hr_resp) = ui.allocate_exact_size(egui::vec2(20.0,20.0), egui::Sense::click());
                if hr_resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                if hist_active { ui.painter().rect_filled(hr, 3.0, egui::Color32::from_rgb(14,99,156)); }
                let p = ui.painter(); let c = hr.center(); let r = 6.0;
                let col = if hist_active { egui::Color32::WHITE } else { egui::Color32::from_gray(170) };
                p.circle_stroke(c, r, egui::Stroke::new(1.3_f32, col));
                p.line_segment([c, c + egui::vec2(0.0, -r+1.0)], egui::Stroke::new(1.3_f32, col));
                p.line_segment([c, c + egui::vec2(3.0, 2.0)],    egui::Stroke::new(1.3_f32, col));
                let _ = hr_resp.clone().on_hover_text("Conversation history");
                if hr_resp.clicked() {
                    self.agent_show_list = !self.agent_show_list;
                    if self.agent_show_list { self.agent_saved = crate::agent_panel::load_conversations(&self.cwd); }
                }
                ui.add_space(4.0);
                // "+" = new tab
                let (nr, nr_resp) = ui.allocate_exact_size(egui::vec2(20.0,20.0), egui::Sense::click());
                if nr_resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                let col2 = if nr_resp.hovered() { egui::Color32::WHITE } else { egui::Color32::from_gray(170) };
                let s2 = egui::Stroke::new(1.5_f32, col2);
                let c2 = nr.center();
                ui.painter().line_segment([c2 - egui::vec2(5.0,0.0), c2 + egui::vec2(5.0,0.0)], s2);
                ui.painter().line_segment([c2 - egui::vec2(0.0,5.0), c2 + egui::vec2(0.0,5.0)], s2);
                let _ = nr_resp.clone().on_hover_text("New conversation tab");
                if nr_resp.clicked() {
                    // Save current tab's conversation before adding new
                    if let Some(tab) = self.agent_tabs.get(self.agent_active) {
                        if !tab.session.items.is_empty() {
                            crate::agent_panel::save_conversation(&tab.session, &tab.conv_id, &self.cwd);
                        }
                    }
                    // Only one untouched "new" conversation should exist at a
                    // time — reuse it instead of spawning a duplicate blank
                    // tab (and process) every time "+" is clicked.
                    if let Some(idx) = self.agent_tabs.iter().position(|t| t.session.is_unused()) {
                        self.agent_active = idx;
                    } else {
                        self.agent_tabs.push(AgentTab::new(&self.cwd, self.settings.default_agent_permission_mode));
                        self.agent_active = self.agent_tabs.len() - 1;
                    }
                    self.agent_show_list = false;
                }
            });
        });
        ui.add_space(4.0);
        ui.separator();

        // ── History list view ─────────────────────────────────────────────────
        if self.agent_show_list {
            let convs = self.agent_saved.clone();
            if convs.is_empty() {
                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(egui::RichText::new("No saved conversations yet.")
                        .size(12.0).color(egui::Color32::from_gray(100)));
                });
                return;
            }
            let mut delete_id: Option<String> = None;
            egui::ScrollArea::vertical()
                .id_salt("conv_list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for conv in &convs {
                        let avail = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail, 52.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 0.0,
                                egui::Color32::from_rgba_premultiplied(255,255,255,8));
                        }
                        ui.painter().text(
                            egui::pos2(rect.left()+12.0, rect.center().y - 8.0),
                            egui::Align2::LEFT_CENTER, &conv.title,
                            egui::FontId::proportional(12.5),
                            egui::Color32::from_gray(220));
                        ui.painter().text(
                            egui::pos2(rect.left()+12.0, rect.center().y + 9.0),
                            egui::Align2::LEFT_CENTER, &conv.model,
                            egui::FontId::proportional(10.5),
                            egui::Color32::from_gray(110));
                        // Delete button (trash X on right)
                        let del_rect = egui::Rect::from_min_size(
                            egui::pos2(rect.right()-32.0, rect.top()+14.0),
                            egui::vec2(24.0, 24.0));
                        let del_id   = ui.id().with(("del", conv.id.as_str()));
                        let del_resp = ui.interact(del_rect, del_id, egui::Sense::click());
                        if del_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(del_rect, 3.0,
                                egui::Color32::from_rgba_premultiplied(255,60,60,30));
                        }
                        let ds = egui::Stroke::new(1.3_f32, egui::Color32::from_gray(if del_resp.hovered() { 220 } else { 130 }));
                        let dc = del_rect.center();
                        ui.painter().line_segment([dc - egui::vec2(4.0,4.0), dc + egui::vec2(4.0,4.0)], ds);
                        ui.painter().line_segment([dc + egui::vec2(-4.0,4.0), dc + egui::vec2(4.0,-4.0)], ds);
                        let _ = del_resp.clone().on_hover_text("Delete conversation");
                        if del_resp.clicked() { delete_id = Some(conv.id.clone()); }

                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(rect.left(), rect.bottom()-1.0),
                                rect.right_bottom()),
                            0.0, egui::Color32::from_gray(45));
                        // Open in new tab on click (not on delete)
                        if resp.clicked() && !del_resp.clicked() {
                            // Already open in a tab? Switch to it instead of
                            // opening a duplicate copy of the same conversation.
                            if let Some(idx) = self.agent_tabs.iter()
                                .position(|t| t.conv_id == conv.id)
                            {
                                self.agent_active = idx;
                            } else {
                                let new_tab = AgentTab::reopen(&self.cwd, self.settings.default_agent_permission_mode, conv);
                                self.agent_tabs.push(new_tab);
                                self.agent_active = self.agent_tabs.len() - 1;
                            }
                            self.agent_show_list = false;
                        }
                    }
                });
            if let Some(id) = delete_id {
                let dir = dirs::config_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("forge-ide").join("conversations");
                let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
                self.agent_saved = crate::agent_panel::load_conversations(&self.cwd);
            }
            return;
        }

        // ── Tab bar ───────────────────────────────────────────────────────────
        if self.agent_tabs.len() > 1 {
            let tab_h   = 26.0;
            let tab_bg  = egui::Color32::from_rgb(30, 30, 30);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), tab_h)),
                0.0, tab_bg);
            let mut close_idx: Option<usize> = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.set_height(tab_h);
                let n = self.agent_tabs.len();
                for i in 0..n {
                    let title = self.agent_tabs[i].title();
                    let active = i == self.agent_active;
                    let tab_w  = ((ui.available_width() - 24.0) / n as f32).min(130.0).max(60.0);
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(tab_w, tab_h), egui::Sense::click());
                    if active {
                        ui.painter().rect_filled(rect, 0.0, egui::Color32::from_rgb(37,37,38));
                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(rect.left(), rect.top()),
                                egui::pos2(rect.right(), rect.top()+2.0)),
                            0.0, egui::Color32::from_rgb(0,120,212));
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, 0.0,
                            egui::Color32::from_rgba_premultiplied(255,255,255,8));
                    }
                    // Title (leave room for × button)
                    ui.painter().text(
                        egui::pos2(rect.left()+8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER, &title,
                        egui::FontId::proportional(11.0),
                        if active { egui::Color32::from_gray(220) } else { egui::Color32::from_gray(140) });
                    // × close button
                    let xr = egui::Rect::from_min_size(
                        egui::pos2(rect.right()-18.0, rect.center().y-8.0),
                        egui::vec2(16.0,16.0));
                    let xi = ui.id().with(("tab_close", i));
                    let xr2 = ui.interact(xr, xi, egui::Sense::click());
                    if xr2.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                    let xc = egui::Color32::from_gray(if xr2.hovered() { 220 } else { 100 });
                    let xs = egui::Stroke::new(1.2_f32, xc);
                    let cc = xr.center();
                    ui.painter().line_segment([cc-egui::vec2(4.0,4.0), cc+egui::vec2(4.0,4.0)], xs);
                    ui.painter().line_segment([cc+egui::vec2(-4.0,4.0), cc+egui::vec2(4.0,-4.0)], xs);
                    if xr2.clicked() { close_idx = Some(i); }
                    else if resp.clicked() { self.agent_active = i; }
                    if resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                }
            });
            ui.separator();
            if let Some(ci) = close_idx {
                // Save before closing
                let tab = &self.agent_tabs[ci];
                if !tab.session.items.is_empty() {
                    crate::agent_panel::save_conversation(&tab.session, &tab.conv_id, &self.cwd);
                }
                self.agent_tabs.remove(ci);
                if self.agent_tabs.is_empty() {
                    self.agent_tabs.push(AgentTab::new(&self.cwd, self.settings.default_agent_permission_mode));
                }
                self.agent_active = self.agent_active.min(self.agent_tabs.len() - 1);
            }
        }

        // ── Active tab content ────────────────────────────────────────────────
        self.agent_active = self.agent_active.min(self.agent_tabs.len().saturating_sub(1));
        let tab = &mut self.agent_tabs[self.agent_active];
        // Built from individual fields (not a `tab` method) so the borrow
        // checker sees this only touches `password_*` — disjoint from the
        // `&mut tab.session` the poll call itself needs.
        let policy = crate::agent_panel::SessionPolicy {
            password_auto_inject: tab.password_auto_inject,
            session_password: tab.session_password.as_deref(),
        };
        tab.session.poll(policy);

        // A resumed session whose log was missing/empty (see
        // `needs_resume_fallback`'s doc comment) reports an error and exits
        // immediately, with no `init` ever coming — respawn fresh in place
        // so the tab keeps working, instead of leaving it permanently dead.
        if std::mem::take(&mut tab.session.needs_resume_fallback) {
            let items = std::mem::take(&mut tab.session.items);
            tab.session = crate::agent_panel::AgentSession::spawn(&self.cwd, tab.permission_mode, None);
            tab.session.items = items;
        }

        // Esc interrupts an in-flight turn — there was previously no way to
        // stop a run mid-flight at all short of killing the whole tab.
        // Guarded on no picker dropdown being open so Esc's other job
        // (dismissing those) doesn't also cancel the run in the same frame.
        if tab.session.is_active()
            && !self.agent_model_picker_open && !self.agent_perm_picker_open
            && !self.agent_thinking_picker_open && !self.agent_context_picker_open
            && ui.input(|i| i.key_pressed(egui::Key::Escape))
        {
            tab.session.cancel_run();
        }

        if let Some(err) = &tab.session.spawn_err.clone() {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(format!("  {err}"))
                    .color(egui::Color32::from_rgb(255, 130, 100)));
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Make sure `forge-agent` is on your PATH.")
                    .size(11.0).color(egui::Color32::from_gray(140)));
            });
            return;
        }

        // Status row — rendered as a distinct toolbar strip (background fill +
        // bottom border), matching the tab-bar convention above, so it reads as
        // a persistent bar rather than blending into the chat scroll area.
        let status_bar_h = 24.0;
        // The strip is painted after the row, once its real height is known:
        // the badges wrap onto a second line in a narrow panel, and a fill of
        // a fixed 24pt would then cover only the first of them. The shape is
        // reserved here so it still ends up *behind* what wraps over it.
        let status_bar_bg = ui.painter().add(egui::Shape::Noop);
        let status_bar_top = ui.cursor().min.y;
        let mut model_badge_rect     = egui::Rect::NOTHING;
        let mut perm_badge_rect      = egui::Rect::NOTHING;
        let mut reasoning_badge_rect = egui::Rect::NOTHING;
        let mut context_badge_rect   = egui::Rect::NOTHING;
        let mut toggle_offline_mode  = false;
        // Wrapped, so a narrow panel reflows the badges instead of running them
        // off the edge — and, since egui sizes a panel from its contents, so
        // that this row cannot hold the panel open at its own width.
        let status_bar = ui.horizontal_wrapped(|ui| {
            ui.set_min_height(status_bar_h);
            ui.add_space(4.0);
            let model = if tab.session.model.is_empty() { "starting…".to_string() }
                        else { display_model_label(&tab.session.model, &tab.session.endpoints) };
            let has_choices = !tab.session.endpoints.is_empty();
            if has_choices {
                let badge = draw_status_badge(ui, "model_badge", &model,
                    egui::Color32::from_gray(140), self.agent_model_picker_open, status_bar_h);
                model_badge_rect = badge.rect;
                if badge.clicked() {
                    self.agent_model_picker_open = !self.agent_model_picker_open;
                    self.agent_model_picker_frame = 0;
                }
            } else {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&model).size(10.5).color(egui::Color32::from_gray(140)));
                ui.add_space(6.0);
            }
            ui.label(egui::RichText::new("·").size(10.5).color(egui::Color32::from_gray(80)));
            let perm_color = match tab.permission_mode {
                crate::settings::AgentPermissionMode::AlwaysAsk          => egui::Color32::from_gray(170),
                crate::settings::AgentPermissionMode::AutoApprove        => egui::Color32::from_rgb(220, 190, 120),
                crate::settings::AgentPermissionMode::DangerouslySkipAll => egui::Color32::from_rgb(230, 110, 100),
            };
            let perm_badge = draw_status_badge(ui, "perm_badge", tab.permission_mode.label(),
                perm_color, self.agent_perm_picker_open, status_bar_h);
            perm_badge_rect = perm_badge.rect;
            if perm_badge.clicked() {
                self.agent_perm_picker_open = !self.agent_perm_picker_open;
                self.agent_perm_picker_frame = 0;
            }
            // Reasoning/thinking-effort control — only meaningful once we
            // know which endpoint is current (need its `reasoning` object).
            let current_ep = tab.session.endpoints.iter()
                .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(tab.session.model.as_str()))
                .cloned();
            if let Some(ep) = &current_ep {
                ui.label(egui::RichText::new("·").size(10.5).color(egui::Color32::from_gray(80)));
                let reasoning_badge = draw_status_badge(ui, "reasoning_badge", &reasoning_badge_label(ep),
                    egui::Color32::from_gray(140), self.agent_thinking_picker_open, status_bar_h);
                reasoning_badge_rect = reasoning_badge.rect;
                if reasoning_badge.clicked() {
                    self.agent_thinking_picker_open = !self.agent_thinking_picker_open;
                    self.agent_thinking_picker_frame = 0;
                }
            }
            if tab.session.thinking {
                ui.label(egui::RichText::new("· thinking").size(10.5)
                    .color(egui::Color32::from_rgb(180, 200, 120)));
            }
            let usage = &tab.session.usage;
            if usage.max_context_tokens > 0 {
                let pct = (usage.last_prompt_tokens as f64 / usage.max_context_tokens as f64 * 100.0)
                    .round() as i64;
                ui.label(egui::RichText::new(format!("· {pct}% ctx")).size(10.5)
                    .color(egui::Color32::from_gray(120)))
                    .on_hover_text(format!("{} / {} tokens (last request)",
                        usage.last_prompt_tokens, usage.max_context_tokens));
            }
            if !tab.session.context_strategy.is_empty() {
                ui.label(egui::RichText::new("·").size(10.5).color(egui::Color32::from_gray(80)));
                let label = context_strategy_label(&tab.session.context_strategy);
                let context_badge = draw_status_badge(ui, "context_strategy_badge", label,
                    egui::Color32::from_gray(140), self.agent_context_picker_open, status_bar_h);
                context_badge_rect = context_badge.rect;
                if context_badge.clicked() {
                    self.agent_context_picker_open = !self.agent_context_picker_open;
                    self.agent_context_picker_frame = 0;
                }
            }
            {
                ui.label(egui::RichText::new("·").size(10.5).color(egui::Color32::from_gray(80)));
                let (label, color, hover) = if tab.session.offline_mode {
                    ("Offline", egui::Color32::from_rgb(140, 180, 140),
                     "No network calls except this session's own model API — click to disable")
                } else {
                    ("Online", egui::Color32::from_gray(110),
                     "web_search/web_fetch and Codex background checks run normally — click to go offline")
                };
                let badge = ui.label(egui::RichText::new(label).size(10.5).color(color))
                    .on_hover_text(hover)
                    .interact(egui::Sense::click());
                if badge.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                if badge.clicked() { toggle_offline_mode = true; }
            }
        }).response.rect;
        ui.painter().set(
            status_bar_bg,
            egui::Shape::rect_filled(
                egui::Rect::from_x_y_ranges(
                    ui.max_rect().x_range(),
                    status_bar_top..=status_bar.bottom().max(status_bar_top + status_bar_h),
                ),
                0.0,
                egui::Color32::from_rgb(30, 30, 30),
            ),
        );
        if toggle_offline_mode {
            let next = !tab.session.offline_mode;
            tab.session.update_offline_mode(next);
        }
        ui.separator();
        ui.add_space(2.0);

        // Model picker dropdown
        if self.agent_model_picker_open {
            let endpoints = tab.session.endpoints.clone();
            let current   = tab.session.model.clone();
            let mut chosen: Option<serde_json::Value> = None;
            let mut dismissed = false;
            let response = egui::Area::new(egui::Id::new("agent_model_picker"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(model_badge_rect.left(), model_badge_rect.bottom() + 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(37, 37, 38))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(6.0, 8.0))
                        .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 4.0), blur: 16.0,
                            spread: 0.0, color: egui::Color32::from_black_alpha(90) })
                        .show(ui, |ui| {
                        ui.set_width(220.0);
                        const GROUP_ORDER: &[&str] =
                            &["ChatGPT", "Anthropic", "OpenAI", "xAI", "OpenRouter", "Local / Custom"];
                        let mut first = true;
                        for group in GROUP_ORDER {
                            let mut in_group: Vec<&serde_json::Value> = endpoints.iter()
                                .filter(|ep| classify_provider(ep) == *group)
                                .collect();
                            in_group.sort_by(|a, b| {
                                let la = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                let lb = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                natural_cmp(&display_model_label(la, &endpoints), &display_model_label(lb, &endpoints))
                            });
                            if in_group.is_empty() { continue; }
                            if !first {
                                ui.add_space(6.0);
                                ui.separator();
                                ui.add_space(2.0);
                            }
                            first = false;
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                ui.add_space(6.0);
                                ui.label(egui::RichText::new(group.to_uppercase()).size(9.5)
                                    .color(egui::Color32::from_gray(130)));
                            });
                            ui.add_space(2.0);
                            for ep in in_group {
                                let name = ep.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                let is_current = name == current;
                                let label = display_model_label(name, &endpoints);
                                let ctx_tokens = ep.get("max_context_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                if draw_model_row(ui, &label, ctx_tokens, is_current).clicked() {
                                    if !is_current { chosen = Some(ep.clone()); }
                                    dismissed = true;
                                }
                            }
                        }
                    });
                });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
            // Skip frame 0 so the click that opened the picker isn't immediately
            // detected as a click-outside (same fix as the SSH overlay).
            self.agent_model_picker_frame = self.agent_model_picker_frame.saturating_add(1);
            if self.agent_model_picker_frame > 1 {
                let primary_click = ui.input(|i| i.pointer.primary_clicked());
                let interact_pos  = ui.input(|i| i.pointer.interact_pos());
                if primary_click {
                    if let Some(pos) = interact_pos {
                        if !response.response.rect.contains(pos) { dismissed = true; }
                    }
                }
            }
            if let Some(ep) = chosen { self.agent_pending_switch = Some(ep); }
            if dismissed { self.agent_model_picker_open = false; }
        }

        // Permission-mode picker
        if self.agent_perm_picker_open {
            use crate::settings::AgentPermissionMode as Mode;
            let current = tab.permission_mode;
            let mut chosen: Option<Mode> = None;
            let mut dismissed = false;
            let response = egui::Area::new(egui::Id::new("agent_perm_picker"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(perm_badge_rect.left(), perm_badge_rect.bottom() + 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(37, 37, 38))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 10.0))
                        .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 4.0), blur: 16.0,
                            spread: 0.0, color: egui::Color32::from_black_alpha(90) })
                        .show(ui, |ui| {
                            ui.set_width(300.0);
                            ui.label(egui::RichText::new(
                                "The agent can always read files, list directories, and search \
                                 code. These tiers only control writes and shell commands.")
                                .size(9.5).color(egui::Color32::from_gray(135)));
                            ui.add_space(8.0);
                            ui.separator();
                            ui.add_space(6.0);
                            for (i, mode) in [Mode::AlwaysAsk, Mode::AutoApprove, Mode::DangerouslySkipAll]
                                .into_iter().enumerate()
                            {
                                let is_current = mode == current;
                                let color = match mode {
                                    Mode::AlwaysAsk          => egui::Color32::from_gray(210),
                                    Mode::AutoApprove        => egui::Color32::from_rgb(220, 190, 120),
                                    Mode::DangerouslySkipAll => egui::Color32::from_rgb(230, 110, 100),
                                };
                                if draw_perm_mode_row(ui, i, mode.label(), mode.description(), color, is_current)
                                    .clicked() && !is_current
                                {
                                    chosen = Some(mode);
                                    dismissed = true;
                                }
                                if i < 2 { ui.add_space(3.0); }
                            }
                        });
                });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
            self.agent_perm_picker_frame = self.agent_perm_picker_frame.saturating_add(1);
            if self.agent_perm_picker_frame > 1 {
                let primary_click = ui.input(|i| i.pointer.primary_clicked());
                let interact_pos  = ui.input(|i| i.pointer.interact_pos());
                if primary_click {
                    if let Some(pos) = interact_pos {
                        if !response.response.rect.contains(pos) { dismissed = true; }
                    }
                }
            }
            if let Some(mode) = chosen {
                let needs_confirm = current == Mode::DangerouslySkipAll || mode == Mode::DangerouslySkipAll;
                if needs_confirm {
                    self.agent_pending_perm_mode = Some(mode);
                } else if !tab.session.is_active() {
                    let cwd = self.cwd.clone();
                    tab.set_permission_mode(mode, &cwd);
                }
            }
            if dismissed { self.agent_perm_picker_open = false; }
        }

        // Permission-mode change confirmation strip — only shown for
        // transitions into/out of DangerouslySkipAll, since those respawn
        // the tab's subprocess (dropping anything in flight).
        if let Some(mode) = self.agent_pending_perm_mode {
            let busy = tab.session.is_active();
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                let text = if busy {
                    format!("Finish the current turn before switching to {} — it restarts this tab's session.", mode.label())
                } else {
                    format!("Switch to {}? This restarts this tab's session (conversation is kept, in-progress tool state is not).", mode.label())
                };
                ui.label(egui::RichText::new(text).size(11.0).color(egui::Color32::from_rgb(220, 190, 120)));
            });
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button("Confirm").clicked() {
                        let cwd = self.cwd.clone();
                        tab.set_permission_mode(mode, &cwd);
                        self.agent_pending_perm_mode = None;
                    }
                });
                if ui.button("Cancel").clicked() {
                    self.agent_pending_perm_mode = None;
                }
            });
            ui.add_space(4.0);
        }

        // Reasoning/thinking-effort picker — content depends on the current
        // endpoint's provider (ChatGPT Codex gets an effort ladder; Anthropic
        // and generic OpenAI-compatible endpoints get an on/off/default
        // toggle, Anthropic additionally getting a thinking-budget field).
        if self.agent_thinking_picker_open {
            let current_ep = tab.session.endpoints.iter()
                .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(tab.session.model.as_str()))
                .cloned();
            if let Some(ep) = current_ep {
                let key = reasoning_provider_key(&ep);
                let is_xai = classify_provider(&ep) == "xAI";
                let mut new_reasoning: Option<serde_json::Value> = None;
                let mut new_priority: Option<bool> = None;
                let mut dismissed = false;
                let response = egui::Area::new(egui::Id::new("agent_reasoning_picker"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(reasoning_badge_rect.left(), reasoning_badge_rect.bottom() + 2.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::none()
                            .fill(egui::Color32::from_rgb(37, 37, 38))
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(6.0, 8.0))
                            .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 4.0), blur: 16.0,
                                spread: 0.0, color: egui::Color32::from_black_alpha(90) })
                            .show(ui, |ui| {
                                ui.set_width(if is_xai { 240.0 } else { 180.0 });
                                if key == "chatgpt_codex" {
                                    let current = reasoning_str_field(&ep, key, "effort").unwrap_or("provider_default");
                                    for opt in ["provider_default", "none", "minimal", "low", "medium", "high", "xhigh"] {
                                        let is_current = opt == current;
                                        if draw_model_row(ui, effort_label(opt), 0, is_current).clicked() && !is_current {
                                            let mut r = ep.get("reasoning").cloned().unwrap_or(serde_json::json!({}));
                                            r["chatgpt_codex"]["effort"] = serde_json::json!(opt);
                                            new_reasoning = Some(r);
                                            dismissed = true;
                                        }
                                    }
                                } else {
                                    let current = reasoning_str_field(&ep, key, "thinking").unwrap_or("provider_default");
                                    for opt in ["provider_default", "on", "off"] {
                                        let is_current = opt == current;
                                        if draw_model_row(ui, toggle_label(opt), 0, is_current).clicked() && !is_current {
                                            let mut r = ep.get("reasoning").cloned().unwrap_or(serde_json::json!({}));
                                            r[key]["thinking"] = serde_json::json!(opt);
                                            new_reasoning = Some(r);
                                            dismissed = true;
                                        }
                                    }
                                }
                                if is_xai {
                                    ui.add_space(6.0);
                                    ui.separator();
                                    ui.add_space(2.0);
                                    ui.horizontal(|ui| {
                                        ui.add_space(6.0);
                                        ui.label(egui::RichText::new("SERVICE TIER").size(9.5)
                                            .color(egui::Color32::from_gray(130)));
                                    });
                                    ui.add_space(2.0);
                                    let priority_on = ep.get("xai_priority_tier").and_then(|v| v.as_bool()).unwrap_or(false);
                                    if draw_model_row(ui, "Standard", 0, !priority_on).clicked() && priority_on {
                                        new_priority = Some(false);
                                        dismissed = true;
                                    }
                                    if draw_model_row(ui, "Priority (2x cost)", 0, priority_on).clicked() && !priority_on {
                                        new_priority = Some(true);
                                        dismissed = true;
                                    }
                                    ui.add_space(2.0);
                                    ui.horizontal(|ui| {
                                        ui.add_space(6.0);
                                        // A label added directly to a horizontal layout never
                                        // wraps (see the ChatItem::User/Error fixes elsewhere in
                                        // this file) — needs its own vertical + max-width here
                                        // since this note is long enough to need it.
                                        ui.vertical(|ui| {
                                            ui.set_max_width(ui.available_width() - 6.0);
                                            ui.label(egui::RichText::new(
                                                "Priority requests higher scheduling priority from xAI \
                                                 during high demand, at double their standard per-token \
                                                 price. This is a charge from xAI, not Forge IDE.")
                                                .size(9.0).color(egui::Color32::from_gray(135)));
                                        });
                                    });
                                }
                            });
                    });
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
                self.agent_thinking_picker_frame = self.agent_thinking_picker_frame.saturating_add(1);
                if self.agent_thinking_picker_frame > 1 {
                    let primary_click = ui.input(|i| i.pointer.primary_clicked());
                    let interact_pos  = ui.input(|i| i.pointer.interact_pos());
                    if primary_click {
                        if let Some(pos) = interact_pos {
                            if !response.response.rect.contains(pos) { dismissed = true; }
                        }
                    }
                }
                if let Some(reasoning) = new_reasoning {
                    let name = ep.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    tab.session.update_endpoint_reasoning(name.clone(), reasoning.clone());
                    // No ack broadcast for this — update our own cached copy
                    // so the badge reflects the change immediately.
                    if let Some(cached) = tab.session.endpoints.iter_mut()
                        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                    {
                        cached["reasoning"] = reasoning;
                    }
                }
                if let Some(enabled) = new_priority {
                    let name = ep.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    tab.session.update_xai_priority_tier(name.clone(), enabled);
                    if let Some(cached) = tab.session.endpoints.iter_mut()
                        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                    {
                        cached["xai_priority_tier"] = serde_json::json!(enabled);
                    }
                }
                if dismissed { self.agent_thinking_picker_open = false; }
            } else {
                self.agent_thinking_picker_open = false;
            }
        }

        // Context-management-strategy picker — how forge-agent handles a
        // conversation that outgrows the model's context window: summarize
        // older messages via an LLM call ("compaction", the default) or just
        // drop the oldest ones ("rolling window", cheaper but lossier).
        if self.agent_context_picker_open {
            let current = tab.session.context_strategy.clone();
            let mut chosen: Option<&'static str> = None;
            let mut dismissed = false;
            let response = egui::Area::new(egui::Id::new("agent_context_picker"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(context_badge_rect.left(), context_badge_rect.bottom() + 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(37, 37, 38))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 10.0))
                        .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 4.0), blur: 16.0,
                            spread: 0.0, color: egui::Color32::from_black_alpha(90) })
                        .show(ui, |ui| {
                            ui.set_width(280.0);
                            for (strategy, label, description) in [
                                ("compaction", "Compaction",
                                 "Summarizes older messages with an extra LLM call once the \
                                  context window fills, so long-running context is preserved \
                                  as a condensed summary instead of lost outright."),
                                ("rolling_window", "Rolling Window",
                                 "Just drops the oldest messages once the context window fills \
                                  — no summarization call, so it's cheaper and faster, but that \
                                  earlier history is gone rather than condensed."),
                            ] {
                                let is_current = strategy == current;
                                if draw_perm_mode_row(ui, if strategy == "compaction" { 0 } else { 1 },
                                    label, description,
                                    egui::Color32::from_gray(210), is_current).clicked() && !is_current
                                {
                                    chosen = Some(strategy);
                                    dismissed = true;
                                }
                                if strategy == "compaction" { ui.add_space(3.0); }
                            }
                        });
                });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
            self.agent_context_picker_frame = self.agent_context_picker_frame.saturating_add(1);
            if self.agent_context_picker_frame > 1 {
                let primary_click = ui.input(|i| i.pointer.primary_clicked());
                let interact_pos  = ui.input(|i| i.pointer.interact_pos());
                if primary_click {
                    if let Some(pos) = interact_pos {
                        if !response.response.rect.contains(pos) { dismissed = true; }
                    }
                }
            }
            if let Some(strategy) = chosen {
                tab.session.update_context_strategy(strategy.to_string());
            }
            if dismissed { self.agent_context_picker_open = false; }
        }

        // Model switch confirmation strip
        if let Some(ep) = self.agent_pending_switch.clone() {
            let name = ep.get("name").and_then(|v| v.as_str()).unwrap_or("this model");
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(format!(
                    "Switch to {name}? May incur extra token cost, even from the same provider."))
                    .size(11.0).color(egui::Color32::from_rgb(220, 190, 120)));
            });
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if ui.button("Confirm").clicked() {
                    tab.session.switch_model(ep);
                    self.agent_pending_switch = None;
                }
                if ui.button("Cancel").clicked() {
                    self.agent_pending_switch = None;
                }
            });
            ui.add_space(4.0);
        }

        // Auto-save after each user message — but only when the item count
        // actually changed since the last save. This ran unconditionally on
        // every single frame regardless of whether anything changed, which
        // meant a long conversation cloned its *entire* item list, JSON-
        // serialized it, and wrote it to disk on every repaint (not just
        // real ones from actual events — every dirty frame, including one
        // triggered by typing a single character elsewhere in the UI).
        // Measured ~40ms/frame from this alone on a real 1442-item
        // conversation — the dominant cost behind a long conversation
        // making the whole app, including typing itself, feel laggy.
        // Toggling a card's `expanded`/`resolved` flag in place (not an
        // item count change) can still go temporarily unsaved by this
        // check alone, but every other save point (tab switch, tab close,
        // new tab, session-restore-on-reboot) still saves unconditionally,
        // so nothing is ever actually lost — just not re-flushed on every
        // single frame for no reason.
        if tab.session.items.iter().any(|i| matches!(i, ChatItem::User(_)))
            && tab.session.items.len() != tab.last_saved_item_count
        {
            let id = tab.conv_id.clone();
            crate::agent_panel::save_conversation(&tab.session, &id, &self.cwd);
            tab.last_saved_item_count = tab.session.items.len();
        }

        // Re-check for a git repo if forge-agent just auto-initialized one —
        // the Source Control panel otherwise only ever checks once, at
        // folder-open time, and would silently stay in its stale "no
        // repository" state until the next reload.
        if std::mem::take(&mut self.agent_tabs[self.agent_active].session.git_just_initialized) {
            self.git = crate::git::GitState::open(&self.cwd);
        }

        // Drain agent-driven file changes and shell output; live-reload matching
        // open buffers and mirror shell output into the integrated Output panel.
        {
            let cwd = self.cwd.clone();
            let (changed, shell) = {
                let session = &mut self.agent_tabs[self.agent_active].session;
                (std::mem::take(&mut session.changed_files), std::mem::take(&mut session.shell_events))
            };
            let mut logs: Vec<(String, OutputLevel)> = Vec::new();
            for line in shell {
                logs.push((line, OutputLevel::Info));
            }
            for rel_path in changed {
                let candidate = std::path::PathBuf::from(&rel_path);
                let full_path = if candidate.is_absolute() { candidate } else { cwd.join(&candidate) };
                if let Some(buf) = self.buffers.iter_mut().find(|b| b.path.as_deref() == Some(full_path.as_path())) {
                    if buf.modified {
                        logs.push((format!(
                            "Agent edited {rel_path} on disk, but it has unsaved changes here — not auto-reloaded."),
                            OutputLevel::Warn));
                    } else {
                        match buf.reload() {
                            Ok(()) => logs.push((format!("Reloaded {rel_path} after agent edit"), OutputLevel::Info)),
                            Err(e) => logs.push((format!("Failed to reload {rel_path}: {e}"), OutputLevel::Warn)),
                        }
                    }
                }
            }
            for (msg, level) in logs {
                self.output_log(msg, level);
            }
        }

        // Input box pinned to bottom
        let tab = &mut self.agent_tabs[self.agent_active];
        let queue_h = if tab.session.queued.is_empty() { 0.0 }
                      else { 24.0 * tab.session.queued.len() as f32 + 4.0 };
        let show_activity = tab.session.is_active();
        let activity_h = if show_activity { 22.0 } else { 0.0 };
        let input_h = 118.0 + queue_h + activity_h;
        let mut revoke_idx: Option<usize> = None;
        let mut send_now_idx: Option<usize> = None;
        egui::TopBottomPanel::bottom("agent_input_panel")
            .exact_height(input_h)
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(30,30,30)))
            .show_inside(ui, |ui| {
                // Whole composer panel is the drop target, not just the text
                // box — see the drop handler below.
                let drop_zone = ui.max_rect();
                ui.add_space(8.0);
                let panel_w = ui.available_width();
                let outer_pad = 6.0;
                let frame_outer = (panel_w - outer_pad * 2.0).max(100.0);
                if !tab.session.queued.is_empty() {
                    for (qi, msg) in tab.session.queued.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_space(outer_pad);
                            egui::Frame::none()
                                .fill(egui::Color32::from_rgb(40,40,44))
                                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                                .rounding(5.0)
                                .inner_margin(egui::Margin::symmetric(8.0, 3.0))
                                .show(ui, |ui| {
                                    ui.set_width(frame_outer - 2.0);
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("queued").size(9.5)
                                            .color(egui::Color32::from_gray(130)));
                                        // `set_width` above only sets a *desired* size — a plain
                                        // `ui.label` in a horizontal layout doesn't wrap, and
                                        // egui's default wrap mode for that case is `Extend`,
                                        // which literally grows the parent `Ui` (and from there,
                                        // the whole resizable side panel/window) to fit instead
                                        // of clipping. A long queued message — or several —
                                        // could snowball the window to full width and it'd never
                                        // shrink back. `.truncate()` + an explicit sized rect
                                        // (accounting for the buttons still to come on this row)
                                        // makes it actually ellipsize instead.
                                        let preview: String = msg.chars().take(200).collect();
                                        let reserved_for_buttons = 70.0; // send-now + delete
                                        let text_avail = (ui.available_width() - reserved_for_buttons).max(20.0);
                                        ui.add_sized(
                                            egui::vec2(text_avail, 16.0),
                                            egui::Label::new(egui::RichText::new(preview).size(11.0)
                                                .color(egui::Color32::from_gray(200)))
                                                .truncate(),
                                        );
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0,18.0), egui::Sense::click());
                                            if resp.hovered() {
                                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                                ui.painter().rect_filled(rect, 3.0, egui::Color32::from_rgb(120,60,60));
                                            }
                                            let c = rect.center();
                                            let s = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(210));
                                            ui.painter().line_segment([c - egui::vec2(4.0,4.0), c + egui::vec2(4.0,4.0)], s);
                                            ui.painter().line_segment([c + egui::vec2(-4.0,4.0), c + egui::vec2(4.0,-4.0)], s);
                                            let _ = resp.clone().on_hover_text("Remove from queue");
                                            if resp.clicked() { revoke_idx = Some(qi); }

                                            ui.add_space(2.0);
                                            let (rect2, resp2) = ui.allocate_exact_size(egui::vec2(18.0,18.0), egui::Sense::click());
                                            if resp2.hovered() {
                                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                                ui.painter().rect_filled(rect2, 3.0, egui::Color32::from_rgb(60,90,60));
                                            }
                                            // Hand-drawn play triangle — same font-independence
                                            // reasoning as `paint_disclosure_triangle`/`paint_checkmark`.
                                            let c2 = rect2.center();
                                            ui.painter().add(egui::Shape::convex_polygon(
                                                vec![c2 + egui::vec2(-3.0, -4.0), c2 + egui::vec2(-3.0, 4.0), c2 + egui::vec2(4.0, 0.0)],
                                                egui::Color32::from_rgb(160, 220, 160), egui::Stroke::NONE,
                                            ));
                                            let _ = resp2.clone().on_hover_text("Interrupt current turn and send this now");
                                            if resp2.clicked() { send_now_idx = Some(qi); }
                                        });
                                    });
                                });
                        });
                        ui.add_space(2.0);
                    }
                }
                if show_activity {
                    ui.horizontal(|ui| {
                        ui.add_space(outer_pad + 2.0);
                        // A slow pulse rather than a flat dot — cheap, honest
                        // signal that something is actually still happening,
                        // not just a static icon that could be stuck. Repaints
                        // ride along with the existing "anything streaming"
                        // scheduling (see `IdeApp::draw`), no extra wakeups.
                        let t = ui.input(|i| i.time);
                        let pulse = 0.35 + 0.65 * ((t * 3.0).sin() * 0.5 + 0.5);
                        let base = egui::Color32::from_rgb(140, 180, 220);
                        let dot = egui::Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), (255.0 * pulse) as u8);
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 3.5, dot);
                        let label = if tab.session.activity.is_empty() { "Working…".to_string() } else { tab.session.activity.clone() };
                        ui.label(egui::RichText::new(label).size(10.5).color(egui::Color32::from_gray(190)));
                        if tab.session.turn_tokens > 0 {
                            ui.label(egui::RichText::new(format!("· {} tokens", tab.session.turn_tokens)).size(10.5)
                                .color(egui::Color32::from_gray(130)));
                        }
                    });
                    ui.add_space(2.0);
                }
                ui.horizontal(|ui| {
                    ui.add_space(outer_pad);
                    ui.allocate_ui_with_layout(
                        egui::vec2(frame_outer, 96.0),
                        egui::Layout::top_down(egui::Align::LEFT),
                        |ui| {
                            egui::Frame::none()
                                .fill(egui::Color32::from_rgb(35,35,38))
                                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(70)))
                                .rounding(8.0)
                                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                .show(ui, |ui| {
                                    let inner_w = ui.available_width();
                                    let resp = ui.add_sized(
                                        egui::vec2(inner_w, 56.0),
                                        egui::TextEdit::multiline(&mut tab.session.input)
                                            .hint_text("Ask Forge…")
                                            .frame(false)
                                            .desired_rows(3));
                                    if std::mem::take(&mut tab.session.request_input_focus) {
                                        resp.request_focus();
                                    }

                                    // Dragging a file in appends its path to the
                                    // prompt, which is how you point the agent at
                                    // something — a screenshot to look at, a file
                                    // to read. Same reasoning as the terminal
                                    // (see `Terminal::draw_sized`): the useful
                                    // thing to hand over is the path, not the
                                    // bytes.
                                    //
                                    // Scoped to a drop that lands on this panel:
                                    // `dropped_files` is global for the frame, so
                                    // without the rect check a drop onto the
                                    // terminal or editor would also land here.
                                    let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
                                    if !dropped.is_empty() {
                                        let over_panel = ui.ctx().input(|i| i.pointer.interact_pos())
                                            .is_some_and(|p| drop_zone.contains(p));
                                        if over_panel {
                                            let paths: Vec<String> = dropped.iter()
                                                .filter_map(|f| f.path.as_ref())
                                                .map(|p| {
                                                    // Quote only when needed — an
                                                    // unquoted path with spaces
                                                    // reads as several arguments.
                                                    let s = p.to_string_lossy();
                                                    if s.contains(' ') { format!("\"{s}\"") } else { s.into_owned() }
                                                })
                                                .collect();
                                            if !paths.is_empty() {
                                                let text = &mut tab.session.input;
                                                if !text.is_empty() && !text.ends_with(' ') {
                                                    text.push(' ');
                                                }
                                                text.push_str(&paths.join(" "));
                                                text.push(' ');
                                                tab.session.request_input_focus = true;
                                            }
                                        }
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        let active = !tab.session.input.trim().is_empty();
                                        let (rect2, btn_resp) = ui.allocate_exact_size(egui::vec2(28.0,28.0), egui::Sense::click());
                                        let bg = if active && btn_resp.hovered() { egui::Color32::from_rgb(20,145,235) }
                                                 else if active { egui::Color32::from_rgb(0,120,212) }
                                                 else { egui::Color32::from_gray(55) };
                                        ui.painter().circle_filled(rect2.center(), 14.0, bg);
                                        paint_send_icon(ui.painter(), rect2.center(),
                                            if active { egui::Color32::WHITE } else { egui::Color32::from_gray(120) });
                                        if active && btn_resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                                        let send_key = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                                        if active && (btn_resp.clicked() || send_key) {
                                            let mut text = std::mem::take(&mut tab.session.input);
                                            if text.ends_with('\n') { text.pop(); }
                                            tab.session.send_user(text);
                                        }
                                    });
                                });
                        });
                });
            });

        if let Some(idx) = revoke_idx {
            tab.session.revoke_queued(idx);
        }
        if let Some(idx) = send_now_idx {
            tab.session.send_queued_now(idx);
        }

        // ── Docked subagent strip ───────────────────────────────────────────
        // Lives between the chat and the input box — a subagent keeps running
        // "on the side" here (with its own tool-call activity and approvals)
        // regardless of what the main conversation above is doing, instead of
        // blocking it or getting buried in the scroll. Only *active*
        // subagents get a slot — once one finishes, its summary already
        // surfaced in the main chat (see the `ChatItem::Subagent` arm above),
        // so it drops out of the strip immediately instead of leaving a
        // permanent "completed" entry sitting above the input box forever.
        let tab = &self.agent_tabs[self.agent_active];
        let is_active_subagent = |i: &ChatItem| matches!(i, ChatItem::Subagent { finished: false, .. });
        let subagent_count = tab.session.items.iter().filter(|i| is_active_subagent(i)).count();
        // Paths (not flat indices) since a subagent can nest another one —
        // `[2, 1]` means "items[2] (a Subagent), then its own items[1]".
        let mut subagent_pending_action: Option<(Vec<usize>, bool)> = None;
        let mut subagent_toggle_expand: Option<Vec<usize>> = None;
        if subagent_count > 0 {
            let expanded_count = tab.session.items.iter()
                .filter(|i| is_active_subagent(i) && matches!(i, ChatItem::Subagent { expanded: true, .. }))
                .count();
            let strip_h = (subagent_count as f32 * 26.0 + expanded_count as f32 * 150.0)
                .clamp(26.0, 320.0) + 10.0;
            egui::TopBottomPanel::bottom("subagent_strip")
                .exact_height(strip_h)
                .frame(egui::Frame::none().fill(egui::Color32::from_rgb(22, 22, 26))
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0)))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("subagent_strip_scroll").auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (idx, item) in tab.session.items.iter().enumerate() {
                                let ChatItem::Subagent { id: _, agent_type, prompt, finished: false, expanded, items, .. } = item
                                    else { continue };
                                draw_subagent_strip_entry(
                                    ui, &[idx], agent_type, prompt, *expanded, items,
                                    &mut subagent_pending_action, &mut subagent_toggle_expand,
                                );
                            }
                        });
                });
        }
        if let Some(path) = subagent_toggle_expand {
            let tab = &mut self.agent_tabs[self.agent_active];
            if let Some(ChatItem::Subagent { expanded, .. }) = subagent_at_path_mut(&mut tab.session.items, &path) {
                *expanded = !*expanded;
            }
        }
        if let Some((path, approve)) = subagent_pending_action {
            let tab = &mut self.agent_tabs[self.agent_active];
            let mut tool_id: Option<String> = None;
            if let Some((container, &last)) = path.split_last().map(|(l, rest)| (rest, l)) {
                if let Some(items) = subagent_container_items_mut(&mut tab.session.items, container) {
                    if let Some(ChatItem::ToolRequest { id, approval, .. }) = items.get_mut(last) {
                        *approval = if approve { ApprovalState::Approved } else { ApprovalState::Denied };
                        tool_id = Some(id.clone());
                    }
                }
            }
            if let Some(tool_id) = tool_id {
                if approve { tab.session.approve(tool_id); }
                else { tab.session.deny(tool_id, "User denied in Forge IDE".into()); }
            }
        }

        // Chat scroll area
        let tab = &self.agent_tabs[self.agent_active];
        let mut pending_action: Option<(usize, bool)> = None;
        let mut toggle_expand: Option<usize> = None;
        let mut question_edit: Option<QuestionEdit> = None;
        let mut plan_edit: Option<PlanEdit> = None;
        let mut input_edit: Option<InputNeededEdit> = None;
        let mut rewind_edit: Option<RewindEdit> = None;
        let mut provider_busy_edit: Option<ProviderBusyEdit> = None;
        let mut show_full_history_clicked = false;
        egui::ScrollArea::vertical()
            .id_salt(format!("agent_chat_{}", self.agent_active))
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            // egui's default `drag_to_scroll(true)` is a touch-screen-style
            // pan gesture: it claims a click-drag on its contents to scroll
            // *before* the selectable labels inside get a chance to
            // interpret the same drag as text selection — on a desktop app
            // with a mouse, that means click-drag-to-select text never
            // actually worked, it always scrolled instead. Wheel/trackpad
            // scroll and the scrollbar itself are untouched by this.
            .drag_to_scroll(false)
            .show(ui, |ui| {
                // Selecting past the top or bottom edge scrolls the list, the
                // way every other text view does. Without this, a selection
                // could only ever cover what happened to be on screen when the
                // drag started: with drag_to_scroll off (see above) a drag
                // that leaves the viewport produced no scrolling at all, and
                // the wheel can't be reached mid-drag.
                if let Some(dy) = selection_autoscroll(ui) {
                    ui.scroll_with_delta_animation(
                        egui::vec2(0.0, -dy),
                        egui::style::ScrollAnimation::none(),
                    );
                    // The pointer sitting still generates no events, so ask
                    // for the next frame explicitly or scrolling stalls until
                    // the mouse moves again.
                    ui.ctx().request_repaint();
                }
                ui.add_space(4.0);
                let pad_l = 10.0;
                let pad_r = 10.0;
                let items: &[ChatItem] = &tab.session.items;
                // Every item here — even a cheap-looking one — goes through
                // markdown parsing and several egui widget calls with no
                // caching (unlike the editor/terminal panels, which already
                // cache their shaped output), so rebuilding the *entire*
                // list from scratch every single frame scales directly with
                // conversation length: measured ~85ms/frame for a real
                // 1442-item conversation, enough on its own to make typing
                // feel delayed since every keystroke's repaint pays that
                // same cost first. Capping how far back rendering goes by
                // default keeps a long-running conversation's per-frame
                // cost roughly constant regardless of how long it's gotten.
                let render_start = if tab.show_full_history || items.len() <= AGENT_HISTORY_RENDER_LIMIT {
                    0
                } else {
                    items.len() - AGENT_HISTORY_RENDER_LIMIT
                };
                if render_start > 0 {
                    ui.horizontal(|ui| {
                        ui.add_space(pad_l);
                        if ui.small_button(format!("Show {render_start} earlier message{}",
                            if render_start == 1 { "" } else { "s" })).clicked()
                        {
                            show_full_history_clicked = true;
                        }
                    });
                    ui.add_space(6.0);
                }
                let mut item_idx = render_start;
                while item_idx < items.len() {
                    // Tool calls are handled up front (not as a match arm
                    // below) since a single call can consume more than one
                    // list entry — its paired `ToolResult`, or, for a run of
                    // consecutive read-only calls, several of each. See
                    // `draw_tool_run`.
                    if matches!(items[item_idx], ChatItem::ToolRequest { .. }) {
                        item_idx = draw_tool_run(
                            ui, items, item_idx, pad_l, pad_r,
                            &mut pending_action, &mut toggle_expand,
                        );
                        continue;
                    }
                    match &items[item_idx] {
                        ChatItem::User(text) => {
                            ui.add_space(8.0);
                            let body = ui.horizontal(|ui| {
                                ui.add_space(pad_l);
                                egui::Frame::none()
                                    .fill(egui::Color32::from_rgb(38,44,56))
                                    .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                                    .rounding(6.0)
                                    .show(ui, |ui| {
                                        ui.set_max_width(ui.available_width() - pad_r);
                                        // Must switch to a vertical layout before adding the label:
                                        // Frame content inherits the layout direction of whatever
                                        // it's placed inside (the `ui.horizontal` above, for the
                                        // left padding) — a label added directly to a horizontal
                                        // layout never wraps, regardless of set_max_width/soft_wrap.
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(soft_wrap(text, wrap_run(ui, 12.5, false))).color(egui::Color32::WHITE));
                                        });
                                    });
                            }).response.rect;
                            copy_message_button(
                                ui, body, ui.make_persistent_id(("copy_msg", item_idx)), text, pad_l,
                            );
                        }
                        ChatItem::Assistant { text, .. } => {
                            ui.add_space(8.0);
                            let body = ui.horizontal_top(|ui| {
                                ui.add_space(pad_l);
                                ui.vertical(|ui| {
                                    ui.set_max_width(ui.available_width() - pad_r);
                                    crate::markdown::render(ui, text, wrap_run(ui, 12.5, false));
                                });
                            }).response.rect;
                            copy_message_button(
                                ui, body, ui.make_persistent_id(("copy_msg", item_idx)), text, pad_l,
                            );
                        }
                        ChatItem::Reasoning { text, .. } => {
                            ui.add_space(6.0);
                            ui.horizontal_top(|ui| {
                                ui.add_space(pad_l);
                                ui.vertical(|ui| {
                                    ui.set_max_width(ui.available_width() - pad_r);
                                    ui.label(egui::RichText::new(soft_wrap(text, wrap_run(ui, 11.0, false))).italics().size(11.0)
                                        .color(egui::Color32::from_gray(130)));
                                });
                            });
                        }
                        ChatItem::ToolRequest { .. } => {
                            // Unreachable: every `ToolRequest` is consumed by
                            // `draw_tool_run` above before this match runs.
                        }
                        ChatItem::ToolResult { name, content, success, expanded } => {
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                ui.add_space(pad_l);
                                let (border, badge) = if *success {
                                    (egui::Color32::from_rgb(40,60,40), egui::Color32::from_rgb(130,200,130))
                                } else {
                                    (egui::Color32::from_rgb(70,35,35), egui::Color32::from_rgb(255,130,100))
                                };
                                egui::Frame::none()
                                    .fill(egui::Color32::from_rgb(24,26,24))
                                    .stroke(egui::Stroke::new(1.0_f32, border))
                                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                    .rounding(4.0)
                                    .show(ui, |ui| {
                                        ui.set_max_width(ui.available_width() - pad_r);
                                        // See the comment on the ToolRequest arm above — Frame content
                                        // inherits the horizontal layout of its parent row and won't
                                        // wrap without an explicit vertical layout here.
                                        ui.vertical(|ui| {
                                        let trimmed_full = strip_ansi(content.trim());
                                        let first_line = trimmed_full.lines().next().unwrap_or("");
                                        let header = ui.horizontal(|ui| {
                                            paint_disclosure_triangle(ui, *expanded, egui::Color32::from_gray(140));
                                            ui.label(egui::RichText::new(if *success { "ok" } else { "err" }).size(11.5).color(badge));
                                            ui.label(egui::RichText::new(name).monospace().size(11.5).color(egui::Color32::from_gray(180)));
                                        });
                                        let mut click_target = header.response.clone();
                                        if !*expanded && !first_line.is_empty() {
                                            let one_liner: String = first_line.chars().take(60).collect();
                                            let resp = ui.label(egui::RichText::new(soft_wrap(&one_liner, wrap_run(ui, 10.5, true))).monospace().size(10.5).color(egui::Color32::from_gray(150)));
                                            click_target = click_target.union(resp);
                                        }
                                        if click_target.interact(egui::Sense::click()).clicked() {
                                            toggle_expand = Some(item_idx);
                                        }
                                        if *expanded {
                                            let preview: String = trimmed_full.chars().take(400).collect();
                                            if !preview.is_empty() {
                                                ui.label(egui::RichText::new(soft_wrap(&preview, wrap_run(ui, 10.5, true))).monospace().size(10.5).color(egui::Color32::from_gray(150)));
                                                if content.chars().count() > 400 {
                                                    ui.label(egui::RichText::new("...").color(egui::Color32::from_gray(110)));
                                                }
                                            }
                                        }
                                        });
                                    });
                            });
                        }
                        ChatItem::Subagent { agent_type, prompt, finished, summary, .. } => {
                            // While running, this is a minimal marker — the
                            // docked subagent strip (below the chat) is where
                            // its live activity and any approvals actually
                            // live. Once finished, the summary surfaces right
                            // here instead, same as before: you shouldn't
                            // have to go hunting in the strip for a result
                            // that's already done.
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.add_space(pad_l);
                                egui::Frame::none()
                                    .fill(egui::Color32::from_rgb(28,26,40))
                                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(75,65,110)))
                                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                                    .rounding(4.0)
                                    .show(ui, |ui| {
                                        ui.set_max_width(ui.available_width() - pad_r);
                                        let narrow_row = ui.available_width() < 300.0;
                                        ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            if *finished {
                                                paint_checkmark(ui, egui::Color32::from_rgb(150,170,255));
                                            } else {
                                                paint_dot(ui, egui::Color32::from_rgb(150,170,255));
                                            }
                                            ui.label(egui::RichText::new("subagent").size(11.5).color(egui::Color32::from_rgb(170,150,230)));
                                            ui.label(egui::RichText::new(agent_type.as_str()).monospace().size(11.5).strong().color(egui::Color32::from_rgb(190,180,240)));
                                            // See the checkpoint row: pinned to
                                            // the right of a row this narrow it
                                            // simply landed on top of the name
                                            // beside it, so below a threshold it
                                            // goes on its own line instead.
                                            if !*finished && !narrow_row {
                                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                    ui.label(egui::RichText::new("running — see below").size(10.0).italics()
                                                        .color(egui::Color32::from_gray(130)));
                                                });
                                            }
                                        });
                                        if !*finished && narrow_row {
                                            ui.label(egui::RichText::new("running — see below").size(10.0).italics()
                                                .color(egui::Color32::from_gray(130)));
                                        }
                                        if *finished {
                                            let prompt_preview: String = prompt.chars().take(90).collect();
                                            ui.label(egui::RichText::new(soft_wrap(&prompt_preview, wrap_run(ui, 10.5, false))).size(10.5).color(egui::Color32::from_gray(160)));
                                            if !summary.trim().is_empty() {
                                                let s: String = summary.trim().chars().take(300).collect();
                                                ui.label(egui::RichText::new(soft_wrap(&s, wrap_run(ui, 10.5, false))).size(10.5).color(egui::Color32::from_gray(180)));
                                            }
                                        }
                                        });
                                    });
                            });
                        }
                        ChatItem::Question { tool_id: _, question, items: qitems, selected, other_text, free_text, answered } => {
                            draw_question_card(
                                ui, item_idx, question, qitems, selected, other_text, free_text, *answered,
                                pad_l, pad_r, &mut question_edit,
                            );
                        }
                        ChatItem::Plan { plan_path, content, resolved, resolution, reject_feedback, expanded } => {
                            draw_plan_card(
                                ui, item_idx, plan_path, content, *resolved, resolution, reject_feedback, *expanded,
                                pad_l, pad_r, &mut plan_edit,
                            );
                        }
                        ChatItem::InputNeeded { bg_id: _, command, prompt, is_password, resolved, resolution, text, remember_confirm } => {
                            draw_input_needed_card(
                                ui, item_idx, command, prompt, *is_password, *resolved, resolution, text, remember_confirm,
                                pad_l, pad_r, &mut input_edit,
                            );
                        }
                        ChatItem::Checkpoint { id: _, preview, message_count, confirming, preview_loading, preview_result } => {
                            draw_checkpoint_card(
                                ui, item_idx, preview, *message_count, *confirming,
                                *preview_loading, preview_result,
                                pad_l, pad_r, &mut rewind_edit,
                            );
                        }
                        ChatItem::ProviderBusy { message, endpoint_name, resolved, resolution } => {
                            draw_provider_busy_card(
                                ui, item_idx, message, endpoint_name, *resolved, resolution,
                                &tab.session.endpoints, pad_l, pad_r, &mut provider_busy_edit,
                            );
                        }
                        ChatItem::Error(msg) => {
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                ui.add_space(pad_l);
                                // See the ChatItem::User arm above — a label added directly to a
                                // horizontal layout never wraps, it just extends the row off-panel.
                                // A long error (context-overflow messages especially) rendered
                                // almost entirely off-screen as a result, with only its first few
                                // words visible at the left edge.
                                ui.vertical(|ui| {
                                    ui.set_max_width(ui.available_width() - pad_r);
                                    ui.label(egui::RichText::new(soft_wrap(&format!("  {msg}"), wrap_run(ui, 12.5, false))).color(egui::Color32::from_rgb(255,110,100)));
                                });
                            });
                        }
                        ChatItem::Status(msg) => {
                            ui.add_space(2.0);
                            ui.horizontal(|ui| {
                                ui.add_space(pad_l);
                                ui.vertical(|ui| {
                                    ui.set_max_width(ui.available_width() - pad_r);
                                    ui.label(egui::RichText::new(soft_wrap(msg, wrap_run(ui, 10.5, false))).size(10.5).color(egui::Color32::from_gray(110)));
                                });
                            });
                        }
                    }
                    item_idx += 1;
                }
                // The conversation otherwise ends flush against the bottom of
                // the viewport, so the newest card — the one being read — sits
                // touching the status line and the input box below it. Inside
                // the scroll area rather than after it, so it is content the
                // view scrolls to and not a permanent band of empty panel.
                ui.add_space(10.0);
            });

        if let Some((idx, approve)) = pending_action {
            let tab = &mut self.agent_tabs[self.agent_active];
            let mut tool_id: Option<String> = None;
            if let Some(ChatItem::ToolRequest { id, approval, .. }) = tab.session.items.get_mut(idx) {
                *approval = if approve { ApprovalState::Approved } else { ApprovalState::Denied };
                tool_id = Some(id.clone());
            }
            if let Some(tool_id) = tool_id {
                if approve {
                    tab.session.approve(tool_id);
                } else {
                    tab.session.deny(tool_id, "User denied in Forge IDE".into());
                }
            }
        }

        if let Some(idx) = toggle_expand {
            let tab = &mut self.agent_tabs[self.agent_active];
            match tab.session.items.get_mut(idx) {
                Some(ChatItem::ToolRequest { expanded, .. }) => *expanded = !*expanded,
                Some(ChatItem::ToolResult { expanded, .. }) => *expanded = !*expanded,
                _ => {}
            }
        }

        if let Some(edit) = question_edit {
            let tab = &mut self.agent_tabs[self.agent_active];
            let item_idx = edit.item_idx();
            let mut answer: Option<String> = None;
            if let Some(ChatItem::Question { question, items, selected, other_text, free_text, answered, .. }) =
                tab.session.items.get_mut(item_idx)
            {
                match edit {
                    QuestionEdit::ToggleOption { question_idx, option_idx, .. } => {
                        if let (Some(q), Some(sel)) = (items.get(question_idx), selected.get_mut(question_idx)) {
                            if q.multi_select {
                                if let Some(pos) = sel.iter().position(|&i| i == option_idx) {
                                    sel.remove(pos);
                                } else {
                                    sel.push(option_idx);
                                }
                            } else {
                                *sel = vec![option_idx];
                            }
                        }
                    }
                    QuestionEdit::OtherText { question_idx, text, .. } => {
                        if let Some(t) = other_text.get_mut(question_idx) { *t = text; }
                    }
                    QuestionEdit::FreeText { text, .. } => { *free_text = text; }
                    QuestionEdit::Submit { .. } => {
                        // Combine every question's selection (or its "Other"
                        // free text, if that's what was picked) into one
                        // human-readable reply — forge has no structured
                        // per-question encoding on the wire, it's handed to
                        // the model as a single opaque string.
                        let built = if items.is_empty() {
                            free_text.trim().to_string()
                        } else {
                            items.iter().enumerate().map(|(qi, q)| {
                                let sel = selected.get(qi).cloned().unwrap_or_default();
                                let parts: Vec<String> = sel.iter().filter_map(|&oi| {
                                    let opt = q.options.get(oi)?;
                                    if opt.label.eq_ignore_ascii_case("other") {
                                        let custom = other_text.get(qi).map(|s| s.trim()).unwrap_or("");
                                        Some(if custom.is_empty() { opt.label.clone() } else { custom.to_string() })
                                    } else {
                                        Some(opt.label.clone())
                                    }
                                }).collect();
                                let answer_part = if parts.is_empty() { "(no answer)".to_string() } else { parts.join(", ") };
                                let label = if q.header.is_empty() { question.clone() } else { q.header.clone() };
                                format!("{label}: {answer_part}")
                            }).collect::<Vec<_>>().join("\n")
                        };
                        *answered = true;
                        answer = Some(built);
                    }
                }
            }
            if let Some(answer) = answer {
                tab.session.answer_question(answer);
            }
        }

        if let Some(edit) = plan_edit {
            let tab = &mut self.agent_tabs[self.agent_active];
            let item_idx = edit.item_idx();
            let mut resolution: Option<PlanResolution> = None;
            if let Some(ChatItem::Plan { reject_feedback, expanded, resolved, .. }) =
                tab.session.items.get_mut(item_idx)
            {
                match edit {
                    PlanEdit::FeedbackText { text, .. } => { *reject_feedback = text; }
                    PlanEdit::ToggleExpand { .. } => { *expanded = !*expanded; }
                    PlanEdit::Approve { .. } if !*resolved => resolution = Some(PlanResolution::Approve),
                    PlanEdit::ApproveClear { .. } if !*resolved => resolution = Some(PlanResolution::ApproveClear),
                    PlanEdit::Discuss { .. } if !*resolved => resolution = Some(PlanResolution::Discuss),
                    PlanEdit::Reject { .. } if !*resolved => {
                        resolution = Some(PlanResolution::Reject(reject_feedback.clone()));
                    }
                    _ => {}
                }
            }
            match resolution {
                Some(PlanResolution::Approve)      => tab.session.approve_plan(),
                Some(PlanResolution::ApproveClear)  => tab.session.approve_plan_clear(),
                Some(PlanResolution::Discuss)       => tab.session.discuss_plan(),
                Some(PlanResolution::Reject(fb))    => tab.session.reject_plan(fb),
                None => {}
            }
        }

        if let Some(edit) = input_edit {
            let tab = &mut self.agent_tabs[self.agent_active];
            let item_idx = edit.item_idx();
            enum InputAction { Send(String, bool), Remember(String), Reject, ForgetSaved }
            let mut action: Option<InputAction> = None;
            if let Some(ChatItem::InputNeeded { text, remember_confirm, resolved, is_password, .. }) =
                tab.session.items.get_mut(item_idx)
            {
                match edit {
                    InputNeededEdit::Text { text: t, .. } => { *text = t; }
                    InputNeededEdit::RememberConfirm { text: t, .. } => { *remember_confirm = t; }
                    InputNeededEdit::Send { .. } if !*resolved => {
                        action = Some(InputAction::Send(text.clone(), *is_password));
                    }
                    InputNeededEdit::Remember { .. } if !*resolved => {
                        action = Some(InputAction::Remember(text.clone()));
                    }
                    InputNeededEdit::Reject { .. } if !*resolved => action = Some(InputAction::Reject),
                    InputNeededEdit::ForgetSaved { .. } => action = Some(InputAction::ForgetSaved),
                    _ => {}
                }
            }
            match action {
                Some(InputAction::Send(content, is_password)) => {
                    let label = if is_password { "Sent (one-time, not stored)." } else { "Sent." };
                    tab.session.answer_input(content, label);
                }
                Some(InputAction::Remember(content)) => {
                    tab.remember_session_password(content.clone());
                    tab.session.answer_input(content, "Sent and remembered for this session.");
                }
                Some(InputAction::Reject) => tab.session.reject_input(),
                Some(InputAction::ForgetSaved) => tab.forget_session_password(),
                None => {}
            }
        }

        if let Some(edit) = rewind_edit {
            let tab = &mut self.agent_tabs[self.agent_active];
            let item_idx = edit.item_idx();
            let mut checkpoint_id: Option<String> = None;
            let mut preview_checkpoint_id: Option<String> = None;
            if let Some(ChatItem::Checkpoint { id, confirming, preview_loading, .. }) = tab.session.items.get_mut(item_idx) {
                match edit {
                    RewindEdit::Arm { .. }    => { *confirming = true; }
                    RewindEdit::Cancel { .. } => { *confirming = false; }
                    RewindEdit::Confirm { .. } => { *confirming = false; checkpoint_id = Some(id.clone()); }
                    RewindEdit::Preview { .. } => { *preview_loading = true; preview_checkpoint_id = Some(id.clone()); }
                }
            }
            if let Some(id) = checkpoint_id {
                tab.session.rewind(Some(id));
            }
            if let Some(id) = preview_checkpoint_id {
                tab.session.rewind_preview(id);
            }
        }

        if let Some(edit) = provider_busy_edit {
            let tab = &mut self.agent_tabs[self.agent_active];
            let item_idx = edit.item_idx();
            let mut switch_endpoint: Option<String> = None;
            if let Some(ChatItem::ProviderBusy { endpoint_name, resolved, resolution, .. }) =
                tab.session.items.get_mut(item_idx)
            {
                match edit {
                    ProviderBusyEdit::SwitchPriority { .. } => {
                        *resolved = true;
                        *resolution = "Switched to priority tier for this endpoint.".into();
                        switch_endpoint = Some(endpoint_name.clone());
                    }
                    ProviderBusyEdit::Dismiss { .. } => {
                        *resolved = true;
                        *resolution = "Dismissed.".into();
                    }
                }
            }
            if let Some(name) = switch_endpoint {
                tab.session.update_xai_priority_tier(name.clone(), true);
                if let Some(cached) = tab.session.endpoints.iter_mut()
                    .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name.as_str()))
                {
                    cached["xai_priority_tier"] = serde_json::json!(true);
                }
            }
        }

        if show_full_history_clicked {
            self.agent_tabs[self.agent_active].show_full_history = true;
        }

        ui.add_space(6.0);
    }

    fn draw_source_control_panel(&mut self, ui: &mut egui::Ui) {
        // ── No-repo state: offer to initialize / publish (VS Code parity) ──
        if self.git.is_none() {
            ui.add_space(10.0);
            let pad = 12.0;

            // Description
            ui.horizontal_wrapped(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new(
                    "The folder currently open doesn't have a Git repository. \
                     You can initialize a repository which will enable source \
                     control features powered by Git.")
                    .size(11.5).color(egui::Color32::from_gray(180)));
            });
            ui.add_space(10.0);

            // Initialize Repository button
            ui.horizontal(|ui| {
                ui.add_space(pad);
                let btn_w = ui.available_width() - pad;
                let resp = ui.add_sized(
                    egui::vec2(btn_w, 28.0),
                    egui::Button::new(
                        egui::RichText::new("Initialize Repository")
                            .color(egui::Color32::WHITE).size(12.0),
                    )
                    .fill(egui::Color32::from_rgb(0, 120, 212))
                    .rounding(3.0),
                );
                if resp.clicked() {
                    match git2::Repository::init(&self.cwd) {
                        Ok(_) => {
                            self.git = crate::git::GitState::open(&self.cwd);
                            self.git_error = None;
                            self.status = "Repository initialized".into();
        }
                        Err(e) => self.git_error = Some(e.to_string()),
    }
}
            });

            ui.add_space(14.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new(
                    "You can directly publish this folder to a GitHub \
                     repository to share it and back it up.")
                    .size(11.5).color(egui::Color32::from_gray(180)));
            });
            ui.add_space(10.0);

            // Publish to GitHub — opens the new-repo page in the browser.
            // (Full OAuth-backed publish is on the roadmap; this gives a
            //  working path today without bundling auth.)
            ui.horizontal(|ui| {
                ui.add_space(pad);
                let btn_w = ui.available_width() - pad;
                let resp = ui.add_sized(
                    egui::vec2(btn_w, 28.0),
                    egui::Button::new(
                        egui::RichText::new("Publish to GitHub")
                            .color(egui::Color32::WHITE).size(12.0),
                    )
                    .fill(egui::Color32::from_rgb(0, 120, 212))
                    .rounding(3.0),
                );
                if resp.clicked() {
                    let _ = std::process::Command::new("open")
                        .arg("https://github.com/new")
                        .spawn();
}
            });

            // Show any error from a failed init
            if let Some(err) = &self.git_error {
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(pad);
                    ui.label(egui::RichText::new(format!("⚠  {err}"))
                        .size(11.0)
                        .color(egui::Color32::from_rgb(255, 130, 110)));
                });
            }
            return;
        }

        // Snapshot per-side data so we don't hold a borrow of self.git across
        // mutating actions (stage / unstage / commit).
        let (workdir, staged, unstaged, branch, has_origin, scanning, truncated) = {
            let g = self.git.as_ref().unwrap();
            (g.workdir().to_path_buf(),
             g.staged.clone(),
             g.unstaged.clone(),
             g.branch.clone(),
             g.has_origin,
             g.scanning(),
             g.truncated)
        };

        // ── Branch line ──
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(12.0, 14.0), egui::Sense::hover(),
            );
            paint_branch_icon(ui.painter(), rect.center(),
                egui::Color32::from_gray(180));
            ui.label(egui::RichText::new(&branch).size(11.5)
                .color(egui::Color32::from_gray(220)));
        });
        ui.add_space(6.0);

        // ── Sync row ──
        // With an origin: Fetch / Pull / Push (run on a background thread).
        // Without one: an inline "add origin remote" field (VSCode's Add Remote).
        if has_origin {
            let mut remote_op: Option<crate::git::RemoteOp> = None;
            let busy = self.git_task.is_some();
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.add(egui::Button::new(egui::RichText::new("⟳ Fetch").size(11.0))
                        .rounding(3.0)).clicked() {
                        remote_op = Some(crate::git::RemoteOp::Fetch);
    }
                    if ui.add(egui::Button::new(egui::RichText::new("↓ Pull").size(11.0))
                        .rounding(3.0)).clicked() {
                        remote_op = Some(crate::git::RemoteOp::Pull);
    }
                    if ui.add(egui::Button::new(egui::RichText::new("↑ Push").size(11.0))
                        .rounding(3.0)).clicked() {
                        remote_op = Some(crate::git::RemoteOp::Push);
    }
                });
                if busy { ui.spinner(); }
            });
            if let Some(op) = remote_op { self.start_remote_op(op); }
        } else {
            // Trigger the one-time gh availability check (no-op once known).
            self.start_gh_check();
            let busy = self.git_task.is_some();

            if self.gh_ready == Some(true) {
                // ── gh is ready → VSCode-style "Publish to GitHub" ──
                let mut publish: Option<bool> = None; // Some(private?)
                // Owner/org field (github.com), e.g. windingcreek or vulkgryph.
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.add_sized(
                        egui::vec2((ui.available_width() - 8.0).max(80.0), 22.0),
                        egui::TextEdit::singleline(&mut self.publish_owner)
                            .hint_text("Owner (blank = your account)")
                            .frame(true),
                    );
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.add_enabled_ui(!busy, |ui| {
                        if ui.add(egui::Button::new(egui::RichText::new(
                                "Publish to GitHub").color(egui::Color32::WHITE).size(11.0))
                                .fill(egui::Color32::from_rgb(0, 120, 212))
                                .rounding(3.0))
                            .on_hover_text("Create a private repo on github.com and push")
                            .clicked() {
                            publish = Some(true); // private by default
        }
                        if ui.add(egui::Button::new(egui::RichText::new("Public").size(11.0))
                                .rounding(3.0))
                            .on_hover_text("Publish as a public repository")
                            .clicked() {
                            publish = Some(false);
        }
                    });
                    if busy { ui.spinner(); }
                });
                if let Some(private) = publish { self.start_publish(private); }
            } else {
                // ── gh missing (or still checking) → manual Add Remote field ──
                let panel_w = ui.available_width();
                let mut do_add = false;
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let input_w = (panel_w - 70.0).max(80.0);
                    let resp = ui.add_sized(
                        egui::vec2(input_w, 22.0),
                        egui::TextEdit::singleline(&mut self.remote_url_input)
                            .hint_text("Add origin URL…")
                            .frame(true),
                    );
                    let enter = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add(egui::Button::new(egui::RichText::new("Add").size(11.0))
                        .rounding(3.0)).clicked() || enter {
                        do_add = true;
    }
                });
                if do_add {
                    let url = std::mem::take(&mut self.remote_url_input);
                    self.add_origin(url);
}
            }
        }
        ui.add_space(8.0);

        // ── Commit message box + full-width Commit button (VSCode-style) ──
        let panel_w = ui.available_width();
        let outer_pad = 8.0;
        let frame_w = (panel_w - outer_pad * 2.0).max(120.0);
        let staged_count = staged.len();
        let msg_ready = !self.commit_msg.trim().is_empty() && staged_count > 0;
        let mut do_commit = false;

        // Message input (thin, single-card like VSCode's SCM input)
        ui.horizontal(|ui| {
            ui.add_space(outer_pad);
            let resp = ui.add_sized(
                egui::vec2(frame_w, 46.0),
                egui::TextEdit::multiline(&mut self.commit_msg)
                    .hint_text("Message (⌘Enter to commit)")
                    .desired_rows(2)
                    .frame(true),
            );
            let key = resp.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter)
                    && (i.modifiers.ctrl || i.modifiers.mac_cmd));
            if msg_ready && key { do_commit = true; }
        });
        ui.add_space(6.0);

        // Full-width blue Commit button
        ui.horizontal(|ui| {
            ui.add_space(outer_pad);
            let (rect, btn) = ui.allocate_exact_size(
                egui::vec2(frame_w, 26.0), egui::Sense::click(),
            );
            let bg = if msg_ready && btn.hovered() {
                egui::Color32::from_rgb(17, 119, 187)
            } else if msg_ready {
                egui::Color32::from_rgb(14, 99, 156)
            } else {
                egui::Color32::from_gray(58)
            };
            ui.painter().rect_filled(rect, 4.0, bg);
            let fg = if msg_ready { egui::Color32::WHITE }
                     else         { egui::Color32::from_gray(130) };
            let c = rect.center();
            // "✓ Commit", centered as a unit
            paint_check_icon(ui.painter(), egui::pos2(c.x - 32.0, c.y), fg);
            ui.painter().text(
                egui::pos2(c.x - 20.0, c.y),
                egui::Align2::LEFT_CENTER,
                "Commit",
                egui::FontId::proportional(12.5),
                fg,
            );
            if msg_ready && btn.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if msg_ready && btn.clicked() { do_commit = true; }
            let _ = btn.on_hover_text(format!("Commit {} staged change{}",
                staged_count, if staged_count == 1 { "" } else { "s" }));
        });
        ui.add_space(8.0);

        // ── Error banner ──
        if let Some(err) = self.git_error.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(format!("⚠  {err}"))
                    .size(11.0)
                    .color(egui::Color32::from_rgb(255, 130, 110)));
            });
            ui.add_space(4.0);
        }

        // ── File-list sections ──
        let mut to_stage:   Vec<PathBuf> = Vec::new();
        let mut to_unstage: Vec<PathBuf> = Vec::new();
        let mut to_open:    Option<PathBuf> = None;

        // Flatten both sections into one uniform-height list so the scroll area
        // can virtualize it — with tens of thousands of changes, rendering every
        // row each frame is the difference between smooth and unusable.
        enum ScItem<'a> {
            Header(&'static str, usize),
            File(&'a PathBuf, crate::git::FileStatus, bool), // path, status, is_staged
        }
        let mut items: Vec<ScItem> =
            Vec::with_capacity(staged.len() + unstaged.len() + 2);
        if !staged.is_empty() {
            items.push(ScItem::Header("Staged Changes", staged.len()));
            for (p, st) in &staged { items.push(ScItem::File(p, *st, true)); }
        }
        if !unstaged.is_empty() {
            items.push(ScItem::Header("Changes", unstaged.len()));
            for (p, st) in &unstaged { items.push(ScItem::File(p, *st, false)); }
        }

        if truncated {
            // Say so rather than silently under-reporting: in a working tree
            // this large the untracked side was collapsed to directories.
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(
                    "Large working tree — untracked folders are collapsed and \
                     the change list is capped.")
                    .size(11.0).color(egui::Color32::from_gray(140)));
            });
        }

        if items.is_empty() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(
                    if scanning { "Scanning…" } else { "No changes" })
                    .size(11.5).color(egui::Color32::from_gray(140)));
            });
        } else {
            egui::ScrollArea::vertical()
                .id_salt("sc_panel_scroll")
                .auto_shrink([false, false])
                .show_rows(ui, SC_ROW_H, items.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        match &items[i] {
                            ScItem::Header(label, count) =>
                                sc_section_header(ui, label, *count),
                            ScItem::File(path, st, is_staged) => {
                                let row = sc_row(ui, &workdir, path, *st, *is_staged);
                                if row.open { to_open = Some((*path).clone()); }
                                if row.action {
                                    if *is_staged { to_unstage.push((*path).clone()); }
                                    else          { to_stage.push((*path).clone()); }
                }
            }
        }
    }
                });
        }

        // Apply actions after the immutable snapshot loop ends.
        let git = self.git.as_mut().unwrap();
        for path in to_stage {
            if let Err(e) = git.stage(&path) { self.git_error = Some(e); }
        }
        for path in to_unstage {
            if let Err(e) = git.unstage(&path) { self.git_error = Some(e); }
        }
        if do_commit {
            let msg = std::mem::take(&mut self.commit_msg);
            match git.commit(&msg) {
                Ok(()) => {
                    self.git_error = None;
                    self.status = "Committed".into();
                    self.gutter_dirty = true; // HEAD moved — recompute bars
                    self.blame_path = None;   // …and blame
}
                Err(e) => { self.git_error = Some(e); self.commit_msg = msg; }
            }
        }
        // Clicking a change in the SC panel opens a diff view (VSCode behavior).
        if let Some(path) = to_open {
            self.open_diff(path);
        }
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        let w = 420.0f32;
        let mut changed = false;
        let mut close   = false;
        let mut enable_updates_now = false;
        let mut open_onboarding = false;

        egui::Window::new("settings_window")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([w, 0.0])
            .frame(egui::Frame::popup(ctx.style().as_ref())
                .fill(egui::Color32::from_rgb(37, 37, 38))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
                .rounding(6.0))
            .show(ctx, |ui| {
                ui.set_min_width(w);

                // Title bar
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("Settings")
                        .size(15.0).strong().color(egui::Color32::from_gray(230)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(10.0);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(20.0, 20.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 3.0,
                                egui::Color32::from_rgba_premultiplied(255,255,255,15));
                        }
                        let s = egui::Stroke::new(1.5_f32, egui::Color32::from_gray(160));
                        let c = rect.center();
                        let d = 5.0;
                        ui.painter().line_segment([c + egui::vec2(-d,-d), c + egui::vec2(d,d)], s);
                        ui.painter().line_segment([c + egui::vec2( d,-d), c + egui::vec2(-d,d)], s);
                        if resp.clicked() { close = true; }
                    });
                });
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(8.0);

                // The window is centred and sized to its contents, so once the
                // settings list grew past the screen the bottom of it simply
                // had nowhere to be — no scrollbar, no way to reach it, and on
                // a shorter display that is most of the list. Bounded to the
                // screen with the title bar kept out of the scroll, so what is
                // being scrolled is always identifiable.
                let body_max = (ctx.screen_rect().height() - 160.0).max(200.0);
                egui::ScrollArea::vertical()
                    .id_salt("settings_body")
                    .max_height(body_max)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {

                let s = &mut self.settings;
                let lbl = |ui: &mut egui::Ui, text: &str| {
                    ui.label(egui::RichText::new(text)
                        .size(11.5).color(egui::Color32::from_gray(180)));
                };

                // ── Editor ────────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "EDITOR"); });
                ui.add_space(4.0);

                // Font size
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Font Size");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        if ui.add(egui::Button::new("+").min_size(egui::vec2(22.0, 22.0))).clicked() {
                            s.font_size = (s.font_size + 1.0).min(32.0); changed = true;
                        }
                        ui.label(egui::RichText::new(format!("{:.0}", s.font_size))
                            .size(13.0).monospace().color(egui::Color32::from_gray(220)));
                        if ui.add(egui::Button::new("−").min_size(egui::vec2(22.0, 22.0))).clicked() {
                            s.font_size = (s.font_size - 1.0).max(8.0); changed = true;
                        }
                    });
                });
                ui.add_space(6.0);

                // Tab width
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Tab Width");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        for w in [2u8, 4, 8] {
                            let active = s.tab_width == w;
                            let btn = ui.add(egui::Button::new(format!("{w}"))
                                .min_size(egui::vec2(28.0, 22.0))
                                .fill(if active { egui::Color32::from_rgb(14, 99, 156) }
                                      else      { egui::Color32::from_gray(55) }));
                            if btn.clicked() { s.tab_width = w; changed = true; }
                        }
                    });
                });
                ui.add_space(6.0);

                // Insert spaces
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Insert Spaces on Tab");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.insert_spaces;
                        ui.checkbox(&mut s.insert_spaces, "");
                        if s.insert_spaces != old { changed = true; }
                    });
                });
                ui.add_space(6.0);

                // Word wrap
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Word Wrap (Alt+Z)");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.word_wrap;
                        ui.checkbox(&mut s.word_wrap, "");
                        if s.word_wrap != old { changed = true; }
                    });
                });
                ui.add_space(6.0);

                // Minimap
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Minimap");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.minimap;
                        ui.checkbox(&mut s.minimap, "");
                        if s.minimap != old { changed = true; }
                    });
                });

                // Auto-close brackets
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Auto-close Brackets");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.auto_close_brackets;
                        ui.checkbox(&mut s.auto_close_brackets, "");
                        if s.auto_close_brackets != old { changed = true; }
                    });
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Terminal ──────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "TERMINAL"); });
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Font Size");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        if ui.add(egui::Button::new("+").min_size(egui::vec2(22.0, 22.0))).clicked() {
                            s.terminal_font_size = (s.terminal_font_size + 1.0).min(32.0); changed = true;
                        }
                        ui.label(egui::RichText::new(format!("{:.0}", s.terminal_font_size))
                            .size(13.0).monospace().color(egui::Color32::from_gray(220)));
                        if ui.add(egui::Button::new("−").min_size(egui::vec2(22.0, 22.0))).clicked() {
                            s.terminal_font_size = (s.terminal_font_size - 1.0).max(8.0); changed = true;
                        }
                    });
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Appearance ────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "APPEARANCE"); });
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Theme");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        for name in crate::theme::theme_names() {
                            let active = s.theme == name;
                            let btn = ui.add(egui::Button::new(&name)
                                .fill(if active { egui::Color32::from_rgb(14, 99, 156) }
                                      else      { egui::Color32::from_gray(55) }));
                            if btn.clicked() { s.theme = name; changed = true; }
                        }
                    });
                });
                ui.add_space(10.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Session ───────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "SESSION"); });
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Restore Tabs & Terminals on Startup");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.restore_session;
                        ui.checkbox(&mut s.restore_session, "");
                        if s.restore_session != old { changed = true; }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Reopen All Windows on Startup");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.restore_windows;
                        ui.checkbox(&mut s.restore_windows, "");
                        if s.restore_windows != old { changed = true; }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    // A label placed directly in a horizontal layout never
                    // wraps regardless of set_max_width — the Frame content
                    // area inherits its parent's layout direction. Breaking
                    // into an explicit `ui.vertical` here is the fix (see
                    // the same gotcha noted for chat-panel cards).
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width() - 14.0);
                        ui.label(egui::RichText::new(
                            "Only applies to fully quitting and relaunching — Reload Window \
                             (Ctrl+Shift+R) always restores tabs and terminals regardless of \
                             this setting.")
                            .size(10.0).color(egui::Color32::from_gray(100)));
                    });
                });
                ui.add_space(6.0);

                // Keeping the app in the Dock is a system-level convenience, not a
                // setting of ours: there is nothing to remember, so it is an action
                // that either happens or explains why it did not.
                #[cfg(target_os = "macos")]
                {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        if ui.button("Add Forge IDE to the Dock").clicked() {
                            self.dock_status = Some(match crate::dock_install::add_to_dock() {
                                Ok(crate::dock_install::Outcome::Added) =>
                                    "Added — the Dock restarts to pick it up.".to_string(),
                                Ok(crate::dock_install::Outcome::InstalledAndAdded) =>
                                    "Copied to /Applications and added. Pinning the build \
                                     directory would break on the next build."
                                        .to_string(),
                                Ok(crate::dock_install::Outcome::AlreadyThere) =>
                                    "Already in the Dock.".to_string(),
                                Err(why) => why,
                            });
                        }
                    });
                    if let Some(status) = self.dock_status.clone() {
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            ui.vertical(|ui| {
                                ui.set_max_width(ui.available_width() - 14.0);
                                ui.label(egui::RichText::new(status)
                                    .size(10.0).color(egui::Color32::from_gray(100)));
                            });
                        });
                    }
                    ui.add_space(6.0);
                }

                ui.separator();
                ui.add_space(8.0);

                // ── Updates ──────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "UPDATES"); });
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "Check for Updates on Startup");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        let old = s.check_for_updates;
                        ui.checkbox(&mut s.check_for_updates, "");
                        if s.check_for_updates != old {
                            changed = true;
                            if s.check_for_updates { enable_updates_now = true; }
                        }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width() - 14.0);
                        ui.label(egui::RichText::new(
                            "One request to GitHub's public releases page on startup. \
                             Off by default — this is the only network call Forge IDE \
                             itself makes.")
                            .size(10.0).color(egui::Color32::from_gray(100)));
                    });
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Agent Setup ──────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "AGENT SETUP"); });
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    lbl(ui, "AI Provider");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(14.0);
                        if ui.button("Set Up…").clicked() { open_onboarding = true; }
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width() - 14.0);
                        ui.label(egui::RichText::new(
                            "Configures ~/.config/forge/config.toml — shared by this IDE's \
                             agent panel, the reference TUI, and any forge-agent invocation.")
                            .size(10.0).color(egui::Color32::from_gray(100)));
                    });
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Layout ───────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "LAYOUT"); });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("Which side panels span the full window height (terminal fills whatever's left).")
                        .size(10.0).color(egui::Color32::from_gray(100)));
                });
                ui.add_space(6.0);

                const LAYOUT_OPTIONS: [(bool, bool, &str); 4] = [
                    (false, false, "Terminal Full Width"),
                    (true,  false, "File Tree Full Height"),
                    (false, true,  "Agent Panel Full Height"),
                    (true,  true,  "Both Full Height"),
                ];
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    for (file_full, agent_full, caption) in LAYOUT_OPTIONS {
                        let selected = s.file_tree_full_height == file_full
                            && s.agent_panel_full_height == agent_full;
                        ui.vertical(|ui| {
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(80.0, 56.0), egui::Sense::click());
                            paint_layout_diagram(ui.painter(), rect, file_full, agent_full, selected);
                            if resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                            if resp.clicked() {
                                s.file_tree_full_height   = file_full;
                                s.agent_panel_full_height = agent_full;
                                changed = true;
                            }
                            ui.add_space(2.0);
                            ui.set_max_width(80.0);
                            ui.label(egui::RichText::new(caption).size(9.0)
                                .color(if selected { egui::Color32::from_gray(220) } else { egui::Color32::from_gray(120) }));
                        });
                        ui.add_space(8.0);
                    }
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);

                // ── Agent ────────────────────────────────────────────────────
                ui.horizontal(|ui| { ui.add_space(14.0); lbl(ui, "AGENT"); });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width() - 14.0);
                        ui.label(egui::RichText::new(
                            "Default permission mode for new agent tabs — each tab can still \
                             override this individually from the dropdown next to its model picker. \
                             Note: reads are always unrestricted regardless of mode — these tiers \
                             only affect writes and shell commands.")
                            .size(10.0).color(egui::Color32::from_gray(100)));
                    });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    for mode in [
                        crate::settings::AgentPermissionMode::AlwaysAsk,
                        crate::settings::AgentPermissionMode::AutoApprove,
                        crate::settings::AgentPermissionMode::DangerouslySkipAll,
                    ] {
                        let selected = s.default_agent_permission_mode == mode;
                        let color = match mode {
                            crate::settings::AgentPermissionMode::AlwaysAsk          => egui::Color32::from_gray(210),
                            crate::settings::AgentPermissionMode::AutoApprove        => egui::Color32::from_rgb(220, 190, 120),
                            crate::settings::AgentPermissionMode::DangerouslySkipAll => egui::Color32::from_rgb(230, 110, 100),
                        };
                        let resp = ui.selectable_label(selected,
                            egui::RichText::new(mode.label()).size(11.0).color(color));
                        if resp.on_hover_text(mode.description()).clicked() && !selected {
                            s.default_agent_permission_mode = mode;
                            changed = true;
                        }
                        ui.add_space(6.0);
                    }
                });
                ui.add_space(6.0);

                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new("Settings are saved automatically.")
                        .size(10.5).color(egui::Color32::from_gray(100)));
                });
                // Build freshness — "Reload Window" restarts the process
                // using whatever's already on disk, it doesn't trigger a
                // rebuild, so there was previously no way to actually
                // confirm a reload picked up a new build versus silently
                // still running the old one.
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(egui::RichText::new(format!("Build: {}", running_binary_age()))
                        .size(10.5).color(egui::Color32::from_gray(100)));
                });
                ui.add_space(8.0);

                });

                if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
            });

        if changed { crate::settings::save(&self.settings); }
        if enable_updates_now { self.update_check_rx = Some(crate::update_check::spawn_check()); }
        if open_onboarding { self.onboarding = Some(crate::onboarding::OnboardingStep::ProviderPicker); }
        if close   { self.settings_open = false; }
    }

    /// The VSCode-style SSH quick-pick overlay: two-step flow.
    fn draw_ssh_overlay(&mut self, ctx: &egui::Context) {
        let w = (ctx.screen_rect().width() * 0.55).min(560.0);
        let hosts = self.ssh_hosts.clone();
        let mut connect_host:         Option<crate::ssh::SshHost> = None; // current window
        let mut connect_host_new_win: Option<crate::ssh::SshHost> = None; // new window
        let mut open_config  = false;
        let mut do_add       = false;
        let mut close        = false;

        // egui::Window avoids the same-frame open/dismiss race that Area has —
        // it doesn't fire click-outside on the frame it first appears.
        let response = egui::Window::new("ssh_picker")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 40.0])
            .default_width(w)   // width hint only, height is content-driven
            .frame(egui::Frame::popup(ctx.style().as_ref())
                .fill(egui::Color32::from_rgb(37, 37, 38))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
                .rounding(6.0))
            .show(ctx, |ui| {
                ui.set_min_width(w);
                ui.spacing_mut().item_spacing.y = 0.0;

                // ── Step 1: choose window mode ────────────────────────────────
                if self.ssh_overlay_step == SshOverlayStep::ChooseWindow {
                    let hint = egui::RichText::new("Select an option to open a Remote Window")
                        .size(11.5).color(egui::Color32::from_gray(160));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| { ui.add_space(12.0); ui.label(hint); });
                    ui.add_space(4.0);
                    ui.separator();

                    let options = [
                        ("Connect to Host…",              "Open in a new window",    SshOverlayStep::PickHostNewWindow,     "Remote-SSH"),
                        ("Connect Current Window to Host…","Replace this workspace",  SshOverlayStep::PickHostCurrentWindow, "Remote-SSH"),
                    ];
                    for (label, desc, step, tag) in &options {
                        let avail = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail, 44.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 0.0,
                                egui::Color32::from_rgb(14, 99, 156));
        }
                        let cy = rect.center().y;
                        ui.painter().text(
                            egui::pos2(rect.left() + 14.0, cy - 7.0),
                            egui::Align2::LEFT_CENTER, *label,
                            egui::FontId::proportional(13.0),
                            egui::Color32::from_gray(225));
                        ui.painter().text(
                            egui::pos2(rect.left() + 14.0, cy + 9.0),
                            egui::Align2::LEFT_CENTER, *desc,
                            egui::FontId::proportional(11.0),
                            egui::Color32::from_gray(130));
                        ui.painter().text(
                            egui::pos2(rect.right() - 12.0, cy),
                            egui::Align2::RIGHT_CENTER, *tag,
                            egui::FontId::proportional(10.5),
                            egui::Color32::from_gray(100));
                        if resp.clicked() {
                            self.ssh_overlay_step  = *step;
                            self.ssh_overlay_query = String::new();
        }
    }
                    ui.separator();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Press Escape to close")
                            .size(10.5).color(egui::Color32::from_gray(80)));
                    });
                    ui.add_space(6.0);
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
                    return;
}

                // ── "Add new SSH host" sub-prompt ─────────────────────────────
                if let Some(ref mut input) = self.ssh_add_input {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Enter SSH connection command")
                            .size(11.0).color(egui::Color32::from_gray(160)));
                    });
                    ui.add_space(4.0);
                    let resp = ui.add_sized(
                        egui::vec2(w - 16.0, 28.0),
                        egui::TextEdit::singleline(input)
                            .hint_text("ssh user@hostname  or  user@hostname")
                            .frame(false)
                            .margin(egui::vec2(8.0, 0.0))
                            .font(egui::FontId::monospace(13.0)),
                    );
                    resp.request_focus();
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        do_add = true;
    }
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Press Enter to add to ~/.ssh/config, Escape to cancel")
                            .size(10.5).color(egui::Color32::from_gray(110)));
                    });
                    ui.add_space(6.0);
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
                    return;
}

                // ── Step 2 header with back button ───────────────────────────
                {
                    let label = match self.ssh_overlay_step {
                        SshOverlayStep::PickHostNewWindow     => "Connect to Host (New Window)",
                        SshOverlayStep::PickHostCurrentWindow => "Connect Current Window to Host",
                        _ => "",
                    };
                    let avail = ui.available_width();
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(avail, 28.0), egui::Sense::click());
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        ui.painter().rect_filled(rect, 0.0,
                            egui::Color32::from_rgba_premultiplied(255,255,255,6));
    }
                    ui.painter().text(
                        egui::pos2(rect.left() + 10.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("← {label}"),
                        egui::FontId::proportional(11.5),
                        egui::Color32::from_gray(160));
                    if resp.clicked() {
                        self.ssh_overlay_step = SshOverlayStep::ChooseWindow;
    }
                    let _ = resp.on_hover_text("Back to window selection");
}
                ui.separator();

                // ── Search box ────────────────────────────────────────────────
                let resp = ui.add_sized(
                    egui::vec2(w, 36.0),
                    egui::TextEdit::singleline(&mut self.ssh_overlay_query)
                        .hint_text("Select configured SSH host or enter user@host")
                        .frame(false)
                        .margin(egui::vec2(12.0, 0.0))
                        .font(egui::FontId::proportional(13.5)),
                );
                resp.request_focus();
                // Enter on the search box = connect to typed address directly
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let q = self.ssh_overlay_query.trim().to_string();
                    if !q.is_empty() {
                        let (user, host) = q.split_once('@')
                            .map(|(u, h)| (u.to_string(), h.to_string()))
                            .unwrap_or_else(|| (std::env::var("USER").unwrap_or_default(), q.clone()));
                        let h = crate::ssh::SshHost {
                            name: host.clone(), host, port: 22,
                            user, key_path: String::new(), remote_dir: "~".into(),
                        };
                        if self.ssh_overlay_step == SshOverlayStep::PickHostNewWindow {
                            connect_host_new_win = Some(h);
                        } else {
                            connect_host = Some(h);
        }
    }
}
                ui.separator();

                // ── Filtered host list ────────────────────────────────────────
                let q = self.ssh_overlay_query.to_lowercase();
                let filtered: Vec<&crate::ssh::SshHost> = hosts.iter().filter(|h|
                    q.is_empty()
                    || h.name.to_lowercase().contains(&q)
                    || h.host.to_lowercase().contains(&q))
                    .collect();

                egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for host in filtered.iter() {
                        let avail = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail, 36.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 3.0,
                                egui::Color32::from_rgb(14, 99, 156));
        }
                        let cy = rect.center().y;
                        ui.painter().text(
                            egui::pos2(rect.left() + 14.0, cy - 6.0),
                            egui::Align2::LEFT_CENTER, &host.name,
                            egui::FontId::proportional(13.0),
                            egui::Color32::from_gray(225));
                        ui.painter().text(
                            egui::pos2(rect.left() + 14.0, cy + 8.0),
                            egui::Align2::LEFT_CENTER,
                            format!("{}@{}", host.user, host.host),
                            egui::FontId::proportional(10.5),
                            egui::Color32::from_gray(130));
                        // Dispatch based on the window mode chosen in step 1
                        if resp.clicked() {
                            match self.ssh_overlay_step {
                                SshOverlayStep::PickHostNewWindow     =>
                                    connect_host_new_win = Some((*host).clone()),
                                _ =>
                                    connect_host = Some((*host).clone()),
            }
        }
    }

                    ui.separator();

                    let items = [
                        ("  + Add New SSH Host...",      false),
                        ("  Configure SSH Hosts...",     true),
                    ];
                    for (label, is_config) in &items {
                        let avail = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail, 30.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 3.0,
                                egui::Color32::from_rgba_premultiplied(255,255,255,10));
        }
                        ui.painter().text(
                            egui::pos2(rect.left() + 4.0, rect.center().y),
                            egui::Align2::LEFT_CENTER, *label,
                            egui::FontId::proportional(12.5),
                            egui::Color32::from_gray(175));
                        if resp.clicked() {
                            if *is_config { open_config = true; }
                            else { self.ssh_add_input = Some(String::new()); }
        }
    }
                });

                if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
            });

        // Dismiss on click-outside — but skip frame 0 so the click that
        // opened the overlay isn't immediately detected as a click-outside.
        self.ssh_overlay_frame = self.ssh_overlay_frame.saturating_add(1);
        if self.ssh_overlay_frame > 1 {
            if let Some(ref r) = response {
                let window_rect   = r.response.rect;
                let primary_click = ctx.input(|i| i.pointer.primary_clicked());
                let interact_pos  = ctx.input(|i| i.pointer.interact_pos());
                let dragging      = ctx.dragged_id().is_some();
                if primary_click && !dragging {
                    if let Some(pos) = interact_pos {
                        if !window_rect.contains(pos) { close = true; }
    }
}
            }
        }

        if do_add {
            if let Some(ref input) = self.ssh_add_input.clone() {
                let addr = input.trim().strip_prefix("ssh ").unwrap_or(input.trim());
                if !addr.is_empty() {
                    let _ = crate::ssh::add_ssh_config_host(addr);
                    self.ssh_hosts = crate::ssh::load_hosts();
}
            }
            self.ssh_add_input = None;
            self.ssh_overlay   = false;
        }
        if open_config {
            self.open_file(crate::ssh::ssh_config_path());
            self.ssh_overlay = false;
        }
        if let Some(host) = connect_host {
            self.ssh_form    = host;
            self.ssh_overlay = false;
            self.ssh_connect();
        }
        if let Some(host) = connect_host_new_win {
            self.ssh_overlay        = false;
            self.pending_new_window = Some(NewWindowSpec {
                ssh_host: Some(host),
                ..Default::default()
            });
        }
        if close { self.ssh_overlay = false; self.ssh_add_input = None; }
    }

    /// Remote file tree rendered in the Explorer sidebar when SSH-connected.
    /// Render the SSH remote terminal (dedicated grid, keyboard → ssh_shell).
    fn draw_ssh_terminal(&mut self, ui: &mut egui::Ui) {
        let bg       = egui::Color32::from_rgb(14, 14, 14);
        let panel_rect = ui.max_rect();
        ui.painter().rect_filled(panel_rect, 0.0, bg);

        let font_id  = egui::FontId::monospace(self.settings.font_size);
        let row_h    = ui.fonts(|f| f.row_height(&font_id));
        let char_w   = ui.fonts(|f| f.glyph_width(&font_id, ' '));
        let focus_id = ui.id().with("ssh_term_focus");

        // Click to focus / click outside to blur
        let click = ui.interact(panel_rect, focus_id.with("click"), egui::Sense::click());
        if click.clicked() { self.ssh_term_focused = true; }
        if ui.input(|i| i.pointer.any_click())
            && !panel_rect.contains(
                ui.input(|i| i.pointer.interact_pos().unwrap_or_default()))
        {
            self.ssh_term_focused = false;
        }

        // Render grid in a scroll area that sticks to the bottom (like a real terminal).
        let (version, scrollback_version, cur_row, cur_col, cursor_visible) = {
            let g = self.ssh_term.lock().unwrap();
            let (r, c) = g.cursor();
            (g.version(), g.scrollback_version(), r, c, g.cursor_visible())
        };
        // Scrollback and viewport cached separately — see `Terminal::draw_sized`'s
        // equivalent cache for why rebuilding either from scratch every frame
        // (or lumping them into one job) is costly.
        let scrollback_galley = match &self.ssh_term_cached_scrollback_galley {
            Some((v, fid, g)) if *v == scrollback_version && *fid == font_id => g.clone(),
            _ => {
                let job = { let g = self.ssh_term.lock().unwrap(); g.to_scrollback_layout_job(font_id.clone()) };
                let galley = ui.fonts(|f| f.layout_job(job));
                self.ssh_term_cached_scrollback_galley = Some((scrollback_version, font_id.clone(), galley.clone()));
                galley
            }
        };
        let galley = match &self.ssh_term_cached_galley {
            Some((v, fid, g)) if *v == version && *fid == font_id => g.clone(),
            _ => {
                let job = { let g = self.ssh_term.lock().unwrap(); g.to_viewport_layout_job(font_id.clone()) };
                let galley = ui.fonts(|f| f.layout_job(job));
                self.ssh_term_cached_galley = Some((version, font_id.clone(), galley.clone()));
                galley
            }
        };
        let scrollback_h = scrollback_galley.size().y;
        let grid_h    = scrollback_h + galley.size().y;
        // Viewport-relative only — added to the already scrollback-offset
        // `text_pos` below, not to the original top-of-scrollback one.
        let cursor_y  = cur_row as f32 * row_h;

        egui::ScrollArea::vertical()
            .id_salt("ssh_term_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            // See the local terminal's identical `drag_to_scroll(false)` fix.
            .drag_to_scroll(false)
            .show(ui, |ui| {
                let text_pos = egui::pos2(
                    panel_rect.left() + 4.0,
                    ui.cursor().min.y,
                );
                // Reserve the full grid height so the scroll area knows the content size.
                let (content_rect, _) = ui.allocate_exact_size(
                    egui::vec2(panel_rect.width() - 8.0, grid_h),
                    egui::Sense::hover(),
                );
                ui.painter().galley(text_pos, scrollback_galley, egui::Color32::WHITE);
                let text_pos = egui::pos2(text_pos.x, text_pos.y + scrollback_h);
                ui.painter().galley(text_pos, galley, egui::Color32::WHITE);

                // Blinking block cursor when focused
                if self.ssh_term_focused && cursor_visible {
                    let t = ui.ctx().input(|i| i.time);
                    let blink_on = (t * 2.0).floor() as i64 % 2 == 0;
                    if blink_on {
                        let cx = text_pos.x + cur_col as f32 * char_w;
                        let cy = text_pos.y + cursor_y;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(cx, cy),
                                egui::vec2(char_w, row_h)),
                            0.0,
                            egui::Color32::from_rgba_premultiplied(200, 200, 200, 180));
                    }
                    // Scroll to keep cursor visible when new output arrives
                    let cursor_rect = egui::Rect::from_min_size(
                        egui::pos2(text_pos.x, text_pos.y + cursor_y),
                        egui::vec2(char_w, row_h));
                    ui.scroll_to_rect(cursor_rect, None);
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
                }
                let _ = content_rect;
            });

        // Keyboard → ssh_shell.tx
        if self.ssh_term_focused {
            let events = ui.input(|i| i.events.clone());
            for event in &events {
                let bytes: Option<Vec<u8>> = match event {
                    egui::Event::Text(t) => Some(t.as_bytes().to_vec()),
                    egui::Event::Key { key, pressed: true, modifiers, .. } =>
                        crate::terminal::key_to_pty(*key, *modifiers),
                    _ => None,
                };
                if let Some(b) = bytes {
                    if let Some(shell) = &self.ssh_shell {
                        let _ = shell.tx.try_send(b);
                    }
                }
            }
        }
    }

    fn draw_remote_explorer(&mut self, ui: &mut egui::Ui) {
        let host_name = self.ssh.as_ref().map(|s| s.host.name.clone())
            .unwrap_or_default();

        // Root label (like VSCode's workspace name)
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("⏻  {host_name}"))
                .size(11.0).strong()
                .color(egui::Color32::from_rgb(80, 200, 120)));
        });
        ui.add_space(4.0);

        let mut open_path: Option<String> = None;
        let mut push_dir:  Option<String> = None;
        let mut pop = false;
        // Remote row geometry, so a dropped file can be attributed to the
        // directory it landed on (see the drop handling after the scroll area).
        let panel_rect = ui.max_rect();
        let mut row_dirs: Vec<(egui::Rect, String)> = Vec::new();

        egui::ScrollArea::vertical()
            .id_salt("remote_explorer_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;

                // Back button when drilled in
                if self.ssh_tree.len() > 1 {
                    let avail_w = ui.available_width();
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(avail_w, 22.0), egui::Sense::click());
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        ui.painter().rect_filled(rect, 0.0,
                            egui::Color32::from_rgba_premultiplied(255,255,255,8));
    }
                    // Show current dir name in breadcrumb style
                    let current = self.ssh_tree.last()
                        .map(|(p, _)| p.rsplit('/').next().unwrap_or("..").to_string())
                        .unwrap_or_default();
                    ui.painter().text(
                        egui::pos2(rect.left() + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("← {current}"),
                        egui::FontId::proportional(12.0),
                        egui::Color32::from_gray(170));
                    if resp.clicked() { pop = true; }
}

                if let Some((_, entries)) = self.ssh_tree.last() {
                    for entry in entries.clone() {
                        let avail_w = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail_w, 22.0), egui::Sense::click());
                        // Directory rows are drop targets; files are not (their
                        // parent is the browsed directory, which is the fallback).
                        if entry.is_dir { row_dirs.push((rect, entry.path.clone())); }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 0.0,
                                egui::Color32::from_rgba_premultiplied(255,255,255,8));
        }
                        // Icon
                        let icon_rect = egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 6.0, rect.center().y - 7.0),
                            egui::vec2(14.0, 14.0));
                        if entry.is_dir {
                            crate::icons::paint_key(ui.painter(), icon_rect, "folder");
                            // Chevron for dirs
                            let s = egui::Stroke::new(1.2_f32, egui::Color32::from_gray(120));
                            let cx = rect.left() + 22.0;
                            let cy = rect.center().y;
                            ui.painter().line_segment(
                                [egui::pos2(cx-3.0, cy-3.0), egui::pos2(cx+1.0, cy)], s);
                            ui.painter().line_segment(
                                [egui::pos2(cx+1.0, cy), egui::pos2(cx-3.0, cy+3.0)], s);
                        } else {
                            let p = std::path::Path::new(&entry.name);
                            crate::icons::paint(ui.painter(), icon_rect, p, false);
        }
                        // Name
                        ui.painter().text(
                            egui::pos2(rect.left() + 28.0, rect.center().y),
                            egui::Align2::LEFT_CENTER, &entry.name,
                            egui::FontId::proportional(12.5),
                            egui::Color32::from_gray(220));
                        if resp.clicked() {
                            if entry.is_dir { push_dir = Some(entry.path.clone()); }
                            else            { open_path = Some(entry.path.clone()); }
        }
    }
                } else if self.ssh_nav_rx.is_some() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| { ui.add_space(8.0); ui.spinner(); });
}
            });

        // ── Dropped files upload to the remote ────────────────────────────
        // Local drops copy into a local folder; on a remote workspace the
        // equivalent is an upload. Target is the row under the cursor when it is
        // a directory, otherwise the directory currently being browsed.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if !dropped.is_empty() {
            if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                if panel_rect.contains(pos) {
                    let target = row_dirs.iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .map(|(_, p)| p.clone())
                        .or_else(|| self.ssh_tree.last().map(|(p, _)| p.clone()));
                    if let Some(dir) = target {
                        self.ssh_upload_files(dropped, dir);
                    }
                }
            }
        }

        if pop { self.ssh_tree.pop(); }
        if let Some(dir) = push_dir  { self.ssh_navigate(dir); }
        if let Some(path) = open_path { self.ssh_open_file(path); }
    }

    /// Send dropped local files to a remote directory, then refresh the view.
    ///
    /// Non-blocking: `SshConnection::fs_upload` spawns the transfer onto the
    /// connection's runtime and hands back a receiver, drained in `draw`.
    fn ssh_upload_files(&mut self, files: Vec<PathBuf>, remote_dir: String) {
        let files: Vec<PathBuf> = files.into_iter().filter(|p| {
            if p.is_dir() {
                self.output_log(
                    format!("Skipped {}: uploading folders isn't supported yet",
                            p.file_name().unwrap_or_default().to_string_lossy()),
                    OutputLevel::Warn);
                false
            } else {
                true
            }
        }).collect();
        if files.is_empty() { return; }

        let Some(conn) = self.ssh.as_ref() else { return };
        let n = files.len();
        self.ssh_upload_rx  = Some(conn.fs_upload(files, &remote_dir));
        self.ssh_upload_dir = Some(remote_dir);
        self.status = format!("Uploading {n} file(s)…");
    }

    // ── Outline / symbol-tree sidebar panel ─────────────────────────────────
    fn draw_outline_panel(&mut self, ui: &mut egui::Ui) {
        let active_path = self.buffers.get(self.active).and_then(|b| b.path.clone());
        if active_path.is_none() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("No active editor").size(11.5).weak());
            });
            return;
        }
        if self.lsp.is_none() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Outline requires a language server")
                    .size(11.5).weak());
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                if let (Some(lsp), Some(p)) = (&mut self.lsp, &active_path) {
                    self.outline_req = lsp.document_symbols(p);
                }
            }
            if self.outline_req > 0 {
                ui.label(egui::RichText::new("loading…").size(10.5).weak());
            }
        });
        ui.add_space(4.0);
        let mut jump: Option<(usize, usize)> = None;
        egui::ScrollArea::vertical().id_salt("outline_scroll").show(ui, |ui| {
            for sym in &self.outline {
                ui.horizontal(|ui| {
                    ui.add_space(10.0 + sym.depth as f32 * 12.0);
                    let (glyph, col) = symbol_kind_glyph(sym.kind);
                    ui.label(egui::RichText::new(glyph).size(11.0).color(col));
                    let resp = ui.add(egui::Label::new(
                        egui::RichText::new(&sym.name).size(12.0)
                            .color(egui::Color32::from_gray(210)))
                        .sense(egui::Sense::click()));
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        jump = Some((sym.line as usize, sym.col as usize));
                    }
                });
            }
        });
        if let Some((line, col)) = jump {
            self.pending_scroll = Some(line);
            if let Some(buf) = self.buffers.get_mut(self.active) {
                buf.cursor = (line, col);
            }
        }
    }

    fn draw_ssh_panel(&mut self, ui: &mut egui::Ui) {
        let pad = 8.0;
        let w   = (ui.available_width() - pad * 2.0).max(80.0);

        // ── Connected state ──────────────────────────────────────────────────
        if self.ssh.is_some() {
            let host_name = self.ssh.as_ref().map(|s| s.host.name.clone())
                .unwrap_or_default();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new(format!("⏻  {host_name}"))
                    .size(11.5).color(egui::Color32::from_rgb(80, 200, 120)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(pad);
                    if ui.add(egui::Button::new(
                        egui::RichText::new("Disconnect").size(10.5))
                        .rounding(3.0)).clicked()
                    {
                        self.ssh      = None;
                        self.ssh_tree = Vec::new();
                        self.ssh_shell = None;
                        self.status   = "Disconnected".into();
    }
                });
            });
            ui.add_space(6.0);
            ui.separator();

            // Remote file tree
            let mut open_path: Option<String>  = None;
            let mut push_dir:  Option<String>  = None;
            let mut pop        = false;

            egui::ScrollArea::vertical()
                .id_salt("ssh_tree_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;

                    // Breadcrumb / back button
                    if self.ssh_tree.len() > 1 {
                        let avail_w = ui.available_width();
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(avail_w, 22.0), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(rect, 0.0,
                                egui::Color32::from_rgba_premultiplied(255,255,255,10));
        }
                        ui.painter().text(
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::Align2::LEFT_CENTER, "← ..",
                            egui::FontId::proportional(12.0),
                            egui::Color32::from_gray(180));
                        if resp.clicked() { pop = true; }
    }

                    // Current directory entries
                    if let Some((_, entries)) = self.ssh_tree.last() {
                        for entry in entries.clone() {
                            let avail_w = ui.available_width();
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(avail_w, 22.0), egui::Sense::click());
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                ui.painter().rect_filled(rect, 0.0,
                                    egui::Color32::from_rgba_premultiplied(255,255,255,10));
            }
                            // Icon
                            let icon_rect = egui::Rect::from_min_size(
                                egui::pos2(rect.left() + 6.0, rect.center().y - 7.0),
                                egui::vec2(14.0, 14.0));
                            if entry.is_dir {
                                crate::icons::paint_key(ui.painter(), icon_rect, "folder");
                            } else {
                                let p = std::path::Path::new(&entry.name);
                                crate::icons::paint(ui.painter(), icon_rect, p, false);
            }
                            ui.painter().text(
                                egui::pos2(rect.left() + 24.0, rect.center().y),
                                egui::Align2::LEFT_CENTER, &entry.name,
                                egui::FontId::proportional(12.5),
                                egui::Color32::from_gray(220));
                            if entry.is_dir && !entry.size == 0 {} // silence warning
                            if resp.clicked() {
                                if entry.is_dir { push_dir = Some(entry.path.clone()); }
                                else            { open_path = Some(entry.path.clone()); }
            }
        }
                    } else if self.ssh.is_some() {
                        // Initial listing — trigger on first draw.
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);
                            ui.spinner();
                            ui.label(egui::RichText::new("Loading…").size(11.0)
                                .color(egui::Color32::from_gray(140)));
                        });
    }
                });

            if pop { self.ssh_tree.pop(); }
            if let Some(dir) = push_dir  { self.ssh_navigate(dir); }
            if let Some(path) = open_path { self.ssh_open_file(path); }
            return;
        }

        // ── Disconnected state: connection form ──────────────────────────────
        ui.add_space(6.0);

        // Saved hosts list
        if !self.ssh_hosts.is_empty() {
            for i in 0..self.ssh_hosts.len() {
                let name = self.ssh_hosts[i].name.clone();
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    if ui.add(egui::Button::new(egui::RichText::new(&name).size(11.5))
                        .rounding(3.0).min_size(egui::vec2(w - 28.0, 22.0))).clicked()
                    {
                        self.ssh_form = self.ssh_hosts[i].clone();
    }
                    if ui.add(egui::Button::new(egui::RichText::new("✕").size(10.0))
                        .rounding(3.0)).clicked()
                    {
                        self.ssh_hosts.remove(i);
                        crate::ssh::save_hosts(&self.ssh_hosts);
    }
                });
                ui.add_space(2.0);
            }
            ui.separator();
            ui.add_space(4.0);
        }

        // Connection form
        let field = |ui: &mut egui::Ui, label: &str, val: &mut String, hint: &str| {
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new(label).size(10.5)
                    .color(egui::Color32::from_gray(160)));
            });
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.add_sized(egui::vec2(w, 22.0),
                    egui::TextEdit::singleline(val).hint_text(hint).frame(true));
            });
            ui.add_space(3.0);
        };

        field(ui, "Name",       &mut self.ssh_form.name,       "my-server");
        field(ui, "Host",       &mut self.ssh_form.host,       "192.168.1.1");

        // Port inline
        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.label(egui::RichText::new("Port").size(10.5)
                .color(egui::Color32::from_gray(160)));
        });
        ui.horizontal(|ui| {
            ui.add_space(pad);
            let mut port_s = self.ssh_form.port.to_string();
            if ui.add_sized(egui::vec2(w, 22.0),
                egui::TextEdit::singleline(&mut port_s).frame(true)).changed()
            {
                self.ssh_form.port = port_s.parse().unwrap_or(22);
            }
        });
        ui.add_space(3.0);

        field(ui, "User",       &mut self.ssh_form.user,       "ubuntu");
        field(ui, "Key path",   &mut self.ssh_form.key_path,   "~/.ssh/id_ed25519");
        field(ui, "Remote dir", &mut self.ssh_form.remote_dir, "/home/user");

        // Password (only if no key)
        if self.ssh_form.key_path.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new("Password").size(10.5)
                    .color(egui::Color32::from_gray(160)));
            });
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.add_sized(egui::vec2(w, 22.0),
                    egui::TextEdit::singleline(&mut self.ssh_password)
                        .password(true).frame(true));
            });
            ui.add_space(3.0);
        }

        // Error banner
        if let Some(err) = &self.ssh_error.clone() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(pad);
                ui.label(egui::RichText::new(format!("⚠  {err}"))
                    .size(10.5).color(egui::Color32::from_rgb(255, 130, 110)));
            });
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(pad);
            let busy = self.ssh_connecting;
            ui.add_enabled_ui(!busy, |ui| {
                if ui.add(egui::Button::new(
                    egui::RichText::new("Connect").color(egui::Color32::WHITE).size(11.5))
                    .fill(egui::Color32::from_rgb(14, 99, 156))
                    .rounding(3.0)
                    .min_size(egui::vec2(w - 70.0, 26.0))).clicked()
                {
                    self.ssh_connect();
}
                if !self.ssh_form.name.is_empty() {
                    if ui.add(egui::Button::new(egui::RichText::new("Save").size(11.0))
                        .rounding(3.0)).clicked()
                    {
                        let h = self.ssh_form.clone();
                        if let Some(e) = self.ssh_hosts.iter_mut().find(|x| x.name == h.name) {
                            *e = h;
                        } else {
                            self.ssh_hosts.push(h);
        }
                        crate::ssh::save_hosts(&self.ssh_hosts);
    }
}
            });
            if busy { ui.spinner(); }
        });
    }

    fn draw_search_panel(&mut self, ui: &mut egui::Ui) {
        // Project-wide search has no project without a workspace folder.
        if !self.has_folder {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Open a folder to search across files")
                    .size(12.0).color(egui::Color32::from_gray(150)));
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if ui.button("Open Folder…").clicked() { self.open_folder_dialog(); }
            });
            return;
        }
        self.search.poll();

        // ── Query input row ──
        // Reserve fixed space for the Aa toggle, give the rest to the input.
        // Allocate the row up front so the TextEdit can't push the panel wider.
        let cwd      = self.cwd.clone();
        let row_w    = ui.available_width() - 12.0;
        let aa_w     = 28.0;
        let input_w  = (row_w - aa_w - 8.0).max(60.0);

        ui.horizontal(|ui| {
            ui.add_space(6.0);

            // Input (constrained width, NOT chasing available_width)
            let resp = ui.add_sized(
                egui::vec2(input_w, 22.0),
                egui::TextEdit::singleline(&mut self.search.query)
                    .hint_text("Search…")
                    .frame(true),
            );
            if self.search.request_focus {
                resp.request_focus();
                self.search.request_focus = false;
            }
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if resp.changed() || enter {
                self.search.start(cwd.clone());
                if enter { resp.request_focus(); }
            }

            // Aa toggle
            let aa_fill = if self.search.case_sensitive {
                egui::Color32::from_rgb(0, 80, 160)
            } else {
                egui::Color32::from_gray(55)
            };
            let aa_resp = egui::Frame::none()
                .fill(aa_fill).rounding(3.0)
                .inner_margin(egui::Margin::symmetric(5.0, 2.0))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Aa").size(11.0).color(egui::Color32::WHITE))
                }).response;
            if aa_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if aa_resp.on_hover_text("Case sensitive").clicked() {
                self.search.case_sensitive = !self.search.case_sensitive;
                self.search.last_query.clear();
                self.search.start(cwd.clone());
            }
        });

        ui.add_space(6.0);

        // ── Status ──
        let status = if self.search.searching {
            "Searching…".to_string()
        } else if self.search.query.is_empty() {
            String::new()
        } else if self.search.results.is_empty() {
            "No results".to_string()
        } else {
            // count files
            let mut files = 0usize;
            let mut last: Option<&PathBuf> = None;
            for h in &self.search.results {
                if last != Some(&h.file) { files += 1; last = Some(&h.file); }
            }
            format!("{} results in {} files", self.search.results.len(), files)
        };
        if !status.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(status).size(11.0)
                    .color(egui::Color32::from_gray(150)));
            });
            ui.add_space(4.0);
        }

        // ── Results list ──
        let mut goto: Option<(PathBuf, usize)> = None;
        egui::ScrollArea::vertical()
            .id_salt("search_results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut last_file: Option<PathBuf> = None;
                for hit in &self.search.results {
                    let new_file = last_file.as_ref() != Some(&hit.file);
                    if new_file {
                        if last_file.is_some() { ui.add_space(4.0); }
                        let rel = hit.file.strip_prefix(&cwd).unwrap_or(&hit.file);
                        let name = hit.file.file_name()
                            .and_then(|n| n.to_str()).unwrap_or("");
                        let dir = rel.parent().map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default();
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new(name).size(12.0)
                                .color(egui::Color32::from_gray(220)));
                            if !dir.is_empty() {
                                ui.label(egui::RichText::new(dir).size(10.5)
                                    .color(egui::Color32::from_gray(110)));
            }
                        });
                        last_file = Some(hit.file.clone());
    }
                    // line preview
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        let line_label = format!("{:>4}", hit.line + 1);
                        let preview = egui::RichText::new(format!("{}  {}", line_label, hit.text))
                            .size(11.5)
                            .color(egui::Color32::from_gray(170));
                        let resp = ui.add(egui::Label::new(preview)
                            .sense(egui::Sense::click())
                            .truncate());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_filled(
                                resp.rect, 0.0,
                                egui::Color32::from_rgba_premultiplied(255, 255, 255, 10),
                            );
        }
                        if resp.clicked() {
                            goto = Some((hit.file.clone(), hit.line));
        }
                    });
}
            });

        if let Some((p, l)) = goto {
            self.open_file(p);
            self.pending_scroll = Some(l);
            if let Some(buf) = self.buffers.get_mut(self.active) {
                let row = l.min(buf.lines.len().saturating_sub(1));
                buf.cursor = (row, 0);
            }
        }
    }

    fn draw_quick_open(&mut self, ctx: &egui::Context) {
        if self.quick_open.is_none() { return; }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.quick_open = None;
            return;
        }

        // Take ownership so we can also borrow the rest of self freely
        let mut qo = self.quick_open.take().unwrap();
        qo.poll();
        // The listing arrives on a background thread, so keep repainting until
        // it lands — otherwise rows only appear on the next input event.
        if qo.listing() { ctx.request_repaint(); }

        let screen = ctx.screen_rect();
        let w = (screen.width() * 0.5).min(600.0);
        let h = 360.0;

        let mut open_path: Option<PathBuf> = None;
        let mut dismissed = false;
        // Navigation is deferred out of the row loop: descending mutates the
        // entry list `qo.filtered` is indexing into.
        let mut descend: Option<PathBuf> = None;
        let mut ascend = false;

        // Backspace or Left goes up a level, but only on an empty query — with
        // text typed they belong to the filter box, where stealing them would
        // make editing a query impossible.
        if qo.query.is_empty() && ctx.input(|i| {
            i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::ArrowLeft)
        }) {
            ascend = true;
        }

        egui::Area::new(egui::Id::new("quick_open"))
            .fixed_pos(egui::pos2(screen.center().x - w * 0.5, screen.top() + 80.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(37, 37, 38))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(80)))
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.set_width(w);

                        // Where we are, so navigating several levels deep stays
                        // legible when the query box only shows a filter.
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(qo.breadcrumb())
                            .size(11.0).color(egui::Color32::from_gray(140)));

                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut qo.query)
                                .font(egui::FontId::proportional(14.0))
                                .desired_width(w - 16.0)
                                .hint_text("Filter this folder…")
                                .frame(false),
                        );
                        resp.request_focus();
                        if resp.changed() { qo.update_filter(); }

                        let rows = qo.filtered.len() + if qo.at_root() { 0 } else { 1 };
                        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                            if qo.cursor + 1 < rows { qo.cursor += 1; }
        }
                        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                            qo.cursor = qo.cursor.saturating_sub(1);
        }
                        // Enter acts on the highlighted row — folders descend,
                        // files open. Right does too, but only on an empty
                        // query, where it isn't text-cursor movement.
                        let empty_query = qo.query.is_empty();
                        let activate = ctx.input(|i| {
                            i.key_pressed(egui::Key::Enter)
                                || (empty_query && i.key_pressed(egui::Key::ArrowRight))
                        });

                        ui.separator();

                        if qo.listing() {
                            ui.add_space(6.0);
                            ui.label(egui::RichText::new("Reading folder…").size(13.0)
                                .color(egui::Color32::from_gray(150)));
                            ui.add_space(6.0);
                        }

                        egui::ScrollArea::vertical().max_height(h - 80.0).show(ui, |ui| {
                            ui.style_mut().spacing.item_spacing.y = 0.0;

                            // `../` occupies cursor slot 0 whenever we're below
                            // the root, so the row indices below are offset by it.
                            let up_offset = if qo.at_root() { 0 } else { 1 };
                            if up_offset == 1 {
                                let sel = qo.cursor == 0;
                                let r = draw_quick_open_row(ui, "../", sel, true);
                                if r.clicked() || (sel && activate) { ascend = true; }
                                if sel { r.scroll_to_me(None); }
                            }

                            for (di, &fi) in qo.filtered.iter().enumerate() {
                                let entry = &qo.entries[fi];
                                let sel   = di + up_offset == qo.cursor;
                                let r = draw_quick_open_row(ui, &entry.name, sel, entry.is_dir);
                                if r.clicked() || (sel && activate) {
                                    if entry.is_dir {
                                        descend = Some(entry.path.clone());
                                    } else {
                                        open_path = Some(entry.path.clone());
                                        dismissed = true;
                                    }
                        }
                                // Keep arrow-key navigation visible; without this
                                // the cursor walks off the bottom of the viewport.
                                if sel { r.scroll_to_me(None); }
            }

                            if !qo.listing() && qo.filtered.is_empty() {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(
                                    if qo.entries.is_empty() { "Empty folder" }
                                    else                     { "No matches in this folder" }
                                ).size(12.0).color(egui::Color32::from_gray(130)));
                            }
                            if qo.truncated {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new(format!(
                                    "Showing first {QUICK_OPEN_MAX_ENTRIES} entries of this folder"
                                )).size(11.0).color(egui::Color32::from_gray(130)));
                            }
                        });
                    });
            });

        // Descend wins over ascend if a click and a key landed on the same frame.
        if let Some(dir) = descend { qo.enter(dir); }
        else if ascend          { qo.go_up(); }

        if !dismissed { self.quick_open = Some(qo); }
        if let Some(path) = open_path { self.open_file(path); }
    }

    fn draw_cmd_palette(&mut self, ctx: &egui::Context) {
        if self.cmd_palette.is_none() { return; }

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cmd_palette = None;
            return;
        }

        let mut cp = self.cmd_palette.take().unwrap();
        let screen = ctx.screen_rect();
        let w = (screen.width() * 0.45).min(520.0);
        let mut chosen: Option<Cmd> = None;

        egui::Area::new(egui::Id::new("cmd_palette"))
            .fixed_pos(egui::pos2(screen.center().x - w * 0.5, screen.top() + 60.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(37, 37, 38))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(80)))
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.set_width(w);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut cp.query)
                                .font(egui::FontId::proportional(14.0))
                                .desired_width(w - 16.0)
                                .hint_text("> command…")
                                .frame(false),
                        );
                        resp.request_focus();
                        if resp.changed() { cp.update_filter(); }

                        if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                            if cp.cursor + 1 < cp.filtered.len() { cp.cursor += 1; }
        }
                        if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                            cp.cursor = cp.cursor.saturating_sub(1);
        }

                        ui.separator();

                        for (di, &ci) in cp.filtered.iter().enumerate() {
                            let (name, kbd, cmd) = cp.entry(ci);
                            let sel = di == cp.cursor;
                            let bg  = if sel { egui::Color32::from_rgb(0, 120, 212) }
                                      else   { egui::Color32::TRANSPARENT };
                            egui::Frame::none().fill(bg)
                                .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let label = egui::RichText::new(name).size(13.0)
                                            .color(if sel { egui::Color32::WHITE }
                                                   else   { egui::Color32::from_gray(210) });
                                        let resp = ui.add(egui::Label::new(label)
                                            .sense(egui::Sense::click()));
                                        if !kbd.is_empty() {
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                ui.label(egui::RichText::new(kbd).size(11.0)
                                                    .color(egui::Color32::from_gray(120)));
                                            });
                        }
                                        if resp.clicked()
                                        || (sel && ctx.input(|i| i.key_pressed(egui::Key::Enter)))
                                        {
                                            chosen = Some(cmd);
                        }
                                    });
                                });
        }
                    });
            });

        if chosen.is_none() { self.cmd_palette = Some(cp); }
        if let Some(cmd) = chosen { self.execute_cmd(cmd); }
    }

    fn execute_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::NewFile       => { self.buffers.push(Buffer::new()); self.active = self.buffers.len()-1; }
            Cmd::SaveFile      => self.save_active(),
            Cmd::OpenFolder    => self.open_folder_dialog(),
            Cmd::NewWindow     => { self.pending_new_window = Some(NewWindowSpec::default()); }
            Cmd::ToggleTerminal  => self.show_term  = !self.show_term,
            Cmd::ToggleFileTree  => self.show_tree  = !self.show_tree,
            Cmd::QuickOpen       => if self.has_folder {
                self.quick_open = Some(QuickOpen::new(&self.cwd));
            } else {
                self.status = "Open a folder first".into();
            },
            Cmd::Plugin(i)       => {
                if let Some(cmd) = self.plugins.commands.get(i) {
                    if let Some(buf) = self.buffers.get_mut(self.active) {
                        let text = buf.text();
                        if let Some(new_text) = self.plugins.run(cmd, &text) {
                            buf.lines = new_text.split('\n').map(String::from).collect();
                            if buf.lines.is_empty() { buf.lines.push(String::new()); }
                            buf.modified = true;
                            self.gutter_dirty = true;
                            self.status = format!("Plugin: {}", cmd.title);
                        }
                    }
                }
            }
        }
    }

    fn open_folder_dialog(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.folder_rx = Some(rx);
        std::thread::spawn(move || {
            let result = rfd::FileDialog::new().pick_folder();
            let _ = tx.send(result);
        });
    }

    fn draw_menu(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(30, 30, 30));
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New File             Ctrl+N").clicked() {
                        self.buffers.push(Buffer::new());
                        self.active = self.buffers.len() - 1;
                        ui.close_menu();
    }
                    ui.separator();
                    if ui.button("Open Folder…").clicked() {
                        self.open_folder_dialog();
                        ui.close_menu();
    }
                    if ui.button("New Window").clicked() {
                        self.pending_new_window = Some(NewWindowSpec::default());
                        ui.close_menu();
    }
                    ui.separator();
                    if ui.button("Save               Ctrl+S").clicked() {
                        self.save_active();
                        ui.close_menu();
    }
                    ui.separator();
                    if ui.button("Settings           Ctrl+,").clicked() {
                        self.settings_open = true;
                        ui.close_menu();
    }
                    ui.separator();
                    if ui.button("Reload Window      Ctrl+Shift+R").clicked() {
                        self.reload_window();
                        ui.close_menu();
    }
                });
                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.show_tree, "File Tree");
                    ui.checkbox(&mut self.show_term, "Terminal");
                });
                ui.menu_button("Go", |ui| {
                    if ui.button("Go to File…         Ctrl+P").clicked() {
                        if self.has_folder {
                            self.quick_open = Some(QuickOpen::new(&self.cwd));
                        } else {
                            self.status = "Open a folder first".into();
                        }
                        ui.close_menu();
    }
                    if ui.button("Command Palette… Ctrl+Shift+P").clicked() {
                        self.cmd_palette = Some(CmdPalette::new(self.plugins.commands.iter().map(|c| format!("Plugin: {}", c.title)).collect()));
                        ui.close_menu();
    }
                });
            });
        });
    }

    fn draw_editor(&mut self, ui: &mut egui::Ui) {
        // ── Context menu — detected first so it works on the welcome screen too
        let editor_rect = ui.max_rect();

        // ── Dropped files open as tabs ─────────────────────────────────────
        // Scoped to the editor area: `dropped_files` is global for the frame,
        // and the terminal, agent composer and file tree each claim their own
        // rect for a different meaning (paste the path / attach it / import the
        // file). Here the obvious meaning is "open it".
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if !dropped.is_empty()
            && ui.ctx().input(|i| i.pointer.interact_pos())
                .is_some_and(|p| editor_rect.contains(p))
        {
            for path in dropped {
                // Folders aren't buffers — treat one as "open this workspace",
                // which is what dragging a folder onto an editor means anywhere
                // else, rather than failing silently.
                if path.is_dir() {
                    self.has_folder = true;
                    self.cwd = path.clone();
                    self.file_tree.set_root(path.clone());
                    self.git = crate::git::GitState::open(&path);
                    self.file_watcher = crate::filewatch::FileWatcher::new(&path);
                    for tab in &mut self.terminal_tabs {
                        tab.terminal.restart_in(&path);
                    }
                    self.status = format!("Opened folder {}", path.display());
                } else {
                    self.open_file(path);
                }
            }
        }
        let menu_id     = ui.id().with("editor_ctx_menu");
        if ui.ctx().input(|i| {
            i.pointer.secondary_clicked()
                && i.pointer.interact_pos().map_or(false, |p| editor_rect.contains(p))
        }) {
            let pos = ui.ctx().pointer_interact_pos().unwrap_or(editor_rect.center());
            ui.ctx().data_mut(|d| d.insert_temp(menu_id, pos));
            ui.ctx().memory_mut(|m| m.open_popup(menu_id));
        }
        if ui.ctx().memory(|m| m.is_popup_open(menu_id)) {
            let cursor   = ui.ctx().data(|d| d.get_temp::<egui::Pos2>(menu_id))
                .unwrap_or(editor_rect.center());
            let n_bufs   = self.buffers.len();
            let active   = self.active;
            let mut close = false;
            egui::Area::new(menu_id)
                .order(egui::Order::Foreground)
                .fixed_pos(cursor)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(160.0);
                        if ui.button("Save          Ctrl+S").clicked() {
                            if let Some(b) = self.buffers.get_mut(active) {
                                match b.save() {
                                    Ok(()) => { self.status = "Saved".into(); if let Some(g) = &mut self.git { g.refresh(); } },
                                    Err(e) => self.status = e,
                }
            }
                            close = true;
        }
                        ui.separator();
                        if ui.button("New File      Ctrl+N").clicked() {
                            self.buffers.push(Buffer::new());
                            self.active = self.buffers.len() - 1;
                            close = true;
        }
                        if n_bufs > 1 && ui.button("Close Tab").clicked() {
                            self.buffers.remove(active);
                            if self.active >= self.buffers.len() {
                                self.active = self.buffers.len().saturating_sub(1);
            }
                            close = true;
        }
                        ui.separator();
                        if ui.button("Open Folder…").clicked() {
                            self.open_folder_dialog();
                            close = true;
        }
                        if ui.input(|i| i.pointer.primary_clicked())
                            && !ui.rect_contains_pointer(ui.min_rect()) { close = true; }
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
                    });
                });
            if close { ui.ctx().memory_mut(|m| m.close_popup()); }
        }

        // ── Welcome state (no open files) ────────────────────────────────────
        // VS Code-style: large faint brand watermark filling the editor area.
        if self.buffers.is_empty() {
            let rect = ui.available_rect_before_wrap();
            ui.painter().rect_filled(rect, 0.0, self.palette.editor_bg_c());

            // Ensure the icon is loaded (the right activity bar normally does
            // this, but it runs after CentralPanel on the first frame).
            if self.forge_icon.is_none() {
                self.forge_icon = load_forge_icon(ui.ctx());
            }

            if let Some(tex) = &self.forge_icon {
                // Watermark: ~55% of the smaller panel dimension, capped at 480px
                let logo_size = (rect.width().min(rect.height()) * 0.55).min(480.0);
                let logo_rect = egui::Rect::from_center_size(
                    rect.center(),
                    egui::vec2(logo_size, logo_size),
                );
                ui.painter().image(
                    tex.id(),
                    logo_rect,
                    egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0),
                    ),
                    // Cold = dim gray, barely visible; warms toward orange/white
                    // the longer the Forge Agent has been actively working.
                    anvil_heat_color(self.anvil_heat),
                );
            } else {
                // Fallback if the asset failed to load
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label(egui::RichText::new("Forge IDE")
                            .size(40.0).color(egui::Color32::from_gray(48)));
                    });
                });
            }
            return;
        }

        // ── Diff view tabs render read-only; skip all editing machinery. ─────
        if self.buffers.get(self.active).map_or(false, |b| b.diff.is_some()) {
            let rows = self.buffers[self.active].diff.clone().unwrap_or_default();
            let path = self.buffers[self.active].path.clone();
            self.draw_diff_view(ui, path.as_deref(), &rows);
            return;
        }

        // ── Image tabs render as a preview; skip all editing machinery. ──────
        if self.buffers.get(self.active).map_or(false, |b| b.image_bytes.is_some()) {
            self.draw_image_view(ui);
            return;
        }

        // ── Recompute diff gutter bars for the active file when needed ───────
        let active_path = self.buffers.get(self.active).and_then(|b| b.path.clone());
        if self.gutter_dirty || self.gutter_path != active_path {
            self.gutter_marks.clear();
            if let (Some(g), Some(p)) = (&self.git, &active_path) {
                self.gutter_marks = g.gutter_marks(p);
            }
            self.gutter_path  = active_path.clone();
            self.gutter_dirty = false;
        }
        let gutter_marks = self.gutter_marks.clone();

        // ── Recompute blame on file switch / after commit-pull (coarse: not
        //    per keystroke — blame only changes when history does). ──────────
        //    Dispatched to a worker rather than computed inline: blame walks
        //    the file's history, which measured ~240ms on this file alone in a
        //    five-commit repo, and it ran on every tab switch.
        if self.blame_path != active_path {
            self.blame_path  = active_path.clone();
            self.blame_lines = Vec::new();
            self.blame_rx    = match (&self.git, &active_path) {
                (Some(g), Some(p)) => {
                    let text = self.buffers.get(self.active)
                        .map(|b| b.text()).unwrap_or_default();
                    Some(crate::git::spawn_blame(
                        g.workdir().to_path_buf(), p.clone(), text))
                }
                _ => None,
            };
        }
        if let Some(rx) = &self.blame_rx {
            match rx.try_recv() {
                // Discard a result for a file we've since navigated away from.
                Ok((path, lines)) => {
                    if Some(&path) == active_path.as_ref() { self.blame_lines = lines; }
                    self.blame_rx = None;
                }
                Err(mpsc::TryRecvError::Empty)        => {}
                Err(mpsc::TryRecvError::Disconnected) => self.blame_rx = None,
            }
        }
        let blame = self.blame_lines.clone();

        // Diagnostics for the active file (for squiggles + hover tooltips).
        let diags: Vec<crate::lsp::Diagnostic> = active_path.as_ref()
            .and_then(|p| self.diagnostics.get(p)).cloned().unwrap_or_default();

        let Some(buf) = self.buffers.get_mut(self.active) else { return };

        // ── Keyboard shortcuts ───────────────────────────────────────────────
        // Whether the editor's own text widget actually has keyboard focus
        // (one-frame-stale — `editor_te_id` is only refreshed later in this
        // same function, after the widget is drawn — same pattern already
        // relied on below for the multi-cursor case). Global IDE shortcuts
        // already suppress themselves while a terminal is focused (see
        // `draw()`'s own `terminal_focused` guard), but this function's own
        // key handling — Tab in particular — didn't check focus at all, so
        // it fired purely because the editor panel was *visible*, stealing
        // Tab (and thus breaking shell tab-completion) any time an editor
        // tab happened to be open alongside a focused terminal.
        let editor_focused = self.editor_te_id
            .map_or(false, |id| ui.ctx().memory(|m| m.has_focus(id)));

        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::S)) {
            match buf.save() { Ok(()) => { self.status = "Saved".into(); if let Some(g) = &mut self.git { g.refresh(); } }, Err(e) => self.status = e }
        }
        let shift = ui.input(|i| i.modifiers.shift);
        if ctrl && !shift && ui.input(|i| i.key_pressed(egui::Key::Z)) { buf.undo(); }
        // Both common redo conventions: Ctrl+Shift+Z (Mac/GTK) and Ctrl+Y (Windows/vim).
        if ctrl && shift && ui.input(|i| i.key_pressed(egui::Key::Z)) { buf.redo(); }
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::Y)) { buf.redo(); }

        // Tab key: insert spaces or a tab character based on settings.
        // Consume the event before TextEdit sees it. Gated on the editor
        // actually having focus — see the comment on `editor_focused` above.
        if editor_focused && !ctrl && ui.input(|i| i.key_pressed(egui::Key::Tab)) {
            if self.settings.insert_spaces {
                let spaces = " ".repeat(self.settings.tab_width as usize);
                for ch in spaces.chars() { buf.insert_char(ch); }
            } else {
                buf.insert_char('\t');
            }
            ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Key { key: egui::Key::Tab, .. })));
        }

        // Ctrl+Space → completions
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::Space)) {
            if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                let (line, col) = buf.cursor;
                self.comp_req = lsp.completions(&path, line as u32, col as u32);
            }
        }
        // F12 → go to definition
        if ui.input(|i| i.key_pressed(egui::Key::F12)) {
            if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                let (line, col) = buf.cursor;
                self.goto_req = lsp.goto_def(&path, line as u32, col as u32);
            }
        }
        // Shift+F12 → find references
        let shift = ui.input(|i| i.modifiers.shift);
        if shift && ui.input(|i| i.key_pressed(egui::Key::F12)) {
            if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                let (line, col) = buf.cursor;
                self.refs_req = lsp.references(&path, line as u32, col as u32);
            }
        }
        // F2 → rename (open input prompt)
        if ui.input(|i| i.key_pressed(egui::Key::F2)) {
            self.rename_input = Some(String::new());
        }
        // Ctrl+. → code actions
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::Period)) {
            let path_opt = buf.path.clone();
            let (line, col) = buf.cursor;
            let diags_snap = active_path.as_ref()
                .and_then(|p| self.diagnostics.get(p)).cloned().unwrap_or_default();
            if let (Some(lsp), Some(path)) = (&mut self.lsp, path_opt) {
                self.action_req = lsp.code_actions(&path, line as u32, col as u32, &diags_snap);
            }
        }
        // Ctrl+Alt+F → format document (LSP for Rust, external formatter otherwise)
        let alt = ui.input(|i| i.modifiers.alt);
        if ctrl && alt && ui.input(|i| i.key_pressed(egui::Key::F)) {
            let ext_now = buf.path.as_ref()
                .and_then(|p| p.extension()).and_then(|e| e.to_str()).unwrap_or("");
            if ext_now == "rs" && self.lsp.is_some() {
                if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                    self.fmt_req = lsp.formatting(&path);
                }
            } else {
                // Shell out on a worker thread. `fmt::format` blocks on
                // `wait_with_output`, and prettier's cold start alone is a
                // sizeable fraction of a second — inline, the window froze for
                // the whole formatter run.
                let name = buf.path.as_ref()
                    .and_then(|p| p.file_name()).and_then(|n| n.to_str())
                    .unwrap_or("untitled").to_string();
                let sent = buf.text();
                let path = buf.path.clone();
                let ext  = ext_now.to_string();
                let text = sent.clone();
                let (tx, rx) = mpsc::channel();
                self.fmt_rx = Some(rx);
                self.status = "Formatting…".into();
                std::thread::spawn(move || {
                    let result = crate::fmt::format(&ext, &name, &text);
                    let _ = tx.send(PendingFormat { path, sent, result });
                });
            }
        }
        // Format on save (Ctrl+S already handled above — re-check just for format)
        if ctrl && ui.input(|i| i.key_pressed(egui::Key::S)) {
            if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    self.fmt_req = lsp.formatting(&path);
}
            }
        }
        // Escape → dismiss all overlays
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.comp_items.clear();
            self.hover_text   = None;
            self.hover_pos    = None; // closes blame popup too
            self.refs_visible = false;
            self.action_items.clear();
            self.sig_help     = None;
            self.rename_input = None;
        }

        // ── Layout constants ─────────────────────────────────────────────────
        let font_id  = egui::FontId::monospace(self.settings.font_size);
        let row_h    = ui.fonts(|f| f.row_height(&font_id));
        let n_lines  = buf.lines.len().max(1);
        let gutter_w = (n_lines.to_string().len() as f32 * 9.0 + 24.0).max(48.0);
        let cur_line = buf.cursor.0;
        let ext = buf.path.as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();

        let editor_bg   = self.palette.editor_bg_c();
        let cur_line_bg = self.palette.cur_line_bg_c();

        // Recompute find matches when query or text changes
        let text_for_find = buf.text();
        if let Some(fb) = &mut self.find_bar {
            if fb.dirty { fb.recompute(&text_for_find); }
        }

        // Extract match info before borrowing buf mutably below
        let match_ranges: Vec<(usize, usize)> =
            self.find_bar.as_ref().map(|fb| fb.matches.clone()).unwrap_or_default();
        let current_match: Option<usize> =
            self.find_bar.as_ref().map(|fb| fb.current);
        let scroll_to_match: bool =
            self.find_bar.as_ref().map_or(false, |fb| fb.scroll_to_cur);

        ui.visuals_mut().extreme_bg_color = editor_bg;

        let mut match_scroll_rect: Option<egui::Rect> = None;

        let word_wrap = self.settings.word_wrap;
        let line_ys: Vec<f32> = if word_wrap { self.wrap_line_ys.clone() } else { Vec::new() };
        let line_y = |i: usize| line_ys.get(i).copied().unwrap_or(i as f32 * row_h);
        let y_to_line = |y: f32| -> usize {
            if line_ys.is_empty() { (y / row_h).floor().max(0.0) as usize }
            else { line_ys.partition_point(|&ly| ly <= y).saturating_sub(1) }
        };
        let scroll_area = if word_wrap { egui::ScrollArea::vertical() }
                          else          { egui::ScrollArea::both() };
        // Keyed per-buffer (by path, falling back to tab index for unsaved
        // buffers) so each open file keeps its own scroll position instead
        // of all tabs sharing one persisted scroll state.
        let scroll_key = buf.path.as_ref().map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("untitled-{}", self.active));
        let scroll_out = scroll_area
            .id_salt(("editor_scroll", scroll_key))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let gutter_top = ui.cursor().min.y;
                    let content_h  = line_ys.last().map(|y| y + row_h)
                        .unwrap_or(row_h * n_lines as f32);
                    let total_h    = (content_h + 8.0).max(ui.available_height());
                    let avail_w    = ui.available_width();

                    let full_rect = egui::Rect::from_min_size(
                        ui.cursor().min, egui::vec2(avail_w, total_h),
                    );
                    ui.painter().rect_filled(full_rect, 0.0, editor_bg);

                    let cur_y = gutter_top + line_y(cur_line);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(full_rect.left(),  cur_y),
                            egui::pos2(full_rect.right(), cur_y + row_h),
                        ),
                        0.0, cur_line_bg,
                    );

                    // Debugger: highlight the line where execution is stopped.
                    if let Some((sp, sl)) = &self.dap_stopped {
                        if buf.path.as_deref() == Some(sp.as_path()) {
                            let sy = gutter_top + line_y(*sl);
                            ui.painter().rect_filled(
                                egui::Rect::from_min_max(
                                    egui::pos2(full_rect.left(),  sy),
                                    egui::pos2(full_rect.right(), sy + row_h),
                                ),
                                0.0, egui::Color32::from_rgba_unmultiplied(220, 200, 80, 30),
                            );
                        }
                    }

                    // ── Gutter ─────────────────────────────────────────────
                    let gutter_rect = egui::Rect::from_min_size(
                        ui.cursor().min, egui::vec2(gutter_w, total_h),
                    );
                    let bp_lines = buf.path.as_ref()
                        .and_then(|p| self.breakpoints.get(p))
                        .cloned()
                        .unwrap_or_default();
                    let bar_x = gutter_rect.right() - 3.0;
                    for i in 0..n_lines {
                        let y = gutter_top + line_y(i);
                        // Breakpoint dot (toggle with F9)
                        if bp_lines.contains(&i) {
                            ui.painter().circle_filled(
                                egui::pos2(gutter_rect.left() + 8.0, y + row_h / 2.0),
                                4.0, egui::Color32::from_rgb(215, 75, 75));
                        }
                        // Diff bar (added / modified / deleted) at the right edge
                        // of the gutter, next to the editor text.
                        if let Some(mark) = gutter_marks.get(&i) {
                            use crate::git::GutterMark;
                            let col = match mark {
                                GutterMark::Added    => egui::Color32::from_rgb( 88, 166, 106),
                                GutterMark::Modified => egui::Color32::from_rgb( 82, 139, 198),
                                GutterMark::Deleted  => egui::Color32::from_rgb(198,  95,  95),
                            };
                            if *mark == GutterMark::Deleted {
                                // Small downward triangle marking removed lines.
                                ui.painter().add(egui::Shape::convex_polygon(
                                    vec![
                                        egui::pos2(bar_x - 2.0, y),
                                        egui::pos2(bar_x + 4.0, y),
                                        egui::pos2(bar_x + 1.0, y + 5.0),
                                    ],
                                    col, egui::Stroke::NONE,
                                ));
                            } else {
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_size(
                                        egui::pos2(bar_x, y), egui::vec2(3.0, row_h)),
                                    0.0, col,
                                );
            }
        }
                        ui.painter().text(
                            egui::pos2(gutter_rect.right() - 8.0, y + 2.0),
                            egui::Align2::RIGHT_TOP,
                            (i + 1).to_string(),
                            egui::FontId::monospace(12.5),
                            if i == cur_line { self.palette.gutter_cur_fg_c() }
                            else             { self.palette.gutter_fg_c() },
                        );
    }
                    ui.allocate_exact_size(egui::vec2(gutter_w, total_h), egui::Sense::hover());

                    // ── Editor ─────────────────────────────────────────────
                    let mut text = buf.text();

                    // Auto-close brackets/quotes: rewrite pending text events
                    // before TextEdit consumes them.
                    let mut autoclose_back = false;  // move cursor left 1 after edit
                    let mut skip_forward   = false;  // move cursor right 1 (typed over closer)
                    if self.settings.auto_close_brackets {
                        let (crow, ccol) = buf.cursor;
                        let next_ch = buf.lines.get(crow)
                            .and_then(|l| l[ccol.min(l.len())..].chars().next());
                        ui.input_mut(|inp| {
                            for ev in inp.events.iter_mut() {
                                let egui::Event::Text(t) = ev else { continue };
                                let pair = match t.as_str() {
                                    "("  => Some("()"),
                                    "["  => Some("[]"),
                                    "{"  => Some("{}"),
                                    "\"" => Some("\"\""),
                                    _    => None,
                                };
                                let closer = matches!(t.as_str(), ")" | "]" | "}" | "\"");
                                if closer && next_ch.map(String::from).as_deref() == Some(t.as_str()) {
                                    // Typing the closer that's already there: skip over it.
                                    *t = String::new();
                                    skip_forward = true;
                                } else if let Some(p) = pair {
                                    *t = p.to_string();
                                    autoclose_back = true;
                                }
                                break;
                            }
                            if skip_forward {
                                inp.events.retain(|e| !matches!(e, egui::Event::Text(t) if t.is_empty()));
                            }
                        });
                    }

                    // ── Multi-cursor: apply pending edits at every cursor ──
                    let primary_ci_before = {
                        let (r, c) = buf.cursor;
                        let byte_off: usize = text.split('\n').take(r)
                            .map(|l| l.len() + 1).sum::<usize>() + c;
                        text[..byte_off.min(text.len())].chars().count()
                    };
                    let mut mc_new_primary: Option<usize> = None;
                    if !self.extra_cursors.is_empty() && editor_focused {
                        let n_chars = text.chars().count();
                        self.extra_cursors.retain(|&ci| ci <= n_chars);
                        let mut insert_str = String::new();
                        let mut backspaces = 0usize;
                        ui.input_mut(|inp| {
                            inp.events.retain(|ev| match ev {
                                egui::Event::Text(t) => { insert_str.push_str(t); false }
                                egui::Event::Key { key: egui::Key::Enter, pressed: true, .. } => {
                                    insert_str.push('\n'); false
                                }
                                egui::Event::Key { key: egui::Key::Backspace, pressed: true, .. } => {
                                    backspaces += 1; false
                                }
                                _ => true,
                            });
                        });
                        if !insert_str.is_empty() || backspaces > 0 {
                            let mut cursors: Vec<usize> = self.extra_cursors.clone();
                            cursors.push(primary_ci_before);
                            cursors.sort_unstable();
                            cursors.dedup();
                            let mut chars: Vec<char> = text.chars().collect();
                            let ins: Vec<char> = insert_str.chars().collect();
                            let mut new_positions = Vec::with_capacity(cursors.len());
                            let mut shift: isize = 0;
                            for &pos0 in &cursors {
                                let mut pos = ((pos0 as isize + shift).max(0) as usize).min(chars.len());
                                for _ in 0..backspaces {
                                    if pos > 0 { chars.remove(pos - 1); pos -= 1; shift -= 1; }
                                }
                                for (k, &ch) in ins.iter().enumerate() { chars.insert(pos + k, ch); }
                                pos += ins.len();
                                shift += ins.len() as isize;
                                new_positions.push(pos);
                            }
                            text = chars.into_iter().collect();
                            buf.lines = text.split('\n').map(String::from).collect();
                            if buf.lines.is_empty() { buf.lines.push(String::new()); }
                            buf.modified = true;
                            self.gutter_dirty = true;
                            let pidx = cursors.iter().position(|&c| c == primary_ci_before).unwrap_or(0);
                            mc_new_primary = Some(new_positions[pidx]);
                            self.extra_cursors = new_positions.iter().enumerate()
                                .filter(|&(i, _)| i != pidx).map(|(_, &p)| p).collect();
                            self.mc_sel_len = 0;
                        }
                    }

                    let ext_hl = ext.clone();
                    let mr = match_ranges.clone();
                    let cm = current_match;
                    let pal = self.palette.clone();
                    let font_size = self.settings.font_size;
                    let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                        let eff_wrap = if word_wrap { wrap_width } else { f32::INFINITY };
                        let hit = self.syntax_cache.as_ref().is_some_and(|c| {
                            c.text == s && c.ext == ext_hl && c.font_size == font_size
                                && c.match_ranges == mr && c.current_match == cm
                                && c.palette_name == pal.name && c.wrap_width == eff_wrap
                        });
                        if hit {
                            return self.syntax_cache.as_ref().unwrap().galley.clone();
                        }
                        let mut job = syntax_highlight(s, &ext_hl, &mr, cm, font_size, &pal);
                        job.wrap.max_width = eff_wrap;
                        let galley = ui.fonts(|f| f.layout_job(job));
                        self.syntax_cache = Some(SyntaxCache {
                            text: s.to_string(), ext: ext_hl.clone(), font_size,
                            match_ranges: mr.clone(), current_match: cm,
                            palette_name: pal.name.clone(), wrap_width: eff_wrap,
                            galley: galley.clone(),
                        });
                        galley
                    };

                    ui.visuals_mut().extreme_bg_color = egui::Color32::TRANSPARENT;

                    let te_out = egui::TextEdit::multiline(&mut text)
                        .font(font_id.clone())
                        .desired_rows(n_lines.max(30))
                        .desired_width(if word_wrap { ui.available_width() } else { f32::INFINITY })
                        .frame(false)
                        .layouter(&mut layouter)
                        .show(ui);

                    self.editor_te_id = Some(te_out.response.id);

                    // Word wrap: record each buffer line's first visual-row y
                    // (used next frame to align the gutter and click mapping).
                    if word_wrap {
                        let mut ys = Vec::with_capacity(n_lines);
                        let mut new_line = true;
                        for row in &te_out.galley.rows {
                            if new_line { ys.push(row.rect.min.y); }
                            new_line = row.ends_with_newline;
                        }
                        self.wrap_line_ys = ys;
                    } else if !self.wrap_line_ys.is_empty() {
                        self.wrap_line_ys.clear();
                    }

                    // Multi-cursor: reposition primary after a synthetic edit,
                    // and keep the buffer's cursor in sync.
                    if let Some(p) = mc_new_primary {
                        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te_out.response.id) {
                            let cur = egui::text::CCursor::new(p);
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cur)));
                            st.store(ui.ctx(), te_out.response.id);
                        }
                    }

                    // Ctrl+D: add a cursor at the next occurrence of the
                    // current word/selection (VS Code behavior, v1).
                    if ctrl && ui.input(|i| i.key_pressed(egui::Key::D)) {
                        let chars: Vec<char> = text.chars().collect();
                        let (word, base_end) = if let Some(cr) = te_out.cursor_range {
                            let a = cr.primary.ccursor.index.min(cr.secondary.ccursor.index);
                            let b = cr.primary.ccursor.index.max(cr.secondary.ccursor.index);
                            if a < b {
                                (chars[a..b.min(chars.len())].iter().collect::<String>(), b)
                            } else {
                                word_at(&chars, a)
                            }
                        } else { (String::new(), 0) };
                        if !word.is_empty() {
                            self.mc_sel_len = word.chars().count();
                            let search_from = self.extra_cursors.iter().copied()
                                .max().unwrap_or(base_end);
                            if let Some(found) = find_next_occurrence(&text, &word, search_from) {
                                let end_ci = found + self.mc_sel_len;
                                if end_ci != base_end && !self.extra_cursors.contains(&end_ci) {
                                    self.extra_cursors.push(end_ci);
                                }
                            }
                        }
                    }

                    // Alt+Click: add a cursor at the click position (the
                    // primary already moved there; keep the old one too).
                    if ui.input(|i| i.modifiers.alt) && te_out.response.clicked() {
                        if !self.extra_cursors.contains(&primary_ci_before) {
                            self.extra_cursors.push(primary_ci_before);
                        }
                    }

                    // Escape clears all extra cursors.
                    if !self.extra_cursors.is_empty()
                        && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.extra_cursors.clear();
                        self.mc_sel_len = 0;
                    }

                    // Paint the extra cursors (and their Ctrl+D highlights).
                    if !self.extra_cursors.is_empty() {
                        let gp = te_out.galley_pos;
                        let cur_col = if self.palette.dark { egui::Color32::WHITE }
                                      else                  { egui::Color32::BLACK };
                        let hl = egui::Color32::from_rgba_unmultiplied(80, 140, 220, 60);
                        let char_w = ui.fonts(|f| f.glyph_width(&font_id, 'm'));
                        for &ci in &self.extra_cursors {
                            let r = te_out.galley.pos_from_ccursor(egui::text::CCursor::new(ci));
                            let top = gp + r.min.to_vec2();
                            ui.painter().line_segment(
                                [top, egui::pos2(top.x, top.y + row_h)],
                                egui::Stroke::new(1.6_f32, cur_col));
                            if self.mc_sel_len > 0 && ci >= self.mc_sel_len {
                                let s = te_out.galley.pos_from_ccursor(
                                    egui::text::CCursor::new(ci - self.mc_sel_len));
                                let a = gp + s.min.to_vec2();
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_size(a,
                                        egui::vec2(self.mc_sel_len as f32 * char_w, row_h)),
                                    2.0, hl);
                            }
                        }
                    }

                    // Reposition the cursor after auto-close / skip-over.
                    if autoclose_back || skip_forward {
                        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te_out.response.id) {
                            if let Some(cr) = st.cursor.char_range() {
                                let idx = if autoclose_back {
                                    cr.primary.index.saturating_sub(1)
                                } else {
                                    (cr.primary.index + 1).min(text.chars().count())
                                };
                                let cur = egui::text::CCursor::new(idx);
                                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cur)));
                                st.store(ui.ctx(), te_out.response.id);
                            }
                        }
                    }

                    // ── Indent guides ──────────────────────────────────────
                    {
                        let char_w = ui.fonts(|f| f.glyph_width(&font_id, ' '));
                        let tabw   = self.settings.tab_width.max(1) as usize;
                        let gp     = te_out.galley_pos;
                        let guide  = self.palette.indent_guide_c();
                        let clip   = ui.clip_rect();
                        for (i, line) in buf.lines.iter().enumerate() {
                            let y0 = gp.y + line_y(i);
                            if y0 + row_h < clip.top() || y0 > clip.bottom() { continue; }
                            if line.trim().is_empty() { continue; }
                            let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t')
                                .map(|c| if c == '\t' { tabw } else { 1 }).sum::<usize>();
                            let levels = indent / tabw;
                            for lvl in 1..levels {
                                let x = gp.x + (lvl * tabw) as f32 * char_w;
                                ui.painter().line_segment(
                                    [egui::pos2(x, y0), egui::pos2(x, y0 + row_h)],
                                    egui::Stroke::new(1.0_f32, guide));
                            }
                        }
                    }

                    // ── Bracket matching: highlight the pair at the cursor ─
                    if let Some(cr) = te_out.cursor_range {
                        let cur_ci = cr.primary.ccursor.index;
                        if let Some((a, b)) = find_matching_bracket(&text, cur_ci) {
                            let gp = te_out.galley_pos;
                            let stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(150));
                            for ci in [a, b] {
                                let r = te_out.galley.pos_from_ccursor(
                                    egui::text::CCursor::new(ci));
                                let char_w = ui.fonts(|f| f.glyph_width(&font_id, '{'));
                                let rect = egui::Rect::from_min_size(
                                    gp + r.min.to_vec2(),
                                    egui::vec2(char_w, row_h));
                                ui.painter().rect_stroke(rect, 1.0, stroke);
                            }
                            // Ctrl+Shift+\ → jump to matching bracket
                            let shift_now = ui.input(|i| i.modifiers.shift);
                            if ctrl && shift_now
                                && ui.input(|i| i.key_pressed(egui::Key::Backslash)
                                             || i.key_pressed(egui::Key::Pipe)) {
                                let target = if cur_ci == a || cur_ci == a + 1 { b } else { a };
                                if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te_out.response.id) {
                                    let cur = egui::text::CCursor::new(target);
                                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(cur)));
                                    st.store(ui.ctx(), te_out.response.id);
                                }
                            }
                        }
                    }

                    // Compute scroll rect for current match
                    if scroll_to_match {
                        if let Some(cur) = current_match {
                            if let Some(&(ms, _)) = match_ranges.get(cur) {
                                let char_idx = text[..ms].chars().count();
                                let r = te_out.galley.pos_from_ccursor(
                                    egui::text::CCursor { index: char_idx, prefer_next_row: false }
                                );
                                match_scroll_rect = Some(egui::Rect::from_min_max(
                                    te_out.galley_pos + r.min.to_vec2(),
                                    te_out.galley_pos + r.max.to_vec2(),
                                ));
            }
        }
    }

                    if let Some(cr) = te_out.cursor_range {
                        let char_idx = cr.primary.ccursor.index;
                        let byte_off: usize = text.chars()
                            .take(char_idx).map(|c| c.len_utf8()).sum();
                        let before = &text[..byte_off.min(text.len())];
                        let line = before.chars().filter(|&c| c == '\n').count();
                        let col  = before.rfind('\n')
                            .map(|p| byte_off - p - 1)
                            .unwrap_or(byte_off);
                        buf.cursor = (line, col);
    }

                    if te_out.response.changed() {
                        buf.lines = text.lines().map(String::from).collect();
                        if buf.lines.is_empty() { buf.lines.push(String::new()); }
                        buf.modified = true;
                        if let Some(fb) = &mut self.find_bar { fb.dirty = true; }
                        self.gutter_dirty = true; // refresh diff bars after an edit
                        // Notify the language server of the edit.
                        if let (Some(lsp), Some(p)) = (&mut self.lsp, buf.path.clone()) {
                            if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                                lsp.did_change(&p, &text);
                                // Trigger signature help when the user types `(`
                                let (line, col) = buf.cursor;
                                let just_typed_paren = col > 0 && {
                                    let ln = &buf.lines[line.min(buf.lines.len()-1)];
                                    ln.as_bytes().get(col.saturating_sub(1)) == Some(&b'(')
                                };
                                if just_typed_paren {
                                    self.sig_req = lsp.signature_help(&p, line as u32, col as u32);
                                } else {
                                    // Clear sig help once cursor leaves the call
                                    self.sig_help = None;
                }
            }
        }
    }

                    // ── Diagnostic squiggles (LSP) ──────────────────────────
                    if !diags.is_empty() {
                        let gp = te_out.galley_pos;
                        let char_w = ui.fonts(|f| f.glyph_width(&font_id, '0'));
                        for d in &diags {
                            let s_idx = char_index(&text, d.start_line, d.start_col);
                            let e_idx = char_index(&text, d.end_line, d.end_col);
                            let r0 = te_out.galley
                                .pos_from_ccursor(egui::text::CCursor::new(s_idx));
                            let r1 = te_out.galley
                                .pos_from_ccursor(egui::text::CCursor::new(e_idx));
                            let y  = gp.y + r0.bottom() - 1.0;
                            let x0 = gp.x + r0.left();
                            let x1 = if (r1.top() - r0.top()).abs() < 0.5 && r1.left() > r0.left() {
                                gp.x + r1.left()
                            } else {
                                x0 + char_w * 6.0 // multiline fallback: short mark
                            };
                            let color = match d.severity {
                                1 => egui::Color32::from_rgb(244,  71,  71),
                                2 => egui::Color32::from_rgb(229, 192, 123),
                                _ => egui::Color32::from_rgb( 90, 160, 220),
                            };
                            paint_squiggle(ui.painter(), x0, x1.max(x0 + 4.0), y, color);
        }
    }

                    // ── Cursor override + hover/blame/diagnostic logic ──────
                    //
                    // Rules:
                    //   Gutter (line numbers)  → Default cursor; click shows blame
                    //   Text area              → Text cursor (normal editing)
                    //   Ctrl+hover text area   → request LSP type info
                    //   Diagnostics            → always show on hover (urgent)
                    //   LSP type info          → shows only after Ctrl+hover fires
                    //   Blame                  → shows only after gutter click

                    let gutter_hover_rect = egui::Rect::from_min_size(
                        egui::pos2(full_rect.left(), gutter_top),
                        egui::vec2(gutter_w, row_h * n_lines as f32),
                    );
                    let content_rect = egui::Rect::from_min_size(
                        egui::pos2(gutter_rect.right(), gutter_top),
                        egui::vec2(full_rect.width() - gutter_w, row_h * n_lines as f32),
                    );

                    let mouse_pos   = ui.ctx().pointer_hover_pos();
                    let in_gutter   = mouse_pos.map_or(false, |p| gutter_hover_rect.contains(p));
                    let in_content  = mouse_pos.map_or(false, |p| content_rect.contains(p));
                    let ctrl_held   = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);

                    // Force Default cursor in gutter (overrides TextEdit's I-beam).
                    if in_gutter {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
    }

                    // Clear LSP hover text when mouse leaves the text area.
                    if !in_content && !in_gutter {
                        self.hover_text = None;
                        self.hover_pos  = None;
    }

                    // Gutter click → toggle blame for that line.
                    let gutter_id  = ui.id().with("gutter_interact");
                    let gutter_int = ui.interact(gutter_hover_rect, gutter_id,
                        egui::Sense::click());
                    if gutter_int.clicked() {
                        if let Some(pos) = mouse_pos {
                            let line = y_to_line(pos.y - gutter_top);
                            // Toggle: clicking the same line clears, different line shows.
                            self.hover_pos = if self.hover_pos
                                .map_or(false, |p| (p.y - pos.y).abs() < row_h)
                            {
                                None // same line — dismiss
                            } else {
                                Some(pos)
                            };
                            let _ = line; // pos encodes the line
                            self.hover_text = None; // clear LSP text; blame uses hover_pos
        }
    }

                    // Show blame popup anchored to the clicked gutter line.
                    if let Some(gpos) = self.hover_pos {
                        if in_gutter || gutter_hover_rect.contains(gpos) || in_content {
                            let line = y_to_line(gpos.y - gutter_top);
                            // Only show if it was a gutter click (no hover_text = blame mode)
                            if self.hover_text.is_none() && line < blame.len() {
                                let bl = &blame[line];
                                let tip_pos = egui::pos2(
                                    gutter_hover_rect.right() + 4.0,
                                    gpos.y - row_h * 0.5,
                                );
                                egui::Area::new(egui::Id::new("blame_popup"))
                                    .order(egui::Order::Foreground)
                                    .fixed_pos(tip_pos)
                                    .show(ui.ctx(), |ui| {
                                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                                            if bl.committed {
                                                ui.label(egui::RichText::new(format!(
                                                    "{}  •  {}  {}",
                                                    bl.author, bl.age, bl.short))
                                                    .strong().size(11.0));
                                                if !bl.summary.is_empty() {
                                                    ui.label(egui::RichText::new(&bl.summary)
                                                        .weak().size(10.5));
                                }
                                            } else {
                                                ui.label(egui::RichText::new("Not committed yet")
                                                    .weak().size(11.0));
                            }
                                        });
                                    });
            }
        }
    }

                    // Diagnostic tooltip — only when mouse is actually over the
                    // squiggled range (line AND column), so the rest of the row
                    // is still accessible for Ctrl+hover type info.
                    if let Some(pos) = mouse_pos {
                        if in_content {
                            let char_w     = ui.fonts(|f| f.glyph_width(&font_id, '0'));
                            let ln         = y_to_line(pos.y - gutter_top) as u32;
                            let col_approx = ((pos.x - gutter_rect.right()) / char_w)
                                .max(0.0) as u32;
                            let diag = diags.iter().find(|d| {
                                if ln < d.start_line || ln > d.end_line { return false; }
                                if ln == d.start_line && ln == d.end_line {
                                    col_approx >= d.start_col && col_approx <= d.end_col + 1
                                } else if ln == d.start_line {
                                    col_approx >= d.start_col
                                } else if ln == d.end_line {
                                    col_approx <= d.end_col + 1
                                } else {
                                    true // middle line of multi-line diagnostic
                }
                            });
                            if let Some(d) = diag {
                                let (label, col) = match d.severity {
                                    1 => ("Error",   egui::Color32::from_rgb(244, 120, 120)),
                                    2 => ("Warning", egui::Color32::from_rgb(229, 200, 140)),
                                    _ => ("Info",    egui::Color32::from_rgb(140, 180, 220)),
                                };
                                egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(),
                                    egui::Id::new("diag_tip"), |ui| {
                                        ui.label(egui::RichText::new(label)
                                            .strong().color(col).size(11.5));
                                        ui.label(egui::RichText::new(&d.message).size(11.0));
                                    });
            }

                            // LSP type info — only on Ctrl+hover, only on new line.
                            if ctrl_held {
                                let new_line = self.hover_pos.map_or(true, |p|
                                    (p.y - pos.y).abs() > row_h * 0.5);
                                if new_line {
                                    if let (Some(lsp), Some(path)) = (&mut self.lsp, buf.path.clone()) {
                                        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                                            let char_w = ui.fonts(|f| f.glyph_width(&font_id, '0'));
                                            let col_approx = ((pos.x - gutter_rect.right())
                                                / char_w).max(0.0) as u32;
                                            let ln32 = y_to_line(pos.y - gutter_top) as u32;
                                            self.hover_req  = lsp.hover(&path, ln32, col_approx);
                                            self.hover_pos  = Some(pos);
                                            self.hover_text = None;
                        }
                    }
                }
                                // Show the result once it arrives.
                                if let Some(txt) = &self.hover_text {
                                    let same_line = self.hover_pos.map_or(false, |p|
                                        (p.y - pos.y).abs() < row_h);
                                    if same_line {
                                        let t = txt.clone();
                                        egui::show_tooltip_at_pointer(ui.ctx(), ui.layer_id(),
                                            egui::Id::new("lsp_hover_tip"), |ui| {
                                                ui.set_max_width(380.0);
                                                ui.label(egui::RichText::new(&t).monospace().size(11.5));
                                            });
                    }
                }
                            } else {
                                // Ctrl released — clear type hover so it doesn't linger.
                                self.hover_text = None;
            }
        }
    }
                });

                // Scroll to current match after layout
                if let Some(r) = match_scroll_rect {
                    ui.scroll_to_rect(r, Some(egui::Align::Center));
                    if let Some(fb) = &mut self.find_bar { fb.scroll_to_cur = false; }
}

                // Scroll to pending line (from search panel click)
                if let Some(line) = self.pending_scroll {
                    let y = ui.min_rect().top() + line_y(line);
                    let r = egui::Rect::from_min_max(
                        egui::pos2(ui.min_rect().left(), y),
                        egui::pos2(ui.min_rect().right(), y + row_h),
                    );
                    ui.scroll_to_rect(r, Some(egui::Align::Center));
                    self.pending_scroll = None;
}
            });

        // ── Minimap ────────────────────────────────────────────────
        if self.settings.minimap {
            let Some(buf) = self.buffers.get(self.active) else { return };
            let mm_w = 90.0;
            let mm_rect = egui::Rect::from_min_max(
                egui::pos2(editor_rect.right() - mm_w - 8.0, editor_rect.top()),
                egui::pos2(editor_rect.right() - 8.0,        editor_rect.bottom()),
            );
            let p = ui.painter_at(mm_rect);
            let bg = self.palette.editor_bg_c();
            p.rect_filled(mm_rect, 0.0, egui::Color32::from_rgba_unmultiplied(
                bg.r(), bg.g(), bg.b(), 235));
            let n = buf.lines.len().max(1);
            let line_h = (mm_rect.height() / n as f32).clamp(1.0, 3.0);
            let fg = self.palette.default_fg_c();
            let line_col = egui::Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 90);
            for (i, line) in buf.lines.iter().enumerate() {
                let t = line.trim_end();
                if t.is_empty() { continue; }
                let y = mm_rect.top() + i as f32 * line_h;
                if y > mm_rect.bottom() { break; }
                let indent = line.len() - line.trim_start().len();
                let x0 = mm_rect.left() + 3.0 + (indent as f32 * 0.7).min(30.0);
                let w  = (t.trim_start().len() as f32 * 0.7).min(mm_rect.width() - 6.0);
                p.line_segment(
                    [egui::pos2(x0, y), egui::pos2((x0 + w).min(mm_rect.right() - 3.0), y)],
                    egui::Stroke::new((line_h - 0.4).max(0.8), line_col));
            }
            // Viewport indicator + click/drag to scroll
            let content_h = line_ys.last().map(|y| y + row_h).unwrap_or(n as f32 * row_h);
            let view_h    = scroll_out.inner_rect.height();
            let top_line  = y_to_line(scroll_out.state.offset.y) as f32;
            let vis_lines = view_h / row_h;
            let vp = egui::Rect::from_min_max(
                egui::pos2(mm_rect.left(),  mm_rect.top() + top_line * line_h),
                egui::pos2(mm_rect.right(), mm_rect.top() + (top_line + vis_lines) * line_h),
            );
            if content_h > view_h {
                p.rect_filled(vp.intersect(mm_rect), 0.0,
                    egui::Color32::from_rgba_unmultiplied(128, 128, 128, 30));
            }
            let resp = ui.interact(mm_rect, ui.id().with("minimap"),
                egui::Sense::click_and_drag());
            if resp.clicked() || resp.dragged() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let line = ((pos.y - mm_rect.top()) / line_h) as usize;
                    self.pending_scroll = Some(line.min(n - 1));
                }
            }
        }

        // ── Completion dropdown ───────────────────────────────────────────────
        // Also gated on editor focus (see `editor_focused` above) — otherwise
        // a completion dropdown left open from an earlier edit could still
        // steal Tab/Enter/arrow keys meant for a since-focused terminal.
        if editor_focused && !self.comp_items.is_empty() {
            let cur_line = self.buffers.get(self.active).map(|b| b.cursor.0).unwrap_or(0);
            let clip  = ui.clip_rect();
            // Position just below the current line in the editor.
            let drop_y = clip.top() + line_y(cur_line) + row_h + 4.0;
            let drop_x = clip.left() + gutter_w + 4.0;
            let drop_pos = egui::pos2(drop_x, drop_y.min(clip.bottom() - 200.0));

            let items = self.comp_items.clone();
            let mut chosen: Option<usize> = None;
            let mut dismissed = false;

            // Arrow-key navigation
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.comp_cursor = (self.comp_cursor + 1).min(items.len().saturating_sub(1));
            }
            if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.comp_cursor = self.comp_cursor.saturating_sub(1);
            }
            if ui.input(|i| i.key_pressed(egui::Key::Tab)
                || i.key_pressed(egui::Key::Enter)) {
                chosen = Some(self.comp_cursor);
            }

            egui::Area::new(egui::Id::new("comp_dropdown"))
                .order(egui::Order::Foreground)
                .fixed_pos(drop_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(320.0);
                        ui.set_max_height(200.0);
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for (i, item) in items.iter().enumerate() {
                                let selected = i == self.comp_cursor;
                                let bg = if selected {
                                    egui::Color32::from_rgb(14, 99, 156)
                                } else { egui::Color32::TRANSPARENT };
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(300.0, 20.0), egui::Sense::click());
                                if resp.hovered() { self.comp_cursor = i; }
                                if resp.clicked() { chosen = Some(i); }
                                ui.painter().rect_filled(rect, 2.0, bg);
                                // kind glyph
                                let glyph = comp_kind_glyph(item.kind);
                                ui.painter().text(
                                    egui::pos2(rect.left() + 4.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER, glyph,
                                    egui::FontId::proportional(11.0),
                                    egui::Color32::from_gray(160));
                                ui.painter().text(
                                    egui::pos2(rect.left() + 18.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER, &item.label,
                                    egui::FontId::monospace(12.0),
                                    egui::Color32::from_gray(220));
                                if !item.detail.is_empty() {
                                    ui.painter().text(
                                        egui::pos2(rect.right() - 4.0, rect.center().y),
                                        egui::Align2::RIGHT_CENTER, &item.detail,
                                        egui::FontId::proportional(10.5),
                                        egui::Color32::from_gray(130));
                }
            }
                        });
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
                    if ui.input(|i| i.pointer.primary_clicked())
                        && !ui.rect_contains_pointer(ui.min_rect()) { dismissed = true; }
                });

            if let Some(idx) = chosen {
                if let Some(item) = items.get(idx) {
                    if let Some(buf) = self.buffers.get_mut(self.active) {
                        // Insert the completion text, replacing the word before cursor.
                        let (row, col) = buf.cursor;
                        let line = &buf.lines[row];
                        let word_start = line[..col].rfind(|c: char| !c.is_alphanumeric() && c != '_')
                            .map(|i| i + 1).unwrap_or(0);
                        let before = line[..word_start].to_string();
                        let after  = line[col..].to_string();
                        buf.lines[row] = format!("{}{}{}", before, item.insert_text, after);
                        buf.cursor = (row, word_start + item.insert_text.len());
                        buf.modified = true;
                        if let (Some(lsp), Some(p)) = (&mut self.lsp, buf.path.clone()) {
                            lsp.did_change(&p, &buf.text());
        }
    }
}
                dismissed = true;
            }
            if dismissed { self.comp_items.clear(); }
        }

        // ── Signature help tooltip (above cursor line) ───────────────────────
        if let Some(sig) = &self.sig_help {
            let cur_line = self.buffers.get(self.active).map(|b| b.cursor.0).unwrap_or(0);
            let clip = ui.clip_rect();
            let tip_pos = egui::pos2(
                clip.left() + gutter_w + 8.0,
                clip.top() + line_y(cur_line) - 4.0,
            );
            let label  = sig.label.clone();
            let active = sig.param_label.clone();
            egui::Area::new(egui::Id::new("sig_help"))
                .order(egui::Order::Foreground)
                .fixed_pos(tip_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(500.0);
                        if let Some(p) = &active {
                            // Highlight active param inside label
                            if let Some(idx) = label.find(p.as_str()) {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 0.0;
                                    ui.label(egui::RichText::new(&label[..idx]).monospace().size(11.5));
                                    ui.label(egui::RichText::new(p).monospace().size(11.5)
                                        .color(egui::Color32::from_rgb(220, 180, 80)).strong());
                                    ui.label(egui::RichText::new(&label[idx+p.len()..]).monospace().size(11.5));
                                });
                                return;
            }
        }
                        ui.label(egui::RichText::new(&label).monospace().size(11.5));
                    });
                });
        }

        // ── Code actions lightbulb dropdown ──────────────────────────────────
        if !self.action_items.is_empty() {
            let cur_line = self.buffers.get(self.active).map(|b| b.cursor.0).unwrap_or(0);
            let clip  = ui.clip_rect();
            let pos   = egui::pos2(clip.left() + gutter_w + 4.0,
                                   clip.top() + line_y(cur_line) + row_h + 4.0);
            let items  = self.action_items.clone();
            let mut chosen: Option<usize> = None;
            let mut dismissed = false;
            // Group by top-level LSP CodeActionKind ("refactor.extract" ->
            // "Refactor"), matching how every other LSP client (VS Code
            // included) presents this dropdown instead of one flat list.
            const GROUPS: [(&str, &str); 3] = [
                ("quickfix", "Quick Fix"),
                ("refactor", "Refactor"),
                ("source",   "Source Action"),
            ];
            egui::Area::new(egui::Id::new("action_dropdown"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(340.0);
                        let mut first_group = true;
                        for (_, label) in GROUPS.iter().chain(std::iter::once(&("", "Other"))) {
                            let group: Vec<(usize, &crate::lsp::CodeAction)> = items.iter().enumerate()
                                .filter(|(_, a)| {
                                    let top = a.kind.split('.').next().unwrap_or("");
                                    if *label == "Other" { !GROUPS.iter().any(|(k, _)| *k == top) }
                                    else { top == GROUPS.iter().find(|(_, l)| l == label).map_or("", |(k, _)| k) }
                                })
                                .collect();
                            if group.is_empty() { continue; }
                            if !first_group { ui.add_space(4.0); ui.separator(); }
                            first_group = false;
                            ui.label(egui::RichText::new(*label).size(10.0).color(egui::Color32::from_gray(140)));
                            for (i, a) in group {
                                if ui.button(egui::RichText::new(&a.title).size(12.0)).clicked() {
                                    chosen = Some(i);
                                }
                            }
                        }
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
                    if ui.input(|i| i.pointer.primary_clicked())
                        && !ui.rect_contains_pointer(ui.min_rect()) { dismissed = true; }
                });
            if let Some(idx) = chosen {
                if let Some(action) = items.get(idx) {
                    // Apply WorkspaceEdit embedded in the action if present.
                    let active_uri = self.buffers.get(self.active)
                        .and_then(|b| b.path.as_ref())
                        .map(|p| crate::lsp::path_to_uri(p));
                    let edits = crate::lsp::parse_text_edits(
                        &action.raw, active_uri.as_deref());
                    if !edits.is_empty() {
                        if let Some(buf) = self.buffers.get_mut(self.active) {
                            crate::lsp::apply_edits(&mut buf.lines, edits);
                            buf.modified = true;
                            self.gutter_dirty = true;
        }
    }
}
                dismissed = true;
            }
            if dismissed { self.action_items.clear(); }
        }

        // ── Rename input prompt ───────────────────────────────────────────────
        if let Some(input) = &mut self.rename_input {
            let clip = ui.clip_rect();
            let cur_line = self.buffers.get(self.active).map(|b| b.cursor.0).unwrap_or(0);
            let pos  = egui::pos2(clip.left() + gutter_w + 4.0,
                                  clip.top() + line_y(cur_line) + row_h + 4.0);
            let mut committed = false;
            let mut cancelled = false;
            let mut new_name  = input.clone();
            egui::Area::new(egui::Id::new("rename_input"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Rename to:").size(11.5));
                            let resp = ui.add(egui::TextEdit::singleline(&mut new_name)
                                .desired_width(180.0).hint_text("new name"));
                            resp.request_focus();
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                committed = true;
            }
                        });
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { cancelled = true; }
                });
            *input = new_name.clone();
            if committed && !new_name.trim().is_empty() {
                let path_opt = self.buffers.get(self.active).and_then(|b| b.path.clone());
                let cursor   = self.buffers.get(self.active).map(|b| b.cursor).unwrap_or((0,0));
                if let (Some(lsp), Some(path)) = (&mut self.lsp, path_opt) {
                    self.rename_req = lsp.rename(&path, cursor.0 as u32, cursor.1 as u32,
                                                  new_name.trim());
}
                self.rename_input = None;
            }
            if cancelled { self.rename_input = None; }
        }

        // ── Find References panel (floating) ─────────────────────────────────
        if self.refs_visible && !self.refs_results.is_empty() {
            let clip     = ui.clip_rect();
            let panel_w  = 360.0f32;
            let panel_h  = 220.0f32;
            let pos      = egui::pos2(clip.right() - panel_w - 8.0, clip.top() + 8.0);
            let refs     = self.refs_results.clone();
            let cwd      = self.cwd.clone();
            let mut jump: Option<usize> = None;
            let mut close = false;
            egui::Area::new(egui::Id::new("refs_panel"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(panel_w);
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(
                                format!("{} references", refs.len())).strong().size(11.5));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("✕").clicked() { close = true; }
                            });
                        });
                        ui.separator();
                        egui::ScrollArea::vertical().max_height(panel_h).show(ui, |ui| {
                            for (i, loc) in refs.iter().enumerate() {
                                let rel = loc.path.strip_prefix(&cwd)
                                    .unwrap_or(&loc.path)
                                    .to_string_lossy();
                                let label = format!("{}:{}", rel, loc.start_line + 1);
                                if ui.add(egui::Label::new(
                                    egui::RichText::new(label).monospace().size(11.0))
                                    .sense(egui::Sense::click())).clicked() {
                                    jump = Some(i);
                }
            }
                        });
                    });
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) { close = true; }
                });
            if let Some(i) = jump {
                if let Some(loc) = refs.get(i) {
                    self.open_file(loc.path.clone());
                    self.pending_scroll = Some(loc.start_line as usize);
                    if let Some(buf) = self.buffers.get_mut(self.active) {
                        buf.cursor = (loc.start_line as usize, loc.start_col as usize);
    }
}
            }
            if close { self.refs_visible = false; }
        }

        // ── Find / Replace bar overlay ────────────────────────────────────────
        if self.find_bar.is_some() {
            let bar_w    = 400.0f32;
            let clip     = ui.clip_rect();
            let bar_pos  = egui::pos2(clip.right() - bar_w - 4.0, clip.top() + 4.0);

            let mut fb          = self.find_bar.take().unwrap();
            let mut dismissed   = false;
            let mut go_next     = false;
            let mut go_prev     = false;
            let mut do_replace  = false;
            let mut do_replace_all = false;

            egui::Area::new(egui::Id::new("find_bar"))
                .order(egui::Order::Foreground)
                .fixed_pos(bar_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(37, 37, 38))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                        .rounding(4.0)
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.set_min_width(bar_w - 12.0);
                            let dim   = egui::Color32::from_gray(160);
                            let white = egui::Color32::from_gray(210);

                            // ── Search row ─────────────────────────────────
                            ui.horizontal(|ui| {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut fb.query)
                                        .desired_width(210.0)
                                        .hint_text("Find…")
                                        .frame(true),
                                );
                                if fb.request_focus { resp.request_focus(); fb.request_focus = false; }
                                if resp.changed() { fb.dirty = true; fb.scroll_to_cur = true; }

                                // Aa — case sensitive toggle
                                let aa_fill = if fb.case_sensitive {
                                    egui::Color32::from_rgb(0, 80, 160)
                                } else {
                                    egui::Color32::from_gray(50)
                                };
                                let aa_resp = egui::Frame::none()
                                    .fill(aa_fill).rounding(3.0)
                                    .inner_margin(egui::Margin::symmetric(4.0, 1.0))
                                    .show(ui, |ui| {
                                        ui.label(egui::RichText::new("Aa").size(11.0).color(white))
                                    }).response;
                                if aa_resp.on_hover_text("Case sensitive").clicked() {
                                    fb.case_sensitive = !fb.case_sensitive;
                                    fb.dirty = true;
                }

                                // Prev / Next — use painter-drawn triangles (font-independent)
                                for (is_prev, fire) in [
                                    (true,  &mut go_prev as &mut bool),
                                    (false, &mut go_next as &mut bool),
                                ] {
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(18.0, 18.0), egui::Sense::click(),
                                    );
                                    if resp.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                                    if resp.clicked() { *fire = true; }
                                    let color = if resp.hovered() { white } else { dim };
                                    let cx = rect.center().x;
                                    let cy = rect.center().y;
                                    let pts = if is_prev {
                                        // up-pointing triangle
                                        [egui::pos2(cx, cy - 4.0),
                                         egui::pos2(cx - 4.5, cy + 3.0),
                                         egui::pos2(cx + 4.5, cy + 3.0)]
                                    } else {
                                        // down-pointing triangle
                                        [egui::pos2(cx, cy + 4.0),
                                         egui::pos2(cx - 4.5, cy - 3.0),
                                         egui::pos2(cx + 4.5, cy - 3.0)]
                                    };
                                    ui.painter().add(egui::Shape::convex_polygon(
                                        pts.to_vec(), color, egui::Stroke::NONE,
                                    ));
                }

                                // Match count
                                let count_txt = if fb.query.is_empty() {
                                    String::new()
                                } else if fb.matches.is_empty() {
                                    "No results".to_string()
                                } else {
                                    format!("{} / {}", fb.current + 1, fb.matches.len())
                                };
                                ui.label(egui::RichText::new(count_txt).size(11.0).color(dim));

                                // Close
                                let xr = ui.add(egui::Label::new(
                                    egui::RichText::new("×").size(16.0).color(dim)
                                ).sense(egui::Sense::click()));
                                if xr.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                                if xr.clicked() { dismissed = true; }
                            });

                            // ── Replace row ────────────────────────────────
                            if fb.show_replace {
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut fb.replace)
                                            .desired_width(210.0)
                                            .hint_text("Replace")
                                            .frame(true),
                                    );
                                    if ui.small_button("Replace").clicked()  { do_replace = true; }
                                    if ui.small_button("All").clicked()       { do_replace_all = true; }
                                });
            }

                            // Keyboard nav
                            let shift = ui.input(|i| i.modifiers.shift);
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                if shift { go_prev = true; } else { go_next = true; }
            }
                            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) { go_next = true; }
                            if ui.input(|i| i.key_pressed(egui::Key::ArrowUp))   { go_prev = true; }
                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) { dismissed = true; }
                        });
                });

            if go_next { fb.next(); }
            if go_prev { fb.prev(); }

            // Replace current match
            if do_replace {
                if let Some(&(ms, me)) = fb.matches.get(fb.current) {
                    if let Some(buf) = self.buffers.get_mut(self.active) {
                        let mut t = buf.text();
                        t.replace_range(ms..me, &fb.replace);
                        buf.lines = t.lines().map(String::from).collect();
                        if buf.lines.is_empty() { buf.lines.push(String::new()); }
                        buf.modified = true;
                        fb.dirty = true;
    }
}
            }

            // Replace all matches (iterate from back to preserve offsets)
            if do_replace_all {
                if let Some(buf) = self.buffers.get_mut(self.active) {
                    let mut t = buf.text();
                    for &(ms, me) in fb.matches.iter().rev() {
                        t.replace_range(ms..me, &fb.replace);
    }
                    buf.lines = t.lines().map(String::from).collect();
                    if buf.lines.is_empty() { buf.lines.push(String::new()); }
                    buf.modified = true;
                    fb.dirty = true;
}
            }

            if !dismissed { self.find_bar = Some(fb); }
        }
    }

    /// Apply a finished external-formatter run to its buffer.
    fn apply_format(&mut self, pf: PendingFormat) {
        let formatted = match pf.result {
            Ok(t)  => t,
            Err(e) => { self.status = format!("Format: {e}"); return; }
        };
        let msg = {
            let Some(buf) = self.buffers.iter_mut()
                .find(|b| b.diff.is_none() && b.path == pf.path)
            else {
                // Tab was closed while the formatter ran.
                return;
            };
            if buf.text() != pf.sent {
                "Format: buffer changed while formatting — not applied".to_string()
            } else {
                buf.lines = if formatted.is_empty() { vec![String::new()] }
                            else { formatted.lines().map(String::from).collect() };
                buf.modified = true;
                let (r, c) = buf.cursor;
                let r = r.min(buf.lines.len() - 1);
                buf.cursor = (r, c.min(buf.lines[r].len()));
                "Formatted".to_string()
            }
        };
        self.gutter_dirty = true;
        self.status = msg;
    }

    /// Bring files dropped from outside the app into `dir`.
    ///
    /// Copy, never move: these come from `dropped_files`, which only carries
    /// OS-level drops (Finder and friends). Dragging a row *within* the tree is
    /// a different egui mechanism entirely and isn't wired up, so there is no
    /// case here where the source is ours to remove.
    ///
    /// Name collisions get a ` (2)`-style suffix rather than overwriting —
    /// silently replacing a file because a drop landed a few pixels off is not
    /// recoverable.
    fn import_dropped_files(&mut self, dir: &Path, paths: &[PathBuf]) {
        let mut copied = 0usize;
        for src in paths {
            let Some(name) = src.file_name() else { continue };
            // Directories would need a recursive walk with all the symlink and
            // depth bounding that implies; not worth it for a drop target.
            if src.is_dir() {
                self.output_log(
                    format!("Skipped {}: dropping folders isn't supported yet",
                            name.to_string_lossy()),
                    OutputLevel::Warn);
                continue;
            }
            let dest = unique_dest(dir, Path::new(name));
            match std::fs::copy(src, &dest) {
                Ok(_) => {
                    copied += 1;
                    let shown = dest.strip_prefix(&self.cwd).unwrap_or(&dest);
                    self.output_log(format!("Copied {}", shown.display()), OutputLevel::Success);
                }
                Err(e) => self.output_log(
                    format!("Copy {} failed: {e}", name.to_string_lossy()),
                    OutputLevel::Error),
            }
        }
        if copied > 0 {
            self.file_tree.refresh();
            self.status = format!("Copied {copied} file(s)");
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        if let Some(i) = self.buffers.iter()
            .position(|b| b.diff.is_none() && b.path.as_ref() == Some(&path))
        {
            self.active = i;
            return;
        }
        match Buffer::from_file(path.clone()) {
            Ok(buf) => {
                self.buffers.push(buf);
                self.active = self.buffers.len() - 1;
                self.gutter_dirty = true;
                self.status = format!("Opened {}", path.display());
                self.lsp_open(&path);
            }
            Err(e) => self.status = e,
        }
    }

    /// Start the language server (lazily) and notify it that `path` is open.
    /// Only Rust files are wired up for now.
    fn lsp_open(&mut self, path: &Path) {
        if path.extension().and_then(|e| e.to_str()) != Some("rs") { return; }
        if self.lsp.is_none() {
            self.lsp = crate::lsp::LspClient::start(&self.cwd);
        }
        let text = self.buffers.get(self.active).map(|b| b.text());
        if let (Some(lsp), Some(text)) = (&mut self.lsp, text) {
            lsp.did_open(path, &text, "rust");
        }
    }

    /// Open a read-only diff view (HEAD ↔ working tree) for `path`.  Reuses an
    /// existing diff tab; falls back to opening the file plainly when there's
    /// no textual diff (e.g. a binary file).
    fn open_diff(&mut self, path: PathBuf) {
        if let Some(i) = self.buffers.iter()
            .position(|b| b.diff.is_some() && b.path.as_ref() == Some(&path))
        {
            self.active = i;
            // Refresh the diff in case the file changed since it was opened.
            if let Some(g) = &self.git {
                if let Ok(rows) = g.file_diff(&path) {
                    if !rows.is_empty() { self.buffers[i].diff = Some(rows); }
}
            }
            return;
        }
        let rows = match &self.git {
            Some(g) => match g.file_diff(&path) {
                Ok(r)  => r,
                Err(e) => { self.status = e; return; }
            },
            None => { self.status = "No git repository".into(); return; }
        };
        if rows.is_empty() {
            // Nothing textual to diff — just open the file.
            self.open_file(path);
            return;
        }
        self.buffers.push(Buffer::diff_view(path.clone(), rows));
        self.active = self.buffers.len() - 1;
        self.status = format!("Diff {}", path.display());
    }

    /// Kick off a one-time background check for whether `gh` is usable.
    fn start_gh_check(&mut self) {
        if self.gh_ready.is_some() || self.gh_check.is_some() { return; }
        let (tx, rx) = mpsc::channel();
        self.gh_check = Some(rx);
        std::thread::spawn(move || { let _ = tx.send(crate::git::gh_ready()); });
    }

    /// Publish the current repo to GitHub via `gh` (creates repo + origin +
    /// push) on a background thread.  Reuses the git-task channel so the result
    /// flows through `poll_git_task` (status + refresh).
    fn start_publish(&mut self, private: bool) {
        if self.git_task.is_some() { return; }
        let Some(g) = &self.git else { return; };
        let workdir = g.workdir().to_path_buf();
        let name = workdir.file_name().and_then(|n| n.to_str())
            .unwrap_or("repo").to_string();
        let owner = self.publish_owner.clone();
        self.status = "Publishing to GitHub…".into();
        self.git_error = None;
        let (tx, rx) = mpsc::channel();
        self.git_task = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(crate::git::gh_publish(workdir, name, owner, private));
        });
    }

    /// Create the "origin" remote from a user-entered URL.
    fn add_origin(&mut self, url: String) {
        let url = url.trim().to_string();
        if url.is_empty() { return; }
        if let Some(g) = &mut self.git {
            match g.set_origin(&url) {
                Ok(()) => {
                    self.status = "Added remote 'origin'".into();
                    self.git_error = None;
}
                Err(e) => self.git_error = Some(e),
            }
        }
    }

    /// Kick off a fetch/pull/push on a background thread.  No-op if one is
    /// already running.  Results are picked up by `poll_git_task`.
    fn start_remote_op(&mut self, op: crate::git::RemoteOp) {
        if self.git_task.is_some() { return; }
        let Some(g) = &self.git else {
            self.status = "No git repository".into();
            return;
        };
        let workdir = g.workdir().to_path_buf();
        self.status = match op {
            crate::git::RemoteOp::Fetch => "Fetching…",
            crate::git::RemoteOp::Pull  => "Pulling…",
            crate::git::RemoteOp::Push  => "Pushing…",
        }.into();
        self.git_error = None;
        let (tx, rx) = mpsc::channel();
        self.git_task = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(crate::git::run_remote_op(workdir, op));
        });
    }

    /// Poll the in-flight remote op; on completion update status + refresh git.
    /// While it's running, `self.git_task.is_some()` keeps the fast repaint
    /// tier active (see the consolidated scheduling check in `draw`).
    fn poll_git_task(&mut self) {
        let Some(rx) = &self.git_task else { return };
        match rx.try_recv() {
            Ok(res) => {
                match res {
                    Ok(msg) => { self.status = msg; self.git_error = None; }
                    Err(e)  => { self.status = "Git error".into(); self.git_error = Some(e); }
}
                self.git_task = None;
                if let Some(g) = &mut self.git { g.refresh(); }
                self.gutter_dirty = true;
                self.blame_path = None; // history may have changed (pull)
            }
            Err(mpsc::TryRecvError::Empty)        => {}
            Err(mpsc::TryRecvError::Disconnected) => { self.git_task = None; }
        }
    }

    /// Render a read-only preview for an image tab, scaled to fit the pane.
    fn draw_image_view(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, self.palette.editor_bg_c());
        let Some(buf) = self.buffers.get_mut(self.active) else { return };

        if buf.image_tex.is_none() {
            if let Some(bytes) = &buf.image_bytes {
                buf.image_tex = load_image_texture(ui.ctx(), bytes);
            }
        }

        match &buf.image_tex {
            Some(tex) => {
                let tex_size = tex.size_vec2();
                let avail = (rect.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(1.0, 1.0));
                let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).min(1.0);
                let draw_size = tex_size * scale;
                let img_rect = egui::Rect::from_center_size(rect.center(), draw_size);
                ui.painter().image(
                    tex.id(), img_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                ui.painter().text(
                    egui::pos2(rect.center().x, img_rect.bottom() + 18.0),
                    egui::Align2::CENTER_TOP,
                    format!("{} × {}", tex_size.x as i32, tex_size.y as i32),
                    egui::FontId::proportional(12.0),
                    egui::Color32::from_gray(140),
                );
            }
            None => {
                ui.painter().text(
                    rect.center(), egui::Align2::CENTER_CENTER,
                    "Couldn't decode this image",
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_gray(150),
                );
            }
        }
    }

    /// Render a read-only unified diff (HEAD ↔ working tree) for a diff tab.
    fn draw_diff_view(&self, ui: &mut egui::Ui, path: Option<&Path>,
                      rows: &[crate::git::DiffRow]) {
        use crate::git::DiffRow;
        let full = ui.available_rect_before_wrap();
        ui.painter().rect_filled(full, 0.0, egui::Color32::from_rgb(30, 30, 30));

        // Sub-header: filename + which sides are being compared.
        let name = path.and_then(|p| p.file_name()).and_then(|n| n.to_str())
            .unwrap_or("diff");
        let (hrect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
        ui.painter().rect_filled(hrect, 0.0, egui::Color32::from_rgb(37, 37, 38));
        ui.painter().text(
            egui::pos2(hrect.left() + 12.0, hrect.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{name}   (Working Tree ↔ HEAD)"),
            egui::FontId::proportional(12.0),
            egui::Color32::from_gray(190),
        );

        let font  = egui::FontId::monospace(13.0);
        let row_h = ui.fonts(|f| f.row_height(&font));

        egui::ScrollArea::vertical()
            .id_salt("diff_view_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let avail_w = ui.available_width();
                let ln_font = egui::FontId::monospace(11.5);
                let ln_col  = egui::Color32::from_gray(110);
                for i in range {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(avail_w, row_h), egui::Sense::hover());
                    let cy = rect.center().y;
                    let (fill, old_s, new_s, body, txt_col) = match &rows[i] {
                        DiffRow::Hunk(h) => (
                            Some(egui::Color32::from_rgb(34, 40, 52)),
                            String::new(), String::new(), h.clone(),
                            egui::Color32::from_rgb(120, 160, 210)),
                        DiffRow::Ctx { old, new, text } => (
                            None, old.to_string(), new.to_string(),
                            format!("  {text}"), egui::Color32::from_gray(180)),
                        DiffRow::Add { new, text } => (
                            Some(egui::Color32::from_rgb(28, 58, 34)),
                            String::new(), new.to_string(),
                            format!("+ {text}"), egui::Color32::from_rgb(180, 220, 180)),
                        DiffRow::Del { old, text } => (
                            Some(egui::Color32::from_rgb(64, 32, 32)),
                            old.to_string(), String::new(),
                            format!("- {text}"), egui::Color32::from_rgb(225, 170, 170)),
                    };
                    let p = ui.painter();
                    if let Some(c) = fill { p.rect_filled(rect, 0.0, c); }
                    p.text(egui::pos2(rect.left() + 44.0, cy),
                        egui::Align2::RIGHT_CENTER, &old_s, ln_font.clone(), ln_col);
                    p.text(egui::pos2(rect.left() + 88.0, cy),
                        egui::Align2::RIGHT_CENTER, &new_s, ln_font.clone(), ln_col);
                    p.text(egui::pos2(rect.left() + 100.0, cy),
                        egui::Align2::LEFT_CENTER, &body, font.clone(), txt_col);
}
            });
    }

    /// Connect on a background thread.  All blocking post-connect work
    /// (initial directory listing + PTY open) happens there too so the UI
    /// never blocks waiting for network I/O.
    /// Appends to the Output log and makes sure the bottom panel is at least
    /// visible — but doesn't force it to the Output *tab* specifically.
    /// Background activity (the agent's file live-reloads/shell mirroring,
    /// an SSH connect result) shouldn't rip focus away from a Terminal tab
    /// the user is actively looking at; it can just sit in Output for them
    /// to check whenever they switch over.
    /// Append one line to the Output panel, bounding both the number of
    /// retained lines and the length of any single line.
    ///
    /// This was an uncapped `Vec<(String, OutputLevel)>` that only ever grew:
    /// task/git/SSH output streams in line by line, so a long session kept
    /// every line forever. Measured at ~120 MB of resident heap growth over a
    /// 15-hour session, and every line is also a widget in the Output panel.
    fn push_output(&mut self, msg: String, level: OutputLevel) {
        let msg = if msg.chars().count() > MAX_OUTPUT_LINE_CHARS {
            // One pathological line (a task dumping a huge blob) shouldn't be
            // held in full, or laid out in full, forever.
            let mut t: String = msg.chars().take(MAX_OUTPUT_LINE_CHARS).collect();
            t.push('…');
            t
        } else {
            msg
        };
        self.output_log.push((msg, level));
        let excess = self.output_log.len().saturating_sub(MAX_OUTPUT_LINES);
        if excess > 0 { self.output_log.drain(..excess); }
    }

    fn output_log(&mut self, msg: impl Into<String>, level: OutputLevel) {
        self.push_output(msg.into(), level);
        self.show_term = true;
    }

    fn ssh_connect(&mut self) {
        let host = self.ssh_form.clone();
        let pw   = if self.ssh_password.is_empty() { None }
                   else { Some(self.ssh_password.clone()) };
        self.ssh_error      = None;
        self.ssh_connecting = true;
        self.status         = format!("Connecting to {}…", host.host);
        self.output_log(format!("Connecting to {}@{}:{}", host.user, host.host, host.port),
            OutputLevel::Info);

        let (tx,    rx)    = mpsc::channel::<Result<crate::ssh::SshReady, String>>();
        let (log_tx, log_rx) = mpsc::channel::<(String, OutputLevel)>();
        self.ssh_log_rx = Some(log_rx);

        std::thread::spawn(move || {
            let log = |msg: &str, level: OutputLevel| { let _ = log_tx.send((msg.to_string(), level)); };
            let result = (|| {
                let conn = std::panic::catch_unwind(|| {
                    crate::ssh::SshConnection::connect(&host, pw.as_deref(), &log)
                }).unwrap_or_else(|e| {
                    let msg = e.downcast_ref::<String>().map(|s| s.as_str())
                        .or_else(|| e.downcast_ref::<&str>().copied())
                        .unwrap_or("unknown panic");
                    Err(format!("SSH panic: {msg}"))
                })?;
                // Do all blocking post-connect work here, on the background thread.
                let root_path = conn.host.remote_dir.clone();
                log(&format!("Listing {root_path}…"), OutputLevel::Info);
                let entries = match conn.fs_list(&root_path) {
                    Ok(e) => {
                        log(&format!("Found {} items in {root_path}", e.len()), OutputLevel::Info);
                        e
                    }
                    Err(e) => {
                        log(&format!("fs/list error: {e}"), OutputLevel::Warn);
                        vec![]
                    }
                };
                // PTY open is best-effort — don't let it block the connection.
                // We skip it entirely here and open it lazily when the user
                // first clicks into the TERMINAL tab.
                log("Remote terminal ready (click TERMINAL tab to connect)", OutputLevel::Info);
                let shell = None;
                let shell_err: Option<String> = None;
                Ok(crate::ssh::SshReady { conn, root_path, entries, shell, shell_err })
            })();
            let _ = tx.send(result);
        });
        self.ssh_connect_rx = Some(rx);
    }

    /// Open a PTY on the remote in the background — called lazily on first
    /// TERMINAL tab view while SSH-connected. Non-blocking.
    fn open_ssh_pty_background(&mut self) {
        if self.ssh_pty_rx.is_some() || self.ssh_shell.is_some() { return; }
        let Some(ssh) = &self.ssh else { return };

        // Clone the Arc fields needed for the background thread.
        let stdin   = std::sync::Arc::clone(&ssh.stdin);
        let pending = std::sync::Arc::clone(&ssh.pending);
        let next_id = std::sync::Arc::clone(&ssh.next_id);
        let pty_pushes = std::sync::Arc::clone(&ssh.pty_pushes);
        let cwd = ssh.host.remote_dir.clone();

        let (tx, rx) = mpsc::channel();
        self.ssh_pty_rx = Some(rx);

        std::thread::spawn(move || {
            let result = (|| {
                // Send pty/open request
                let id = { let mut n = next_id.lock().unwrap(); let i = *n; *n += 1; i };
                let msg = forge_proto::Rpc::request(id, "pty/open",
                    serde_json::json!({ "id": 0u32, "cols": 220u16, "rows": 50u16, "cwd": cwd }));
                let (resp_tx, resp_rx) = mpsc::sync_channel(1);
                pending.lock().unwrap().insert(id, resp_tx);
                if let Ok(mut w) = stdin.lock() {
                    forge_proto::write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
                }
                resp_rx.recv_timeout(std::time::Duration::from_secs(15))
                    .map_err(|_| "pty/open timeout".to_string())
                    .and_then(|r| r)?;

                // Wire up I/O channels
                let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(256);
                let (in_tx,  in_rx)  = mpsc::sync_channel::<Vec<u8>>(256);
                pty_pushes.lock().unwrap().insert(0, out_tx);

                // Stdin forwarding thread
                let stdin2   = std::sync::Arc::clone(&stdin);
                let pending2 = std::sync::Arc::clone(&pending);
                let next2    = std::sync::Arc::clone(&next_id);
                std::thread::spawn(move || {
                    for bytes in in_rx {
                        let rid = { let mut n = next2.lock().unwrap(); let i = *n; *n += 1; i };
                        let m   = forge_proto::Rpc::request(rid, "pty/write",
                            serde_json::json!({ "id": 0u32, "data": bytes }));
                        let (t, _) = mpsc::sync_channel(1);
                        pending2.lock().unwrap().insert(rid, t);
                        if let Ok(mut w) = stdin2.lock() { let _ = forge_proto::write_rpc(&mut *w, &m); }
                    }
                });

                Ok::<_, String>(crate::ssh::ShellChannel { rx: out_rx, tx: in_tx })
            })();
            let _ = tx.send(result);
        });
    }

    /// Navigate to a remote directory — non-blocking; result arrives via ssh_nav_rx.
    fn ssh_navigate(&mut self, path: String) {
        if self.ssh.is_none() || self.ssh_nav_rx.is_some() { return; }
        // Clone the internals needed for the background thread.
        // SshConnection::fs_list only needs the Arc fields so we can pass a
        // snapshot of the stdin/pending/next_id via a clone of the connection
        // — but SshConnection isn't Clone.  Simplest workaround: capture the
        // Arc fields directly.
        let stdin   = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.stdin));
        let pending = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.pending));
        let next_id = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.next_id));
        let (Some(stdin), Some(pending), Some(next_id)) = (stdin, pending, next_id)
            else { return };
        let (tx, rx) = mpsc::channel();
        self.ssh_nav_rx = Some(rx);
        std::thread::spawn(move || {
            // Build a minimal call helper inline (mirrors SshConnection::call)
            let result = (|| {
                let id = { let mut n = next_id.lock().unwrap(); let i = *n; *n += 1; i };
                let msg = forge_proto::Rpc::request(id, "fs/list",
                    serde_json::json!({ "path": path }));
                let (resp_tx, resp_rx) = mpsc::sync_channel(1);
                pending.lock().unwrap().insert(id, resp_tx);
                if let Ok(mut w) = stdin.lock() {
                    forge_proto::write_rpc(&mut *w, &msg)
                        .map_err(|e| e.to_string())?;
}
                let r = resp_rx.recv_timeout(std::time::Duration::from_secs(15))
                    .map_err(|_| "fs/list timeout".to_string())??;
                let entries: Vec<forge_proto::FsEntry> =
                    serde_json::from_value(r.get("entries").cloned()
                        .unwrap_or_default()).map_err(|e| e.to_string())?;
                Ok::<_, String>((path, entries.into_iter().map(|e| crate::ssh::RemoteEntry {
                    name: e.name, path: e.path, is_dir: e.is_dir, size: e.size
                }).collect()))
            })();
            let _ = tx.send(result);
        });
    }

    fn ssh_open_file(&mut self, path: String) {
        // If already open, just switch to it.
        let fake = std::path::PathBuf::from(&path);
        if let Some(i) = self.buffers.iter().position(|b| {
            b.diff.is_none() && b.path.as_ref().map(|p| *p == fake).unwrap_or(false)
        }) {
            self.active = i;
            return;
        }
        if self.ssh_open_rx.is_some() { return; } // already loading

        let stdin   = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.stdin));
        let pending = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.pending));
        let next_id = self.ssh.as_ref().map(|s| std::sync::Arc::clone(&s.next_id));
        let (Some(stdin), Some(pending), Some(next_id)) = (stdin, pending, next_id)
            else { return };

        self.status = format!("Loading {}…",
            std::path::Path::new(&path).file_name()
                .and_then(|n| n.to_str()).unwrap_or(&path));

        let (tx, rx) = mpsc::channel();
        self.ssh_open_rx = Some(rx);

        std::thread::spawn(move || {
            let result = (|| {
                let id = { let mut n = next_id.lock().unwrap(); let i = *n; *n += 1; i };
                let msg = forge_proto::Rpc::request(id, "fs/read",
                    serde_json::json!({ "path": path.clone() }));
                let (resp_tx, resp_rx) = mpsc::sync_channel(1);
                pending.lock().unwrap().insert(id, resp_tx);
                if let Ok(mut w) = stdin.lock() {
                    forge_proto::write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
                }
                let r = resp_rx.recv_timeout(std::time::Duration::from_secs(60))
                    .map_err(|_| "fs/read timeout (file may be very large)".to_string())??;
                let text = r.get("text").and_then(|v| v.as_str())
                    .ok_or_else(|| "missing text field".to_string())?
                    .to_string();
                Ok::<_, String>((path, text))
            })();
            let _ = tx.send(result);
        });
    }

    fn save_active(&mut self) {
        if let Some(buf) = self.buffers.get_mut(self.active) {
            if buf.image_bytes.is_some() { return; } // read-only preview
            // Remote save via SFTP when connected.
            if let Some(ssh) = &self.ssh {
                if let Some(path) = buf.path.clone() {
                    let text = buf.text_for_disk();
                    match ssh.fs_write(&path.to_string_lossy(), &text) {
                        Ok(()) => { buf.modified = false; self.status = "Saved (remote)".into(); }
                        Err(e) => self.ssh_error = Some(e),
    }
                    return;
}
            }
            match buf.save() {
                Ok(())  => { self.status = "Saved".into(); if let Some(g) = &mut self.git { g.refresh(); } },
                Err(e)  => self.status = e,
            }
        }
    }
}

// ── Syntax highlighting ───────────────────────────────────────────────────────

/// Caches the fully-shaped `Galley` (egui's render-ready laid-out text) for
/// the editor's active buffer. `syntax_highlight` re-tokenizes the *entire*
/// file on every call, and even a cached `LayoutJob` still means cloning the
/// whole file's text + every color span and handing it to egui's font system
/// to re-hash and re-shape — real, continuous cost for a large file since
/// `TextEdit`'s layouter runs every frame, not just on edits. Caching the
/// `Arc<Galley>` itself instead means a hit is just an `Arc::clone` (a
/// pointer bump) with zero tokenizing, hashing, or shaping.
struct SyntaxCache {
    text:          String,
    ext:           String,
    font_size:     f32,
    match_ranges:  Vec<(usize, usize)>,
    current_match: Option<usize>,
    palette_name:  String,
    wrap_width:    f32,
    galley:        std::sync::Arc<egui::Galley>,
}

fn syntax_highlight(
    text: &str,
    ext: &str,
    matches: &[(usize, usize)],
    current: Option<usize>,
    font_size: f32,
    pal: &crate::theme::Palette,
) -> egui::text::LayoutJob {
    let default  = pal.default_fg_c();
    let keyword  = pal.keyword_c();
    let type_col = pal.type_c();
    let string   = pal.string_c();
    let comment  = pal.comment_c();
    let number   = pal.number_c();
    let mac_pre  = pal.macro_c();
    let func     = pal.func_c();

    let font_id = egui::FontId::monospace(font_size);
    let fmt = |c: egui::Color32| egui::text::TextFormat {
        font_id: font_id.clone(), color: c, ..Default::default()
    };

    let is_rust     = ext == "rs";
    let is_toml     = ext == "toml";
    let is_glsl     = matches!(ext, "vert" | "frag" | "glsl" | "comp");
    let is_json     = ext == "json";
    let is_yaml     = matches!(ext, "yaml" | "yml");
    let is_shell    = matches!(ext, "sh" | "bash" | "zsh");
    let is_markdown = matches!(ext, "md" | "markdown");

    let kw: &[&str] = if is_rust {
        &[
            "as","break","const","continue","crate","dyn","else","enum",
            "extern","false","fn","for","if","impl","in","let","loop",
            "match","mod","move","mut","pub","ref","return","self","Self",
            "static","struct","super","trait","true","type","unsafe","use",
            "where","while","async","await",
        ]
    } else if is_glsl {
        &[
            "void","bool","int","uint","float","double",
            "vec2","vec3","vec4","bvec2","bvec3","bvec4",
            "ivec2","ivec3","ivec4","mat2","mat3","mat4",
            "sampler2D","sampler3D","samplerCube",
            "in","out","inout","uniform","layout","const",
            "if","else","for","while","do","return","discard","break","continue",
        ]
    } else if is_json {
        &["true", "false", "null"]
    } else if is_yaml {
        &[
            "true","false","null","Yes","No","On","Off",
            "yes","no","on","off","True","False","Null",
        ]
    } else if is_shell {
        &[
            "if","then","else","elif","fi","for","while","do","done",
            "case","esac","function","return","exit","local","export",
            "echo","in","select","until","break","continue",
        ]
    } else { &[] };

    // Pass 1: collect (start, end, TextFormat) spans from the tokenizer
    let mut spans: Vec<(usize, usize, egui::text::TextFormat)> = Vec::new();
    let b   = text.as_bytes();
    let len = b.len();
    let mut i = 0usize;
    let mut bracket_depth = 0usize; // for bracket-pair colorization

    while i < len {
        // Only meaningful for Markdown (heading/fence detection needs to know
        // whether `#`/` ``` ` is at the start of a line, not mid-sentence).
        let at_line_start = i == 0 || b[i - 1] == b'\n';

        if i + 1 < len && b[i] == b'/' && b[i+1] == b'/' {
            let s = i;
            while i < len && b[i] != b'\n' { i += 1; }
            spans.push((s, i, fmt(comment))); continue;
        }
        if i + 1 < len && b[i] == b'/' && b[i+1] == b'*' {
            let s = i; i += 2;
            while i + 1 < len && !(b[i] == b'*' && b[i+1] == b'/') { i += 1; }
            if i + 1 < len { i += 2; }
            spans.push((s, i, fmt(comment))); continue;
        }
        if (is_toml || is_yaml || is_shell) && b[i] == b'#' {
            let s = i;
            while i < len && b[i] != b'\n' { i += 1; }
            spans.push((s, i, fmt(comment))); continue;
        }
        if is_glsl && b[i] == b'#' {
            let s = i;
            while i < len && b[i] != b'\n' { i += 1; }
            spans.push((s, i, fmt(mac_pre))); continue;
        }
        if is_markdown && at_line_start && b[i] == b'#' {
            let s = i;
            while i < len && b[i] != b'\n' { i += 1; }
            spans.push((s, i, fmt(type_col))); continue;
        }
        if is_markdown && at_line_start && i + 2 < len
            && b[i] == b'`' && b[i+1] == b'`' && b[i+2] == b'`' {
            let s = i; i += 3;
            while i < len && b[i] != b'\n' { i += 1; } // rest of the opening fence line
            while i < len {
                if i + 2 < len && b[i] == b'`' && b[i+1] == b'`' && b[i+2] == b'`' { i += 3; break; }
                i += 1;
            }
            spans.push((s, i, fmt(comment))); continue;
        }
        if is_markdown && b[i] == b'`' {
            let s = i; i += 1;
            while i < len && b[i] != b'`' && b[i] != b'\n' { i += 1; }
            if i < len && b[i] == b'`' { i += 1; }
            spans.push((s, i, fmt(string))); continue;
        }
        if is_shell && b[i] == b'\'' {
            let s = i; i += 1;
            while i < len && b[i] != b'\'' { i += 1; } // shell single quotes don't escape
            if i < len { i += 1; }
            spans.push((s, i, fmt(string))); continue;
        }
        if is_shell && b[i] == b'$' {
            let s = i; i += 1;
            if i < len && b[i] == b'{' {
                i += 1;
                while i < len && b[i] != b'}' { i += 1; }
                if i < len { i += 1; }
            } else {
                while i < len && (b[i].is_ascii_alphanumeric() || b[i] == b'_') { i += 1; }
            }
            spans.push((s, i, fmt(mac_pre))); continue;
        }
        if b[i] == b'"' {
            let s = i; i += 1;
            while i < len {
                if b[i] == b'\\' { i += 2; continue; }
                if b[i] == b'"'  { i += 1; break; }
                i += 1;
            }
            spans.push((s, i, fmt(string))); continue;
        }
        if is_rust && b[i] == b'\'' {
            let s = i; i += 1;
            if i < len && b[i] == b'\\' {
                i += 2;
                if i < len && b[i] == b'\'' { i += 1; }
                spans.push((s, i, fmt(string)));
            } else {
                while i < len && (b[i].is_ascii_alphanumeric() || b[i] == b'_') { i += 1; }
                if i < len && b[i] == b'\'' { i += 1; spans.push((s, i, fmt(string))); }
                else { spans.push((s, i, fmt(keyword))); }
            }
            continue;
        }
        if b[i].is_ascii_digit() {
            let s = i;
            if i + 1 < len && b[i] == b'0' && (b[i+1] == b'x' || b[i+1] == b'X') {
                i += 2;
                while i < len && b[i].is_ascii_hexdigit() { i += 1; }
            } else {
                while i < len && (b[i].is_ascii_digit() || b[i] == b'.') { i += 1; }
                if i < len && (b[i] == b'e' || b[i] == b'E') {
                    i += 1;
                    if i < len && (b[i] == b'+' || b[i] == b'-') { i += 1; }
                    while i < len && b[i].is_ascii_digit() { i += 1; }
}
                while i < len && b[i].is_ascii_alphanumeric() { i += 1; }
            }
            spans.push((s, i, fmt(number))); continue;
        }
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let s = i;
            while i < len && (b[i].is_ascii_alphanumeric() || b[i] == b'_') { i += 1; }
            let word = &text[s..i];
            let is_mac = is_rust && i < len && b[i] == b'!';
            if is_mac { i += 1; }
            let col = if is_mac { mac_pre }
                else if kw.contains(&word) { keyword }
                else if is_rust && word.chars().next().map_or(false, |c| c.is_uppercase()) { type_col }
                else if i < len && b[i] == b'(' { func }
                else { default };
            spans.push((s, i, fmt(col))); continue;
        }
        // Bracket-pair colorization by nesting depth (only reached for
        // brackets outside strings/comments — those are consumed above).
        if matches!(b[i], b'(' | b'[' | b'{') {
            spans.push((i, i + 1, fmt(pal.bracket_c(bracket_depth))));
            bracket_depth += 1;
            i += 1; continue;
        }
        if matches!(b[i], b')' | b']' | b'}') {
            bracket_depth = bracket_depth.saturating_sub(1);
            spans.push((i, i + 1, fmt(pal.bracket_c(bracket_depth))));
            i += 1; continue;
        }
        let end = text[i..].char_indices().nth(1).map(|(o, _)| i + o).unwrap_or(len);
        spans.push((i, end, fmt(default)));
        i = end;
    }

    // Pass 2: build LayoutJob, splitting spans at match boundaries
    let match_bg = egui::Color32::from_rgb(49, 49, 16);   // dim yellow bg
    let cur_bg   = egui::Color32::from_rgb(99, 79,  3);   // bright yellow for current

    let mut job = egui::text::LayoutJob::default();
    for (span_s, span_e, ref base_fmt) in spans {
        let mut pos = span_s;
        for (mi, &(ms, me)) in matches.iter().enumerate() {
            let clip_s = ms.max(pos);
            let clip_e = me.min(span_e);
            if clip_s >= clip_e || clip_s >= span_e { continue; }
            if pos < clip_s {
                job.append(&text[pos..clip_s], 0.0, base_fmt.clone());
            }
            let mut mfmt = base_fmt.clone();
            mfmt.background = if Some(mi) == current { cur_bg } else { match_bg };
            job.append(&text[clip_s..clip_e], 0.0, mfmt);
            pos = clip_e;
        }
        if pos < span_e {
            job.append(&text[pos..span_e], 0.0, base_fmt.clone());
        }
    }

    job
}

// ── Font setup ────────────────────────────────────────────────────────────────

/// The identifier-like word ending at (or containing) char index `i`.
/// Returns (word, end_char_index).
fn word_at(chars: &[char], i: usize) -> (String, usize) {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = i.min(chars.len());
    while start > 0 && is_w(chars[start - 1]) { start -= 1; }
    let mut end = i.min(chars.len());
    while end < chars.len() && is_w(chars[end]) { end += 1; }
    (chars[start..end].iter().collect(), end)
}

/// Char index of the next occurrence of `word` after char index `after`
/// (wrapping around to the start).
fn find_next_occurrence(text: &str, word: &str, after: usize) -> Option<usize> {
    let byte_after: usize = text.chars().take(after).map(|c| c.len_utf8()).sum();
    let found_byte = text[byte_after.min(text.len())..].find(word)
        .map(|p| p + byte_after)
        .or_else(|| text.find(word))?;
    Some(text[..found_byte].chars().count())
}

/// Glyph + color for an LSP SymbolKind (outline view).
fn symbol_kind_glyph(kind: u8) -> (&'static str, egui::Color32) {
    match kind {
        5         => ("C", egui::Color32::from_rgb(238, 156,  71)), // Class
        23        => ("S", egui::Color32::from_rgb(238, 156,  71)), // Struct
        10        => ("E", egui::Color32::from_rgb(238, 156,  71)), // Enum
        11        => ("I", egui::Color32::from_rgb(110, 168, 220)), // Interface/trait
        6 | 12    => ("ƒ", egui::Color32::from_rgb(178, 140, 220)), // Method/Function
        7 | 8     => ("p", egui::Color32::from_rgb(110, 168, 220)), // Property/Field
        13 | 14   => ("v", egui::Color32::from_rgb(110, 168, 220)), // Variable/Constant
        2         => ("m", egui::Color32::from_rgb(150, 150, 150)), // Module
        22        => ("e", egui::Color32::from_rgb(110, 168, 220)), // EnumMember
        26        => ("T", egui::Color32::from_rgb( 78, 201, 176)), // TypeParameter
        _         => ("•", egui::Color32::from_gray(150)),
    }
}

/// Given a char index in `text`, find the bracket at (or just before) the
/// cursor and its matching partner. Returns (bracket_ci, match_ci).
fn find_matching_bracket(text: &str, char_idx: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let at = |ci: usize| chars.get(ci).copied();
    let is_bracket = |c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}');
    // Prefer the char at the cursor; fall back to the one before it.
    let ci = if at(char_idx).map_or(false, is_bracket) { char_idx }
             else if char_idx > 0 && at(char_idx - 1).map_or(false, is_bracket) { char_idx - 1 }
             else { return None };
    let c = at(ci)?;
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),  ')' => ('(', ')', false),
        '[' => ('[', ']', true),  ']' => ('[', ']', false),
        '{' => ('{', '}', true),  '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0i32;
    if forward {
        for (j, &ch) in chars.iter().enumerate().skip(ci) {
            if ch == open  { depth += 1; }
            if ch == close { depth -= 1; if depth == 0 { return Some((ci, j)); } }
        }
    } else {
        for j in (0..=ci).rev() {
            let ch = chars[j];
            if ch == close { depth += 1; }
            if ch == open  { depth -= 1; if depth == 0 { return Some((ci, j)); } }
        }
    }
    None
}

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // ── UI font (Proportional) ──
    // Prefer STATIC font files.  macOS's SFNS.ttf is a variable font and
    // egui's ab_glyph backend doesn't pick the correct weight axis — it
    // falls back to the default instance which is often very thin.
    // HelveticaNeue.ttc and Helvetica.ttc are static collections whose
    // first font is regular weight (400) — what we actually want.
    let ui_candidates = [
        "/System/Library/Fonts/HelveticaNeue.ttc",        // STATIC, regular first
        "/System/Library/Fonts/Helvetica.ttc",            // STATIC fallback
        "/System/Library/Fonts/SFNS.ttf",                 // variable, last resort
        "C:/Windows/Fonts/segoeui.ttf",                   // Windows
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",// Linux
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
    ];
    for path in ui_candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert("ide_ui".into(), egui::FontData::from_owned(data));
            fonts.families.get_mut(&egui::FontFamily::Proportional)
                .unwrap().insert(0, "ide_ui".into());
            break;
        }
    }

    // ── Symbol fallback (Proportional) ──
    // HelveticaNeue (macOS's default UI font pick above) has no glyphs at
    // all for common symbols that agent-generated text actually uses —
    // arrows (→ ←), checkmarks (✓), etc. — and egui's own bundled default
    // fonts don't cover them either, so with no fallback these rendered as
    // a "tofu" missing-glyph box. Inserted right after the main UI font
    // (position 1): ordinary text still prefers HelveticaNeue, only the
    // glyphs it's missing fall through to this.
    //
    // Ordered smallest-sufficient-first. `Arial Unicode.ttf` covers essentially
    // everything, but it is a 22 MB file and `FontData::from_owned` keeps it
    // resident: measured at **67 MB** of process footprint (551 MB -> 485 MB
    // when swapped out), for a fallback whose job is arrows and checkmarks.
    // Apple Symbols is 877 KB and covers the symbol ranges that actually
    // triggered this. Arial Unicode stays as a later candidate so a machine
    // without Apple Symbols still gets broad coverage.
    //
    // TRADEOFF: with Apple Symbols first, text outside its coverage — CJK
    // especially — falls back to egui's built-in fonts and can render as tofu.
    // To restore the old behavior, move the Arial Unicode line back to the top.
    let symbol_candidates = [
        "/System/Library/Fonts/Apple Symbols.ttf",              // macOS — 877 KB, arrows/math/misc
        "C:/Windows/Fonts/seguisym.ttf",                        // Windows — Segoe UI Symbol
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",      // Linux — already broad
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf", // macOS — 22 MB, near-total coverage
    ];
    for path in symbol_candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert("ide_symbols".into(), egui::FontData::from_owned(data));
            fonts.families.get_mut(&egui::FontFamily::Proportional)
                .unwrap().insert(1, "ide_symbols".into());
            break;
        }
    }

    // ── Code font (Monospace) ──
    // Same story: prefer static fonts.  Menlo.ttc is static (the first font
    // in the collection is Menlo-Regular); Monaco.ttf is single-weight static.
    // SFNSMono.ttf is variable and will render thin.
    let mono_candidates = [
        "/System/Library/Fonts/Menlo.ttc",                // STATIC — VS Code default
        "/System/Library/Fonts/Monaco.ttf",               // STATIC, single weight
        "/Applications/Utilities/Terminal.app/Contents/Resources/Fonts/SF-Mono-Regular.otf",
        "/System/Library/Fonts/SFNSMono.ttf",             // variable, last resort
        "C:/Windows/Fonts/consola.ttf",                   // Windows
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", // Linux
    ];
    for path in mono_candidates {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert("ide_mono".into(), egui::FontData::from_owned(data));
            fonts.families.get_mut(&egui::FontFamily::Monospace)
                .unwrap().insert(0, "ide_mono".into());
            break;
        }
    }

    ctx.set_fonts(fonts);
}


#[cfg(test)]
mod quick_open_tests {
    use super::{QuickOpen, QuickOpenEntry, list_dir, fuzzy_match, QUICK_OPEN_MAX_ENTRIES};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    /// Unique scratch dir; no `tempfile` dependency in this crate.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-qo-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list(dir: &Path) -> Vec<QuickOpenEntry> {
        list_dir(dir, &AtomicBool::new(false)).0
    }

    fn names(entries: &[QuickOpenEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The whole point of listing one level: what lives in subdirectories is
    /// never touched, so cost tracks this folder's width, not the tree's size.
    #[test]
    fn lists_only_the_immediate_level() {
        let root = scratch("one-level");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("top.rs"), "").unwrap();
        std::fs::write(root.join("sub/nested.rs"), "").unwrap();
        std::fs::write(root.join("sub/deeper/buried.rs"), "").unwrap();

        // Directories first, then case-insensitive by name.
        assert_eq!(names(&list(&root)), vec!["sub/", "top.rs"]);
        assert_eq!(names(&list(&root.join("sub"))), vec!["deeper/", "nested.rs"]);
        assert_eq!(names(&list(&root.join("sub/deeper"))), vec!["buried.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink cycle used to hang the old recursive index. With no recursion
    /// the link is just a navigable row, and listing it terminates trivially.
    #[test]
    #[cfg(unix)]
    fn symlink_to_ancestor_is_listable_not_fatal() {
        let root = scratch("cycle");
        std::fs::write(root.join("real.rs"), "").unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();

        let entries = list(&root);
        // The link is presented as a directory, so it can be navigated…
        let link = entries.iter().find(|e| e.name == "loop/").expect("link listed");
        assert!(link.is_dir);
        // …and stepping into it is one more bounded listing, not a descent.
        assert_eq!(names(&list(&root.join("loop"))), vec!["loop/", "real.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_a_pathologically_wide_folder() {
        let root = scratch("wide");
        for i in 0..(QUICK_OPEN_MAX_ENTRIES + 10) {
            std::fs::write(root.join(format!("f{i}.txt")), "").unwrap();
        }
        let (entries, truncated) = list_dir(&root, &AtomicBool::new(false));
        assert_eq!(entries.len(), QUICK_OPEN_MAX_ENTRIES);
        assert!(truncated);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_flag_abandons_the_listing() {
        let root = scratch("cancel");
        for i in 0..50 { std::fs::write(root.join(format!("f{i}.txt")), "").unwrap(); }
        let (entries, _) = list_dir(&root, &AtomicBool::new(true));
        assert!(entries.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Matches `filetree::FileTree::walk` so the two views agree on what exists.
    #[test]
    fn hides_dotfiles_and_node_modules_but_keeps_target() {
        let root = scratch("skip");
        for d in [".git", "node_modules", "target"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".hidden"), "").unwrap();
        std::fs::write(root.join("keep.rs"), "").unwrap();

        assert_eq!(names(&list(&root)), vec!["target/", "keep.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn filters_within_the_current_folder_case_insensitively() {
        let root = scratch("filter");
        std::fs::write(root.join("MyFile.rs"), "").unwrap();
        std::fs::write(root.join("other.rs"), "").unwrap();
        std::fs::create_dir(root.join("Docs")).unwrap();

        let entries = list(&root);
        let dir_entry = entries.iter().find(|e| e.is_dir).unwrap();
        assert_eq!(dir_entry.name, "Docs/");
        assert_eq!(dir_entry.name_lower, "docs/");
        let file = entries.iter().find(|e| e.name == "MyFile.rs").unwrap();
        assert!(fuzzy_match(&file.name_lower, "myfile"));
        assert!(!fuzzy_match(&file.name_lower, "zzz"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Navigation is confined to the opened folder: `..` stops at the root
    /// rather than letting you wander up into the filesystem.
    #[test]
    fn go_up_stops_at_the_root() {
        let root = scratch("bounds");
        std::fs::create_dir(root.join("sub")).unwrap();

        let mut qo = QuickOpen::new(&root);
        assert!(qo.at_root());
        qo.go_up();
        assert_eq!(qo.dir, root, "must not escape above the project root");

        qo.enter(root.join("sub"));
        assert!(!qo.at_root());
        assert_eq!(qo.breadcrumb(), "sub");

        qo.go_up();
        assert_eq!(qo.dir, root);
        assert!(qo.at_root());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Navigating clears the filter and resets the cursor, so the previous
    /// folder's query can't hide everything in the new one.
    #[test]
    fn navigating_resets_query_and_cursor() {
        let root = scratch("reset");
        std::fs::create_dir(root.join("sub")).unwrap();

        let mut qo = QuickOpen::new(&root);
        qo.query = "zzz".into();
        qo.cursor = 7;
        qo.enter(root.join("sub"));
        assert!(qo.query.is_empty());
        assert_eq!(qo.cursor, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Row 0 is `../` below the root, so the default highlight has to skip it —
    /// otherwise Enter in a folder you just opened would bounce you back out.
    #[test]
    fn default_highlight_skips_the_up_row() {
        let root = scratch("highlight");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.rs"), "").unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();

        // At the root there is no `../` row, so slot 0 is the first entry.
        let mut qo = QuickOpen::new(&root);
        qo.entries = list(&root);
        qo.update_filter();
        assert_eq!(qo.cursor, 0);

        // One level down, slot 0 is `../` and the highlight starts past it.
        qo.dir = root.join("sub");
        qo.entries = list(&qo.dir.clone());
        qo.update_filter();
        assert_eq!(qo.cursor, 1);
        assert_eq!(qo.entries[qo.filtered[qo.cursor - 1]].name, "a.rs");

        // With nothing to list, `../` is the only actionable row.
        qo.dir = root.join("empty");
        qo.entries = list(&qo.dir.clone());
        qo.update_filter();
        assert!(qo.filtered.is_empty());
        assert_eq!(qo.cursor, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A listing that lands after the user has already navigated on must be
    /// discarded, or a slow mount would repopulate the wrong folder.
    #[test]
    fn stale_listing_is_discarded() {
        let root = scratch("stale");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("root-file.rs"), "").unwrap();
        std::fs::write(root.join("sub/sub-file.rs"), "").unwrap();

        let mut qo = QuickOpen::new(&root);
        // Navigate before polling, so the root listing is now stale.
        qo.enter(root.join("sub"));
        for _ in 0..200 {
            qo.poll();
            if !qo.listing() { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!qo.entries.iter().any(|e| e.name == "root-file.rs"),
                "stale root listing leaked into sub: {:?}", names(&qo.entries));
        assert_eq!(names(&qo.entries), vec!["sub-file.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod walk_search_tests {
    use super::{SearchHit, walk_search, SEARCH_MAX_FILE_BYTES, SEARCH_MAX_DEPTH};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-search-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn search(root: &PathBuf) -> Vec<SearchHit> {
        let mut hits = Vec::new();
        walk_search(root, "needle", false, &mut hits, 2000, 0, &AtomicBool::new(false));
        hits
    }

    #[test]
    fn finds_matches_in_nested_files() {
        let root = scratch("basic");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "nothing\nNEEDLE here\n").unwrap();
        std::fs::write(root.join("sub/b.txt"), "needle\n").unwrap();
        std::fs::write(root.join("c.txt"), "no match\n").unwrap();

        let hits = search(&root);
        assert_eq!(hits.len(), 2, "case-insensitive across nesting");
        assert!(hits.iter().any(|h| h.file.ends_with("a.txt") && h.line == 1));
        assert!(hits.iter().any(|h| h.file.ends_with("sub/b.txt") && h.line == 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink cycle used to recurse until the search thread died.
    #[test]
    #[cfg(unix)]
    fn symlink_cycle_terminates() {
        let root = scratch("cycle");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/hit.txt"), "needle\n").unwrap();
        std::os::unix::fs::symlink(&root, root.join("sub/loop")).unwrap();

        // Terminating at all is the assertion; the link is not descended, so
        // the single match is found exactly once.
        assert_eq!(search(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Files over the cap are skipped without being read into memory.
    #[test]
    fn skips_files_over_the_size_cap() {
        let root = scratch("bigfile");
        std::fs::write(root.join("small.txt"), "needle\n").unwrap();
        let big = root.join("big.txt");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(SEARCH_MAX_FILE_BYTES + 1).unwrap();
        drop(f);

        let hits = search(&root);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].file.ends_with("small.txt"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stops_at_the_depth_cap() {
        let root = scratch("deep");
        let mut deep = root.clone();
        for i in 0..(SEARCH_MAX_DEPTH + 4) {
            deep = deep.join(format!("d{i}"));
            std::fs::create_dir(&deep).unwrap();
            std::fs::write(deep.join("f.txt"), "needle\n").unwrap();
        }
        // One match per level within the cap, none below it.
        assert_eq!(search(&root).len(), SEARCH_MAX_DEPTH);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn honors_the_hit_cap() {
        let root = scratch("maxhits");
        for i in 0..40 {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let mut hits = Vec::new();
        walk_search(&root, "needle", false, &mut hits, 10, 0, &AtomicBool::new(false));
        assert_eq!(hits.len(), 10);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancel_flag_abandons_the_walk() {
        let root = scratch("cancel");
        for i in 0..20 {
            std::fs::write(root.join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let mut hits = Vec::new();
        walk_search(&root, "needle", false, &mut hits, 2000, 0, &AtomicBool::new(true));
        assert!(hits.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn case_sensitive_mode_distinguishes() {
        let root = scratch("case");
        std::fs::write(root.join("a.txt"), "Needle\nneedle\n").unwrap();
        let mut hits = Vec::new();
        walk_search(&root, "needle", true, &mut hits, 2000, 0, &AtomicBool::new(false));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Measures the cost of laying out an entire file into one galley — the current
/// editor behavior — against laying out only a screenful. Ignored by default:
///   cargo test -p forge-ide layout_cost -- --ignored --nocapture
#[cfg(test)]
mod layout_cost_bench {
    #[test]
    #[ignore]
    fn layout_cost() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/app.rs");
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        eprintln!("file: {} lines, {} KB", lines.len(), text.len() / 1024);
        eprintln!("size_of::<epaint::text::Glyph>() = {}",
                  std::mem::size_of::<egui::epaint::text::Glyph>());

        let ctx = egui::Context::default();
        // Force font initialization.
        let _ = ctx.run(Default::default(), |_| {});
        let pal = crate::theme::Palette::default();

        let measure = |label: &str, src: &str| {
            let t = std::time::Instant::now();
            let mut job = super::syntax_highlight(src, "rs", &[], None, 13.0, &pal);
            let hl = t.elapsed();
            job.wrap.max_width = f32::INFINITY;
            let t2 = std::time::Instant::now();
            let galley = ctx.fonts(|f| f.layout_job(job));
            let lay = t2.elapsed();
            let glyphs: usize = galley.rows.iter().map(|r| r.glyphs.len()).sum();
            let bytes = glyphs * std::mem::size_of::<egui::epaint::text::Glyph>();
            eprintln!("{label}: highlight {hl:?} + layout {lay:?} = {:?} | \
                       {} rows, {glyphs} glyphs, ~{:.1} MB of glyph data",
                      hl + lay, galley.rows.len(), bytes as f64 / 1_048_576.0);
        };

        measure("WHOLE FILE (current)", &text);
        // A generous screenful at a typical window height.
        let visible: String = lines.iter().take(60).cloned().collect::<Vec<_>>().join("\n");
        measure("60 VISIBLE LINES    ", &visible);
    }
}

#[cfg(test)]
mod output_log_tests {
    use super::{IdeApp, OutputLevel, MAX_OUTPUT_LINES, MAX_OUTPUT_LINE_CHARS};

    /// A real app rooted at an empty temp dir. `IdeApp` has no cheap
    /// constructor, so this goes through the normal one.
    fn app() -> IdeApp {
        let dir = std::env::temp_dir()
            .join(format!("forge-outlog-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        IdeApp::new_with_spec(crate::app::NewWindowSpec {
            cwd: Some(dir), ..Default::default()
        })
    }

    /// The log used to grow without limit for the life of the process.
    #[test]
    fn retained_lines_are_capped() {
        let mut a = app();
        for i in 0..(MAX_OUTPUT_LINES + 500) {
            a.push_output(format!("line {i}"), OutputLevel::Info);
        }
        assert_eq!(a.output_log.len(), MAX_OUTPUT_LINES);
        // Oldest lines are the ones dropped; the newest is still present.
        let last = &a.output_log.last().unwrap().0;
        assert_eq!(last, &format!("line {}", MAX_OUTPUT_LINES + 499));
        assert!(!a.output_log.iter().any(|(m, _)| m == "line 0"));
    }

    /// One pathological line must not be retained in full.
    #[test]
    fn a_single_huge_line_is_truncated() {
        let mut a = app();
        a.push_output("x".repeat(MAX_OUTPUT_LINE_CHARS * 3), OutputLevel::Warn);
        let (msg, _) = &a.output_log[0];
        assert_eq!(msg.chars().count(), MAX_OUTPUT_LINE_CHARS + 1, "cap + ellipsis");
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn short_lines_are_untouched() {
        let mut a = app();
        a.push_output("hello".into(), OutputLevel::Info);
        assert_eq!(a.output_log[0].0, "hello");
    }
}

#[cfg(test)]
mod folderless_window_tests {
    use super::{IdeApp, NewWindowSpec};

    fn spec(cwd: Option<std::path::PathBuf>) -> NewWindowSpec {
        NewWindowSpec { cwd, ..Default::default() }
    }

    /// "New Window" — including from the Dock menu — opens no workspace.
    ///
    /// `cwd: None` used to fall back to `std::env::current_dir()`, so a window
    /// launched from the Dock (process cwd `/`) rendered the whole filesystem as
    /// its workspace root.
    #[test]
    fn no_folder_means_no_workspace() {
        let app = IdeApp::new_with_spec(spec(None));
        assert!(!app.has_folder, "a folderless window must not claim a workspace");
        // Terminals/agent/LSP still need somewhere to run: a real absolute
        // directory, and specifically not the filesystem root. Deliberately not
        // compared against `dirs::home_dir()` — other tests in this binary
        // mutate `HOME`, so that value is not stable across a test run.
        assert!(app.cwd.is_absolute(), "cwd must be absolute, got {:?}", app.cwd);
        assert!(app.cwd.is_dir(), "cwd must exist, got {:?}", app.cwd);
        assert_ne!(app.cwd, std::path::Path::new("/"),
                   "a folderless window must not root at the filesystem root");
        // Nothing workspace-shaped was started.
        assert!(app.git.is_none(), "no repo should be opened without a workspace");
        assert!(app.file_watcher.is_none(), "nothing to watch without a workspace");
    }

    /// An explicit folder is a real workspace.
    #[test]
    fn explicit_folder_opens_a_workspace() {
        let dir = std::env::temp_dir()
            .join(format!("forge-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let app = IdeApp::new_with_spec(spec(Some(dir.clone())));
        assert!(app.has_folder);
        assert_eq!(app.cwd, dir.canonicalize().unwrap_or(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod selection_autoscroll_tests {
    use super::edge_scroll_step;

    const FRAME: f32 = 1.0 / 60.0;

    #[test]
    fn inside_the_edge_does_not_scroll() {
        assert_eq!(edge_scroll_step(0.0, FRAME), None);
        assert_eq!(edge_scroll_step(0.5, FRAME), None, "sub-pixel jitter is not a drag past the edge");
    }

    #[test]
    fn direction_follows_which_edge_was_passed() {
        assert!(edge_scroll_step(20.0, FRAME).unwrap() > 0.0, "below the bottom scrolls down");
        assert!(edge_scroll_step(-20.0, FRAME).unwrap() < 0.0, "above the top scrolls up");
    }

    #[test]
    fn speed_ramps_with_overshoot_then_caps() {
        let near = edge_scroll_step(10.0, FRAME).unwrap();
        let far = edge_scroll_step(50.0, FRAME).unwrap();
        assert!(far > near, "further out should scroll faster: {far} vs {near}");
        // Past the ramp everything moves at the same capped speed, so parking
        // the pointer at the bottom of the screen can't outrun one at the
        // window's edge.
        assert_eq!(edge_scroll_step(60.0, FRAME), edge_scroll_step(4000.0, FRAME));
    }

    #[test]
    fn a_stalled_frame_cannot_jump_the_whole_conversation() {
        // dt is wall-clock: a frame that took a second (a big repaint, a
        // laptop waking up) would otherwise scroll 900 points in one go.
        let hitch = edge_scroll_step(200.0, 1.0).unwrap();
        assert!(hitch <= 90.0, "one hitched frame scrolled {hitch} points");
    }
}

#[cfg(test)]
mod elide_tests {
    use super::elide_path_head_by;

    /// A fixed-width stand-in for the real font: every character is one unit,
    /// so an expected result can be written down rather than measured.
    fn fits(limit: f32) -> impl Fn(&str) -> f32 {
        let _ = limit;
        |s: &str| s.chars().count() as f32
    }

    #[test]
    fn something_that_already_fits_is_left_alone() {
        let text = "Edited /a/b.md";
        assert_eq!(elide_path_head_by(text, 100.0, fits(100.0)), text);
    }

    #[test]
    fn the_file_name_survives_when_the_path_does_not() {
        // The whole point: egui truncates the tail, which on a path throws away
        // the only part that says which file it was.
        let text = "Edited /Users/someone/CascadeProjects/NN-Revival/paper/note.md";
        let out = elide_path_head_by(text, 30.0, fits(30.0));
        assert!(out.starts_with("Edited …/"), "got {out:?}");
        assert!(out.ends_with("note.md"), "the name must survive: {out:?}");
        assert!(out.chars().count() as f32 <= 30.0, "still too wide: {out:?}");
    }

    #[test]
    fn as_much_of_the_tail_as_fits_is_kept() {
        let text = "Edited /one/two/three/four/five.md";
        // Room for more than just the name, but not for the whole path.
        let out = elide_path_head_by(text, 30.0, fits(30.0));
        assert_eq!(out, "Edited …/three/four/five.md");
    }

    #[test]
    fn a_name_too_long_for_the_space_is_left_for_the_label_to_truncate() {
        let text = "Edited /a/an-extremely-long-file-name-that-cannot-fit.md";
        let out = elide_path_head_by(text, 12.0, fits(12.0));
        assert_eq!(out, "Edited …/an-extremely-long-file-name-that-cannot-fit.md");
    }

    #[test]
    fn text_with_no_path_in_it_is_untouched() {
        // Nothing to shorten by dropping components; tail truncation is right.
        let text = "Ran a command with a very long description indeed";
        assert_eq!(elide_path_head_by(text, 5.0, fits(5.0)), text);
    }
}

#[cfg(test)]
mod wrap_run_tests {
    use super::run_for_width;

    #[test]
    fn a_run_fits_the_width_it_was_measured_for() {
        // 224pt of panel, ~11pt per glyph at the size these cards use.
        assert_eq!(run_for_width(224.0, 11.0), 20);
        // The constant it replaced was 44 — over twice what fits, which is
        // why every one of these lines ran off the edge of a narrow panel.
        assert!(run_for_width(224.0, 11.0) < 44);
    }

    #[test]
    fn a_wider_column_earns_a_longer_run() {
        assert!(run_for_width(600.0, 11.0) > run_for_width(224.0, 11.0));
    }

    #[test]
    fn there_is_always_room_for_something() {
        // A break between every character is worse than overflowing.
        assert_eq!(run_for_width(0.0, 11.0), 8);
        assert_eq!(run_for_width(-5.0, 11.0), 8);
        assert_eq!(run_for_width(224.0, 0.0), 8, "an unmeasurable font must not divide by zero");
    }
}
