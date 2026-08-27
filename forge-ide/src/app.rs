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

/// A finished listing, on its way back to the render thread.
struct Listing {
    /// Which directory this answers for. Compared on arrival, so a slow listing
    /// that lands after the user navigated on is dropped rather than shown.
    dir:       PathBuf,
    entries:   Vec<QuickOpenEntry>,
    truncated: bool,
    error:     Option<String>,
}

/// Which filesystem Quick Open is looking at.
///
/// Remote paths live in `PathBuf` here too. Both ends are Unix, and everything
/// done to a path in this module — `parent`, `strip_prefix`, `file_name` — is
/// string work with no filesystem behind it; the only calls that touch a disk
/// are in the listing, which this enum is what selects.
#[derive(Clone)]
enum QuickOpenSource {
    Local,
    Remote(crate::ssh::FsHandles),
}

impl QuickOpenSource {
    fn is_remote(&self) -> bool { matches!(self, Self::Remote(_)) }
}

struct QuickOpen {
    source:   QuickOpenSource,
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
    /// Why the last listing came back with nothing. A remote one can time out
    /// or be refused, and reporting that as "Empty folder" would be a lie.
    error:    Option<String>,
    /// `Some` while a directory listing is in flight.
    rx:       Option<mpsc::Receiver<Listing>>,
    /// Tells an in-flight listing its result is unwanted; tripped by `Drop`
    /// and on every navigation, so a stalled network mount can't pile up.
    cancel:   Arc<AtomicBool>,
}

impl Drop for QuickOpen {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Relaxed); }
}

impl QuickOpen {
    fn new(root: &Path, source: QuickOpenSource) -> Self {
        let mut qo = Self {
            source,
            dir: root.to_path_buf(), root: root.to_path_buf(),
            query: String::new(), entries: Vec::new(), filtered: Vec::new(),
            cursor: 0, truncated: false, error: None, rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        qo.load(root.to_path_buf());
        qo
    }

    /// Kick off a listing of `dir`. Off-thread even though a local one is a
    /// single `read_dir`: on a stalled network mount that one call can block for
    /// seconds, and blocking the render thread is what froze the window. A
    /// remote listing is a round trip, so it has no other option.
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
        self.error     = None;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let source = self.source.clone();
        std::thread::spawn(move || {
            let listing = match &source {
                QuickOpenSource::Local => {
                    let (entries, truncated) = list_dir(&dir, &flag);
                    Listing { dir, entries, truncated, error: None }
                }
                QuickOpenSource::Remote(h) => match list_dir_remote(h, &dir, &flag) {
                    Ok((entries, truncated)) => Listing { dir, entries, truncated, error: None },
                    Err(e) => Listing {
                        dir, entries: Vec::new(), truncated: false, error: Some(e),
                    },
                },
            };
            if !flag.load(Ordering::Relaxed) {
                let _ = tx.send(listing);
                // A remote listing can land while nothing else is driving the
                // UI, and this window only repaints when woken.
                if source.is_remote() { crate::wake::wake(); }
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
            Ok(l) => {
                self.rx = None;
                // Guard against a listing that finished after we navigated on.
                if l.dir != self.dir { return; }
                self.entries   = l.entries;
                self.truncated = l.truncated;
                self.error     = l.error;
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
    // Lazily, so the cap below actually stops reading rather than filtering a
    // list already paid for.
    quick_open_entries(
        iter.flatten().filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?.to_string();
            // Resolve through symlinks so a linked directory is still navigable.
            // Safe to follow here precisely because nothing recurses: a link
            // cycle costs one extra `read_dir` when the user walks into it, not
            // an unbounded descent.
            let path = entry.path();
            let is_dir = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => path.is_dir(),
                Ok(ft)                    => ft.is_dir(),
                Err(_)                    => return None,
            };
            Some((name, path, is_dir))
        }),
        cancel,
    )
}

/// One remote directory, over the connection instead of this machine's disk.
///
/// Same shape as the local listing and the same rules applied to the result, so
/// `Ctrl+P` behaves identically whichever end the workspace is on. Its own
/// request, not the explorer's, so opening Quick Open does not move the tree.
fn list_dir_remote(
    handles: &crate::ssh::FsHandles,
    dir:     &Path,
    cancel:  &AtomicBool,
) -> Result<(Vec<QuickOpenEntry>, bool), String> {
    let entries = crate::ssh::fs_list_with(
        handles,
        &dir.to_string_lossy(),
        std::time::Duration::from_secs(20),
    )?;
    Ok(quick_open_entries(
        entries.into_iter().map(|e| (e.name, PathBuf::from(e.path), e.is_dir)),
        cancel,
    ))
}

/// Filter, label and order one directory's worth of entries.
///
/// Shared by both listings on purpose: which entries are shown, how a directory
/// is marked, what order rows come in and where the cap falls are properties of
/// Quick Open, not of the machine being listed. Two copies would eventually
/// disagree, and a remote workspace would quietly behave unlike a local one.
fn quick_open_entries(
    raw:    impl Iterator<Item = (String, PathBuf, bool)>,
    cancel: &AtomicBool,
) -> (Vec<QuickOpenEntry>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    for (name, path, is_dir) in raw {
        if cancel.load(Ordering::Relaxed) { return (out, truncated); }
        if out.len() >= QUICK_OPEN_MAX_ENTRIES { truncated = true; break; }
        // Hidden entries and `node_modules` are skipped, matching what the file
        // tree shows (see `filetree::FileTree::walk`) so the two views agree.
        if name.starts_with('.') || name == "node_modules" { continue; }
        let display = if is_dir { format!("{name}/") } else { name };
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

/// Whether a window opening should reopen what it had.
///
/// A reload restores unconditionally: it is one session continuing, not a fresh
/// start, and a window that came back empty from `Reload Window` would be a
/// worse version of closing it. The `restore_session` setting governs the other
/// case — a genuine quit and relaunch later.
///
/// Its own function so both halves can be tested; as an inline `||` the reload
/// half was untestable on a machine with the setting turned on, which is to say
/// it was not tested at all.
fn should_restore_session(setting: bool, is_reload: bool) -> bool {
    setting || is_reload
}

/// Whether a window opening should reopen what it had, given that it may also be
/// connecting to a remote host as it opens.
///
/// A window *opening* a remote workspace does not want some other window's local
/// files and terminals — that is what the connecting check was for. A window
/// *reloading* one is the same window, and wants exactly its own back: in
/// particular its terminals, which are the local pty daemon's and, left
/// unrestored, keep running with nothing pointing at them.
fn should_restore_on_open(setting: bool, is_reload: bool, connecting: bool) -> bool {
    should_restore_session(setting, is_reload) && (is_reload || !connecting)
}

// ── Remote paths ──────────────────────────────────────────────────────────────

/// A remote workspace path, absolute.
///
/// `~` is expanded against the home directory learned at connect rather than
/// sent as-is, because `fs/list` answers with entries and no path: anything that
/// has to show where it is, or walk up from there, only knows the path it asked
/// with.
fn resolve_remote_dir(configured: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    let home = if home.is_empty() { "/" } else { home };
    match configured {
        "" | "~" => home.to_string(),
        p if p.starts_with("~/") => format!("{}/{}", home.trim_end_matches('/'), &p[2..]),
        p => p.to_string(),
    }
}

/// One level up a remote path, stopping at the root rather than going above it
/// or returning the empty string, which would list nothing.
fn remote_parent(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".to_string(),
        Some((head, _))      => head.to_string(),
    }
}

// ── Command palette ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Cmd {
    NewFile, SaveFile, OpenFolder, NewWindow,
    ToggleTerminal, ToggleFileTree,
    QuickOpen,
    ReloadWindow, RestartWindow, RestartAll, Consolidate,
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
    // Also in the Window menu. Here as well because typing the name is how
    // anyone who knows what they want reaches a command — it is how VS Code
    // offers its reload, and the only place it offers it.
    ("Reload Window",     "Ctrl+Shift+R", Cmd::ReloadWindow),
    ("Restart This Window", "",           Cmd::RestartWindow),
    ("Restart All Windows", "",           Cmd::RestartAll),
    ("Collect All Windows Into One Process", "", Cmd::Consolidate),
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
    /// How many times *this window* has been rebuilt in place. Carried by
    /// `reload_spec` so each rebuild can say which one it is — a reload that
    /// takes 40ms is hard to tell from one that did not happen, and a number
    /// that goes up settles it.
    ///
    /// In-memory only: a process restart cannot carry it through an argument
    /// list, and starts again from zero, which is honest — that is a new process.
    pub reload_count: u32,
    /// What was decided about this window before it existed, for its OUTPUT
    /// panel.
    ///
    /// Whether a restarted window found its record, and whether that record had a
    /// connection to remake, is exactly what nobody could see while a remote
    /// window kept coming back local — every explanation for it was invisible
    /// from inside the window it happened to. It is written down now.
    pub notes: Vec<String>,
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
    /// A mode that has been chosen but needs a new agent process to fully take
    /// effect, waiting for the turn to end so it does not take the turn with it.
    /// See `mode_switch_plan`.
    pending_mode_respawn: Option<crate::settings::AgentPermissionMode>,
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

/// Whether moving from `from` to `to` needs a new agent process.
///
/// `--dangerously-allow-all` is only settable as a flag at spawn time, so
/// crossing into or out of it cannot be done live; every other transition is a
/// message to the running agent.
fn mode_change_needs_respawn(
    from: crate::settings::AgentPermissionMode,
    to: crate::settings::AgentPermissionMode,
) -> bool {
    use crate::settings::AgentPermissionMode as Mode;
    from != to && (from == Mode::DangerouslySkipAll || to == Mode::DangerouslySkipAll)
}

/// How to apply a permission-mode change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeSwitch {
    /// Take effect now, on the running agent. No process is replaced.
    Live,
    /// Take effect now as far as it can, and replace the agent once the turn is
    /// over. `Skip All Permissions` is a spawn-time flag — forge-agent has no
    /// runtime toggle for it — so the part that needs a new process waits, and
    /// the part that unblocks the user does not.
    LiveThenRespawnAfterTurn,
    /// Replace the agent now, ending the turn with it.
    RespawnNow,
}

/// Deciding how to apply a mode change, including mid-turn.
///
/// Changing modes used to mean the same thing whatever was happening: if the
/// change touched `Skip All Permissions` the agent was replaced, and if a turn
/// was running it died with it. So the way to approve a call you were being
/// asked about was to stop the turn, change the mode, and ask again.
///
/// Loosening can be applied live, and mid-turn that is the whole point: the call
/// the agent is blocked on gets approved and it carries on.
///
/// Tightening *out of* `Skip All Permissions` cannot wait, and is the one case
/// worth interrupting a turn for — the mode exists to skip confirmation on
/// anything at all, including tools nothing recognises, and "it will stop doing
/// that when it finishes" is not a safety property. Restarting is abrupt; an
/// agent that keeps skipping everything after being told to stop is worse.
fn mode_switch_plan(
    from: crate::settings::AgentPermissionMode,
    to:   crate::settings::AgentPermissionMode,
    turn_active: bool,
) -> ModeSwitch {
    use crate::settings::AgentPermissionMode as Mode;
    if !mode_change_needs_respawn(from, to) {
        return ModeSwitch::Live;
    }
    if turn_active && to == Mode::DangerouslySkipAll {
        return ModeSwitch::LiveThenRespawnAfterTurn;
    }
    ModeSwitch::RespawnNow
}

/// Whether this mode answers approval requests by itself.
fn mode_auto_approves(mode: crate::settings::AgentPermissionMode) -> bool {
    use crate::settings::AgentPermissionMode as Mode;
    !matches!(mode, Mode::AlwaysAsk)
}

/// Approve every tool call currently waiting on an answer, returning their ids.
///
/// Only `ToolRequest` cards, which is the point rather than an oversight: a plan
/// waiting for approval, a question the agent asked, and a password prompt are
/// each a decision only the user can make, and none of them is what a permission
/// mode is about. Nested subagent requests are included — one of those blocks its
/// subagent, which blocks the turn just as surely.
fn release_pending_approvals(items: &mut [ChatItem]) -> Vec<String> {
    let mut ids = Vec::new();
    for item in items {
        match item {
            ChatItem::ToolRequest { id, approval, .. }
                if matches!(approval, ApprovalState::Pending) =>
            {
                *approval = ApprovalState::Approved;
                ids.push(id.clone());
            }
            ChatItem::Subagent { items: nested, .. } => {
                ids.extend(release_pending_approvals(nested));
            }
            _ => {}
        }
    }
    ids
}

impl AgentTab {
    /// `session` is supplied rather than started here: whether the agent runs
    /// locally or on the machine being worked on is the app's decision, and it
    /// is the only thing holding the SSH connection. See
    /// `IdeApp::start_agent_session`.
    fn new(session: crate::agent_panel::AgentSession,
           mode: crate::settings::AgentPermissionMode) -> Self {
        Self {
            session,
            conv_id: new_conv_id(),
            permission_mode: mode,
            session_password: None,
            password_auto_inject: false,
            password_tmp_path: None,
            show_full_history: false,
            last_saved_item_count: 0,
            pending_mode_respawn: None,
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
    fn reopen(session: crate::agent_panel::AgentSession,
              mode: crate::settings::AgentPermissionMode,
              conv: &crate::agent_panel::SavedConversation) -> Self {
        let mut tab = Self {
            session,
            conv_id: conv.id.clone(),
            permission_mode: mode,
            session_password: None,
            password_auto_inject: false,
            password_tmp_path: None,
            show_full_history: false,
            last_saved_item_count: 0,
            pending_mode_respawn: None,
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
    /// Whether changing to `mode` needs a new agent process.
    ///
    /// `--dangerously-allow-all` is only settable as a flag at spawn time, so
    /// crossing into or out of it cannot be done live.
    /// `replacement` is supplied by the caller when `mode_change_needs_respawn`
    /// said one was needed. Spawning it here would always produce a *local*
    /// agent, which on a remote workspace would quietly move the agent back
    /// onto this machine the first time the permission mode changed.
    /// Swap in a replacement agent for a mode this tab is already in — the
    /// deferred half of `ModeSwitch::LiveThenRespawnAfterTurn`, where the mode
    /// was set when the user chose it and only the process is catching up.
    fn set_permission_mode_forced(
        &mut self,
        mode: crate::settings::AgentPermissionMode,
        replacement: Option<crate::agent_panel::AgentSession>,
    ) {
        if let Some(replacement) = replacement {
            let items = std::mem::take(&mut self.session.items);
            let model = self.session.model.clone();
            self.session = replacement;
            self.session.items = items;
            self.session.model = model;
        }
        self.permission_mode = mode;
    }

    fn set_permission_mode(
        &mut self,
        mode: crate::settings::AgentPermissionMode,
        replacement: Option<crate::agent_panel::AgentSession>,
    ) {
        if self.permission_mode == mode { return; }
        if let Some(replacement) = replacement {
            let items = std::mem::take(&mut self.session.items);
            let model = self.session.model.clone();
            self.session = replacement;
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

/// The reasoning badge without the word "Reasoning", for a narrow strip.
///
/// The value is the part that changes and the part worth reading; the noun is
/// nearly half the width and says the same thing every time.
fn reasoning_badge_label_short(ep: &serde_json::Value) -> String {
    reasoning_badge_label(ep)
        .strip_prefix("Reasoning: ")
        .unwrap_or("Default")
        .to_string()
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

/// A dot that breathes — a call that is *running*, as opposed to one that has
/// finished.
///
/// The distinction was carried by colour alone, so a call that had been running
/// for five minutes looked exactly like a stale icon, and there was no way to
/// tell a working agent from a stuck one. Movement is the only thing that
/// answers that, and it has to be where the eye already is: on the card that is
/// executing, not only on the status line above the input box.
///
/// The ring around it grows with the dot, so this reads as motion even at a
/// glance and even for a viewer who cannot see the colour change. Costs nothing
/// when nothing is running: the panel already repaints on a 50ms cadence while a
/// turn is live and not at all when it is not, so this adds no wakeups.
/// What the user chose from the "a newer build is installed" banner.
enum BuildUpdateAction { ThisWindow, Everything, Later }

/// Which build this process is running.
///
/// The version alone does not distinguish two builds of the same version, which
/// is exactly the case that matters while rolling a new build out one window at a
/// time — every window says `0.3.0` and none of them says which `0.3.0`. The
/// binary's own timestamp does distinguish them, and needs nothing compiled in.
///
/// Read once per process: it is the file this process was started from, and
/// replacing that file on disk does not change what is already running — which is
/// the whole point of being able to ask.
fn build_stamp() -> &'static str {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(|| {
        let built = running_build().map(format_build_time)
            .unwrap_or_else(|| "unknown build".to_string());
        format!("{} · {built}", env!("CARGO_PKG_VERSION"))
    })
}

fn exe_mtime() -> Option<std::time::SystemTime> {
    std::env::current_exe().and_then(|p| p.metadata()).and_then(|m| m.modified()).ok()
}

/// The binary this process started from, captured once.
///
/// Captured rather than read on demand: installing a new build replaces the file
/// underneath a running process, and what this process is *running* does not
/// change when that happens. That difference is the whole signal — see
/// `newer_build_installed`.
fn running_build() -> Option<std::time::SystemTime> {
    static AT_START: std::sync::OnceLock<Option<std::time::SystemTime>> =
        std::sync::OnceLock::new();
    *AT_START.get_or_init(exe_mtime)
}

/// Whether a newer build has been installed since this process started.
///
/// Installing an update does not change a running window — it cannot, the
/// process is already up. Until now nothing said so, which left the state where
/// a user installs an update, sees no change, and has no way to tell whether it
/// worked. Comparing what is on disk against what this process started from says
/// it exactly.
fn newer_build_installed() -> bool {
    build_is_newer(running_build(), exe_mtime())
}

/// The comparison behind `newer_build_installed`, separated so both answers can
/// be tested — the interesting one being "yes", which a test cannot arrange by
/// replacing the binary it is running from.
///
/// Unknown either way is "not behind": a window that cannot read its own
/// timestamp should say nothing rather than nag about an update it cannot
/// confirm.
fn build_is_newer(
    started: Option<std::time::SystemTime>,
    on_disk: Option<std::time::SystemTime>,
) -> bool {
    match (started, on_disk) {
        (Some(started), Some(now)) => now > started,
        _ => false,
    }
}

/// This machine's offset from UTC, in seconds, read once.
///
/// Asked of `date`, which knows about the zone and about daylight saving, rather
/// than reimplemented or pulled in as a dependency. Once per process, so the cost
/// is one subprocess at the first mention of a build time.
fn utc_offset_secs() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        // `+%z` is `-0400`, `+0530`, and so on.
        let out = std::process::Command::new("date").arg("+%z").output().ok();
        let text = out
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        parse_utc_offset(text.trim())
    })
}

/// `-0400` as seconds. Unparseable is zero, which shows UTC rather than a wrong
/// local time — a build time nobody can compare against is better than one that
/// looks comparable and is four hours out.
fn parse_utc_offset(text: &str) -> i64 {
    let sign = match text.as_bytes().first() {
        Some(b'-') => -1,
        Some(b'+') => 1,
        _ => return 0,
    };
    let digits = &text[1..];
    if digits.len() < 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let hours: i64 = digits[..2].parse().unwrap_or(0);
    let mins:  i64 = digits[2..4].parse().unwrap_or(0);
    sign * (hours * 3600 + mins * 60)
}

/// A build time as `Aug 21 23:16`, in *this machine's* time.
///
/// It was rendered in UTC, which for the one thing it is for — telling whether a
/// window is running the build you just made — is worse than useless: it looked
/// like a local time and was four hours out, so a window on an old build could
/// read as newer than the one on disk.
fn format_build_time(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format_civil((secs as i64 + utc_offset_secs()).max(0) as u64)
}

/// Seconds since the epoch as `Aug 21 23:16`, with no notion of a timezone —
/// whatever it is handed is what it renders. Kept apart from the shift above so
/// the calendar arithmetic can be tested against known instants without the test
/// depending on where the machine is.
fn format_civil(secs: u64) -> String {
    // Civil date from a unix timestamp, without reaching for a date library for
    // one line of output. Days-from-epoch to y/m/d by Howard Hinnant's method.
    let days = (secs / 86_400) as i64;
    let rem  = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let _year = y + i64::from(m <= 2);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let name = MONTHS.get((m as usize).saturating_sub(1)).copied().unwrap_or("???");
    format!("{name} {d} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Seconds a step must run before its elapsed time is shown.
///
/// Most calls finish faster than this, and a number that appears and vanishes on
/// every one of them is noise — the point is the ones that do not finish.
const ELAPSED_SHOWN_AFTER: u64 = 2;

/// The countdown to show beside the elapsed time, if one is still honest.
///
/// Two ways an estimate goes wrong, and both have to fail quietly.
///
/// It can be *too long*: the command finishes early. Nothing here can see that,
/// but nothing needs to — the expectation belongs to the step, and the step ends
/// when the result arrives, taking its countdown with it. There is no timer left
/// running against a command that is already done.
///
/// And it can be *too short*: the command outlives its own estimate. From that
/// moment the estimate predicts nothing, so it stops being shown rather than
/// sitting at "0s left" or counting into the negative. The elapsed time carries
/// on, which is the honest thing left to say.
fn countdown_label(
    expect:  Option<crate::agent_panel::Expectation>,
    elapsed: std::time::Duration,
) -> Option<String> {
    use crate::agent_panel::Expectation;
    let expect = expect?;
    let remaining = expect.duration().checked_sub(elapsed)?;
    if remaining.as_secs() == 0 { return None; }
    Some(match expect {
        // "about" for a sleep, which is waiting on the clock and will take that
        // long; "up to" for a timeout, which is a ceiling it may come in under.
        Expectation::About(_)  => format!("~{} left", human_elapsed(remaining)),
        Expectation::AtMost(_) => format!("up to {} left", human_elapsed(remaining)),
    })
}

/// A duration as a person would say it.
fn human_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

fn paint_running_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let t = ui.input(|i| i.time);
    // ~1.6s per breath: slow enough to read as deliberate, fast enough that a
    // glance catches it.
    let phase = ((t * 4.0).sin() * 0.5 + 0.5) as f32;
    let radius = 2.4 + 1.4 * phase;
    ui.painter().circle_filled(rect.center(), radius, color);
    let halo = egui::Color32::from_rgba_unmultiplied(
        color.r(), color.g(), color.b(), (70.0 * (1.0 - phase)) as u8,
    );
    ui.painter().circle_stroke(rect.center(), radius + 2.0, egui::Stroke::new(1.0_f32, halo));
    // Matched to the cadence the panel already keeps while a turn is live, not a
    // bare `request_repaint()`: that asks for the next frame as fast as the loop
    // will give it, which is how an animation quietly becomes a busy loop.
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
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
/// A status badge's own padding, the gap before its disclosure triangle, and
/// the triangle's width. Constants rather than literals at each call site
/// because the width test adds them up, and a test that measures numbers the
/// drawing code no longer uses would pass while the row wrapped.
const BADGE_PAD: f32 = 6.0;
const BADGE_GAP: f32 = 2.0;
const TRIANGLE_W: f32 = 10.0;

/// What a badge costs beyond its text. Only the width test needs the total —
/// the drawing code spends it a piece at a time.
#[cfg(test)]
fn status_badge_chrome(triangle: bool) -> f32 {
    if triangle { BADGE_PAD + BADGE_GAP + TRIANGLE_W + BADGE_PAD } else { BADGE_PAD * 2.0 }
}

fn draw_status_badge(
    ui: &mut egui::Ui,
    salt: &str,
    text: &str,
    color: egui::Color32,
    expanded: bool,
    height: f32,
    triangle: bool,
) -> egui::Response {
    let bg = ui.painter().add(egui::Shape::Noop);
    let inner = ui.allocate_ui(egui::vec2(0.0, height), |ui| {
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.add_space(BADGE_PAD);
            ui.label(egui::RichText::new(text).size(10.5).color(color));
            if triangle {
                ui.add_space(BADGE_GAP);
                paint_disclosure_triangle(ui, expanded, color);
            }
            ui.add_space(BADGE_PAD);
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
    let (rect, _) = ui.allocate_exact_size(egui::vec2(TRIANGLE_W, 14.0), egui::Sense::hover());
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
/// Where the minimap's viewport box starts and ends horizontally, given how
/// far the editor is scrolled sideways.
///
/// `first_col` and `cols` are in characters — the minimap's own axis is a
/// fixed number of points per character, so the conversion is one multiply.
/// Everything is in the same unit deliberately: mixing points and columns is
/// how the vertical indicator would have gone wrong too.
fn minimap_span(left: f32, first_col: f32, cols: f32, points_per_col: f32) -> (f32, f32) {
    let x0 = left + first_col.max(0.0) * points_per_col;
    (x0, x0 + cols.max(1.0) * points_per_col)
}

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
/// How the agent's status strip is drawn at a given width.
///
/// Four things earn a place in the strip, because they are what you need while
/// working: which model, what it is allowed to do without asking, how hard it is
/// thinking when that applies, and how full the context is. Everything else —
/// the context strategy, whether the session is online — is a setting you touch
/// occasionally, and lives behind the overflow rather than competing for the row.
///
/// Below that, the labels shorten before anything is dropped: `Always Ask`
/// becomes `Ask`, `Reasoning: Default` becomes `Default`, `0% ctx` becomes `0%`.
/// The value is the part that changes; the noun is nearly half the width and
/// reads the same every time.
///
/// The narrow case used to wrap raggedly instead. The `·` separators are their
/// own widgets, so a wrap could leave one at the *start* of a row, or split an
/// item from the separator that introduced it — which is why a narrow panel
/// looked broken rather than merely full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StripDetail {
    /// The `·` between items. Decoration, and the first thing to go.
    separators: bool,
    /// Short labels rather than full ones.
    compact: bool,
    /// How much model name there is room for. Names run long — dated Anthropic
    /// ids, local `qwen3-coder-30b-a3b`-style tags — and one of those was
    /// enough to wrap the row by itself, so it gets an explicit budget rather
    /// than whatever is left.
    model_chars: usize,
    /// The little disclosure triangles. A badge still highlights on hover and
    /// still opens its menu without one, so at the narrowest the panel goes
    /// they are 40pt of chrome bought back across the four badges.
    triangles: bool,
}

/// What to give up first as the panel narrows. Decoration goes before words,
/// words shorten before anything is dropped, and nothing is dropped at all —
/// model, permissions, reasoning and context usage survive to 240pt, which is
/// as narrow as the splitter goes. The thresholds are measured against real
/// font layout in `strip_width_tests`, not estimated.
fn strip_detail(avail: f32) -> StripDetail {
    StripDetail {
        separators: avail >= 620.0,
        compact:    avail < 520.0,
        triangles:  avail >= 310.0,
        model_chars: if avail < 310.0 { 7 }
                     else if avail < 520.0 { 14 }
                     else if avail < 620.0 { 20 }
                     else { 34 },
    }
}

/// The model, for a strip too narrow to spell it out. The vendor is the least
/// useful part of the name when there is one badge showing it, so it goes
/// first, and what is left is cut to fit.
fn model_badge_label_short(label: &str, max: usize) -> String {
    let trimmed = ["claude-", "gpt-", "grok-", "gemini-", "llama-", "qwen-", "deepseek-"]
        .iter()
        .find_map(|p| label.strip_prefix(*p))
        .unwrap_or(label);
    elide_chars(trimmed, max)
}

fn model_badge_label(label: &str, detail: StripDetail) -> String {
    model_badge_label_short(label, detail.model_chars)
}

/// How many characters of a host name the status badge shows.
///
/// Long enough for the names people actually use, short enough that the badge
/// stays a badge. `Admin-1-Tailscale` is 17.
const HOST_BADGE_CHARS: usize = 14;

/// The connected badge's text: a power symbol and the host, cut to a length the
/// badge can hold.
///
/// The badge used to be a fixed 36pt slot with this painted centred inside it, so
/// a name of any real length spilled out of both sides — clipped at the window
/// edge on the left and drawn over the next thing along on the right. Two fixes,
/// and this is the one that bounds it: past a limit the name ends in an ellipsis
/// rather than growing without end. The badge sizes itself to whatever this
/// returns.
fn ssh_badge_label(name: &str) -> String {
    format!("⏻ {}", elide_chars(name, HOST_BADGE_CHARS))
}

/// `text`, or its first `max` characters and an ellipsis.
///
/// Counts characters, not bytes: cutting a name mid-character is a panic, and a
/// remote host can be called anything.
fn elide_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

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
                                (_, false, _)                => paint_running_dot(ui, rail),
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
                            (_, false, _)                => paint_running_dot(ui, status_color),
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
                    // Two lines, always. The preview and the controls cannot
                    // share one: reserving space by measuring buttons is never
                    // exact enough — padding and style both get a say — and
                    // being a few points short draws the text *under* them,
                    // which is what it did. Stacking needs no measurement and
                    // is right at every width; a checkpoint is one row in a
                    // conversation, not a dense list, so the line costs little.
                    ui.horizontal(|ui| {
                        paint_dot(ui, egui::Color32::from_gray(140));
                        ui.label(egui::RichText::new("checkpoint").size(10.5).color(egui::Color32::from_gray(160)));
                        let preview_short: String = preview.chars().take(60).collect();
                        ui.add_sized(
                            egui::vec2(ui.available_width(), 16.0),
                            egui::Label::new(egui::RichText::new(preview_short).size(10.5)
                                .color(egui::Color32::from_gray(190))).truncate(),
                        );
                    });
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
        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(52, 84, 92)))
        .inner_margin(egui::Margin::symmetric(8.0, 5.0))
        .rounding(5.0)
        .show(ui, |ui| {
            ui.vertical(|ui| {
                let header = ui.horizontal(|ui| {
                    paint_disclosure_triangle(ui, expanded, egui::Color32::from_gray(150));
                    paint_dot(ui, egui::Color32::from_rgb(150, 170, 255));
                    ui.label(egui::RichText::new(agent_type).monospace().size(11.5).strong()
                        .color(egui::Color32::from_rgb(150, 205, 210)));
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
                                                .color(egui::Color32::from_rgb(150, 205, 210)));
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

/// Append the "this ran elsewhere" divider, once.
///
/// Idempotent: reloading twice in a row must not stack two of them, and a window
/// that never reconnected has nothing new to mark.
/// Part company with a remote host: mark the transcript, and give up the session
/// id that belonged to it.
///
/// The id names a log on a machine we are no longer connected to, so resuming it
/// cannot work. It did *recover* — forge-agent reports the session as missing and
/// the tab falls back to a fresh one — but it recovered from a spawn that never
/// could have worked, and reported it as an error the reader then has to
/// interpret. Better not to ask.
fn leave_remote(items: &mut Vec<ChatItem>, session_id: &mut String, host: &str) {
    note_remote_boundary(items, host);
    session_id.clear();
}

fn note_remote_boundary(items: &mut Vec<ChatItem>, host: &str) -> bool {
    if items.is_empty() { return false; }
    let note = format!(
        "Connection to {host} ended here. Everything above ran on {host} — \
         including its session, which stays there. This window is local now, so \
         continuing below continues a different conversation."
    );
    if matches!(items.last(), Some(ChatItem::Status(s)) if *s == note) { return false; }
    items.push(ChatItem::Status(note));
    true
}

/// Whether this subagent has an approval waiting on it.
///
/// The one reason left for the docked strip to exist. A subagent's activity is
/// in the transcript now, but an approval three screens up in the scroll is an
/// approval nobody answers — and the turn stays blocked until someone does. So
/// the strip appears for exactly this, and is otherwise absent rather than
/// being a second copy of the block.
fn subagent_awaiting_approval(item: &ChatItem) -> bool {
    let ChatItem::Subagent { finished: false, items, .. } = item else { return false };
    items.iter().any(|c| match c {
        ChatItem::ToolRequest { approval: ApprovalState::Pending, .. } => true,
        // A nested subagent's approval blocks this one just as surely.
        nested => subagent_awaiting_approval(nested),
    })
}

/// The subagent's own region of the transcript.
///
/// A delegated task used to be a one-line marker saying "running — see below",
/// with the work itself in the docked strip. Reading the transcript, there was
/// no telling which lines were the main agent's and which belonged to a
/// subagent — the complaint this exists to answer.
///
/// So the subagent gets a *container*: a teal rule down the left edge, its name
/// at the top, and everything it did inside. Anything inside the rule is the
/// subagent's; anything outside it is the agent you are talking to. Collapsible,
/// because a finished subagent's tool calls are exactly the sort of thing you
/// want folded away once you have its answer.
///
/// `path` is this subagent's address in the item tree (`[7]` at the top level,
/// `[7, 2]` for one it delegated to in turn), so an approval clicked inside a
/// nested block still lands on the right tool call.
#[allow(clippy::too_many_arguments)]
fn draw_subagent_block(
    ui:           &mut egui::Ui,
    path:         &[usize],
    agent_type:   &str,
    prompt:       &str,
    current_tool: &str,
    detail:       &str,
    finished:     bool,
    summary:      &str,
    expanded:     bool,
    items:        &[ChatItem],
    pad_l:        f32,
    pad_r:        f32,
    pending_action: &mut Option<(Vec<usize>, bool)>,
    toggle_expand:  &mut Option<Vec<usize>>,
) {
    const RULE: egui::Color32 = egui::Color32::from_rgb(72, 132, 142);
    const NAME: egui::Color32 = egui::Color32::from_rgb(150, 205, 210);

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(pad_l);
        let block = egui::Frame::none()
            .fill(egui::Color32::from_rgb(26, 30, 32))
            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(48, 72, 78)))
            .inner_margin(egui::Margin { left: 10.0, right: 8.0, top: 6.0, bottom: 6.0 })
            .rounding(4.0)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() - pad_r);
                ui.vertical(|ui| {
                    let header = ui.horizontal(|ui| {
                        paint_disclosure_triangle(ui, expanded, egui::Color32::from_gray(150));
                        if finished { paint_checkmark(ui, NAME); } else { paint_dot(ui, NAME); }
                        ui.label(egui::RichText::new("subagent").size(11.0)
                            .color(egui::Color32::from_gray(140)));
                        ui.label(egui::RichText::new(agent_type).monospace().size(11.5).strong()
                            .color(NAME));
                        // What it is doing *now*, in the transcript, rather than
                        // a pointer to somewhere else. `detail` is the argument
                        // the tool was called with, which is the part that says
                        // which file or which command.
                        //
                        // Sized to what is left and truncated, never laid out at
                        // its natural width: a label that big sets a minimum
                        // width for the row, which sets one for the block, which
                        // makes the whole transcript wider than the panel it is
                        // in — and then every line in it is clipped at the right
                        // edge. `add_sized` cannot push the row wider.
                        if !finished && !current_tool.is_empty() {
                            let live = if detail.is_empty() {
                                current_tool.to_string()
                            } else {
                                format!("{current_tool} · {detail}")
                            };
                            let font = egui::FontId::monospace(10.0);
                            let room = ui.available_width();
                            if room > 24.0 {
                                let text = elide_path_head(ui, &live, &font, room);
                                ui.add_sized(
                                    egui::vec2(room, 14.0),
                                    egui::Label::new(
                                        egui::RichText::new(text).monospace().size(10.0)
                                            .color(egui::Color32::from_gray(140)),
                                    ).truncate(),
                                );
                            }
                        }
                    });
                    if header.response.interact(egui::Sense::click()).clicked() {
                        *toggle_expand = Some(path.to_vec());
                    }

                    // The task it was given, always — it is the one line that
                    // says why this block exists.
                    let ask: String = prompt.trim().chars().take(120).collect();
                    if !ask.is_empty() {
                        ui.label(egui::RichText::new(soft_wrap(&ask, wrap_run(ui, 10.5, false)))
                            .size(10.5).color(egui::Color32::from_gray(150)));
                    }

                    if expanded {
                        let inner = ui.min_rect();
                        let mut i = 0;
                        while i < items.len() {
                            match &items[i] {
                                ChatItem::ToolRequest { .. } => {
                                    let mut local_pending: Option<(usize, bool)> = None;
                                    let mut local_toggle:  Option<usize> = None;
                                    i = draw_tool_run(ui, items, i, 0.0, 0.0,
                                                      &mut local_pending, &mut local_toggle);
                                    if let Some((li, approve)) = local_pending {
                                        let mut p = path.to_vec();
                                        p.push(li);
                                        *pending_action = Some((p, approve));
                                    }
                                }
                                // A subagent that delegated in turn. Nested
                                // inside its parent's rule, because that is
                                // where it belongs: the parent is blocked
                                // waiting on it, not doing something else.
                                ChatItem::Subagent {
                                    agent_type: cat, prompt: cprompt, current_tool: ctool,
                                    detail: cdetail, finished: cfin, summary: csum,
                                    expanded: cexp, items: nested, ..
                                } => {
                                    let mut child = path.to_vec();
                                    child.push(i);
                                    draw_subagent_block(
                                        ui, &child, cat, cprompt, ctool, cdetail, *cfin, csum,
                                        *cexp, nested, 0.0, 0.0, pending_action, toggle_expand,
                                    );
                                    i += 1;
                                }
                                _ => { i += 1; }
                            }
                        }
                        if items.is_empty() {
                            ui.label(egui::RichText::new("no tool calls").italics().size(10.0)
                                .color(egui::Color32::from_gray(110)));
                        }
                        let _ = inner;
                    }

                    // Its answer, which is the part worth reading once it is
                    // done — shown whether or not the work above is folded away.
                    if finished && !summary.trim().is_empty() {
                        ui.add_space(3.0);
                        let s: String = summary.trim().chars().take(300).collect();
                        ui.label(egui::RichText::new(soft_wrap(&s, wrap_run(ui, 10.5, false)))
                            .size(10.5).color(egui::Color32::from_gray(185)));
                    }
                });
            });
        // The rule itself, painted down the finished block so it spans
        // everything inside — including nested blocks, which get their own.
        let r = block.response.rect;
        ui.painter().rect_filled(
            egui::Rect::from_min_max(r.left_top(), egui::pos2(r.left() + 2.0, r.bottom())),
            0.0,
            RULE,
        );
    });
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
    /// Whether a newer build is sitting on disk than the one this window is
    /// running, and when that was last checked. Checked on a slow cadence
    /// because it is a `stat`, and only while the window is drawing anyway — an
    /// untouched window learning about it a few seconds late costs nothing.
    build_is_stale: bool,
    build_checked:  Option<std::time::Instant>,
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
    /// The `⋯` menu at the end of the agent's status strip: the settings you
    /// touch occasionally, kept out of the row that has to fit while working.
    agent_overflow_open:        bool,
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
    /// Fingerprint of a host not in known_hosts, awaiting the user's answer.
    ssh_host_key_prompt: Option<String>,
    /// A host whose key no longer matches known_hosts. Shown, never offered as
    /// a choice — nothing here can distinguish a rebuilt machine from an
    /// interception, so it is not Forge's decision to make.
    ssh_host_key_changed: Option<String>,
    /// Size the remote pty and the grid were last told about.
    ssh_term_size: (u16, u16),
    /// A size seen but not yet held long enough to act on — see the resize
    /// debounce in `draw_ssh_terminal`.
    ssh_term_pending_size: Option<((u16, u16), std::time::Instant)>,
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
    /// The remote folder chooser, when open. "Open Folder" cannot use the
    /// native dialog on a remote workspace — that browses this machine, which
    /// is the one the user is not working on.
    remote_picker:   Option<RemotePicker>,
    /// The "New Folder…" prompt, and the result of the last attempt.
    remote_new_folder: Option<RemoteNewFolder>,
    mkdir_rx:          Option<mpsc::Receiver<Result<String, String>>>,
    /// Listing in flight for the picker, kept separate from `ssh_nav_rx` so
    /// browsing in the dialog does not disturb the explorer's own position.
    picker_rx:       Option<mpsc::Receiver<Result<(String, Vec<crate::ssh::RemoteEntry>), String>>>,
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
    /// Rebuild this one window in place, leaving the process and every other
    /// window alone. See `reload_window`.
    pub pending_window_reload: bool,
    /// Restart this one window into a process of its own. See `restart_window`.
    pub pending_window_restart: bool,
    /// Reopen *these* folders in one new process and end this one — the window
    /// list gathered from every Forge process, not just this one's. See
    /// `consolidate_windows`.
    pub pending_consolidate: Option<Vec<(u64, Option<PathBuf>)>>,
    /// Exit without replacing this process: another one is taking its windows.
    pub pending_quit: bool,
    /// Which rebuild of this window this is. See `NewWindowSpec::reload_count`.
    reload_count: u32,
    /// SSH host to connect on the first draw (set by new_with_spec).
    pending_ssh_connect: Option<crate::ssh::SshHost>,
}

/// Asking for a folder on the remote, and where.
struct RemoteNewFolder {
    parent: String,
    name:   String,
    error:  Option<String>,
    busy:   bool,
}

/// State of the remote folder chooser.
struct RemotePicker {
    /// Directory being shown. Absolute on the remote, or `~` before the first
    /// listing resolves it.
    path:    String,
    entries: Vec<crate::ssh::RemoteEntry>,
    /// Set while a listing is in flight, so the dialog can say so rather than
    /// looking frozen on a slow link.
    loading: bool,
    error:   Option<String>,
    /// What is in the path box. Kept apart from `path`, which is where the
    /// listing came from — otherwise every keystroke would look like navigation.
    typed:   String,
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

    pub fn new_with_spec(mut spec: NewWindowSpec) -> Self {
        let built_from = std::time::Instant::now();
        let is_reload = spec.is_reload;
        let spec_reload_count = spec.reload_count;
        let notes = std::mem::take(&mut spec.notes);
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
            agent_overflow_open:        false,
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
            ssh_host_key_prompt: None,
            ssh_host_key_changed: None,
            ssh_term_size: (0, 0),
            ssh_term_pending_size: None,
            ssh_term_cached_galley: None,
            ssh_term_cached_scrollback_galley: None,
            ssh_term_last_output: None,
            last_interaction_at: None,
            ssh_connect_rx:   None,
            ssh_pty_rx:       None,
            ssh_log_rx:       None,
            ssh_nav_rx:      None,
            remote_picker:   None,
            remote_new_folder: None,
            mkdir_rx:          None,
            picker_rx:       None,
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
            pending_window_reload: false,
            pending_window_restart: false,
            pending_consolidate: None,
            pending_quit: false,
            reload_count: spec_reload_count,
            pending_ssh_connect: pending_ssh,
            show_update_prompt: false,
            update_check_rx: None,
            update_available: None,
            build_is_stale: false,
            build_checked:  None,
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

        // A window *opening* a remote workspace does not want some other
        // window's local files and terminals; a window *reloading* one is the
        // same window and wants exactly its own back — including its terminals,
        // which are the local pty daemon's and would otherwise be left running
        // with nothing pointing at them.
        if should_restore_on_open(
            app.settings.restore_session, is_reload, app.pending_ssh_connect.is_some(),
        ) {
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
                        .map(|conv| {
                            let mode = app.settings.default_agent_permission_mode;
                            let resume = (!conv.forge_session_id.is_empty())
                                .then_some(conv.forge_session_id.as_str());
                            // With a reconnect in flight, no process yet: a
                            // local agent started here would be on the wrong
                            // machine, and would try to resume a session that
                            // is on the other one. `finish_pending_agents`
                            // gives these real sessions once the connection
                            // resolves, either way it resolves.
                            let session = match &app.pending_ssh_connect {
                                Some(host) => crate::agent_panel::AgentSession::pending(
                                    format!("Reconnecting to {}…", host.host), resume,
                                ),
                                None => app.start_agent_session(mode, resume),
                            };
                            AgentTab::reopen(session, mode, conv)
                        })
                        .collect();
                    if !app.agent_tabs.is_empty() {
                        app.agent_active = state.agent_active.min(app.agent_tabs.len() - 1);
                    }
                }
            }
        }

        // A rebuild is fast enough to be hard to believe: the OS window, the
        // swapchain and the egui context all stay, so nothing flickers and the
        // whole thing lands inside a frame or two. So it says so — with a count
        // that goes up, which is the part a suspicious user can actually check,
        // and what it brought back with it.
        let took = built_from.elapsed();
        // Every window, not just a rebuilt one: with a build rolled out one
        // window at a time, "which one is this?" is a question about all of them.
        app.output_log(format!("Forge IDE {}", build_stamp()), OutputLevel::Info);
        for note in notes {
            app.output_log(note, OutputLevel::Info);
        }
        if spec_reload_count > 0 {
            let files = app.buffers.len();
            let terms = app.terminal_tabs.len();
            let tabs  = app.agent_tabs.len();
            app.status = format!(
                "Window reloaded (#{spec_reload_count}) in {}ms", took.as_millis(),
            );
            app.output_log(
                format!(
                    "Window rebuilt (#{spec_reload_count}) in {}ms —                      {files} file(s), {terms} terminal(s), {tabs} agent tab(s)",
                    took.as_millis(),
                ),
                OutputLevel::Success,
            );
        } else if is_reload {
            // The other kind: a new process, which cannot be handed a count
            // through its argument list and legitimately starts again at one.
            app.status = "Forge restarted".into();
            app.output_log(
                format!("New process ready in {}ms", took.as_millis()),
                OutputLevel::Success,
            );
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
    pub fn restart_process(&mut self) {
        self.note_remote_session_end();
        self.save_session_for_reload();
        self.pending_reload = true;
    }

    /// Restart the windows *this* process has.
    ///
    /// Not offered as a command any more: a menu with both this and the
    /// cross-process restart on it asked the user to care which process a window
    /// happens to live in, which is exactly the thing they should not have to
    /// think about. It stays as the thing a process does when it *receives* the
    /// broadcast — one process's share of a restart everyone was asked to make.
    fn restart_all_windows(&mut self) {
        self.restart_process();
    }

    /// Bring every window back into one process.
    ///
    /// The rollout leaves a process per window, and macOS gives every instance of
    /// an app its own Dock tile — so three projects rolled out one at a time is
    /// three Forge icons, and the pinned one goes dark once the instance it was
    /// associated with has been replaced. That is the cost of the model, and this
    /// is the way back from it: when the rollout is done and every window is on
    /// the new build, collapse them into a single process again.
    ///
    /// The window list comes from the record rather than from this process, which
    /// only knows its own windows. The record knows all of them because each
    /// process writes its own entries and keeps everyone else's.
    pub fn consolidate_windows(&mut self) {
        // By id as well as folder: the id is what carries a window's geometry and
        // its remote host through the record, neither of which fits in an
        // argument list.
        let mut folders: Vec<(u64, Option<PathBuf>)> = crate::session::load_windows()
            .into_iter()
            .filter(|r| r.cwd.as_ref().is_none_or(|p| p.is_dir()))
            .map(|r| (r.id, r.cwd))
            .collect();
        // This process's own window may not be in the record yet — a window
        // opened seconds ago writes on a settle timer.
        if !folders.iter().any(|(id, _)| *id == self.window_id) {
            folders.push((self.window_id, self.workspace_root()));
        }

        let reached = crate::ptyhost::shared()
            .map(|c| c.broadcast("handover"))
            .unwrap_or_else(|| Err("no pty host to carry it".into()));
        match reached {
            Ok(n) => self.output_log(
                format!("Collecting {} window(s) from {} other process{} into this one",
                        folders.len(), n, if n == 1 { "" } else { "es" }),
                OutputLevel::Info,
            ),
            Err(e) => self.output_log(
                format!("Could not reach the other processes ({e}) — they will keep their \
                         own windows; close them by hand to finish consolidating"),
                OutputLevel::Warn,
            ),
        }

        self.save_session_for_reload();
        self.pending_consolidate = Some(folders);
    }

    /// Ask every Forge window, in every process, to restart.
    ///
    /// One process can only restart its own windows. Once windows have been
    /// moved out one at a time — which is the whole point of the per-window
    /// restart — "all of them" spans processes that have no way to talk to each
    /// other. Except that they do: every one of them is already connected to the
    /// pty host, so it carries the message. Nothing new to run, and it reaches an
    /// idle window, which a file on disk would not.
    ///
    /// Reports what it reached rather than claiming to have told everyone: an
    /// older daemon does not know how to pass this on, and a process that has
    /// never opened a terminal is not connected to hear it.
    pub fn restart_every_window(&mut self) {
        let reached = crate::ptyhost::shared()
            .map(|c| c.broadcast("restart"))
            .unwrap_or_else(|| Err("no pty host to carry it".into()));
        match reached {
            Ok(0) => self.output_log(
                "No other Forge processes to tell — restarting this one".to_string(),
                OutputLevel::Info,
            ),
            Ok(n) => self.output_log(
                format!("Asked {n} other Forge process{} to restart", if n == 1 { "" } else { "es" }),
                OutputLevel::Success,
            ),
            Err(e) => self.output_log(
                format!("Could not reach other Forge processes ({e}); \
                         restarting this one only — use Restart All Windows in each of the others"),
                OutputLevel::Warn,
            ),
        }
        self.restart_all_windows();
    }

    /// Act on anything another process asked for.
    fn poll_broadcasts(&mut self) {
        let Some(client) = crate::ptyhost::shared() else { return };
        for kind in client.take_broadcasts() {
            match kind.as_str() {
                "restart" => {
                    self.output_log(
                        "Another Forge window asked everything to restart".to_string(),
                        OutputLevel::Info,
                    );
                    self.restart_all_windows();
                }
                // Somebody is collecting every window into one process. Save
                // what this window has and step aside — *without* replacing this
                // process, which is the difference from a restart, and without
                // deregistering its windows from the record, which is where the
                // process taking over reads them from.
                "handover" => {
                    self.output_log(
                        "Handing this window over to another Forge process".to_string(),
                        OutputLevel::Info,
                    );
                    self.save_session_for_reload();
                    self.pending_quit = true;
                }
                _ => {}
            }
        }
    }

    /// Restart this one window into a new process, leaving the others in this one.
    ///
    /// The hard counterpart to `reload_window`. That one rebuilds the window's
    /// state inside the process it is already in, which is fast and keeps
    /// everything — and cannot replace the process, so it cannot pick up a newly
    /// built binary, cannot clear anything the process itself holds (a leaked
    /// handle, a wedged language server, the GPU state), and gives a suspicious
    /// user nothing to see because there is nothing to see.
    ///
    /// This is the other kind. The window is handed to a genuinely new process —
    /// new PID, current binary on disk — and closed here. Every other window
    /// carries on in this process, untouched, which is the part `Restart Forge`
    /// cannot offer.
    ///
    /// The terminals survive it: they belong to the pty daemon, and the new
    /// process reattaches by id, the same way it does across a full restart.
    pub fn restart_window(&mut self) {
        // Said out loud, because this is the one link in the reconnect chain that
        // cannot be tested: everything from the record onwards is, and a live
        // connection recording itself is, but whether *this* window had a
        // connection at the moment it was replaced is only knowable from here.
        // Three rounds of "it still comes back local" were spent guessing at it.
        match self.ssh.as_ref().map(|s| (s.host.name.clone(), s.host.host.clone())) {
            Some((name, host)) => self.output_log(
                format!("Restarting this window — carrying the connection to {name} ({host})"),
                OutputLevel::Info,
            ),
            None => self.output_log(
                "Restarting this window — it has no remote connection to carry".to_string(),
                OutputLevel::Info,
            ),
        }
        self.note_remote_session_end();
        self.save_session_for_reload();
        self.pending_window_restart = true;
    }

    /// Rebuild *this* window and nothing else.
    ///
    /// What a reload is actually wanted for — settings, plugins, language
    /// servers, an agent that has got itself into a state, files changed
    /// underneath the editor — is all per-window, and taking every other
    /// window's conversations and terminals down with it was collateral damage.
    /// It is also what "Reload Window" means everywhere else.
    ///
    /// Done by rebuilding the `IdeApp` from the session it just saved, keeping
    /// the OS window, the swapchain and the egui context. Teardown is by drop:
    /// the agent child and the language servers are killed by their own `Drop`,
    /// and the shells are not affected at all — they live in the pty daemon, so
    /// the new app reattaches to the same running ones by id.
    ///
    /// Deferred to the event loop for the same reason the process restart is:
    /// this is called mid-`draw`, from inside the very value that is about to be
    /// replaced.
    ///
    /// Two things this cannot do, both of which need `Restart Forge`: pick up a
    /// newly built `forge-ide` binary, and keep an SSH session — the connection
    /// belongs to the app being dropped, so a remote window comes back local.
    pub fn reload_window(&mut self) {
        // No boundary written here, unlike the process restart: this window
        // reconnects, and if that works the conversation carries on where it
        // was. `finish_pending_agents` marks it only if the reconnect fails.
        self.save_session_for_reload();
        self.pending_window_reload = true;
    }

    /// Apply a permission-mode change to the active tab, mid-turn included.
    ///
    /// Mid-turn, a loosening change now does what the user meant by making it:
    /// the call the agent is blocked on is approved and the turn carries on. The
    /// alternative was interrupting the turn, changing the mode, and asking
    /// again — three steps to say "yes, go ahead".
    fn change_permission_mode(&mut self, mode: crate::settings::AgentPermissionMode) {
        let idx = self.agent_active;
        let Some(tab) = self.agent_tabs.get(idx) else { return };
        if tab.permission_mode == mode { return; }

        let turn_active = tab.session.is_active();
        let plan = mode_switch_plan(tab.permission_mode, mode, turn_active);
        let resume_id = tab.session.forge_session_id.clone();

        let replacement = matches!(plan, ModeSwitch::RespawnNow).then(|| {
            let resume = (!resume_id.is_empty()).then_some(resume_id.as_str());
            self.start_agent_session(mode, resume)
        });

        match plan {
            ModeSwitch::RespawnNow => {
                self.agent_tabs[idx].set_permission_mode(mode, replacement);
                if turn_active {
                    // Worth saying: the turn stopping is a consequence of the
                    // mode, not something that went wrong.
                    self.output_log(
                        format!("{} needs a new agent, so this turn ended with the old one",
                                mode.label()),
                        OutputLevel::Warn,
                    );
                }
            }
            ModeSwitch::Live | ModeSwitch::LiveThenRespawnAfterTurn => {
                // The live part: forge-agent's auto mode, which it can be told
                // to change while running.
                let tab = &mut self.agent_tabs[idx];
                let want_auto = mode_auto_approves(mode);
                if tab.session.auto_mode != want_auto {
                    tab.session.toggle_auto_mode();
                }
                tab.permission_mode = mode;
                if plan == ModeSwitch::LiveThenRespawnAfterTurn {
                    tab.pending_mode_respawn = Some(mode);
                }

                // And the reason for doing this mid-turn at all.
                if want_auto {
                    let ids = release_pending_approvals(&mut tab.session.items);
                    let n = ids.len();
                    for id in ids {
                        tab.session.approve(id);
                    }
                    if n > 0 {
                        self.output_log(
                            format!("{}: approved {n} call{} the agent was waiting on",
                                    mode.label(), if n == 1 { "" } else { "s" }),
                            OutputLevel::Info,
                        );
                    }
                }
                if plan == ModeSwitch::LiveThenRespawnAfterTurn {
                    self.output_log(
                        format!("{} applies to the rest of this turn; the agent restarts \
                                 after it to skip unrecognised tools too", mode.label()),
                        OutputLevel::Info,
                    );
                }
            }
        }
    }

    /// Give every tab still waiting on a connection a real session.
    ///
    /// Called from both ends of the reconnect: with `ssh` set the tab continues
    /// on the remote, resuming the session that is still sitting on that machine
    /// — the conversation genuinely carries on, which is the whole point of
    /// reconnecting rather than starting over. Without it, the tab continues
    /// locally, and *that* is the case that gets the boundary written into its
    /// transcript, because it is the case where the conversation really did
    /// change machines.
    fn finish_pending_agents(&mut self) {
        let waiting: Vec<usize> = self.agent_tabs.iter().enumerate()
            .filter(|(_, t)| t.session.pending.is_some())
            .map(|(i, _)| i)
            .collect();
        if waiting.is_empty() { return; }

        let connected = self.ssh.is_some();
        let host = self.ssh.as_ref().map(|s| s.host.host.clone())
            .or_else(|| Some(self.ssh_form.host.clone()))
            .unwrap_or_default();

        for i in waiting {
            let mode   = self.agent_tabs[i].permission_mode;
            let resume = self.agent_tabs[i].session.forge_session_id.clone();
            let items  = std::mem::take(&mut self.agent_tabs[i].session.items);
            // Anything typed while it was waiting is still wanted — whether it
            // is sitting in the box or was already sent and held.
            let typed  = std::mem::take(&mut self.agent_tabs[i].session.input);
            let held   = std::mem::take(&mut self.agent_tabs[i].session.queued);

            let resume_arg = (connected && !resume.is_empty()).then_some(resume.as_str());
            let mut session = self.start_agent_session(mode, resume_arg);
            session.items = items;
            session.input = typed;
            if !connected {
                leave_remote(&mut session.items, &mut session.forge_session_id, &host);
            }
            session.queued = held;
            // Sent now that there is something to send it to. The boundary note
            // above it, when there is one, is what says which machine answered.
            session.dispatch_next_queued();
            self.agent_tabs[i].session = session;
        }

        if connected {
            self.output_log(format!("Reconnected to {host}; agent tabs resumed there"),
                            OutputLevel::Success);
        } else {
            self.output_log(
                format!("Could not reconnect to {host} — agent tabs continue on this machine"),
                OutputLevel::Warn,
            );
        }
    }

    /// Mark, in the transcript itself, that everything above it happened
    /// somewhere else.
    ///
    /// Neither reload can keep an SSH session — the connection belongs to the
    /// app being dropped — so the window comes back local while the transcript
    /// comes back whole. Keeping it is right; you can still read what happened.
    /// But without a boundary in it, it reads as one continuous conversation
    /// with the machine you are on, and it is not: the agent that produced it
    /// was on the remote, its session log is on the remote, and continuing here
    /// continues something else. Written here, by the code that drops the
    /// connection, so the mark lands exactly where the break is — and it is part
    /// of the transcript that gets saved a moment later, so it survives.
    fn note_remote_session_end(&mut self) {
        let Some(host) = self.ssh.as_ref().map(|s| s.host.host.clone()) else { return };
        for tab in &mut self.agent_tabs {
            leave_remote(&mut tab.session.items, &mut tab.session.forge_session_id, &host);
        }
    }

    /// How the window comes back: same workspace, same session identity,
    /// restoring unconditionally.
    ///
    /// Every field here is load-bearing. A fresh `window_id` would look up
    /// nobody's session and the window would return empty; `is_reload` false
    /// would leave restoring to a user setting; and `frame`/`maximized` are for
    /// placing a *new* OS window, which this is not — the window stays exactly
    /// where it is.
    pub fn reload_spec(&self) -> NewWindowSpec {
        NewWindowSpec {
            cwd:       self.workspace_root(),
            window_id: self.window_id,
            is_reload: true,
            reload_count: self.reload_count + 1,
            // A remote window comes back remote: the connection cannot survive
            // the rebuild, so it is made again. Key-based hosts reconnect
            // without asking; one that wants a password cannot be reconnected
            // silently — the password was never stored — and falls back to
            // local with the transcript marked.
            ssh_host:  self.ssh.as_ref().map(|s| s.host.clone()),
            frame:     None,
            maximized: false,
            notes:     Vec::new(),
        }
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
                            } else if self.ssh_connecting || self.pending_ssh_connect.is_some() {
                                // Coming back to a remote host. Starting a remote
                                // session from a window with no local folder is
                                // the ordinary way in — you open the IDE and
                                // connect from what is already there — so a
                                // restarted remote window has no folder to show
                                // and used to read as a brand new window for the
                                // second or two the connection takes, and for good
                                // if it failed. It says what it is doing instead.
                                let host = self.pending_ssh_connect.as_ref()
                                    .map(|h| h.host.clone())
                                    .unwrap_or_else(|| self.ssh_form.host.clone());
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.add_space(10.0);
                                    ui.spinner();
                                    ui.label(egui::RichText::new(format!("Reconnecting to {host}…"))
                                        .size(12.0).color(egui::Color32::from_gray(190)));
                                });
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
                                // And if it *was* on a host and could not get back,
                                // the window says so where the folder would be
                                // rather than only in the OUTPUT panel — this is
                                // the state that looked like a fresh window.
                                if let Some(err) = &self.ssh_error.clone() {
                                    ui.add_space(8.0);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.add_space(10.0);
                                        ui.label(egui::RichText::new(format!(
                                            "Could not reconnect: {err}"))
                                            .size(11.0)
                                            .color(egui::Color32::from_rgb(235, 130, 120)));
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_space(10.0);
                                        if ui.button("Try again").clicked() {
                                            self.ssh_connect();
                                        }
                                    });
                                }
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

        // Poll the folder chooser's listing
        if let Some(rx) = &self.picker_rx {
            match rx.try_recv() {
                Ok(Ok((path, entries))) => {
                    if let Some(p) = &mut self.remote_picker {
                        // The box follows a successful listing, so walking the
                        // tree keeps it accurate and a typed path that worked is
                        // left as typed.
                        p.typed = path.clone();
                        p.path = path;
                        // Directories only: this chooses a workspace, and a
                        // file in the list is something to mis-click.
                        p.entries = entries.into_iter().filter(|e| e.is_dir).collect();
                        p.entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                        p.loading = false;
                        p.error = None;
                    }
                    self.picker_rx = None;
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    if let Some(p) = &mut self.remote_picker {
                        p.loading = false;
                        p.error = Some(e);
                    }
                    self.picker_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.picker_rx = None; }
            }
        }

        // Poll a remote mkdir
        if let Some(rx) = &self.mkdir_rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.mkdir_rx = None;
                    self.remote_new_folder = None;
                    self.output_log(format!("Created {path}"), OutputLevel::Success);
                    // Show it: the folder exists but the listing on screen was
                    // taken before it did.
                    if let Some((here, _)) = self.ssh_tree.pop() {
                        self.ssh_navigate(here);
                    }
                    // And if the chooser is open, re-list there too.
                    if let Some(p) = &self.remote_picker {
                        let at = p.path.clone();
                        self.picker_list(at);
                    }
                }
                Ok(Err(e)) => {
                    self.mkdir_rx = None;
                    if let Some(p) = &mut self.remote_new_folder {
                        p.busy = false;
                        p.error = Some(e);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => { self.mkdir_rx = None; }
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
                    // Unset, so the next frame fits it to the panel rather than
                    // leaving the opening size in place.
                    self.ssh_term_size = (0, 0);
                    self.ssh_term_pending_size = None;
                    self.ssh_term_focused = false;
                    self.bottom_tab = BottomTab::Terminal; // switch to terminal tab
                    self.ssh = Some(ready.conn);
                    // Any tab that was holding its transcript waiting for this
                    // gets a real session now — on the remote, resuming the
                    // session it had there, which is still on that machine.
                    self.finish_pending_agents();
                    ctx.request_repaint();
}
                Ok(Err(e)) => {
                    self.ssh_connecting = false;
                    self.ssh_connect_rx = None;
                    self.ssh_log_rx     = None;
                    // A host-key outcome is a question or a warning, not an
                    // error line to scroll past.
                    if let Some(fp) = e.strip_prefix(crate::ssh::UNKNOWN_HOST_PREFIX) {
                        self.ssh_host_key_prompt = Some(fp.to_string());
                        self.status = "Unknown host key".to_string();
                    } else if let Some(detail) = e.strip_prefix(crate::ssh::CHANGED_HOST_PREFIX) {
                        self.ssh_host_key_changed = Some(detail.to_string());
                        self.output_log(
                            "REFUSED: the host key has changed — see the warning".to_string(),
                            OutputLevel::Error,
                        );
                        self.status = "Host key changed — refused".to_string();
                    } else {
                        self.output_log(format!("Error: {e}"), OutputLevel::Error);
                        self.status    = format!("SSH error: {e}");
                        self.ssh_error = Some(e);
                    }
                    // Whatever the reason, tabs cannot wait on a connection
                    // that is not coming. They continue locally, with the break
                    // written into the transcript so the part that ran on the
                    // other machine is not mistaken for this one.
                    self.finish_pending_agents();
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
            if self.quick_open.is_some() {
                self.quick_open = None;
            } else {
                self.open_quick_open();
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
        // Ctrl+Shift+R: rebuild this window in place, same workspace. Only this
        // one — the whole-process restart is File → Restart Forge, which is
        // deliberately not on a chord: it is not a stronger reload, it takes
        // every other window down with it.
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
                    let connected = self.ssh.is_some();
                    // Sized to its text rather than a fixed 36pt. Centring a name
                    // inside a slot too small for it put half of it outside the
                    // badge: off the left edge of the window, and on top of the
                    // label to its right.
                    let badge_font = egui::FontId::proportional(10.5);
                    let badge_text = connected.then(|| {
                        let name = self.ssh.as_ref().map(|s| s.host.name.clone())
                            .unwrap_or_default();
                        ssh_badge_label(&name)
                    });
                    let badge_w = match &badge_text {
                        Some(t) => text_width(ui, t, badge_font.size, false) + 14.0,
                        None    => 36.0,
                    }.max(36.0);
                    let (ssh_rect, ssh_resp) = ui.allocate_exact_size(
                        egui::vec2(badge_w, 22.0), egui::Sense::click());
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
                    if let Some(text) = &badge_text {
                        // Left-aligned inside its own box, so it cannot grow
                        // leftwards past the edge of the window.
                        p.text(egui::pos2(ssh_rect.left() + 7.0, cy),
                            egui::Align2::LEFT_CENTER, text, badge_font.clone(), fg);
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
        self.draw_host_key_dialogs(ctx);
        self.draw_remote_folder_picker(ctx);
        self.draw_remote_new_folder(ctx);

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
        self.refresh_build_staleness();
        self.poll_broadcasts();
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

    /// How often to look for a newly installed build.
    const BUILD_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

    fn refresh_build_staleness(&mut self) {
        let due = self.build_checked
            .is_none_or(|t| t.elapsed() >= Self::BUILD_CHECK_EVERY);
        if !due { return; }
        self.build_checked = Some(std::time::Instant::now());
        let stale = newer_build_installed();
        if stale && !self.build_is_stale {
            // Said once, when it becomes true, rather than every check.
            self.output_log(
                format!("A newer build is installed. This window is running {} — \
                         restart it (File → Restart This Window) to use the new one; \
                         other windows keep running until you restart them too.",
                        build_stamp()),
                OutputLevel::Info,
            );
        }
        self.build_is_stale = stale;
    }

    /// Dismissible banner once a newer release is found. Purely informational
    /// — no auto-download, just a link to the release page.
    fn draw_update_banner(&mut self, ctx: &egui::Context) {
        // A newer build actually *installed* outranks news of a newer release:
        // one is something to read about, the other is something this window is
        // not running yet.
        if self.build_is_stale && !self.update_banner_dismissed {
            let mut action: Option<BuildUpdateAction> = None;
            let running = build_stamp().to_string();
            egui::TopBottomPanel::top("build_stale_banner").show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("A newer build is installed.")
                        .strong().color(egui::Color32::from_rgb(230, 210, 150)));
                    ui.label(egui::RichText::new(format!("This window is running {running}."))
                        .color(egui::Color32::from_gray(190)));
                    if ui.button("Restart this window").clicked() {
                        action = Some(BuildUpdateAction::ThisWindow);
                    }
                    if ui.button("Restart all windows").clicked() {
                        action = Some(BuildUpdateAction::Everything);
                    }
                    // The part worth saying out loud: this is not all-or-nothing.
                    // One window can move to the new build while the others carry
                    // on with whatever they are in the middle of.
                    ui.label(egui::RichText::new(
                        "— one window at a time is fine; the others keep running.")
                        .size(11.0).color(egui::Color32::from_gray(140)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if ui.button("×").clicked() { action = Some(BuildUpdateAction::Later); }
                    });
                });
            });
            match action {
                Some(BuildUpdateAction::ThisWindow) => self.restart_window(),
                Some(BuildUpdateAction::Everything) => self.restart_every_window(),
                Some(BuildUpdateAction::Later) => self.update_banner_dismissed = true,
                None => {}
            }
            return;
        }
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
                    if ui.button("×").clicked() { dismissed = true; }
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
                    // `×`, `‖` and `■` rather than the dedicated symbols
                    // (`✕`, `⏸`, `⏹`): those live in ranges Apple Symbols does
                    // not cover, and Apple Symbols is loaded ahead of Arial
                    // Unicode on purpose — 877 KB against 22 MB resident. They
                    // rendered as an empty box.
                    if ui.button("‖").on_hover_text("Pause").clicked() {
                        if let Some(d) = &mut self.dap { d.pause(); }
                    }
                    if ui.button("■").on_hover_text("Stop (Shift+F5)").clicked() {
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
                if ui.small_button("×").on_hover_text("Close split").clicked() {
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
            let mode = self.settings.default_agent_permission_mode;
            let session = self.start_agent_session(mode, None);
            self.agent_tabs.push(AgentTab::new(session, mode));
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
                        let mode = self.settings.default_agent_permission_mode;
            let session = self.start_agent_session(mode, None);
            self.agent_tabs.push(AgentTab::new(session, mode));
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
                                let mode = self.settings.default_agent_permission_mode;
                                let resume = (!conv.forge_session_id.is_empty())
                                    .then_some(conv.forge_session_id.as_str());
                                let session = self.start_agent_session(mode, resume);
                                let new_tab = AgentTab::reopen(session, mode, conv);
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
                    let mode = self.settings.default_agent_permission_mode;
            let session = self.start_agent_session(mode, None);
            self.agent_tabs.push(AgentTab::new(session, mode));
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
        // Taken from the tab and then released, because starting a replacement
        // needs the app itself — that is where the SSH connection lives — and
        // the tab above is held mutably.
        let fallback = std::mem::take(&mut tab.session.needs_resume_fallback)
            .then(|| (std::mem::take(&mut tab.session.items), tab.permission_mode));
        if let Some((items, mode)) = fallback {
            let replacement = self.start_agent_session(mode, None);
            let tab = &mut self.agent_tabs[self.agent_active];
            tab.session = replacement;
            tab.session.items = items;
        }
        let tab = &mut self.agent_tabs[self.agent_active];

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

        // Waiting on something, deliberately — not a failure, so it does not
        // get the red text or the "check your PATH" advice below.
        if let Some(note) = &tab.session.pending.clone() {
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(format!("  {note}"))
                    .size(11.5).color(egui::Color32::from_gray(150)));
            });
            ui.add_space(4.0);
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
        // Applied after the panel is drawn: a respawn needs the app (for the
        // SSH connection, so a remote workspace gets a remote agent), and the
        // tab is borrowed mutably throughout the drawing below.
        let mut perm_change: Option<crate::settings::AgentPermissionMode> = None;
        // Wrapped, so a narrow panel reflows the badges instead of running them
        // off the edge — and, since egui sizes a panel from its contents, so
        // that this row cannot hold the panel open at its own width.
        let detail = strip_detail(ui.available_width());
        let status_bar = ui.horizontal_wrapped(|ui| {
            ui.set_min_height(status_bar_h);
            ui.add_space(4.0);
            // Drawn only while there is room for decoration; see `strip_detail`.
            let sep = |ui: &mut egui::Ui| {
                if detail.separators {
                    ui.label(egui::RichText::new("·").size(10.5)
                        .color(egui::Color32::from_gray(80)));
                }
            };
            let model = if tab.session.model.is_empty() { "starting…".to_string() }
                        else { display_model_label(&tab.session.model, &tab.session.endpoints) };
            let model = model_badge_label(&model, detail);
            let has_choices = !tab.session.endpoints.is_empty();
            if has_choices {
                let badge = draw_status_badge(ui, "model_badge", &model,
                    egui::Color32::from_gray(140), self.agent_model_picker_open, status_bar_h,
                    detail.triangles);
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
            sep(ui);
            let perm_color = match tab.permission_mode {
                crate::settings::AgentPermissionMode::AlwaysAsk          => egui::Color32::from_gray(170),
                crate::settings::AgentPermissionMode::AutoApprove        => egui::Color32::from_rgb(220, 190, 120),
                crate::settings::AgentPermissionMode::DangerouslySkipAll => egui::Color32::from_rgb(230, 110, 100),
            };
            let perm_label = if detail.compact { tab.permission_mode.short_label() }
                             else               { tab.permission_mode.label() };
            let perm_badge = draw_status_badge(ui, "perm_badge", perm_label,
                perm_color, self.agent_perm_picker_open, status_bar_h, detail.triangles);
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
                sep(ui);
                let reasoning_label = if detail.compact { reasoning_badge_label_short(ep) }
                                      else               { reasoning_badge_label(ep) };
                let reasoning_badge = draw_status_badge(ui, "reasoning_badge", &reasoning_label,
                    egui::Color32::from_gray(140), self.agent_thinking_picker_open, status_bar_h,
                    detail.triangles);
                reasoning_badge_rect = reasoning_badge.rect;
                if reasoning_badge.clicked() {
                    self.agent_thinking_picker_open = !self.agent_thinking_picker_open;
                    self.agent_thinking_picker_frame = 0;
                }
            }
            if tab.session.thinking && !detail.compact {
                let text = if detail.separators { "· thinking" } else { "thinking" };
                ui.label(egui::RichText::new(text).size(10.5)
                    .color(egui::Color32::from_rgb(180, 200, 120)));
            }
            let usage = &tab.session.usage;
            if usage.max_context_tokens > 0 {
                let pct = (usage.last_prompt_tokens as f64 / usage.max_context_tokens as f64 * 100.0)
                    .round() as i64;
                let dot = if detail.separators { "· " } else { "" };
                let unit = if detail.compact { "" } else { " ctx" };
                ui.label(egui::RichText::new(format!("{dot}{pct}%{unit}")).size(10.5)
                    .color(egui::Color32::from_gray(120)))
                    .on_hover_text(format!("{} / {} tokens (last request)",
                        usage.last_prompt_tokens, usage.max_context_tokens));
            }
            // Offline is not a setting you glance at, it is a state that changes
            // what the agent can do — so it stays in the row, while its normal
            // counterpart (online) says nothing and lives in the menu.
            if tab.session.offline_mode {
                let dot = if detail.separators { "· " } else { " " };
                ui.label(egui::RichText::new(format!("{dot}offline")).size(10.5)
                    .color(egui::Color32::from_rgb(200, 150, 90)))
                    .on_hover_text("The agent will not reach the network. Change it under ⋯.");
            }
            // Everything you touch occasionally rather than while working. It
            // was in the row, and between them the context strategy and the
            // network state are the two widest items — which is why the strip
            // wrapped at the width the panel actually opens at.
            sep(ui);
            let overflow = draw_status_badge(ui, "agent_overflow", "⋯",
                egui::Color32::from_gray(140), self.agent_overflow_open, status_bar_h,
                detail.triangles);
            context_badge_rect = overflow.rect;
            if overflow.clicked() {
                self.agent_overflow_open = !self.agent_overflow_open;
            }
            let _ = overflow.on_hover_text("Context strategy, network");
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
                    perm_change = Some(mode);
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
                        perm_change = Some(mode);
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

        // The `⋯` menu: the settings that used to sit in the strip and made it
        // wrap at the width the panel actually opens at. Two entries, each
        // opening or toggling exactly what the badge beside it used to.
        if self.agent_overflow_open {
            let strategy_label = context_strategy_label(&tab.session.context_strategy);
            let offline = tab.session.offline_mode;
            let mut open_strategy = false;
            let mut toggle_network = false;
            let mut dismissed = false;
            let response = egui::Area::new(egui::Id::new("agent_overflow_menu"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(context_badge_rect.left() - 150.0,
                                      context_badge_rect.bottom() + 2.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(37, 37, 38))
                        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(65)))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(8.0, 8.0))
                        .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 4.0), blur: 16.0,
                            spread: 0.0, color: egui::Color32::from_black_alpha(90) })
                        .show(ui, |ui| {
                            ui.set_width(210.0);
                            // Each row says what it is and what it currently is,
                            // since neither is on screen any more.
                            let row = |ui: &mut egui::Ui, name: &str, value: &str| {
                                let r = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 22.0),
                                    egui::Sense::click(),
                                );
                                if r.1.hovered() {
                                    ui.painter().rect_filled(r.0, 3.0,
                                        egui::Color32::from_gray(55));
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                ui.painter().text(
                                    r.0.left_center() + egui::vec2(6.0, 0.0),
                                    egui::Align2::LEFT_CENTER, name,
                                    egui::FontId::proportional(11.5),
                                    egui::Color32::from_gray(210));
                                ui.painter().text(
                                    r.0.right_center() - egui::vec2(6.0, 0.0),
                                    egui::Align2::RIGHT_CENTER, value,
                                    egui::FontId::proportional(11.0),
                                    egui::Color32::from_gray(140));
                                r.1
                            };
                            if row(ui, "Context strategy", strategy_label).clicked() {
                                open_strategy = true;
                            }
                            if row(ui, "Network", if offline { "Offline" } else { "Online" })
                                .on_hover_text(if offline {
                                    "No network calls except this session's own model API"
                                } else {
                                    "web_fetch and Codex background checks run normally"
                                })
                                .clicked()
                            {
                                toggle_network = true;
                            }
                        });
                })
                .response;
            // Click anywhere else closes it, as the other pickers do.
            if ui.ctx().input(|i| i.pointer.any_click())
                && !response.rect.contains(
                    ui.ctx().input(|i| i.pointer.interact_pos().unwrap_or_default()))
                && !context_badge_rect.contains(
                    ui.ctx().input(|i| i.pointer.interact_pos().unwrap_or_default()))
            {
                dismissed = true;
            }
            if open_strategy {
                self.agent_overflow_open = false;
                self.agent_context_picker_open = true;
                self.agent_context_picker_frame = 0;
            } else if toggle_network {
                self.agent_overflow_open = false;
                // Applied here rather than through the strip's own flag: that one
                // is read further up, before this menu is drawn, so setting it
                // would do nothing — which the compiler said plainly.
                let next = !tab.session.offline_mode;
                tab.session.update_offline_mode(next);
            } else if dismissed {
                self.agent_overflow_open = false;
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
                        // How long this step has been going. The difference
                        // between a model that is thinking and one that has hung
                        // is entirely in this number, and a five-minute `sleep`
                        // is only obviously fine once you can see it counting.
                        if let Some(since) = tab.session.activity_elapsed()
                            .filter(|d| d.as_secs() >= ELAPSED_SHOWN_AFTER)
                        {
                            ui.label(egui::RichText::new(format!("· {}", human_elapsed(since)))
                                .size(10.5).color(egui::Color32::from_gray(150)));
                            // And what the command itself said about how long it
                            // would be, while that is still true.
                            if let Some(left) = countdown_label(tab.session.activity_expect(), since) {
                                ui.label(egui::RichText::new(format!("· {left}"))
                                    .size(10.5).color(egui::Color32::from_gray(130)));
                            }
                        }
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
        // A subagent's activity now lives in the transcript, inside its own
        // block (see `draw_subagent_block`), which is where you can tell whose
        // work you are reading. The strip is no longer a second copy of that —
        // it appears only for a subagent with an approval waiting, which is the
        // one thing the transcript cannot do on its own: an approval buried
        // three screens up in the scroll is an approval you never answer, and
        // the turn sits there blocked.
        let tab = &self.agent_tabs[self.agent_active];
        let subagent_count = tab.session.items.iter().filter(|i| subagent_awaiting_approval(i)).count();
        // Paths (not flat indices) since a subagent can nest another one —
        // `[2, 1]` means "items[2] (a Subagent), then its own items[1]".
        let mut subagent_pending_action: Option<(Vec<usize>, bool)> = None;
        let mut subagent_toggle_expand: Option<Vec<usize>> = None;
        if subagent_count > 0 {
            let expanded_count = tab.session.items.iter()
                .filter(|i| subagent_awaiting_approval(i)
                            && matches!(i, ChatItem::Subagent { expanded: true, .. }))
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
                                if !subagent_awaiting_approval(item) { continue; }
                                let ChatItem::Subagent { agent_type, prompt, expanded, items, .. } = item
                                    else { continue };
                                draw_subagent_strip_entry(
                                    ui, &[idx], agent_type, prompt, *expanded, items,
                                    &mut subagent_pending_action, &mut subagent_toggle_expand,
                                );
                            }
                        });
                });
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
                        ChatItem::Subagent {
                            agent_type, prompt, current_tool, detail,
                            finished, summary, expanded, items: sub_items, ..
                        } => {
                            // The subagent's own region, rule and all. Its work
                            // used to live in the docked strip, leaving no way
                            // to tell in the transcript which lines were the
                            // main agent's and which were a subagent's.
                            draw_subagent_block(
                                ui, &[item_idx], agent_type, prompt, current_tool, detail,
                                *finished, summary, *expanded, sub_items, pad_l, pad_r,
                                &mut subagent_pending_action, &mut subagent_toggle_expand,
                            );
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

        // Applied here, after the chat area, because both the transcript's
        // subagent blocks and the docked strip write into these — and a path
        // recorded during rendering cannot be acted on while the items it
        // addresses are still borrowed for drawing.
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

        // The permission change, now that nothing holds the tab. A respawn is
        // built through `start_agent_session`, so a remote workspace keeps its
        // remote agent instead of quietly getting a local one.
        // Applied here, after the panel is drawn: the picker that sets
        // `perm_change` is drawn above, and starting a replacement agent needs
        // the app itself — the tab is borrowed throughout the drawing.
        //
        // Read here too, not earlier. Reading it before the picker ran meant
        // reading a `perm_change` nothing had set yet, so it was always None and
        // no permission change ever took effect at all.
        if let Some(mode) = perm_change {
            self.change_permission_mode(mode);
        }

        // A mode that was waiting on the turn to finish taking effect.
        let idx = self.agent_active;
        if let Some(mode) = self.agent_tabs.get(idx).and_then(|t| {
            (!t.session.is_active()).then_some(t.pending_mode_respawn).flatten()
        }) {
            self.agent_tabs[idx].pending_mode_respawn = None;
            let resume_id = self.agent_tabs[idx].session.forge_session_id.clone();
            let resume = (!resume_id.is_empty()).then_some(resume_id.as_str());
            let replacement = self.start_agent_session(mode, resume);
            // Already the tab's mode — this is the process catching up with it.
            self.agent_tabs[idx].permission_mode = mode;
            self.agent_tabs[idx].set_permission_mode_forced(mode, Some(replacement));
            self.output_log(format!("{} is now fully in effect", mode.label()), OutputLevel::Info);
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

        // Resizable, with a real default height. It was `fixed_size([w, 0.0])`
        // and not resizable, which worked only while the window grew to its
        // contents — once those contents went inside a scroll area (so the list
        // could be reached at all) that zero became the height, and the dialog
        // collapsed to two visible settings with no way to enlarge it.
        //
        // Two thirds of the screen, so it is usable on a laptop and not absurd
        // on a large display, and bounded so a very short screen still leaves
        // the title bar and buttons reachable.
        let h = (ctx.screen_rect().height() * 0.66).clamp(320.0, 900.0);
        egui::Window::new("settings_window")
            .title_bar(false)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_size([w, h])
            .min_width(w)
            .min_height(240.0)
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
                // Fills what the window has, rather than measuring the screen:
                // the window is resizable now, so its own height is the answer,
                // and `auto_shrink` false on both axes makes the area take it.
                // Room is left for the title row and the footer above and below.
                let body_max = (ui.available_height() - 52.0).max(120.0);
                egui::ScrollArea::vertical()
                    .id_salt("settings_body")
                    .max_height(body_max)
                    .auto_shrink([false, false])
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
        // The same measurement the local terminal uses, and for the reason given
        // where it lives: a column is not the font's advance for a space, and the
        // difference is a fraction of a pixel per character that shows up as a
        // gap between the end of the prompt and the cursor.
        let char_w   = crate::terminal::mono_advance(ui, &font_id);
        let focus_id = ui.id().with("ssh_term_focus");

        // Fit the grid to the panel, as the local terminal does.
        //
        // The remote pty and the grid were both opened at a fixed 50x220 and
        // never resized, so the viewport was taller than the space it was drawn
        // in. The scroll area sticks to the bottom, as a terminal should, and
        // the bottom of a 50-row viewport holding one line of prompt is blank —
        // so a freshly opened remote terminal showed nothing at all, with the
        // prompt scrolled off the top. 220 columns wide with the panel narrower
        // than that also meant the remote wrapped its lines somewhere off the
        // right-hand edge.
        let visible_rows = ((panel_rect.height() / row_h.max(1.0)).floor() as u16).max(1);
        let visible_cols = ((panel_rect.width() - 8.0) / char_w.max(1.0)).floor().max(1.0) as u16;
        if (visible_rows, visible_cols) != self.ssh_term_size {
            // Same debounce as the local terminal: a drag across the splitter
            // is a stream of sizes, and telling the far end about every one of
            // them is a round trip each.
            const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);
            let candidate = (visible_rows, visible_cols);
            // The *first* fit is not a drag and has nothing to debounce: the
            // grid opened at a guess, and waiting 400ms to correct it is 400ms
            // of showing the wrong thing. Only later changes wait to settle.
            let first_fit = self.ssh_term_size == (0, 0);
            let settled = first_fit || match self.ssh_term_pending_size {
                Some((c, since)) if c == candidate => since.elapsed() >= RESIZE_SETTLE,
                _ => {
                    self.ssh_term_pending_size = Some((candidate, std::time::Instant::now()));
                    false
                }
            };
            if settled {
                if let Ok(mut g) = self.ssh_term.lock() {
                    g.resize(visible_rows as usize, visible_cols as usize);
                }
                self.ssh_resize_pty(visible_cols, visible_rows);
                self.ssh_term_size = candidate;
                self.ssh_term_pending_size = None;
            } else {
                // Nothing else is animating, so without this the debounce timer
                // is never looked at again and the size never settles.
                ui.ctx().request_repaint_after(RESIZE_SETTLE);
            }
        }

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
        // Chosen from the context menu, acted on after the rows are drawn — the
        // tree is borrowed for drawing while the menu is open.
        let mut new_dir_in:    Option<String> = None;
        let mut open_workspace: Option<String> = None;
        let mut cd_to:          Option<String> = None;
        let mut refresh = false;
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
                        // Right-click, which on this tree did nothing at all —
                        // the local tree has had a menu here since it was
                        // written, and a remote workspace is where you are least
                        // able to fall back on Finder.
                        resp.context_menu(|ui| {
                            ui.set_min_width(190.0);
                            // A folder's own contents; a file's containing
                            // folder, since that is where a sibling would go.
                            let dir = if entry.is_dir {
                                entry.path.clone()
                            } else {
                                remote_parent(&entry.path)
                            };
                            if ui.button("New Folder…").clicked() {
                                new_dir_in = Some(dir.clone());
                                ui.close_menu();
                            }
                            // The local menu has had this since it was written.
                            // Separate from opening the folder as the workspace:
                            // sometimes you want the shell somewhere without
                            // moving everything else there.
                            if ui.button("Open in Terminal").clicked() {
                                cd_to = Some(dir.clone());
                                ui.close_menu();
                            }
                            if entry.is_dir && ui.button("Open This Folder").clicked() {
                                open_workspace = Some(entry.path.clone());
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Copy Path").clicked() {
                                ui.output_mut(|o| o.copied_text = entry.path.clone());
                                ui.close_menu();
                            }
                            if ui.button("Refresh").clicked() {
                                refresh = true;
                                ui.close_menu();
                            }
                        });
    }
                } else if self.ssh_nav_rx.is_some() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| { ui.add_space(8.0); ui.spinner(); });
}
            });

        // The same menu for the empty space below the rows, which is where you
        // right-click when what you want is "a new folder *here*" and there is no
        // row that means here.
        let here = self.ssh_tree.last().map(|(p, _)| p.clone());
        if let Some(here) = here {
            ui.interact(panel_rect, ui.id().with("ssh_tree_bg"), egui::Sense::click())
                .context_menu(|ui| {
                    ui.set_min_width(190.0);
                    if ui.button("New Folder…").clicked() {
                        new_dir_in = Some(here.clone());
                        ui.close_menu();
                    }
                    if ui.button("Open in Terminal").clicked() {
                        cd_to = Some(here.clone());
                        ui.close_menu();
                    }
                    if ui.button("Copy Path").clicked() {
                        ui.output_mut(|o| o.copied_text = here.clone());
                        ui.close_menu();
                    }
                    if ui.button("Refresh").clicked() {
                        refresh = true;
                        ui.close_menu();
                    }
                });
        }

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
        if let Some(dir) = new_dir_in { self.remote_new_folder = Some(RemoteNewFolder {
            parent: dir, name: String::new(), error: None, busy: false,
        }); }
        if let Some(path) = open_workspace { self.open_remote_workspace(path); }
        if let Some(dir) = cd_to {
            self.ssh_cd(&dir);
            // Shown, since the shell may be busy and the cd will not appear at a
            // prompt until whatever is running there finishes.
            self.output_log(format!("Terminal → {dir}"), OutputLevel::Info);
            self.bottom_tab = BottomTab::Terminal;
            self.show_term = true;
        }
        if refresh {
            // Re-list where we are: the same request the explorer makes when it
            // navigates, so a folder made outside Forge shows up.
            if let Some((here, _)) = self.ssh_tree.pop() {
                self.ssh_navigate(here);
            }
        }
    }

    /// Send the remote shell to a directory.
    ///
    /// Typed into the shell rather than reopening it. The local terminal restarts
    /// when the workspace moves, which is fine for a shell that starts in a
    /// fraction of a second — but a remote shell may have a build running in it,
    /// and its scrollback is the record of a session that took a round trip per
    /// keystroke to produce. `cd` keeps both. If the shell is busy the line waits
    /// in the pty and runs when the prompt comes back, which is exactly what
    /// typing it would have done.
    fn ssh_cd(&mut self, dir: &str) {
        let Some(shell) = &self.ssh_shell else { return };
        // Quoted, because a remote path is not a promise about spaces.
        let line = format!("cd {}\n", crate::ssh::shell_quote_path(dir));
        let _ = shell.tx.try_send(line.into_bytes());
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
                    if ui.add(egui::Button::new(egui::RichText::new("×").size(10.0))
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
        let host = if qo.source.is_remote() {
            self.ssh.as_ref().map(|s| s.host.host.clone())
        } else {
            None
        };
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
                        // legible when the query box only shows a filter. On a
                        // remote workspace the host is named too: the whole bug
                        // this fixes was not being able to tell which machine
                        // you were looking at.
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(match &host {
                            Some(h) => format!("{h}:{}", qo.breadcrumb()),
                            None    => qo.breadcrumb(),
                        }).size(11.0).color(egui::Color32::from_gray(140)));

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

                        // A remote listing can be refused or time out. Saying
                        // "Empty folder" for that would send the user looking
                        // for a file they would never find.
                        if let Some(err) = &qo.error {
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new(err).size(12.0)
                                .color(egui::Color32::from_rgb(230, 130, 130)));
                            ui.add_space(4.0);
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

                            if !qo.listing() && qo.filtered.is_empty() && qo.error.is_none() {
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

        let remote = qo.source.is_remote();
        if !dismissed { self.quick_open = Some(qo); }
        if let Some(path) = open_path {
            // A remote pick is read over the connection, not off this disk —
            // where that path names either nothing or, worse, a different file.
            if remote { self.ssh_open_file(path.to_string_lossy().into_owned()); }
            else      { self.open_file(path); }
        }
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
            Cmd::QuickOpen       => self.open_quick_open(),
            Cmd::ReloadWindow    => self.reload_window(),
            Cmd::RestartWindow   => self.restart_window(),
            Cmd::RestartAll      => self.restart_every_window(),
            Cmd::Consolidate     => self.consolidate_windows(),
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

    /// The host this window is connected to and the folder it has open there, for
    /// the record — so a restart into a new process can connect again rather than
    /// coming back local.
    pub fn remote_identity(&self) -> Option<(crate::ssh::SshHost, String)> {
        let ssh = self.ssh.as_ref()?;
        // Anything with somewhere to connect to. This used to require a *name*,
        // which a connection typed in by hand does not have — so those recorded
        // nothing and came back local with no way for the user to see why.
        (!ssh.host.host.is_empty())
            .then(|| (ssh.host.clone(), ssh.host.remote_dir.clone()))
    }

    /// The remote workspace root as an absolute path, or `None` when this window
    /// is local.
    ///
    /// Resolved here rather than sent as `~`, because `fs/list` answers with
    /// entries and no path: anything that needs to display where it is, or walk
    /// up from there, has to know the path it asked with. The home directory is
    /// already known from connect, so this costs nothing.
    fn remote_root(&self) -> Option<String> {
        let ssh = self.ssh.as_ref()?;
        Some(resolve_remote_dir(&ssh.host.remote_dir, &ssh.remote_home))
    }

    /// Go to File, on whichever filesystem the workspace is actually on.
    ///
    /// The same reason the folder chooser branches: on a remote session the
    /// local `read_dir` is listing the wrong machine, and it would offer files
    /// the agent and the editor there cannot see.
    fn open_quick_open(&mut self) {
        if let Some((handles, root)) =
            self.ssh.as_ref().map(|s| s.fs_handles()).zip(self.remote_root())
        {
            self.quick_open =
                Some(QuickOpen::new(Path::new(&root), QuickOpenSource::Remote(handles)));
            return;
        }
        if !self.has_folder {
            self.status = "Open a folder first".into();
            return;
        }
        let root = self.cwd.clone();
        self.quick_open = Some(QuickOpen::new(&root, QuickOpenSource::Local));
    }

    fn open_folder_dialog(&mut self) {
        // The native dialog browses *this* machine. On a remote workspace that
        // is the wrong filesystem — it opened a Mac folder while the user was
        // working on a Linux host — so the remote gets its own chooser.
        if let Some(start) = self.remote_root() {
            self.remote_picker = Some(RemotePicker {
                path: start.clone(),
                entries: Vec::new(),
                loading: true,
                error: None,
                typed: start.clone(),
            });
            self.picker_list(start);
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.folder_rx = Some(rx);
        std::thread::spawn(move || {
            let result = rfd::FileDialog::new().pick_folder();
            let _ = tx.send(result);
        });
    }

    /// Ask the remote to make a directory, then show it.
    fn remote_mkdir(&mut self, path: String) {
        let Some(handles) = self.ssh.as_ref().map(|s| s.fs_handles()) else { return };
        let (tx, rx) = mpsc::channel();
        self.mkdir_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::ssh::fs_mkdir_with(
                &handles, &path, std::time::Duration::from_secs(15),
            ).map(|()| path);
            let _ = tx.send(result);
            crate::wake::wake();
        });
    }

    /// "New Folder…", asked plainly: where it will go, and what to call it.
    fn draw_remote_new_folder(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.remote_new_folder else { return };
        let (parent, mut name, error, busy) =
            (prompt.parent.clone(), prompt.name.clone(), prompt.error.clone(), prompt.busy);
        let mut create = false;
        let mut cancel = false;

        egui::Window::new("remote_new_folder")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(460.0)
            .frame(egui::Frame::popup(ctx.style().as_ref())
                .fill(egui::Color32::from_rgb(37, 37, 38))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
                .rounding(6.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(egui::RichText::new("New folder").size(14.0).strong()
                    .color(egui::Color32::from_gray(230)));
                ui.add_space(2.0);
                // Where, in full: a name alone does not say which folder it lands
                // in, and on a remote tree you may be several levels from where
                // you think you are.
                ui.label(egui::RichText::new(format!("in {parent}"))
                    .monospace().size(11.0).color(egui::Color32::from_rgb(150, 205, 210)));
                ui.add_space(6.0);
                let resp = ui.add(egui::TextEdit::singleline(&mut name)
                    .desired_width(f32::INFINITY)
                    .hint_text("name, or a/nested/path"));
                resp.request_focus();
                // Enter is the button, since typing a name and reaching for the
                // mouse is not how anyone makes a folder.
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                if let Some(e) = &error {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(e).size(11.0)
                        .color(egui::Color32::from_rgb(235, 130, 120)));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if busy {
                        ui.spinner();
                    } else {
                        if ui.add_enabled(!name.trim().is_empty(),
                                          egui::Button::new("Create")).clicked() {
                            create = true;
                        }
                        if ui.button("Cancel").clicked() { cancel = true; }
                    }
                });
                ui.add_space(4.0);
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) { cancel = true; }

        if let Some(p) = &mut self.remote_new_folder { p.name = name.clone(); }
        if cancel {
            self.remote_new_folder = None;
            self.mkdir_rx = None;
            return;
        }
        if create && !name.trim().is_empty() && !busy {
            let path = format!("{}/{}", parent.trim_end_matches('/'), name.trim().trim_matches('/'));
            if let Some(p) = &mut self.remote_new_folder {
                p.busy = true;
                p.error = None;
            }
            self.remote_mkdir(path);
        }
    }

    /// The remote folder chooser.
    ///
    /// Directories only, since it is choosing a workspace. Navigation is a
    /// listing per step rather than a cached tree: a remote filesystem changes
    /// under you, and this is opened rarely enough that a round trip per click
    /// is cheaper than keeping a tree honest.
    fn draw_remote_folder_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = &self.remote_picker else { return };
        let (path, loading, error) = (picker.path.clone(), picker.loading, picker.error.clone());
        let entries: Vec<(String, String)> = picker
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.path.clone()))
            .collect();
        let host = self.ssh.as_ref().map(|s| s.host.host.clone()).unwrap_or_default();

        let mut go: Option<String> = None;
        let mut choose = false;
        let mut cancel = false;
        let mut new_folder_here = false;
        // The box starts as wherever the list is, so typing means editing that
        // path rather than retyping it from `/`.
        let mut typed = picker.typed.clone();

        egui::Window::new("remote_folder_picker")
            .title_bar(false)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_size([560.0, (ctx.screen_rect().height() * 0.6).clamp(320.0, 720.0)])
            .min_height(260.0)
            .frame(egui::Frame::popup(ctx.style().as_ref())
                .fill(egui::Color32::from_rgb(37, 37, 38))
                .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
                .rounding(6.0))
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(format!("Open folder on {host}"))
                    .size(14.0).strong().color(egui::Color32::from_gray(230)));
                ui.add_space(4.0);
                // The path, editable. Clicking down through a tree is fine when
                // you are looking for a folder and hopeless when you already
                // know where it is — and on a remote box the answer is usually
                // already known, from a terminal in the next panel.
                ui.horizontal(|ui| {
                    let resp = ui.add(egui::TextEdit::singleline(&mut typed)
                        .desired_width(ui.available_width() - 150.0)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("/path/on/the/remote"));
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        // Go there, rather than opening it blind: the listing
                        // that comes back is the confirmation it exists.
                        go = Some(typed.trim().to_string());
                    }
                    if ui.button("Go").clicked() { go = Some(typed.trim().to_string()); }
                    if ui.button("New Folder…").clicked() { new_folder_here = true; }
                });
                ui.add_space(6.0);
                ui.separator();

                let body = (ui.available_height() - 46.0).max(120.0);
                egui::ScrollArea::vertical()
                    .id_salt("remote_picker_list")
                    .max_height(body)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Up first, and absent at the root, where there is
                        // nowhere above to go.
                        if path != "/" {
                            if ui.button("↑  ..").clicked() { go = Some(remote_parent(&path)); }
                        }
                        if loading {
                            ui.label(egui::RichText::new("Listing…").size(11.0)
                                .color(egui::Color32::from_gray(150)));
                        }
                        if let Some(e) = &error {
                            ui.label(egui::RichText::new(e).size(11.0)
                                .color(egui::Color32::from_rgb(255, 130, 110)));
                        }
                        for (name, full) in &entries {
                            if ui.button(format!("▸  {name}")).clicked() {
                                go = Some(full.clone());
                            }
                        }
                        if !loading && error.is_none() && entries.is_empty() {
                            ui.label(egui::RichText::new("No subdirectories here.").size(11.0)
                                .color(egui::Color32::from_gray(140)));
                        }
                    });

                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Open this folder").clicked() { choose = true; }
                    if ui.button("Cancel").clicked() { cancel = true; }
                });
                ui.add_space(6.0);
            });

        if let Some(p) = &mut self.remote_picker { p.typed = typed; }
        if new_folder_here {
            self.remote_new_folder = Some(RemoteNewFolder {
                parent: path.clone(), name: String::new(), error: None, busy: false,
            });
        }
        if cancel {
            self.remote_picker = None;
            self.picker_rx = None;
        } else if let Some(next) = go.filter(|p| !p.is_empty()) {
            if let Some(p) = &mut self.remote_picker {
                p.loading = true;
            }
            self.picker_list(next);
        } else if choose {
            self.remote_picker = None;
            self.picker_rx = None;
            self.open_remote_workspace(path);
        }
    }

    /// Make `path` the remote workspace root.
    ///
    /// The agent is restarted there, because its project root is fixed at spawn
    /// — it is `cd`-ed into the workspace when the process starts, so nothing
    /// short of a new process moves it. The transcript is carried across, as it
    /// is for a permission-mode change, which restarts for the same reason.
    fn open_remote_workspace(&mut self, path: String) {
        if let Some(ssh) = &mut self.ssh {
            ssh.host.remote_dir = path.clone();
        }
        self.ssh_form.remote_dir = path.clone();
        // The explorer starts again at the new root rather than keeping a trail
        // that leads somewhere else.
        self.ssh_tree.clear();
        self.ssh_navigate(path.clone());
        // And the terminal goes there too. Opening a folder locally restarts
        // every local shell in it; the remote shell was left wherever it was, so
        // the workspace and the prompt disagreed and the first thing you had to
        // do in a new folder was cd into it by hand.
        self.ssh_cd(&path);
        self.output_log(format!("Remote workspace is now {path}"), OutputLevel::Info);

        let idx = self.agent_active;
        if idx < self.agent_tabs.len() {
            let mode = self.agent_tabs[idx].permission_mode;
            let items = std::mem::take(&mut self.agent_tabs[idx].session.items);
            let replacement = self.start_agent_session(mode, None);
            self.agent_tabs[idx].session = replacement;
            self.agent_tabs[idx].session.items = items;
        }
    }

    /// List `path` on the remote for the picker.
    ///
    /// Its own request rather than `ssh_navigate`'s, so opening the dialog does
    /// not move the explorer's position underneath the user.
    fn picker_list(&mut self, path: String) {
        let Some(handles) = self.ssh.as_ref().map(|s| s.fs_handles()) else { return };
        let (tx, rx) = mpsc::channel();
        self.picker_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::ssh::fs_list_with(
                &handles, &path, std::time::Duration::from_secs(20),
            ).map(|entries| (path, entries));
            let _ = tx.send(result);
            crate::wake::wake();
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
                });
                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.show_tree, "File Tree");
                    ui.checkbox(&mut self.show_term, "Terminal");
                });
                // Window, rather than more of File.
                //
                // File is for documents; every one of these acts on a window or
                // on the process behind it, which is what a Window menu is for —
                // and it is where a macOS user looks for New Window anyway. They
                // had accumulated at the bottom of File because that is where the
                // first one went.
                //
                // Not hidden behind a "Developer" submenu, which is where VS Code
                // keeps its reload: with a build rolled out one window at a time,
                // these are the ordinary controls of a normal day's work, not
                // diagnostics.
                ui.menu_button("Window", |ui| {
                    if ui.button("New Window").clicked() {
                        self.pending_new_window = Some(NewWindowSpec::default());
                        ui.close_menu();
    }
                    ui.separator();
                    if ui.button("Reload Window      Ctrl+Shift+R").clicked() {
                        self.reload_window();
                        ui.close_menu();
    }
                    // A new process, but only for this window. The soft reload
                    // cannot pick up a new binary or clear anything the process
                    // itself is holding; this can, without touching the others.
                    if ui.button("Restart This Window").clicked() {
                        self.restart_window();
                        ui.close_menu();
    }
                    ui.separator();
                    // Every window there is, across every Forge process. When the
                    // message cannot reach the others it says so and does what it
                    // can.
                    if ui.button("Restart All Windows").clicked() {
                        self.restart_every_window();
                        ui.close_menu();
    }
                    // The way back to one process, and one Dock icon, after a
                    // window-by-window rollout has left several.
                    if ui.button("Collect All Windows Into One Process").clicked() {
                        self.consolidate_windows();
                        ui.close_menu();
    }
                    ui.separator();
                    // Which build *this* window is running, at the bottom where a
                    // footer belongs rather than in the middle of the actions. The
                    // version number alone cannot tell two builds of one version
                    // apart, which is the whole case for showing it.
                    let label = if self.build_is_stale {
                        format!("Forge IDE {} — newer build installed", build_stamp())
                    } else {
                        format!("Forge IDE {}", build_stamp())
                    };
                    ui.add_enabled(false, egui::Button::new(
                        egui::RichText::new(label).size(11.0),
                    ));
                });
                ui.menu_button("Go", |ui| {
                    if ui.button("Go to File…         Ctrl+P").clicked() {
                        self.open_quick_open();
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
            // Points per character across the minimap. Named because the
            // viewport box maps a scroll offset through the same scale, and
            // two copies of it would drift apart the first time either moved.
            const MM_CHAR_W: f32 = 0.7;
            /// Left inset, so a line at column 0 is not flush against the edge.
            const MM_PAD: f32 = 3.0;
            let fg = self.palette.default_fg_c();
            let line_col = egui::Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 90);
            for (i, line) in buf.lines.iter().enumerate() {
                let t = line.trim_end();
                if t.is_empty() { continue; }
                let y = mm_rect.top() + i as f32 * line_h;
                if y > mm_rect.bottom() { break; }
                let indent = line.len() - line.trim_start().len();
                let x0 = mm_rect.left() + MM_PAD + (indent as f32 * MM_CHAR_W).min(30.0);
                let w  = (t.trim_start().len() as f32 * MM_CHAR_W).min(mm_rect.width() - MM_PAD * 2.0);
                p.line_segment(
                    [egui::pos2(x0, y), egui::pos2((x0 + w).min(mm_rect.right() - MM_PAD), y)],
                    egui::Stroke::new((line_h - 0.4).max(0.8), line_col));
            }
            // Viewport indicator + click/drag to scroll
            let content_h = line_ys.last().map(|y| y + row_h).unwrap_or(n as f32 * row_h);
            let view_h    = scroll_out.inner_rect.height();
            let top_line  = y_to_line(scroll_out.state.offset.y) as f32;
            let vis_lines = view_h / row_h;
            // Sideways too, when the file can scroll that way at all — with
            // word wrap on it cannot, and the box stays full width rather than
            // implying a horizontal position that has no meaning.
            let char_w = ui.fonts(|f| f.glyph_width(&font_id, '0')).max(1.0);
            let text_view_w = (scroll_out.inner_rect.width() - gutter_w).max(1.0);
            let widest = buf.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32;
            let scrolls_sideways = !word_wrap && widest * char_w > text_view_w;
            let (vx0, vx1) = if scrolls_sideways {
                minimap_span(
                    mm_rect.left() + MM_PAD,
                    scroll_out.state.offset.x / char_w,
                    text_view_w / char_w,
                    MM_CHAR_W,
                )
            } else {
                (mm_rect.left(), mm_rect.right())
            };
            let vp = egui::Rect::from_min_max(
                egui::pos2(vx0, mm_rect.top() + top_line * line_h),
                egui::pos2(vx1, mm_rect.top() + (top_line + vis_lines) * line_h),
            );
            if content_h > view_h || scrolls_sideways {
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
                                if ui.small_button("×").clicked() { close = true; }
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

    fn ssh_connect(&mut self) { self.ssh_connect_inner(false) }

    /// `trust_new_host_key` is set only by the fingerprint dialog, for one
    /// attempt against one host.
    fn ssh_connect_inner(&mut self, trust_new_host_key: bool) {
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
                    crate::ssh::SshConnection::connect(&host, pw.as_deref(), trust_new_host_key, &log)
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
        // Opened at the size the panel is now, so the first prompt is drawn
        // where it will be read. A fixed size here meant the very first frame
        // was already wrong, and nothing corrected it.
        let (rows, cols) = match self.ssh_term_size {
            (0, _) | (_, 0) => (24u16, 80u16),
            (r, c) => (r, c),
        };

        let (tx, rx) = mpsc::channel();
        self.ssh_pty_rx = Some(rx);

        std::thread::spawn(move || {
            let result = (|| {
                // Send pty/open request
                let id = { let mut n = next_id.lock().unwrap(); let i = *n; *n += 1; i };
                let msg = forge_proto::Rpc::request(id, "pty/open",
                    serde_json::json!({ "id": 0u32, "cols": cols, "rows": rows, "cwd": cwd }));
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

    /// Start an agent for the workspace in front of the user.
    ///
    /// On a remote workspace the agent runs *there*, so every file it reads and
    /// every command it runs is on the machine being worked on. It used to be
    /// spawned locally whatever was open, which meant a remote session showed a
    /// remote file tree and a remote terminal beside an agent quietly working
    /// on this Mac.
    ///
    /// A remote agent that cannot be started is reported as a failed session
    /// rather than silently replaced by a local one — an agent on the wrong
    /// machine, editing files that merely share a path, is worse than an agent
    /// that says it could not start.
    fn start_agent_session(
        &self,
        mode: crate::settings::AgentPermissionMode,
        resume: Option<&str>,
    ) -> crate::agent_panel::AgentSession {
        use crate::settings::AgentPermissionMode;
        if let Some(ssh) = &self.ssh {

            let cwd = ssh.host.remote_dir.clone();
            let allow_all = mode == AgentPermissionMode::DangerouslySkipAll;
            return match ssh.spawn_agent(&cwd, resume, allow_all) {
                Ok((stdout, stdin)) => {
                    let mut session =
                        crate::agent_panel::AgentSession::over_channel(stdout, stdin, resume);
                    // Hand over one endpoint, with its key, for this process
                    // only. That machine has no reason to hold these — and on a
                    // box you do not administer, every reason not to — so they
                    // stay here and travel per session. The agent applies a
                    // switch in memory and writes nothing, so nothing is left
                    // behind when it exits.
                    //
                    // Sent unconditionally rather than only when the remote
                    // looks unconfigured: a key it already has is simply
                    // replaced by the same one, and guessing wrong the other
                    // way means an agent that cannot reach a model at all.
                    // Every endpoint this machine can lend, on one tunnel,
                    // each reachable at its own path. The remote agent starts
                    // with only whatever config it has — a wiped machine has
                    // none — so without this the model picker over there offers
                    // exactly the one endpoint it was handed, which is what it
                    // did on the first real test.
                    let locals = crate::onboarding::local_endpoints();
                    // One secret per session, handed to the remote agent as the
                    // api_key of every endpoint it is lent. The agent then
                    // presents it on each request without knowing it is
                    // authenticating, and the proxy serves nothing that does
                    // not — loopback on a shared host is reachable by every
                    // other process on it, and the credential is added here.
                    let token = match crate::model_proxy::session_token() {
                        Ok(t) => t,
                        Err(e) => {
                            // Reported and then skipped, not worked around: the
                            // remote session still runs, with the models it has
                            // of its own rather than an open tunnel to ours.
                            eprintln!("forge-ide: {e}; not lending endpoints to the remote");
                            String::new()
                        }
                    };
                    let mut routes = crate::model_proxy::Routes::new(token.clone());
                    let mut lent: Vec<serde_json::Value> = Vec::new();
                    for ep in locals {
                        let kind = ep["endpoint_type"].as_str().unwrap_or("").to_string();
                        let key = ep["api_key"].as_str().unwrap_or("").to_string();
                        let Some((credential, extra_headers)) =
                            crate::onboarding::credential_for(&kind, &key)
                        else {
                            // No credential here for it, so lending it would
                            // offer a model that cannot answer.
                            continue;
                        };
                        let prefix = routes.add(crate::model_proxy::Upstream {
                            base_url: ep["base_url"].as_str().unwrap_or("").to_string(),
                            style: crate::model_proxy::AuthStyle::from_endpoint_type(&kind),
                            credential,
                            extra_headers,
                        });
                        let mut lent_ep = ep.clone();
                        lent_ep["api_key"] = serde_json::json!(token);
                        lent_ep["base_url"] = serde_json::json!(prefix);
                        lent.push(lent_ep);
                    }

                    // An empty token means the random read failed, and an
                    // unauthenticated tunnel is worse than no remote models.
                    if !routes.is_empty() && !token.is_empty() {
                        match ssh.open_model_proxy(routes) {
                            Ok(port) => {
                                // The prefix stored above becomes a whole URL
                                // now that the port is known.
                                for ep in &mut lent {
                                    let prefix = ep["base_url"].as_str().unwrap_or("").to_string();
                                    ep["base_url"] =
                                        serde_json::json!(format!("http://127.0.0.1:{port}{prefix}"));
                                    // A tunnel address is good for this session
                                    // only, so the agent must not write it to
                                    // its machine's config.
                                    ep["ephemeral"] = serde_json::json!(true);
                                }
                                session.set_lent_endpoints(lent.clone());
                                // Start on this machine's default where it is
                                // among them, so a remote session opens on the
                                // model the user already chose.
                                let default = crate::onboarding::local_default_name();
                                let start_on = default
                                    .and_then(|d| {
                                        lent.iter()
                                            .find(|e| e["name"].as_str() == Some(d.as_str()))
                                            .cloned()
                                    })
                                    .or_else(|| lent.first().cloned());
                                if let Some(ep) = start_on {
                                    session.switch_model(ep);
                                }
                            }
                            Err(e) => {
                                session.spawn_err = Some(format!(
                                    "Could not open the model tunnel to {}: {e}",
                                    ssh.host.host,
                                ));
                            }
                        }
                    }
                    session
                }
                Err(e) => crate::agent_panel::AgentSession::failed(format!(
                    "Could not start the agent on {}: {e}",
                    ssh.host.host,
                )),
            };
        }
        crate::agent_panel::AgentSession::spawn(&self.cwd, mode, resume)
    }

    /// The two host-key conversations: an unknown host, and a changed one.
    ///
    /// Deliberately different shapes. An unknown host is a question, asked once
    /// with the fingerprint to check against — the same bargain `ssh` offers. A
    /// changed key is not a question: nothing here can tell a rebuilt machine
    /// from someone answering in its place, so there is no button that
    /// proceeds, and getting past it means editing known_hosts by hand, which
    /// is a deliberate enough act to be worth the friction.
    fn draw_host_key_dialogs(&mut self, ctx: &egui::Context) {
        if let Some(fingerprint) = self.ssh_host_key_prompt.clone() {
            let host = self.ssh_form.host.clone();
            let mut answer: Option<bool> = None;
            egui::Window::new("host_key_unknown")
                .title_bar(false).collapsible(false).resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .fixed_size([540.0, 0.0])
                .frame(egui::Frame::popup(ctx.style().as_ref())
                    .fill(egui::Color32::from_rgb(37, 37, 38))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_gray(60)))
                    .rounding(6.0))
                .show(ctx, |ui| {
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new(format!("{host} is not in your known_hosts"))
                        .size(15.0).strong().color(egui::Color32::from_gray(230)));
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(
                        "Check this fingerprint against the machine itself before trusting it — \
                         run `ssh-keyscan` on it, or read it from its console. If it does not \
                         match, something is answering in its place."
                    ).size(12.0).color(egui::Color32::from_gray(190)));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(&fingerprint).monospace().size(12.5)
                        .color(egui::Color32::from_rgb(150, 205, 210)));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Trust and connect").clicked() { answer = Some(true); }
                        if ui.button("Cancel").clicked() { answer = Some(false); }
                    });
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(
                        "Trusting it adds the key to ~/.ssh/known_hosts, and Forge will refuse \
                         to connect if it ever changes."
                    ).size(10.5).color(egui::Color32::from_gray(140)));
                    ui.add_space(8.0);
                });
            match answer {
                Some(true) => {
                    self.ssh_host_key_prompt = None;
                    self.ssh_connect_inner(true);
                }
                Some(false) => {
                    self.ssh_host_key_prompt = None;
                    self.status = "Cancelled — host key not trusted".to_string();
                }
                None => {}
            }
        }

        if let Some(detail) = self.ssh_host_key_changed.clone() {
            let mut close = false;
            egui::Window::new("host_key_changed")
                .title_bar(false).collapsible(false).resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .fixed_size([560.0, 0.0])
                .frame(egui::Frame::popup(ctx.style().as_ref())
                    .fill(egui::Color32::from_rgb(44, 30, 30))
                    .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(200, 90, 80)))
                    .rounding(6.0))
                .show(ctx, |ui| {
                    ui.add_space(10.0);
                    ui.label(egui::RichText::new("Host key changed — connection refused")
                        .size(15.0).strong().color(egui::Color32::from_rgb(255, 150, 140)));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(&detail).size(12.0)
                        .color(egui::Color32::from_gray(210)));
                    ui.add_space(10.0);
                    if ui.button("Close").clicked() { close = true; }
                    ui.add_space(8.0);
                });
            if close { self.ssh_host_key_changed = None; }
        }
    }

    /// Tell the remote pty its new size.
    ///
    /// Fire-and-forget: the reply carries nothing worth waiting for, and the
    /// alternative is blocking a frame on a network round trip. A dropped
    /// resize costs a mis-wrapped line until the next one, which is what
    /// happened permanently before any were sent at all.
    fn ssh_resize_pty(&mut self, cols: u16, rows: u16) {
        let Some(ssh) = &self.ssh else { return };
        if self.ssh_shell.is_none() {
            return; // no pty open yet; it will be opened at the right size
        }
        let id = { let mut n = ssh.next_id.lock().unwrap(); let i = *n; *n += 1; i };
        let msg = forge_proto::Rpc::request(
            id,
            "pty/resize",
            serde_json::json!({ "id": 0u32, "cols": cols, "rows": rows }),
        );
        if let Ok(mut w) = ssh.stdin.lock() {
            let _ = forge_proto::write_rpc(&mut *w, &msg);
        }
    }

    /// Navigate to a remote directory — non-blocking; result arrives via ssh_nav_rx.
    ///
    /// Off the render thread because it is a round trip: the connection cannot
    /// cross into a thread, so the listing goes through the shared handles.
    fn ssh_navigate(&mut self, path: String) {
        if self.ssh_nav_rx.is_some() { return; }
        let Some(handles) = self.ssh.as_ref().map(|s| s.fs_handles()) else { return };
        let (tx, rx) = mpsc::channel();
        self.ssh_nav_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::ssh::fs_list_with(
                &handles, &path, std::time::Duration::from_secs(15),
            ).map(|entries| (path, entries));
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
        // Gold rather than the violet convention: nothing in Forge is purple,
        // and this is the one symbol kind that had no relative in the palette
        // already — classes are orange, everything nominal is blue.
        6 | 12    => ("ƒ", egui::Color32::from_rgb(220, 190, 110)), // Method/Function
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
    use super::{QuickOpen, QuickOpenEntry, QuickOpenSource, list_dir, fuzzy_match,
                QUICK_OPEN_MAX_ENTRIES};
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

        let mut qo = QuickOpen::new(&root, QuickOpenSource::Local);
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

        let mut qo = QuickOpen::new(&root, QuickOpenSource::Local);
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
        let mut qo = QuickOpen::new(&root, QuickOpenSource::Local);
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

        let mut qo = QuickOpen::new(&root, QuickOpenSource::Local);
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
        // The line just pushed, not the first in the log: a window logs which
        // build it is running as it opens, so index 0 belongs to that.
        let (msg, _) = a.output_log.last().unwrap();
        assert_eq!(msg.chars().count(), MAX_OUTPUT_LINE_CHARS + 1, "cap + ellipsis");
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn short_lines_are_untouched() {
        let mut a = app();
        a.push_output("hello".into(), OutputLevel::Info);
        assert_eq!(a.output_log.last().unwrap().0, "hello");
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
        let text = "Edited /Users/someone/projects/a-long-project-name/paper/note.md";
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

#[cfg(test)]
mod minimap_tests {
    use super::minimap_span;

    /// The minimap's scale: points per character across it.
    const SCALE: f32 = 0.7;

    #[test]
    fn unscrolled_the_box_starts_at_the_left_edge() {
        let (x0, x1) = minimap_span(100.0, 0.0, 80.0, SCALE);
        assert_eq!(x0, 100.0);
        assert!((x1 - (100.0 + 80.0 * SCALE)).abs() < 0.01, "got {x1}");
    }

    #[test]
    fn scrolling_right_moves_the_box_right_by_the_same_scale() {
        let (a0, a1) = minimap_span(100.0, 0.0, 80.0, SCALE);
        let (b0, b1) = minimap_span(100.0, 40.0, 80.0, SCALE);
        assert!(b0 > a0, "the box should follow the view: {a0} → {b0}");
        assert!((b0 - a0 - 40.0 * SCALE).abs() < 0.01, "moved {} points", b0 - a0);
        assert!((b1 - b0 - (a1 - a0)).abs() < 0.01, "width should not change with position");
    }

    #[test]
    fn a_wider_window_shows_a_wider_box() {
        let (_, narrow) = minimap_span(0.0, 0.0, 40.0, SCALE);
        let (_, wide) = minimap_span(0.0, 0.0, 120.0, SCALE);
        assert!(wide > narrow);
    }

    #[test]
    fn a_negative_offset_cannot_push_the_box_off_the_left() {
        // egui can report a small negative offset mid-overscroll.
        let (x0, _) = minimap_span(100.0, -25.0, 80.0, SCALE);
        assert_eq!(x0, 100.0);
    }

    #[test]
    fn the_box_is_never_invisible() {
        // A viewport measured as zero columns (a pane dragged shut) should
        // still leave something to see rather than a zero-width rectangle.
        let (x0, x1) = minimap_span(0.0, 0.0, 0.0, SCALE);
        assert!(x1 > x0);
    }
}

#[cfg(test)]
mod permission_mode_tests {
    use super::mode_change_needs_respawn as needs;
    use crate::settings::AgentPermissionMode as M;

    /// Only the `--dangerously-allow-all` boundary needs a new process, because
    /// that one is a spawn-time flag. Everything else is a live message.
    #[test]
    fn only_the_allow_all_boundary_restarts_the_agent() {
        assert!(needs(M::AlwaysAsk, M::DangerouslySkipAll), "into allow-all");
        assert!(needs(M::DangerouslySkipAll, M::AlwaysAsk), "and back out of it");
        assert!(needs(M::AutoApprove, M::DangerouslySkipAll));
        assert!(!needs(M::AlwaysAsk, M::AutoApprove), "a live change, no restart");
    }

    #[test]
    fn changing_to_the_mode_already_set_does_nothing() {
        for m in [M::AlwaysAsk, M::AutoApprove, M::DangerouslySkipAll] {
            assert!(!needs(m, m), "{m:?} to itself");
        }
    }
}

#[cfg(test)]
mod glyph_coverage {
    /// Every glyph the UI draws must exist in a font that is actually loaded.
    ///
    /// The symbol fallback is Apple Symbols, chosen ahead of Arial Unicode
    /// because it is 877 KB against 22 MB resident — see `install_fonts`. The
    /// cost is narrower coverage, and three glyphs were outside it: `✕` on
    /// every close button, and `⏸`/`⏹` on the debugger. They rendered as an
    /// empty box, which is indistinguishable from a missing button.
    ///
    /// Reads the fonts this machine has, so it is macOS-only and skips itself
    /// where they are absent rather than failing for the wrong reason.
    #[test]
    fn no_ui_glyph_falls_outside_the_loaded_fonts() {
        let fonts = ["/System/Library/Fonts/Apple Symbols.ttf", "/System/Library/Fonts/SFNS.ttf"];
        let mut covered: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for path in fonts {
            match std::fs::read(path) {
                Ok(bytes) => covered.extend(codepoints(&bytes)),
                Err(_) => return, // not this platform; nothing to check against
            }
        }

        let source = include_str!("app.rs");
        let mut missing: Vec<(char, usize)> = Vec::new();
        for (n, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            // Comments describe the problem and name the glyphs; only what is
            // drawn matters.
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            for ch in string_literal_chars(line) {
                if ch as u32 > 0x2000 && !covered.contains(&(ch as u32)) {
                    missing.push((ch, n + 1));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "glyphs with no loaded font: {:?}",
            missing.iter().map(|(c, l)| format!("{c:?} U+{:04X} at app.rs:{l}", *c as u32))
                .collect::<Vec<_>>(),
        );
    }

    /// Characters inside double-quoted literals on a line. Crude on purpose:
    /// over-reporting would be caught by eye, under-reporting would let a tofu
    /// glyph through.
    fn string_literal_chars(line: &str) -> Vec<char> {
        let mut out = Vec::new();
        let mut inside = false;
        let mut prev_escape = false;
        for ch in line.chars() {
            if ch == '"' && !prev_escape {
                inside = !inside;
                continue;
            }
            prev_escape = ch == '\\' && !prev_escape;
            if inside {
                out.push(ch);
            }
        }
        out
    }

    /// Every codepoint a TrueType font's cmap maps, from formats 4 and 12.
    fn codepoints(d: &[u8]) -> Vec<u32> {
        let be16 = |at: usize| -> u32 {
            if at + 2 > d.len() { return 0 }
            u32::from(d[at]) << 8 | u32::from(d[at + 1])
        };
        let be32 = |at: usize| -> u32 {
            if at + 4 > d.len() { return 0 }
            (be16(at) << 16) | be16(at + 2)
        };
        let mut out = Vec::new();
        let tables = be16(4) as usize;
        let mut cmap = None;
        for i in 0..tables {
            let rec = 12 + i * 16;
            if d.get(rec..rec + 4) == Some(b"cmap") {
                cmap = Some(be32(rec + 8) as usize);
                break;
            }
        }
        let Some(cmap) = cmap else { return out };
        for i in 0..be16(cmap + 2) as usize {
            let sub = cmap + be32(cmap + 4 + i * 8 + 4) as usize;
            match be16(sub) {
                4 => {
                    let seg2 = be16(sub + 6) as usize;
                    for j in 0..seg2 / 2 {
                        let end = be16(sub + 14 + j * 2);
                        let start = be16(sub + 16 + seg2 + j * 2);
                        // 0xFFFF terminates the table.
                        if start <= end && end != 0xFFFF {
                            out.extend(start..=end);
                        }
                    }
                }
                12 => {
                    for j in 0..be32(sub + 12) as usize {
                        let g = sub + 16 + j * 12;
                        let (start, end) = (be32(g), be32(g + 4));
                        // Guard against a malformed group claiming the plane.
                        if start <= end && end - start < 0x10000 {
                            out.extend(start..=end);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod remote_path_tests {
    use super::{remote_parent, resolve_remote_dir};

    /// Walking up from a remote path. Computed here, not read back from the
    /// server: `fs/list` answers with entries and no path of its own.
    #[test]
    fn walking_up_stops_at_the_root() {
        // Not above it, and never the empty string, which would list nothing.
        assert_eq!(remote_parent("/home/sysadmin/code"), "/home/sysadmin");
        assert_eq!(remote_parent("/home/sysadmin"), "/home");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("/"), "/");
    }

    #[test]
    fn a_trailing_slash_does_not_cost_a_level() {
        assert_eq!(remote_parent("/home/sysadmin/"), "/home");
    }

    #[test]
    fn home_is_resolved_before_the_listing_is_asked_for() {
        assert_eq!(resolve_remote_dir("~", "/home/sysadmin"), "/home/sysadmin");
        assert_eq!(resolve_remote_dir("", "/home/sysadmin"), "/home/sysadmin");
        assert_eq!(resolve_remote_dir("~/code", "/home/sysadmin"), "/home/sysadmin/code");
        assert_eq!(resolve_remote_dir("~/code", "/home/sysadmin/"), "/home/sysadmin/code");
        assert_eq!(resolve_remote_dir("/opt/thing", "/home/sysadmin"), "/opt/thing");
    }

    /// A root account's home is `/`, and joining onto it naively gives `//code`.
    #[test]
    fn a_root_home_does_not_double_its_slash() {
        assert_eq!(resolve_remote_dir("~", "/"), "/");
        assert_eq!(resolve_remote_dir("~/code", "/"), "/code");
    }
}

#[cfg(test)]
mod remote_quick_open_tests {
    use super::{list_dir_remote, quick_open_entries, QUICK_OPEN_MAX_ENTRIES};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn reply(entries: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "entries": entries })
    }

    fn entry(name: &str, dir: bool) -> serde_json::Value {
        serde_json::json!({
            "name": name, "path": format!("/home/sysadmin/code/{name}"),
            "is_dir": dir, "size": 0,
        })
    }

    fn names(entries: &[super::QuickOpenEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The bug this exists for: on a remote workspace the listing must come off
    /// the remote. Asserted by what goes on the wire — a local `read_dir` of
    /// this path would send nothing at all.
    #[test]
    fn the_remote_is_the_one_asked() {
        let fake = crate::ssh::fake_fs(Ok(reply(serde_json::json!([]))), true);
        let dir = PathBuf::from("/home/sysadmin/code");
        list_dir_remote(&fake.handles, &dir, &AtomicBool::new(false)).unwrap();

        let reqs = fake.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1, "one listing per directory, not per row");
        assert_eq!(reqs[0]["method"], "fs/list");
        assert_eq!(reqs[0]["params"]["path"], "/home/sysadmin/code");
    }

    /// Directories are marked and ordered the same way the local listing marks
    /// and orders them, and hidden entries are skipped by both.
    #[test]
    fn a_remote_listing_obeys_the_same_rules_as_a_local_one() {
        let fake = crate::ssh::fake_fs(
            Ok(reply(serde_json::json!([
                entry("zeta.rs", false),
                entry(".git", true),
                entry("Cargo.toml", false),
                entry("src", true),
                entry("node_modules", true),
                entry("assets", true),
            ]))),
            true,
        );
        let (entries, truncated) = list_dir_remote(
            &fake.handles, &PathBuf::from("/home/sysadmin/code"), &AtomicBool::new(false),
        ).unwrap();

        assert!(!truncated);
        // Directories first with a trailing slash, then files, each group
        // case-insensitively by name. `.git` and `node_modules` are not shown.
        assert_eq!(names(&entries), ["assets/", "src/", "Cargo.toml", "zeta.rs"]);
    }

    /// Paths come from the server rather than being rebuilt by joining, so the
    /// path opened is the path the remote named.
    #[test]
    fn the_path_opened_is_the_one_the_remote_gave() {
        let fake = crate::ssh::fake_fs(
            Ok(reply(serde_json::json!([
                { "name": "main.rs", "path": "/srv/elsewhere/main.rs", "is_dir": false, "size": 1 }
            ]))),
            true,
        );
        let (entries, _) = list_dir_remote(
            &fake.handles, &PathBuf::from("/home/sysadmin/code"), &AtomicBool::new(false),
        ).unwrap();
        assert_eq!(entries[0].path, PathBuf::from("/srv/elsewhere/main.rs"));
    }

    /// A refused or unreachable listing is an error, not an empty folder. The
    /// dialog says so instead of sending the user looking for a file that is
    /// there.
    #[test]
    fn a_failed_listing_is_not_an_empty_folder() {
        let fake = crate::ssh::fake_fs(Err("permission denied".into()), true);
        let err = match list_dir_remote(
            &fake.handles, &PathBuf::from("/root"), &AtomicBool::new(false),
        ) {
            Err(e) => e,
            Ok((entries, _)) => panic!("a refusal came back as {} entries", entries.len()),
        };
        assert!(err.contains("permission denied"), "got {err:?}");
    }

    #[test]
    fn a_listing_that_is_never_answered_gives_up() {
        let fake = crate::ssh::fake_fs(Ok(reply(serde_json::json!([]))), false);
        let err = crate::ssh::fs_list_with(
            &fake.handles, "/home/sysadmin", std::time::Duration::from_millis(50),
        ).unwrap_err();
        assert!(err.contains("timed out"), "got {err:?}");
    }

    /// The cap is Quick Open's, not the local filesystem's, so a remote
    /// directory of a million entries cannot build a million rows either.
    #[test]
    fn the_entry_cap_applies_to_remote_listings_too() {
        let raw = (0..QUICK_OPEN_MAX_ENTRIES + 50)
            .map(|i| (format!("f{i:07}.rs"), PathBuf::from(format!("/d/f{i:07}.rs")), false));
        let (entries, truncated) = quick_open_entries(raw, &AtomicBool::new(false));
        assert_eq!(entries.len(), QUICK_OPEN_MAX_ENTRIES);
        assert!(truncated, "the dialog must be able to say it is showing a prefix");
    }
}

#[cfg(test)]
mod window_reload_tests {
    use super::{IdeApp, NewWindowSpec};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-reload-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The two commands must not be the same button wearing two labels: one
    /// rebuilds this window, the other replaces the process and takes every
    /// other window's conversations and language servers with it.
    #[test]
    fn reloading_a_window_does_not_ask_for_a_new_process() {
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("flags")), ..Default::default()
        });
        app.reload_window();
        assert!(app.pending_window_reload, "this window should be rebuilt");
        assert!(!app.pending_reload, "the process must be left alone");

        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("flags2")), ..Default::default()
        });
        app.restart_process();
        assert!(app.pending_reload);
        assert!(!app.pending_window_reload);
    }

    /// What makes an in-place rebuild acceptable: the window comes back with the
    /// files it had. A reload that reopened empty would be a worse version of
    /// closing the window.
    #[test]
    fn a_rebuilt_window_comes_back_with_its_files() {
        let dir = scratch("restore");
        let file = dir.join("kept.rs");
        std::fs::write(&file, "fn kept() {}\n").unwrap();

        // What `reload_window` writes, written where a window with no state of
        // its own still finds it — so this test never touches the real config
        // directory.
        crate::session::save(&dir, &crate::session::SessionState {
            open_files:  vec![file.clone()],
            active_file: 0,
            ..Default::default()
        });

        // The rebuild: same spec the event loop passes, `is_reload` and all.
        let app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(dir.clone()), is_reload: true, ..Default::default()
        });
        let open: Vec<_> = app.buffers.iter().filter_map(|b| b.path.clone()).collect();
        // The stored path is reopened as written, not re-derived.
        assert_eq!(open, vec![file]);
    }

    /// The window must come back as *itself*: same workspace, same session
    /// identity. A fresh id looks up nobody's state and the window returns
    /// empty, which is the failure mode a reload cannot have.
    #[test]
    fn a_reload_keeps_the_window_it_is_reloading() {
        let dir = scratch("spec");
        let app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(dir.clone()), ..Default::default()
        });
        let spec = app.reload_spec();
        assert_eq!(spec.window_id, app.window_id, "a new id would lose the session");
        assert_eq!(spec.cwd, Some(dir.canonicalize().unwrap_or(dir)));
        assert!(spec.is_reload, "without this, restoring is left to a setting");
        // Placement is for creating an OS window. This one is not being created.
        assert!(spec.frame.is_none() && !spec.maximized);
    }

    /// A window with no folder open must come back with no folder open — not
    /// rooted at `$HOME`, which is what its `cwd` reports and what an earlier
    /// version of the process restart got wrong.
    #[test]
    fn a_folderless_window_reloads_folderless() {
        let app = IdeApp::new_with_spec(NewWindowSpec::default());
        assert_eq!(app.reload_spec().cwd, None);
    }

    /// Restoring is unconditional on a reload. Asserted on the decision itself
    /// rather than through a constructed window, because `restore_session` is a
    /// user setting: on a machine with it turned on, an end-to-end test passes
    /// whether or not the reload half works.
    #[test]
    fn a_reload_restores_whatever_the_setting_says() {
        use super::should_restore_session;
        assert!(should_restore_session(false, true), "a reload must restore");
        assert!(should_restore_session(true, true));
        // And the setting still decides for a window that was merely opened.
        assert!(should_restore_session(true, false));
        assert!(!should_restore_session(false, false));
    }
}

#[cfg(test)]
mod subagent_block_tests {
    use super::*;
    use crate::agent_panel::{ApprovalState, ChatItem};

    fn tool(name: &str, approval: ApprovalState) -> ChatItem {
        ChatItem::ToolRequest {
            name: name.into(), args: "{\"path\":\".\"}".into(), id: format!("call-{name}"),
            kind: "read".into(), approval, expanded: false,
        }
    }

    fn sub(id: &str, finished: bool, items: Vec<ChatItem>) -> ChatItem {
        ChatItem::Subagent {
            id: id.into(), agent_type: "explore".into(),
            prompt: "Explore the codebase".into(),
            current_tool: "read_file".into(), detail: "src/cascor.rs".into(),
            finished, summary: "the answer".into(), items, expanded: true,
        }
    }

    /// A rendering harness: lay the block out at a given width and hand back
    /// every rectangle and text run it produced, so what it drew can be
    /// asserted rather than eyeballed in a screenshot.
    fn render(item: &ChatItem, width: f32) -> (Vec<egui::Rect>, Vec<String>) {
        let ctx = egui::Context::default();
        setup_fonts(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0), egui::vec2(width, 900.0),
            )),
            ..Default::default()
        };
        let mut pending = None;
        let mut toggle = None;
        let draw = |ctx: &egui::Context, pending: &mut _, toggle: &mut _| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let ChatItem::Subagent {
                    agent_type, prompt, current_tool, detail, finished, summary, expanded, items, ..
                } = item else { panic!("not a subagent") };
                draw_subagent_block(
                    ui, &[0], agent_type, prompt, current_tool, detail, *finished, summary,
                    *expanded, items, 10.0, 10.0, pending, toggle,
                );
            });
        };
        // Two passes: egui sizes some things from the previous frame.
        let _ = ctx.run(input.clone(), |ctx| draw(ctx, &mut pending, &mut toggle));
        let out = ctx.run(input, |ctx| draw(ctx, &mut pending, &mut toggle));

        let mut rects = Vec::new();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            match &cs.shape {
                egui::Shape::Text(t) => {
                    texts.push(t.galley.text().to_string());
                    rects.push(t.visual_bounding_rect());
                }
                egui::Shape::Rect(r) => rects.push(r.rect),
                _ => {}
            }
        }
        (rects, texts)
    }

    /// The point of the whole change: what the subagent did is inside the
    /// transcript, under the subagent's own name — not in a strip somewhere
    /// else with a "see below" pointing at it.
    #[test]
    fn a_running_subagents_work_is_shown_in_place() {
        let item = sub("sub_0", false, vec![
            // An execute call, which is shown by name — consecutive *read*
            // calls fold into a summarised checklist, so a lone read would be
            // testing the grouping rather than what is inside the block.
            ChatItem::ToolRequest {
                name: "shell_exec".into(), args: "{\"command\":\"cargo build\"}".into(),
                id: "call-1".into(), kind: "execute".into(),
                approval: ApprovalState::Approved, expanded: false,
            },
        ]);
        let (_, texts) = render(&item, 420.0);
        let all = texts.join(" | ");
        assert!(all.contains("explore"), "the subagent is not named: {all}");
        // Rendered the way the transcript renders any shell call — "shell"
        // and the command — so this asserts the work is *there*, not that it
        // is labelled with the raw tool name.
        assert!(all.contains("cargo build"), "its tool calls are missing: {all}");
        assert!(!all.contains("see below"), "still pointing somewhere else: {all}");
        // And what it is doing right now, which the card never used to show.
        assert!(all.contains("read_file"), "no live activity: {all}");
    }

    /// The rule down the left edge is what separates the subagent's lines from
    /// the main agent's. Without it the block is just another card.
    #[test]
    fn the_block_is_ruled_down_its_left_edge() {
        let item = sub("sub_0", false, vec![tool("list_directory", ApprovalState::Approved)]);
        let (rects, _) = render(&item, 420.0);
        let rule = rects.iter().find(|r| r.width() <= 2.5 && r.height() > 30.0);
        let rule = rule.unwrap_or_else(|| panic!("no left rule was painted; got {} rects", rects.len()));
        // Tall and thin, at the block's left edge — the whole block's height, so
        // everything inside falls under it.
        assert!(rule.height() > 30.0, "the rule does not span the block: {rule:?}");
    }

    /// A finished subagent keeps its answer visible, since that is the part
    /// worth reading once the work is folded away.
    #[test]
    fn a_finished_subagent_still_shows_its_answer() {
        let item = sub("sub_0", true, Vec::new());
        let (_, texts) = render(&item, 420.0);
        assert!(texts.join(" | ").contains("the answer"), "{texts:?}");
    }

    /// Nothing overruns the panel — including at the narrow width the panel can
    /// actually be dragged to.
    #[test]
    fn the_block_stays_inside_a_narrow_panel() {
        for width in [220.0_f32, 260.0, 300.0, 380.0, 700.0] {
            let item = sub("sub_0", false, vec![
                tool("list_directory", ApprovalState::Approved),
                sub("sub_0:call-1", false, vec![tool("read_file", ApprovalState::Pending)]),
            ]);
            let (rects, _) = render(&item, width);
            let mut over: Vec<&egui::Rect> = rects.iter().filter(|r| r.right() > width).collect();
            over.sort_by(|a, b| b.right().total_cmp(&a.right()));
            if !over.is_empty() {
                for r in over.iter().take(5) {
                    eprintln!("  at {width}pt: right {:.1} left {:.1} w {:.1} h {:.1}",
                              r.right(), r.left(), r.width(), r.height());
                }
                panic!("at {width}pt, {} shapes overran", over.len());
            }
        }
    }
}

#[cfg(test)]
mod subagent_strip_tests {
    use super::subagent_awaiting_approval;
    use crate::agent_panel::{ApprovalState, ChatItem};

    fn req(approval: ApprovalState) -> ChatItem {
        ChatItem::ToolRequest {
            name: "shell_exec".into(), args: "{}".into(), id: "c1".into(),
            kind: "execute".into(), approval, expanded: false,
        }
    }

    fn sub(finished: bool, items: Vec<ChatItem>) -> ChatItem {
        ChatItem::Subagent {
            id: "sub_0".into(), agent_type: "explore".into(), prompt: String::new(),
            current_tool: String::new(), detail: String::new(),
            finished, summary: String::new(), items, expanded: true,
        }
    }

    /// A running subagent with nothing to answer no longer gets a strip slot —
    /// that was the duplication: the same subagent in the transcript and again
    /// above the input box.
    #[test]
    fn merely_running_does_not_earn_a_strip_slot() {
        assert!(!subagent_awaiting_approval(&sub(false, vec![req(ApprovalState::Approved)])));
        assert!(!subagent_awaiting_approval(&sub(false, Vec::new())));
    }

    /// An approval does, because it is the one thing that cannot wait for the
    /// user to scroll to it.
    #[test]
    fn a_pending_approval_earns_one() {
        assert!(subagent_awaiting_approval(&sub(false, vec![req(ApprovalState::Pending)])));
    }

    /// Including one belonging to a subagent this subagent delegated to — the
    /// whole chain is blocked on it.
    #[test]
    fn a_nested_approval_earns_one_too() {
        let inner = sub(false, vec![req(ApprovalState::Pending)]);
        assert!(subagent_awaiting_approval(&sub(false, vec![inner])));
    }

    /// And a finished subagent never does, whatever it is holding.
    #[test]
    fn a_finished_subagent_never_does() {
        assert!(!subagent_awaiting_approval(&sub(true, vec![req(ApprovalState::Pending)])));
    }
}

#[cfg(test)]
mod remote_boundary_tests {
    use super::note_remote_boundary;
    use crate::agent_panel::ChatItem;

    fn transcript() -> Vec<ChatItem> {
        vec![
            ChatItem::User("build the kernel".into()),
            ChatItem::Assistant { text: "done".into(), done: true },
        ]
    }

    /// The mark says where it happened, that the session stayed there, and that
    /// carrying on here is not carrying on the same conversation — the three
    /// things a reader needs to not be misled by a transcript that survived a
    /// reload the connection did not.
    #[test]
    fn the_mark_says_what_happened_and_where() {
        let mut items = transcript();
        assert!(note_remote_boundary(&mut items, "admin-1"));
        let ChatItem::Status(note) = items.last().unwrap() else { panic!("not a status line") };
        assert!(note.contains("admin-1"), "{note}");
        assert!(note.contains("local now"), "{note}");
        assert!(note.contains("different conversation"), "{note}");
    }

    /// Reloading twice must not stack two of them.
    #[test]
    fn marking_twice_marks_once() {
        let mut items = transcript();
        assert!(note_remote_boundary(&mut items, "admin-1"));
        let after_first = items.len();
        assert!(!note_remote_boundary(&mut items, "admin-1"), "marked again");
        assert_eq!(items.len(), after_first);
    }

    /// A later remote session that ends the same way gets its own mark, since
    /// there is real conversation between the two boundaries.
    #[test]
    fn a_second_session_gets_its_own_mark() {
        let mut items = transcript();
        note_remote_boundary(&mut items, "admin-1");
        items.push(ChatItem::User("now on the mac".into()));
        assert!(note_remote_boundary(&mut items, "admin-2"));
        assert_eq!(
            items.iter().filter(|i| matches!(i, ChatItem::Status(s) if s.contains("ended here"))).count(),
            2,
        );
    }

    /// Leaving also gives up the remote's session id: resuming a log that lives
    /// on a machine we are no longer connected to cannot work, and asking
    /// produces an error the reader has to interpret.
    #[test]
    fn leaving_gives_up_the_remote_session_id() {
        use super::leave_remote;
        let mut items = transcript();
        let mut id = "20260821_020146_7f9".to_string();
        leave_remote(&mut items, &mut id, "admin-1");
        assert!(id.is_empty(), "still holding a session id from the remote: {id}");
        assert!(matches!(items.last(), Some(ChatItem::Status(_))), "no boundary was marked");
    }

    /// An empty transcript has nothing to mark the end of.
    #[test]
    fn an_empty_transcript_is_left_alone() {
        let mut items = Vec::new();
        assert!(!note_remote_boundary(&mut items, "admin-1"));
        assert!(items.is_empty());
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::{IdeApp, NewWindowSpec, should_restore_on_open};
    use crate::agent_panel::{AgentSession, ChatItem};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-reconnect-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A reloading remote window must restore its own state, connection or no
    /// connection. Its terminals are the local pty daemon's: not restoring them
    /// leaves shells running with nothing pointing at them, which is why
    /// carrying the host across was not done until now.
    #[test]
    fn a_reloading_window_restores_even_while_reconnecting() {
        assert!(should_restore_on_open(false, true, true), "a reload must restore");
        assert!(should_restore_on_open(true, true, true));
        // A window merely *opening* a remote workspace is not that window, and
        // must not adopt whatever local state was last saved.
        assert!(!should_restore_on_open(true, false, true));
        // And with nothing remote in play, the setting decides as before.
        assert!(should_restore_on_open(true, false, false));
        assert!(!should_restore_on_open(false, false, false));
    }

    /// The rebuild counts itself, so a reload that takes 40ms can be told apart
    /// from one that did not happen. Each rebuild is one higher than the last.
    #[test]
    fn each_rebuild_counts_itself() {
        let dir = scratch("counted");
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(dir.clone()), ..Default::default()
        });

        // The first rebuild is #1, and the app it produces says so.
        let spec = app.reload_spec();
        assert_eq!(spec.reload_count, 1);
        app = IdeApp::new_with_spec(spec);
        assert!(
            app.output_log.iter().any(|(m, _)| m.contains("Window rebuilt (#1)")),
            "a rebuild left no trace in {} log line(s)", app.output_log.len(),
        );
        assert!(app.status.contains("reloaded (#1)"), "{}", app.status);

        // And the next carries on from it rather than starting again.
        let spec = app.reload_spec();
        assert_eq!(spec.reload_count, 2);
        let app = IdeApp::new_with_spec(spec);
        assert!(app.status.contains("reloaded (#2)"), "{}", app.status);
    }

    /// A window that was merely opened says nothing — the line is there to prove
    /// a rebuild, and printing it on a fresh window would prove nothing.
    #[test]
    fn a_window_that_was_not_rebuilt_says_nothing_about_rebuilding() {
        let app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("fresh")), ..Default::default()
        });
        assert!(!app.status.contains("reloaded"), "{}", app.status);
        assert!(!app.output_log.iter().any(|(m, _)| m.contains("rebuilt")));
    }

    /// A local window has no host to carry, so nothing about reconnecting
    /// applies to it.
    #[test]
    fn a_local_reload_carries_no_host() {
        let app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("local")), ..Default::default()
        });
        assert!(app.reload_spec().ssh_host.is_none());
    }

    /// The failing half of the reconnect, which is the half that can be driven
    /// without a remote machine: a tab that was waiting continues locally, keeps
    /// its transcript and anything typed while it waited, gets the boundary
    /// written in, and gives up the remote's session id.
    #[test]
    fn a_reconnect_that_fails_continues_locally_and_says_so() {
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("failed")), ..Default::default()
        });
        app.ssh_form.host = "admin-1".into();

        let mut session = AgentSession::pending(
            "Reconnecting to admin-1…".into(), Some("20260821_020146_7f9"),
        );
        session.items = vec![ChatItem::User("built it on the remote".into())];
        session.input = "half-typed reply".into();
        let mode = app.settings.default_agent_permission_mode;
        app.agent_tabs.push(crate::app::AgentTab::new(session, mode));

        app.finish_pending_agents();

        let tab = &app.agent_tabs[0];
        assert!(tab.session.pending.is_none(), "still waiting on a connection");
        assert!(tab.session.forge_session_id.is_empty(),
                "kept a session id belonging to the remote");
        assert_eq!(tab.session.input, "half-typed reply", "lost what was typed");
        assert!(matches!(tab.session.items.first(), Some(ChatItem::User(_))),
                "lost the transcript");
        let last = tab.session.items.last().unwrap();
        match last {
            ChatItem::Status(note) => {
                assert!(note.contains("admin-1"), "{note}");
                assert!(note.contains("local now"), "{note}");
            }
            _ => panic!("the last item is not the boundary"),
        }
    }

    /// A message sent while the tab was waiting is held, not lost, and goes out
    /// once there is something to send it to. Writing into a session with no
    /// agent behind it drops the message and leaves the tab looking like it is
    /// thinking about it.
    #[test]
    fn a_message_sent_while_reconnecting_is_delivered_afterwards() {
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("held")), ..Default::default()
        });
        app.ssh_form.host = "admin-1".into();

        let mut session = AgentSession::pending("Reconnecting to admin-1…".into(), None);
        session.send_user("what did the build say?".into());
        assert!(session.items.is_empty(), "sent into a session with no agent");
        assert_eq!(session.queued.len(), 1, "the message was not held");

        let mode = app.settings.default_agent_permission_mode;
        app.agent_tabs.push(crate::app::AgentTab::new(session, mode));
        app.finish_pending_agents();

        let tab = &app.agent_tabs[0];
        assert!(tab.session.queued.is_empty(), "still holding it");
        assert!(
            tab.session.items.iter().any(|i| matches!(i, ChatItem::User(t) if t.contains("build"))),
            "the held message never went out",
        );
    }

    /// Tabs that already have a process are left alone — this runs on every
    /// connect result, including ones that have nothing to do with a reload.
    #[test]
    fn a_tab_that_is_not_waiting_is_untouched() {
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("untouched")), ..Default::default()
        });
        let mut session = AgentSession::failed("no agent binary".into());
        session.items = vec![ChatItem::User("hello".into())];
        let mode = app.settings.default_agent_permission_mode;
        app.agent_tabs.push(crate::app::AgentTab::new(session, mode));

        app.finish_pending_agents();

        let tab = &app.agent_tabs[0];
        assert_eq!(tab.session.spawn_err.as_deref(), Some("no agent binary"));
        assert_eq!(tab.session.items.len(), 1, "a boundary was written where none belonged");
    }
}

#[cfg(test)]
mod mode_switch_tests {
    use super::{ModeSwitch, mode_auto_approves, mode_switch_plan, release_pending_approvals};
    use crate::agent_panel::{ApprovalState, ChatItem};
    use crate::settings::AgentPermissionMode as Mode;

    /// Loosening mid-turn is applied to the running agent. The whole point: the
    /// call it is blocked on gets approved and it carries on, instead of the
    /// user interrupting the turn, changing the mode and asking again.
    #[test]
    fn loosening_mid_turn_is_applied_live() {
        assert_eq!(mode_switch_plan(Mode::AlwaysAsk, Mode::AutoApprove, true), ModeSwitch::Live);
        assert_eq!(mode_switch_plan(Mode::AlwaysAsk, Mode::AutoApprove, false), ModeSwitch::Live);
    }

    /// Skip All is a spawn-time flag, so the process has to be replaced — but
    /// mid-turn the part that unblocks the user happens now and the process
    /// waits, rather than the turn dying for a flag it did not need yet.
    #[test]
    fn skip_all_mid_turn_waits_for_the_turn_to_replace_the_agent() {
        assert_eq!(
            mode_switch_plan(Mode::AlwaysAsk, Mode::DangerouslySkipAll, true),
            ModeSwitch::LiveThenRespawnAfterTurn,
        );
        // Idle, there is no turn to protect, so it is done properly at once.
        assert_eq!(
            mode_switch_plan(Mode::AlwaysAsk, Mode::DangerouslySkipAll, false),
            ModeSwitch::RespawnNow,
        );
    }

    /// Tightening *out of* Skip All does not wait, even mid-turn. The mode skips
    /// confirmation on anything at all, including tools nothing recognises, and
    /// "it will stop when it finishes" is not a safety property.
    #[test]
    fn leaving_skip_all_takes_effect_immediately() {
        for to in [Mode::AlwaysAsk, Mode::AutoApprove] {
            assert_eq!(
                mode_switch_plan(Mode::DangerouslySkipAll, to, true),
                ModeSwitch::RespawnNow,
                "mid-turn switch to {to:?} was deferred",
            );
        }
    }

    #[test]
    fn only_always_ask_asks() {
        assert!(!mode_auto_approves(Mode::AlwaysAsk));
        assert!(mode_auto_approves(Mode::AutoApprove));
        assert!(mode_auto_approves(Mode::DangerouslySkipAll));
    }

    fn req(id: &str, approval: ApprovalState) -> ChatItem {
        ChatItem::ToolRequest {
            name: "shell_exec".into(), args: "{}".into(), id: id.into(),
            kind: "execute".into(), approval, expanded: false,
        }
    }

    /// Every call the turn is blocked on, including one inside a subagent —
    /// which blocks its subagent, which blocks the turn.
    #[test]
    fn every_waiting_call_is_released_including_a_subagents() {
        let mut items = vec![
            req("done", ApprovalState::Approved),
            req("waiting", ApprovalState::Pending),
            ChatItem::Subagent {
                id: "sub_0".into(), agent_type: "explore".into(), prompt: String::new(),
                current_tool: String::new(), detail: String::new(),
                finished: false, summary: String::new(),
                items: vec![req("nested", ApprovalState::Pending)], expanded: true,
            },
        ];

        let ids = release_pending_approvals(&mut items);

        assert_eq!(ids, vec!["waiting".to_string(), "nested".to_string()]);
        // And the cards say so, rather than still showing a question.
        assert!(matches!(&items[1], ChatItem::ToolRequest { approval: ApprovalState::Approved, .. }));
        let ChatItem::Subagent { items: nested, .. } = &items[2] else { panic!() };
        assert!(matches!(&nested[0], ChatItem::ToolRequest { approval: ApprovalState::Approved, .. }));
    }

    /// A decision that is not a permission decision is left for the user, which
    /// is the reason this only looks at tool requests: a plan, a question, and a
    /// password prompt are each theirs to answer, whatever the mode says.
    #[test]
    fn a_plan_a_question_and_a_password_are_not_permissions() {
        let mut items = vec![
            ChatItem::Plan {
                plan_path: "plan.md".into(), content: "do the thing".into(),
                resolved: false, resolution: String::new(),
                reject_feedback: String::new(), expanded: true,
            },
            ChatItem::Question {
                tool_id: "q1".into(), question: "which one?".into(), items: Vec::new(),
                selected: Vec::new(), other_text: Vec::new(),
                free_text: String::new(), answered: false,
            },
            ChatItem::InputNeeded {
                bg_id: None, command: "sudo make install".into(),
                prompt: "Password:".into(), is_password: true,
                resolved: false, resolution: String::new(),
                text: String::new(), remember_confirm: String::new(),
            },
        ];

        assert!(release_pending_approvals(&mut items).is_empty());
        assert!(matches!(&items[0], ChatItem::Plan { resolved: false, .. }));
        assert!(matches!(&items[1], ChatItem::Question { answered: false, .. }));
        assert!(matches!(&items[2], ChatItem::InputNeeded { resolved: false, .. }));
    }
}

#[cfg(test)]
mod activity_visibility_tests {
    use super::human_elapsed;
    use crate::agent_panel::activity_phrase;
    use std::time::Duration;

    /// Said the way a person would say it, because it is read at a glance while
    /// waiting — "154s" makes you do arithmetic to find out if you should worry.
    #[test]
    fn elapsed_reads_as_time() {
        assert_eq!(human_elapsed(Duration::from_secs(3)), "3s");
        assert_eq!(human_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(human_elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(human_elapsed(Duration::from_secs(154)), "2m 34s");
        assert_eq!(human_elapsed(Duration::from_secs(3599)), "59m 59s");
        assert_eq!(human_elapsed(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(human_elapsed(Duration::from_secs(7860)), "2h 11m");
    }

    /// The line used to read "Running shell_exec…", which says a tool is running
    /// and nothing about which one or why it might be slow. What it is doing to
    /// what is already in the arguments.
    #[test]
    fn the_line_says_what_is_being_done_to_what() {
        assert_eq!(
            activity_phrase("shell_exec", r#"{"command":"cargo build --release"}"#),
            "Running cargo build --release",
        );
        assert_eq!(
            activity_phrase("edit_file", r#"{"path":"/a/b/src/cascor.rs"}"#),
            "Editing cascor.rs…",
        );
        assert_eq!(
            activity_phrase("read_file", r#"{"path":"src/lib.rs"}"#),
            "Reading lib.rs…",
        );
        assert_eq!(
            activity_phrase("search_code", r#"{"query":"TODO"}"#),
            "Searching for TODO…",
        );
        assert_eq!(
            activity_phrase("delegate_task", r#"{"agent_type":"explore"}"#),
            "Waiting on the explore subagent…",
        );
    }

    /// A long command must not push the input box down the screen, so it is cut
    /// here rather than wrapped by the label.
    #[test]
    fn a_long_command_is_cut_to_one_line() {
        let long = "sleep 300\nps -p 72164 -o etime=,pcpu= 2>/dev/null || echo process_gone\ngrep -E 'seed=|====' /tmp/n4_rethinks_v2.log | tail -40";
        let args = serde_json::json!({ "command": long }).to_string();
        let phrase = activity_phrase("shell_exec", &args);
        assert!(phrase.chars().count() <= 58, "too long to sit on one line: {phrase:?}");
        assert!(!phrase.contains('\n'), "a newline would grow the row: {phrase:?}");
        // And it still says what it is, not just that something is running.
        assert!(phrase.starts_with("Running sleep 300"), "{phrase:?}");
    }

    /// Nothing recognisable still beats a bare tool name.
    #[test]
    fn an_unknown_tool_still_reads_as_english() {
        assert_eq!(activity_phrase("some_new_tool", "{}"), "Running some new tool…");
        assert_eq!(activity_phrase("shell_exec", "not json at all"), "Running a command…");
    }
}

#[cfg(test)]
mod running_dot_tests {
    /// The dot has to actually move. A static icon is exactly what it replaces,
    /// and "it looks animated in the code" is not the property — so this draws
    /// it at two points in its cycle and insists the geometry differs.
    #[test]
    fn the_running_dot_moves() {
        let ctx = egui::Context::default();
        let radius_at = |t: f64| -> f32 {
            let input = egui::RawInput {
                time: Some(t),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0),
                )),
                ..Default::default()
            };
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    super::paint_running_dot(ui, egui::Color32::from_rgb(110, 150, 220));
                });
            });
            out.shapes.iter().find_map(|cs| match &cs.shape {
                egui::Shape::Circle(c) if c.fill.a() > 0 => Some(c.radius),
                _ => None,
            }).expect("no dot was painted")
        };

        // A quarter of the way through the cycle: the widest separation there is.
        let small = radius_at(0.0);
        let large = radius_at(std::f64::consts::FRAC_PI_2 / 4.0);
        assert!((large - small).abs() > 0.5, "the dot did not move: {small} vs {large}");
    }

    // Not asserted here: that this asks for frames no faster than the panel's
    // own 50ms cadence. egui's own animation scheduling already floors a frame
    // in this harness at 33ms, so the measurement cannot see this function's
    // request at all — the test passed with a bare `request_repaint()`, which is
    // exactly the mistake it was meant to catch. The cadence is set at the call
    // site instead; see `paint_running_dot`.
}

#[cfg(test)]
mod countdown_tests {
    use super::{countdown_label, human_elapsed};
    use crate::agent_panel::{Expectation, expected_runtime};
    use std::time::Duration;

    fn secs(n: u64) -> Duration { Duration::from_secs(n) }

    /// A sleep is a promise, so the countdown says "about".
    #[test]
    fn a_sleep_counts_down() {
        let e = expected_runtime("sleep 300").unwrap();
        assert_eq!(e, Expectation::About(secs(300)));
        assert_eq!(countdown_label(Some(e), secs(41)).as_deref(), Some("~4m 19s left"));
    }

    /// A timeout is a ceiling, and says so — "~5m left" for `timeout 300 cargo
    /// test` would be wrong every time the tests pass in twenty seconds.
    #[test]
    fn a_timeout_is_a_ceiling_not_a_promise() {
        let e = expected_runtime("timeout 300 cargo test").unwrap();
        assert_eq!(e, Expectation::AtMost(secs(300)));
        assert_eq!(countdown_label(Some(e), secs(60)).as_deref(), Some("up to 4m 00s left"));
    }

    /// Outliving the estimate: the estimate predicts nothing from then on, so it
    /// stops being shown rather than sitting at zero or going negative. The
    /// elapsed time carries on beside it, which is the honest part.
    #[test]
    fn an_estimate_that_runs_out_stops_claiming_anything() {
        let e = Expectation::About(secs(30));
        assert!(countdown_label(Some(e), secs(29)).is_some());
        assert_eq!(countdown_label(Some(e), secs(30)), None, "sat at zero");
        assert_eq!(countdown_label(Some(e), secs(31)), None, "counted past the end");
        assert_eq!(countdown_label(Some(e), secs(9_999)), None, "still claiming a finish");
    }

    /// And with nothing claimed, nothing is shown — most commands say nothing
    /// about their own length.
    #[test]
    fn no_claim_no_countdown() {
        assert_eq!(countdown_label(None, secs(5)), None);
        assert!(expected_runtime("cargo build --release").is_none());
        assert!(expected_runtime("ps -p 72164 -o etime=").is_none());
    }

    /// The forms an agent actually writes, on the three platforms.
    #[test]
    fn the_platforms_agree() {
        assert_eq!(expected_runtime("sleep 5m"), Some(Expectation::About(secs(300))));
        assert_eq!(expected_runtime("sleep 1.5"), Some(Expectation::About(Duration::from_millis(1500))));
        assert_eq!(expected_runtime("sleep 2h"), Some(Expectation::About(secs(7200))));
        // GNU sleep sums its arguments.
        assert_eq!(expected_runtime("sleep 1m 30s"), Some(Expectation::About(secs(90))));
        // Windows.
        assert_eq!(expected_runtime("timeout /t 30 /nobreak"), Some(Expectation::AtMost(secs(30))));
        // PowerShell.
        assert_eq!(expected_runtime("Start-Sleep -Seconds 45"), Some(Expectation::About(secs(45))));
        assert_eq!(expected_runtime("Start-Sleep -Milliseconds 500"),
                   Some(Expectation::About(Duration::from_millis(500))));
        assert_eq!(expected_runtime("Start-Sleep 12"), Some(Expectation::About(secs(12))));
        // GNU timeout's own flags are not the duration.
        assert_eq!(expected_runtime("timeout -k 5 30 ./run.sh"), Some(Expectation::AtMost(secs(30))));
        assert_eq!(expected_runtime("timeout --signal=KILL 45 ./run.sh"),
                   Some(Expectation::AtMost(secs(45))));
    }

    /// A wrapped sleep cannot outlast the limit wrapped around it: `timeout 60
    /// sleep 300` is a minute, not six.
    #[test]
    fn a_limit_beats_the_sleep_it_wraps() {
        assert_eq!(expected_runtime("timeout 60 sleep 300"), Some(Expectation::AtMost(secs(60))));
        // And the other way round, the sleep is what happens.
        assert_eq!(expected_runtime("timeout 600 sleep 30"), Some(Expectation::AtMost(secs(30))));
    }

    /// Sequential parts add up, and a ceiling anywhere makes the total a ceiling
    /// — the command line from the screenshot that started this is one of these.
    #[test]
    fn parts_of_a_command_line_add_up() {
        assert_eq!(expected_runtime("sleep 30; sleep 15"), Some(Expectation::About(secs(45))));
        assert_eq!(expected_runtime("sleep 30 && timeout 10 ./x"), Some(Expectation::AtMost(secs(40))));
        let real = "sleep 300\nps -p 72164 -o etime=,pcpu= 2>/dev/null || echo process_gone\ngrep -E 'seed=|====' /tmp/n4.log | tail -40";
        assert_eq!(expected_runtime(real), Some(Expectation::About(secs(300))));
    }

    /// A command that only *mentions* a sleep does not sleep. The pattern in the
    /// screenshot's own `grep` is exactly this, and a naive scan reports five
    /// minutes for a command that returns instantly.
    #[test]
    fn a_quoted_sleep_is_not_a_sleep() {
        assert!(expected_runtime("grep -E 'sleep 300' /tmp/log").is_none());
        assert!(expected_runtime("echo \"sleep 300\"").is_none());
        assert!(expected_runtime("grep -E 'seed=|sleep 60|STOP' log | tail -40").is_none());
        // But a real one after a mention of one still counts.
        assert_eq!(expected_runtime("echo 'sleep 300'; sleep 5"),
                   Some(Expectation::About(secs(5))));
    }

    #[test]
    fn a_countdown_reads_as_time() {
        assert_eq!(human_elapsed(secs(259)), "4m 19s");
    }
}

#[cfg(test)]
mod build_stamp_tests {
    use super::{build_stamp, format_civil};

    /// The version alone cannot tell two builds of the same version apart, which
    /// is the whole case for having this: rolling one out window by window, every
    /// window says 0.3.0 and none says *which* 0.3.0.
    #[test]
    fn the_stamp_carries_more_than_the_version() {
        let s = build_stamp();
        assert!(s.starts_with(env!("CARGO_PKG_VERSION")), "{s}");
        assert!(s.contains('·'), "no build time in {s:?}");
        assert!(s.len() > env!("CARGO_PKG_VERSION").len() + 3, "{s}");
    }

    /// Read from the binary this process started from, so it does not change
    /// under a running window when the file on disk is replaced.
    #[test]
    fn the_stamp_is_stable_within_a_process() {
        assert_eq!(build_stamp(), build_stamp());
    }

    #[test]
    fn a_build_time_reads_as_a_date() {
        // The calendar arithmetic, with no timezone in the way: what it is
        // handed is what it renders.
        let t = format_civil;
        // 2026-08-21 23:16:00 UTC — checked against a date library rather than
        // guessed; my first guess for this constant was five days out, and the
        // arithmetic was right.
        assert_eq!(t(1_787_354_160), "Aug 21 23:16");
        // Epoch itself, and a leap day, since the civil-date arithmetic is hand
        // rolled rather than borrowed.
        assert_eq!(t(0), "Jan 1 00:00");
        assert_eq!(t(1_709_164_800), "Feb 29 00:00");
        assert_eq!(t(1_735_689_599), "Dec 31 23:59");
    }
}

#[cfg(test)]
mod stale_build_tests {
    use super::{IdeApp, NewWindowSpec, build_stamp, exe_mtime, newer_build_installed};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-stale-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Installing a build cannot change a running process, so "am I behind?" is a
    /// question about the file on disk versus the one this process started from.
    /// Nothing has replaced it during the test, so nothing is stale.
    #[test]
    fn a_process_running_its_own_binary_is_not_behind() {
        assert!(exe_mtime().is_some(), "no binary to compare against");
        assert!(!newer_build_installed());
    }

    /// And a window says which build it is on, which is what makes a staggered
    /// rollout legible: the ones that are behind can be told from the ones that
    /// are not.
    #[test]
    fn a_window_records_the_build_it_opened_with() {
        let app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("stamp")), ..Default::default()
        });
        assert!(
            app.output_log.iter().any(|(m, _)| m.contains(build_stamp())),
            "a window opened without saying what it is running",
        );
        // Nothing has been installed over it, so no banner and no menu note.
        assert!(!app.build_is_stale);
    }

    /// The direction that matters, which cannot be arranged by replacing the
    /// binary a test is running from: a file on disk newer than the one this
    /// process started from means this window is behind.
    #[test]
    fn a_newer_file_on_disk_means_behind() {
        use super::build_is_newer;
        use std::time::{Duration, SystemTime};
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_787_354_160);
        let later   = started + Duration::from_secs(60);

        assert!(build_is_newer(Some(started), Some(later)), "a new build went unnoticed");
        assert!(!build_is_newer(Some(started), Some(started)), "same build reported as new");
        // Rolling *back* to an older build is not "behind" either — it is what is
        // running, and nagging about it would be wrong.
        assert!(!build_is_newer(Some(later), Some(started)));
        // And an unreadable timestamp says nothing rather than nagging about an
        // update it cannot confirm.
        assert!(!build_is_newer(None, Some(later)));
        assert!(!build_is_newer(Some(started), None));
        assert!(!build_is_newer(None, None));
    }

    /// The check is throttled: it is a filesystem call, and this runs from the
    /// draw loop. A stale-build notice arriving a few seconds late costs nothing;
    /// a `stat` per frame is not free.
    #[test]
    fn the_check_does_not_run_every_frame() {
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(scratch("throttle")), ..Default::default()
        });
        app.refresh_build_staleness();
        let first = app.build_checked.expect("the first check did not happen");
        app.refresh_build_staleness();
        assert_eq!(app.build_checked, Some(first), "checked again immediately");
    }
}

#[cfg(test)]
mod reload_cost_probe {
    use super::{IdeApp, NewWindowSpec};
    use std::path::PathBuf;
    use std::time::Instant;

    /// What a soft reload actually costs, since it is the number the status line
    /// reports and the reason to prefer it over a restart.
    ///
    /// Measured on an M-series Mac, release build, a workspace with six source
    /// files reopened: 23–27ms per rebuild, 40ms for the first construction in a
    /// process (which pays for the plugin scan and the git open once).
    ///
    /// Run it in release. The same probe in a debug build reports ~465ms, which
    /// is a fact about `cargo test` and not about the reload — I quoted a figure
    /// from the wrong profile once and it was ten times out.
    ///
    /// Ignored by default: it is a measurement, not a pass/fail, and it touches
    /// the real pty host to ask which sessions exist — the same read-only call a
    /// window makes when it opens.
    ///
    ///   cargo test -p forge-ide reload_cost -- --ignored --nocapture
    #[test]
    #[ignore = "measurement"]
    fn what_a_rebuild_costs() {
        let dir = std::env::temp_dir()
            .join(format!("forge-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A few real files to reopen, of the size a source file actually is.
        let mut files: Vec<PathBuf> = Vec::new();
        for i in 0..6 {
            let p = dir.join(format!("file{i}.rs"));
            let body = (0..400).map(|n| format!("fn f{n}() {{ let x = {n}; }}\n"))
                .collect::<String>();
            std::fs::write(&p, body).unwrap();
            files.push(p);
        }
        crate::session::save(&dir, &crate::session::SessionState {
            open_files: files.clone(), active_file: 0, ..Default::default()
        });

        let time = |label: &str, spec: NewWindowSpec| {
            let t = Instant::now();
            let app = IdeApp::new_with_spec(spec);
            let took = t.elapsed();
            eprintln!("{label:<34} {:>7.1}ms   {} file(s), {} terminal(s)",
                      took.as_secs_f64() * 1000.0, app.buffers.len(), app.terminal_tabs.len());
            took
        };

        // A window with nothing to restore: the floor.
        time("empty window", NewWindowSpec {
            cwd: Some(dir.clone()), ..Default::default()
        });
        // The rebuild a reload actually performs.
        let mut worst = std::time::Duration::ZERO;
        for _ in 0..5 {
            let t = time("rebuild, 6 files restored", NewWindowSpec {
                cwd: Some(dir.clone()), is_reload: true, reload_count: 1, ..Default::default()
            });
            worst = worst.max(t);
        }
        eprintln!("\nworst of five: {:.1}ms", worst.as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod command_palette_tests {
    use super::COMMANDS;

    /// Everything on the Window menu is typeable too. A command reachable only by
    /// hunting through menus is one that has to be remembered by position.
    #[test]
    fn the_window_commands_are_in_the_palette() {
        let named = |name: &str| COMMANDS.iter().any(|(t, _, _)| *t == name);
        for name in [
            "New Window",
            "Reload Window",
            "Restart This Window",
            "Restart All Windows",
            "Collect All Windows Into One Process",
        ] {
            assert!(named(name), "{name:?} cannot be reached by typing it");
        }
    }

    /// Each entry does something distinct — the restarts differ only by scope,
    /// which is exactly the kind of thing that gets miswired by copy-paste.
    #[test]
    fn the_restarts_are_wired_to_different_things() {
        let of = |name: &str| COMMANDS.iter()
            .find(|(t, _, _)| *t == name)
            .map(|(_, _, c)| std::mem::discriminant(c))
            .unwrap();
        assert_ne!(of("Reload Window"), of("Restart This Window"));
        assert_ne!(of("Restart This Window"), of("Restart All Windows"));
        assert_ne!(of("Restart All Windows"), of("Collect All Windows Into One Process"));
    }

    /// A shortcut listed in the palette has to be the one that actually works;
    /// the only one here is the reload's.
    #[test]
    fn the_listed_shortcut_matches_the_binding() {
        let (_, keys, _) = COMMANDS.iter().find(|(t, _, _)| *t == "Reload Window").unwrap();
        assert_eq!(*keys, "Ctrl+Shift+R");
        // And the ones with no chord say nothing rather than inventing one.
        for name in ["Restart This Window", "Restart All Windows"] {
            let (_, keys, _) = COMMANDS.iter().find(|(t, _, _)| *t == name).unwrap();
            assert!(keys.is_empty(), "{name} claims a shortcut it does not have: {keys}");
        }
    }
}

#[cfg(test)]
mod live_remote_record_tests {
    use super::{IdeApp, NewWindowSpec};

    /// The recording path, against a real connection.
    ///
    /// Twice now I have said a restart "should" reconnect and been wrong, both
    /// times because something upstream never wrote the connection down. This
    /// asserts the write actually happens, end to end, rather than reasoning
    /// about it: connect to a host from the SSH config, then ask the window what
    /// it would record.
    ///
    /// Ignored by default — it needs the network and that host:
    ///   cargo test -p forge-ide live_remote_record -- --ignored --nocapture
    #[test]
    #[ignore = "needs a reachable host from ~/.ssh/config"]
    fn a_connected_window_records_its_connection() {
        // Only the hosts this is meant for. Connecting is not a read-only act —
        // it uploads `forge-server` when the remote's copy is out of date — so a
        // test must not go wandering through whatever is in the SSH config
        // looking for something that answers.
        let hosts = crate::ssh::load_hosts();
        let answers = |h: &crate::ssh::SshHost| {
            std::net::ToSocketAddrs::to_socket_addrs(&(h.host.as_str(), h.port))
                .ok()
                .and_then(|mut a| a.next())
                .map(|addr| {
                    std::net::TcpStream::connect_timeout(
                        &addr, std::time::Duration::from_millis(1500),
                    ).is_ok()
                })
                .unwrap_or(false)
        };
        // Case-insensitively, and matching anywhere in the name: the entry that
        // actually answers is `Admin-1-Tailscale`, and a case-sensitive
        // `starts_with` skipped it — so this reported "not answering" about a
        // host that was up the whole time.
        let Some(host) = hosts.iter()
            .filter(|h| h.name.to_ascii_lowercase().contains("admin-1"))
            .find(|h| answers(h))
            .cloned()
        else {
            eprintln!("admin-1 is not answering on 22; nothing to test against");
            return;
        };
        eprintln!("testing against {} ({})", host.name, host.host);

        let dir = std::env::temp_dir().join(format!("forge-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut app = IdeApp::new_with_spec(NewWindowSpec {
            cwd: Some(dir), ..Default::default()
        });

        // Nothing to record before connecting, which is the control.
        assert!(app.remote_identity().is_none());

        app.ssh_form = host.clone();
        app.ssh_connect();
        let rx = app.ssh_connect_rx.take().expect("no connect in flight");
        let ready = match rx.recv_timeout(std::time::Duration::from_secs(45)) {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => panic!("could not connect to {}: {e}", host.host),
            Err(_) => panic!("connecting to {} timed out", host.host),
        };
        app.ssh = Some(ready.conn);

        let (recorded, dir_on_remote) =
            app.remote_identity().expect("a connected window recorded nothing");
        // And the record this window actually produces — the link the simulation
        // below used to have to assume, built by the same function the running
        // app builds it with.
        let record = crate::record_for(&app, None, false);
        let carried = record.remote_host.as_ref()
            .expect("a connected window produced a record with no connection in it");
        assert_eq!(carried.host, host.host);
        assert_eq!(record.remote_dir.as_deref(), Some(dir_on_remote.as_str()));
        eprintln!("record built from a live window carries {}", carried.host);

        // The rest of the chain, from here to a window that will connect again.
        crate::app::simulate_restart(&recorded, &dir_on_remote, app.window_id);
        eprintln!("recorded: name={:?} host={:?} user={:?} dir={dir_on_remote:?}",
                  recorded.name, recorded.host, recorded.user);
        assert_eq!(recorded.host, host.host);
        assert_eq!(recorded.user, host.user);
        // And enough to connect again without the config, which is the point of
        // recording the connection rather than a name to look up.
        assert!(!recorded.host.is_empty());
        assert!(!dir_on_remote.is_empty());
    }
}

#[cfg(test)]
mod host_badge_tests {
    use super::{HOST_BADGE_CHARS, elide_chars, ssh_badge_label};

    /// The reported symptom: `Admin-1-Tailscale` in a badge that could not hold
    /// it, spilling off the left edge of the window and over the label to its
    /// right. The badge sizes itself now, and the name is bounded so "sizes
    /// itself" cannot mean "grows without end".
    #[test]
    fn a_long_host_name_is_cut_with_an_ellipsis() {
        let label = ssh_badge_label("Admin-1-Tailscale");
        assert!(label.ends_with('…'), "not cut: {label:?}");
        // The power symbol, a space, the cap, and the ellipsis.
        assert_eq!(label.chars().count(), HOST_BADGE_CHARS + 3);
        assert!(label.starts_with("⏻ Admin-1-Tail"), "{label:?}");
    }

    /// A name that fits is left exactly as it is — no ellipsis on names that
    /// never needed one.
    #[test]
    fn a_short_host_name_is_untouched() {
        assert_eq!(ssh_badge_label("admin-1"), "⏻ admin-1");
        assert_eq!(ssh_badge_label(""), "⏻ ");
    }

    /// Exactly at the limit is not "over" it.
    #[test]
    fn the_limit_is_inclusive() {
        let exact: String = "a".repeat(HOST_BADGE_CHARS);
        assert_eq!(elide_chars(&exact, HOST_BADGE_CHARS), exact);
        let one_more: String = "a".repeat(HOST_BADGE_CHARS + 1);
        assert!(elide_chars(&one_more, HOST_BADGE_CHARS).ends_with('…'));
    }

    /// Counted in characters, not bytes. A host can be called anything, and
    /// slicing a name mid-character is a panic rather than a cosmetic bug.
    /// Written as escapes rather than the characters themselves: the
    /// glyph-coverage test scans this file's literals for anything the loaded
    /// fonts cannot draw, and these are inputs to a test rather than text the UI
    /// draws. Spelled out, they are a Japanese host name and three crabs.
    #[test]
    fn a_multibyte_name_is_not_sliced_apart() {
        let name = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30db}\u{30b9}\u{30c8}\u{540d}";
        let out = elide_chars(name, 5);
        assert_eq!(out.chars().count(), 6, "{out:?}");
        assert!(out.starts_with("\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30db}"), "{out:?}");
        // And an emoji: more than one byte, and exactly one `char`.
        let crabs = "\u{1f980}\u{1f980}\u{1f980}";
        assert_eq!(elide_chars(crabs, 2).chars().count(), 3);
    }
}

/// Walk a connected window's details through the restart chain and check the
/// window that comes out will connect again.
///
/// Called by the live test. Written as a function on this side so it can also be
/// called with a hand-made host, which is how the chain is covered when there is
/// no machine to connect to.
#[cfg(test)]
pub(crate) fn simulate_restart(
    host: &crate::ssh::SshHost,
    dir_on_remote: &str,
    window_id: u64,
) {
    // 1. What `window_records()` builds for a connected window.
    let record = crate::session::WindowRecord {
        cwd: None, // the ordinary case: a remote session started from a folderless window
        id: window_id,
        remote_host: Some(host.clone()),
        remote_dir: Some(dir_on_remote.to_string()),
        ..Default::default()
    };

    // 2. What the parent spawns, and what the child makes of it.
    let spec = crate::plan_one_restarted_window(window_id, vec![record]);
    let carried = spec.ssh_host.clone().expect(
        "the restarted window has no connection to remake — the chain breaks here",
    );
    assert_eq!(carried.host, host.host, "reconnecting to the wrong host");
    assert_eq!(carried.remote_dir, dir_on_remote, "lost the folder on the remote");

    // 3. And the window that spec produces really does intend to connect.
    let app = IdeApp::new_with_spec(spec);
    let pending = app.pending_ssh_connect.as_ref().expect(
        "the window was built without a pending connection",
    );
    assert_eq!(pending.host, host.host);
    eprintln!("chain ok: record -> argv -> spec -> window pending connect to {}", pending.host);
}

#[cfg(test)]
mod build_time_tests {
    use super::{format_build_time, parse_utc_offset, utc_offset_secs};

    /// The offset `date +%z` prints, in the forms it prints it in.
    #[test]
    fn an_offset_parses() {
        assert_eq!(parse_utc_offset("-0400"), -4 * 3600);
        assert_eq!(parse_utc_offset("+0000"), 0);
        assert_eq!(parse_utc_offset("+0530"), 5 * 3600 + 30 * 60);
        assert_eq!(parse_utc_offset("-0930"), -(9 * 3600 + 30 * 60));
    }

    /// Anything unexpected reads as UTC rather than as a wrong local time: a
    /// build time nobody can compare against beats one that looks comparable and
    /// is hours out, which is what this whole fix is about.
    #[test]
    fn nonsense_reads_as_utc() {
        for bad in ["", "0400", "-04", "-04xx", "z", "--0400"] {
            assert_eq!(parse_utc_offset(bad), 0, "{bad:?} was not treated as UTC");
        }
    }

    /// The offset is actually applied. Rendering a known instant in UTC gave
    /// 17:13 for a binary built at 13:13 in New York — it looked like a local
    /// time and was four hours ahead, so a window on an old build could read as
    /// newer than the one on disk.
    #[test]
    fn the_rendered_time_follows_this_machines_clock() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_787_354_160);
        let rendered = format_build_time(t);

        // Whatever this machine's zone is, the rendering has to match it — worked
        // out here the same way, from the same offset, rather than hard-coding a
        // timezone the test machine may not be in.
        let shifted = 1_787_354_160_i64 + utc_offset_secs();
        let hh = (shifted % 86_400) / 3600;
        let mm = (shifted % 3600) / 60;
        assert!(
            rendered.ends_with(&format!("{hh:02}:{mm:02}")),
            "{rendered} does not end in this machine's local time {hh:02}:{mm:02}",
        );
    }
}

#[cfg(test)]
mod embedded_font_attribution {
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    /// egui's default font set is compiled into this binary, so four
    /// third-party fonts ship inside `forge-ide` whether or not they are the
    /// fonts anyone sees — they stay as the fallback for characters the system
    /// fonts do not cover. Three of the four licences (OFL-1.1, UFL-1.0, and
    /// Bitstream Vera by way of Hack) condition redistribution on the notice
    /// travelling with the font, so NOTICE has to name them and the texts have
    /// to be in the tree.
    ///
    /// This is a test rather than a comment because the obligation follows the
    /// dependency: turning off egui's `default_fonts`, or an upgrade changing
    /// the set, changes what has to be attributed, and nothing else in the
    /// build would say so.
    #[test]
    fn every_embedded_font_is_attributed() {
        let root = repo_root();
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("Cargo.lock");
        if !lock.contains("name = \"epaint_default_fonts\"") {
            // The fonts are no longer embedded. Then NOTICE may say so, and
            // this test has nothing to hold — but say why out loud.
            eprintln!("epaint_default_fonts is gone from the graph; font attribution is moot");
            return;
        }

        let notice = std::fs::read_to_string(root.join("NOTICE")).expect("NOTICE");
        for font in ["Hack", "Noto Emoji", "Ubuntu", "emoji-icon-font"] {
            assert!(notice.contains(font), "NOTICE does not name {font}");
        }
        // Hack carries Bitstream Vera, with reserved font names, and that
        // condition is the one most easily dropped when tidying prose.
        assert!(notice.contains("Bitstream"), "NOTICE drops the Bitstream Vera attribution");

        for licence in ["OFL-1.1.txt", "UFL-1.0.txt", "Hack.txt", "emoji-icon-font-MIT.txt"] {
            let path = root.join("licenses").join(licence);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("licenses/{licence} is required and unreadable: {e}"));
            assert!(text.len() > 500, "licenses/{licence} looks truncated ({} bytes)", text.len());
        }
    }

    /// The sentence this replaced said the bundle "contains only Vulkgryph
    /// LLC's own executable and generated resources", which was false the day
    /// it was written — the fonts were already in the binary. Kept as a test so
    /// a future tidy-up cannot reintroduce the claim.
    #[test]
    fn notice_does_not_claim_there_are_no_third_party_binaries() {
        let notice = std::fs::read_to_string(repo_root().join("NOTICE")).expect("NOTICE");
        assert!(!notice.contains("distributes no third-party binaries"),
            "NOTICE claims no third-party binaries ship; four fonts are linked into forge-ide");
    }
}

#[cfg(test)]
mod strip_width_tests {
    /// The thresholds in `strip_detail` were eyeballed once and the row still
    /// wrapped at the width the panel actually opens at, so they are measured
    /// here against real font layout rather than trusted.
    ///
    /// Returns the width the four items worth having while working occupy at a
    /// given detail level: model, permissions, reasoning, context usage — plus
    /// the `⋯` that holds the rest.
    fn row_width(ctx: &egui::Context, detail: super::StripDetail, model: &str) -> f32 {
        let text_w = |s: &str| {
            ctx.fonts(|f| f.layout_no_wrap(
                s.to_string(), egui::FontId::proportional(10.5), egui::Color32::WHITE).size().x)
        };
        // A badge, plus the 4pt egui puts between two items in a row.
        let badge = |s: &str| text_w(s) + super::status_badge_chrome(detail.triangles) + 4.0;
        let perm = if detail.compact { "Skip All" } else { "Dangerously skip all" };
        let reasoning = if detail.compact { "high" } else { "Reasoning: high" };
        let ctx_pct = if detail.compact { "· 100%" } else { "· 100% ctx" };
        let model = super::model_badge_label(model, detail);
        let mut w = 4.0                       // the leading add_space
            + badge(&model) + badge(perm) + badge(reasoning)
            + text_w(ctx_pct) + 4.0
            + badge("\u{22ef}");
        if detail.separators { w += 3.0 * (text_w("\u{b7}") + 4.0); }
        w
    }

    /// The panel opens at 360pt and its frame takes some of that. The row has
    /// to fit in what is left, at the detail level `strip_detail` picks for it —
    /// this is exactly the case the user saw wrap.
    #[test]
    fn the_row_fits_the_width_the_panel_opens_at() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let avail = 360.0 - 16.0;
        let w = row_width(&ctx, super::strip_detail(avail), "claude-opus-4-6");
        assert!(w <= avail, "{w:.0}pt of badges in {avail:.0}pt of panel — it wraps");
    }

    /// And at the narrowest the splitter allows, which is the real floor.
    #[test]
    fn the_row_fits_the_narrowest_panel_allowed() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let avail = 240.0 - 16.0;
        let w = row_width(&ctx, super::strip_detail(avail), "claude-opus-4-6");
        assert!(w <= avail, "{w:.0}pt of badges in {avail:.0}pt of panel — it wraps");
    }

    /// Every width the splitter allows, not just the interesting ones: a
    /// threshold set a few points off leaves a band that still wraps, and a
    /// band is exactly what the eye finds and a spot-check misses.
    #[test]
    fn no_width_the_splitter_allows_wraps_the_row() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        for model in ["claude-opus-4-6", "claude-opus-4-6-20260501", "qwen3-coder-30b-a3b"] {
            let mut panel = 240.0_f32;
            while panel <= 800.0 {
                let avail = panel - 16.0;
                let d = super::strip_detail(avail);
                let w = row_width(&ctx, d, model);
                assert!(w <= avail,
                    "{model} at {panel:.0}pt: {w:.0}pt of badges in {avail:.0}pt ({d:?})");
                panel += 2.0;
            }
        }
    }

    /// Full labels are only claimed to fit where `strip_detail` still asks for
    /// them; if that boundary is wrong, the wide case wraps instead.
    #[test]
    fn full_labels_fit_where_they_are_still_asked_for() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        for avail in [430.0_f32, 470.0, 520.0, 700.0] {
            let d = super::strip_detail(avail);
            let w = row_width(&ctx, d, "claude-opus-4-6");
            assert!(w <= avail, "{w:.0}pt at {avail:.0}pt ({d:?}) — it wraps");
        }
    }
}

#[cfg(test)]
mod strip_detail_tests {
    use super::strip_detail;

    /// Wide, the labels are the full ones and the separators are drawn.
    #[test]
    fn a_wide_panel_shows_everything_in_full() {
        let d = strip_detail(700.0);
        assert!(d.separators && !d.compact && d.triangles);
    }

    /// The first thing given up is decoration. The `·` separators are their own
    /// widgets, so a wrap could leave one at the start of a row — which is what
    /// made a narrow panel look broken rather than merely full.
    #[test]
    fn decoration_goes_before_words() {
        let d = strip_detail(560.0);
        assert!(!d.separators, "kept the separators past the point they fit");
        assert!(!d.compact, "shortened the labels while decoration was still there to lose");
    }

    /// Then the labels shorten. Nothing is dropped: the four things worth having
    /// while working — model, permissions, reasoning, context usage — all stay,
    /// which is the point of the `⋯` menu taking the rest.
    #[test]
    fn the_labels_shorten_rather_than_the_row_losing_items() {
        assert!(strip_detail(500.0).compact);
        assert!(strip_detail(380.0).compact);
        assert!(strip_detail(300.0).compact);
    }

    /// Monotonic: getting wider never takes something away, and never lengthens
    /// a label that had already shortened. A threshold written the wrong way
    /// round would flicker while the splitter moves.
    #[test]
    fn widening_never_takes_anything_back() {
        let mut w = 100.0_f32;
        let mut prev = strip_detail(w);
        while w < 900.0 {
            w += 5.0;
            let now = strip_detail(w);
            assert!(!(prev.separators && !now.separators), "separators vanished at {w}pt");
            assert!(!(!prev.compact && now.compact), "labels re-shortened at {w}pt");
            assert!(!(prev.triangles && !now.triangles), "triangles vanished at {w}pt");
            assert!(now.model_chars >= prev.model_chars, "model name shrank at {w}pt");
            prev = now;
        }
    }

    /// The two states do not contradict: separators are decoration around full
    /// labels, so a strip should never be drawing them while also cutting words
    /// to fit.
    #[test]
    fn separators_and_short_labels_never_happen_together() {
        let mut w = 100.0_f32;
        while w < 900.0 {
            let d = strip_detail(w);
            assert!(!(d.separators && d.compact), "at {w}pt: decoration beside cut labels");
            w += 5.0;
        }
    }
}
