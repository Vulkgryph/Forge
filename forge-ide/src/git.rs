//! Git status integration via libgit2 (vendored).
//!
//! Phase 1 scope: discover the repo, read per-file working-tree status, look
//! up the current branch name and ahead/behind counts against upstream.  No
//! network, no commit/push/pull yet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use git2::{BranchType, Repository, StatusOptions};

/// Cap on retained status entries, and the budget for the probe walk that
/// decides whether a fully-recursive untracked scan is affordable.
///
/// A repo whose working tree is enormous and mostly untracked — a `git init`
/// in `$HOME` with no `.gitignore` is the case that motivated this — has over
/// a million untracked files. Recursing them costs tens of seconds inside
/// libgit2 plus a `PathBuf` per file and one per ancestor directory. None of
/// that is useful to render, so it is bounded rather than merely deferred.
const MAX_STATUS_ENTRIES: usize = 20_000;

/// Depth cap for the probe walk. Anything below this is left uncounted and so
/// assumed to fit — a tree both deeper than this and enormous would still get
/// the recursive scan. That is survivable because the scan is off-thread and
/// `MAX_STATUS_ENTRIES` still bounds what is retained; the property that keeps
/// the probe itself terminating is not following symlinks, not this cap.
const MAX_PROBE_DEPTH: usize = 24;

/// Result of one background status scan, applied wholesale by `poll`.
struct StatusScan {
    statuses:   HashMap<PathBuf, FileStatus>,
    dirty_dirs: HashSet<PathBuf>,
    staged:     Vec<(PathBuf, FileStatus)>,
    unstaged:   Vec<(PathBuf, FileStatus)>,
    truncated:  bool,
}

/// What an individual file's state is, condensed into a single category for
/// rendering. We intentionally collapse "staged + worktree" overlaps; the
/// Source Control panel (later) will surface the per-side detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Untracked,
    Renamed,
    Conflicted,
}

impl FileStatus {
    /// Tint used to color the filename in the file tree.
    pub fn color(self) -> egui::Color32 {
        match self {
            // VS Code-ish: orange for modified, green for new, red for deleted.
            FileStatus::Modified   => egui::Color32::from_rgb(225, 175,  90),
            FileStatus::Added      => egui::Color32::from_rgb(129, 184, 117),
            FileStatus::Untracked  => egui::Color32::from_rgb(129, 184, 117),
            FileStatus::Deleted    => egui::Color32::from_rgb(220, 120, 120),
            FileStatus::Renamed    => egui::Color32::from_rgb(180, 140, 210),
            FileStatus::Conflicted => egui::Color32::from_rgb(255,  90,  90),
        }
    }

    /// One-letter status badge shown after the filename ("M", "U", etc.).
    pub fn letter(self) -> &'static str {
        match self {
            FileStatus::Modified   => "M",
            FileStatus::Added      => "A",
            FileStatus::Deleted    => "D",
            FileStatus::Untracked  => "U",
            FileStatus::Renamed    => "R",
            FileStatus::Conflicted => "!",
        }
    }
}

/// One rendered line of a unified diff (for the diff view).
#[derive(Clone, Debug)]
pub enum DiffRow {
    /// Hunk header, e.g. "@@ -1,4 +1,6 @@".
    Hunk(String),
    /// Unchanged context line, present on both sides.
    Ctx { old: u32, new: u32, text: String },
    /// Line added on the new side.
    Add { new: u32, text: String },
    /// Line removed from the old side.
    Del { old: u32, text: String },
}

/// A network operation against the default remote ("origin").
#[derive(Clone, Copy, Debug)]
pub enum RemoteOp { Fetch, Pull, Push }

/// Per-line change marker for the editor's line-number gutter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterMark {
    Added,    // line is new since HEAD
    Modified, // line changed since HEAD
    Deleted,  // one or more lines were removed just above this line
}

pub struct GitState {
    repo:           Repository,
    workdir:        PathBuf,
    pub branch:     String,
    pub ahead:      usize,
    pub behind:     usize,
    /// Whether an "origin" remote is configured (gates fetch/pull/push UI).
    pub has_origin: bool,
    /// Absolute paths → status (collapsed across staged + worktree).  Used by
    /// the file tree to color filenames.
    statuses:       HashMap<PathBuf, FileStatus>,
    /// Directory paths that contain at least one modified file underneath.
    /// Computed once per refresh so the file tree can highlight folders.
    dirty_dirs:     std::collections::HashSet<PathBuf>,
    /// Source-control panel data: staged + unstaged files, with the absolute
    /// path so opening + per-side actions are unambiguous.
    pub staged:     Vec<(PathBuf, FileStatus)>,
    pub unstaged:   Vec<(PathBuf, FileStatus)>,
    /// `Some` while a background status scan is in flight.
    scan_rx:        Option<mpsc::Receiver<StatusScan>>,
    /// Set when the last scan collapsed untracked directories, or hit
    /// `MAX_STATUS_ENTRIES`, instead of reporting every file.
    pub truncated:  bool,
}

impl GitState {
    /// Try to discover a git repo at or above `path`. Returns None if there's
    /// no repo (working in a non-git folder is a valid mode).
    pub fn open(path: &Path) -> Option<Self> {
        let repo = Repository::discover(path).ok()?;
        let workdir = repo.workdir()?.to_path_buf();
        let mut me = Self {
            repo, workdir,
            branch:     String::new(),
            ahead:      0,
            behind:     0,
            has_origin: false,
            statuses:   HashMap::new(),
            dirty_dirs: HashSet::new(),
            staged:     Vec::new(),
            unstaged:   Vec::new(),
            scan_rx:    None,
            truncated:  false,
        };
        me.refresh();
        Some(me)
    }

    /// Re-read branch info, and kick off a per-file status scan in the
    /// background.
    ///
    /// The status walk used to run inline here, which meant every caller —
    /// including `open`, itself called during window creation on the event
    /// loop thread — blocked on it. In a large working tree that is tens of
    /// seconds with no frames rendered, i.e. an apparently hung window.
    /// Everything below is metadata-only and cheap; the walk is now
    /// `start_status_scan` plus `poll`.
    pub fn refresh(&mut self) {
        self.refresh_meta();
        self.start_status_scan();
    }

    fn refresh_meta(&mut self) {
        // ── Branch name ──
        self.branch = self.repo.head().ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_else(|| "HEAD".into());

        // ── Remote presence ──
        self.has_origin = self.repo.find_remote("origin").is_ok();

        // ── Ahead/behind vs upstream ──
        self.ahead = 0;
        self.behind = 0;
        if let Ok(head) = self.repo.head() {
            let shorthand = head.shorthand().unwrap_or("").to_string();
            if let Some(local_oid) = head.target() {
                if let Ok(local_branch) = self.repo.find_branch(&shorthand, BranchType::Local) {
                    if let Ok(upstream) = local_branch.upstream() {
                        if let Some(up_oid) = upstream.get().target() {
                            if let Ok((a, b)) = self.repo.graph_ahead_behind(local_oid, up_oid) {
                                self.ahead = a;
                                self.behind = b;
                            }
                        }
                    }
                }
            }
        }

    }

    /// Spawn the working-tree status walk. Any scan already running is
    /// abandoned — its receiver is dropped, so its send fails harmlessly and
    /// a stale result can never overwrite a newer one.
    fn start_status_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let workdir = self.workdir.clone();
        std::thread::spawn(move || {
            if let Some(scan) = scan_status(&workdir) {
                let _ = tx.send(scan);
                crate::wake::wake();
            }
        });
    }

    /// True while a scan is in flight, so the UI can keep repainting until
    /// decorations arrive.
    pub fn scanning(&self) -> bool { self.scan_rx.is_some() }

    /// Apply a finished scan. Returns true when new data landed, so callers
    /// can invalidate anything derived from the old statuses.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.scan_rx else { return false };
        match rx.try_recv() {
            Ok(scan) => {
                self.statuses   = scan.statuses;
                self.dirty_dirs = scan.dirty_dirs;
                self.staged     = scan.staged;
                self.unstaged   = scan.unstaged;
                self.truncated  = scan.truncated;
                self.scan_rx    = None;
                true
            }
            Err(mpsc::TryRecvError::Empty)        => false,
            Err(mpsc::TryRecvError::Disconnected) => { self.scan_rx = None; false }
        }
    }

    /// Workspace root (for displaying paths relative to it).
    pub fn workdir(&self) -> &Path { &self.workdir }

    /// Stage a single file path. Handles add (modified/new files) and remove
    /// (deleted-from-disk files) cases.
    pub fn stage(&mut self, abs_path: &Path) -> Result<(), String> {
        let rel = abs_path.strip_prefix(&self.workdir)
            .map_err(|e| format!("path outside workdir: {e}"))?;
        let mut index = self.repo.index().map_err(|e| e.to_string())?;
        if abs_path.exists() {
            index.add_path(rel).map_err(|e| e.to_string())?;
        } else {
            // File deleted on disk — remove from index too.
            index.remove_path(rel).map_err(|e| e.to_string())?;
        }
        index.write().map_err(|e| e.to_string())?;
        self.refresh();
        Ok(())
    }

    /// Unstage a single file path (reset its index entry to HEAD).
    pub fn unstage(&mut self, abs_path: &Path) -> Result<(), String> {
        let rel = abs_path.strip_prefix(&self.workdir)
            .map_err(|e| format!("path outside workdir: {e}"))?;
        match self.repo.head().ok().and_then(|h| h.target()) {
            Some(head_oid) => {
                let head_obj = self.repo.find_object(head_oid, None)
                    .map_err(|e| e.to_string())?;
                self.repo.reset_default(Some(&head_obj), [rel])
                    .map_err(|e| e.to_string())?;
            }
            None => {
                // No HEAD yet (first commit hasn't been made) — drop from index.
                let mut index = self.repo.index().map_err(|e| e.to_string())?;
                index.remove_path(rel).map_err(|e| e.to_string())?;
                index.write().map_err(|e| e.to_string())?;
            }
        }
        self.refresh();
        Ok(())
    }

    /// Commit the current index with `message`.  Uses the signature from
    /// `git config user.name / user.email`.
    pub fn commit(&mut self, message: &str) -> Result<(), String> {
        if message.trim().is_empty() {
            return Err("Empty commit message".into());
        }
        // Scope the immutable borrows of `self.repo` so they drop before we
        // call `self.refresh()` (which takes `&mut self`).
        {
            let sig = self.repo.signature()
                .map_err(|e| format!("missing git identity (set user.name / user.email): {e}"))?;
            let mut index = self.repo.index().map_err(|e| e.to_string())?;
            let tree_oid = index.write_tree().map_err(|e| e.to_string())?;
            let tree = self.repo.find_tree(tree_oid).map_err(|e| e.to_string())?;

            let parent = self.repo.head().ok()
                .and_then(|h| h.target())
                .and_then(|oid| self.repo.find_commit(oid).ok());
            let parents: Vec<&git2::Commit> = parent.iter().collect();

            self.repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
                .map_err(|e| e.to_string())?;
        }
        self.refresh();
        Ok(())
    }

    /// Configure the "origin" remote to `url` (used when a repo has no remote
    /// yet).  Creates the remote with the default fetch refspec so subsequent
    /// fetch/pull/push work.
    pub fn set_origin(&mut self, url: &str) -> Result<(), String> {
        self.repo.remote("origin", url).map_err(|e| e.to_string())?;
        self.refresh();
        Ok(())
    }

    /// Returns the working-tree status of an individual file, if non-clean.
    pub fn status_for(&self, path: &Path) -> Option<FileStatus> {
        self.statuses.get(path).copied()
    }

    /// Returns true if any file under this directory has a non-clean status.
    /// O(1) — uses a precomputed set populated by `refresh`.
    pub fn folder_has_changes(&self, dir: &Path) -> bool {
        self.dirty_dirs.contains(dir)
    }

    /// Total number of changed files (used by the status bar / SC panel).
    pub fn change_count(&self) -> usize {
        self.statuses.len()
    }

    /// The contents of `rel` (workdir-relative path) as committed at HEAD, or
    /// None if the file isn't tracked yet (new/untracked file).
    fn head_blob(&self, rel: &Path) -> Option<Vec<u8>> {
        let tree = self.repo.head().ok()?.peel_to_tree().ok()?;
        let entry = tree.get_path(rel).ok()?;
        let blob = entry.to_object(&self.repo).ok()?.into_blob().ok()?;
        Some(blob.content().to_vec())
    }

    /// The HEAD and working-tree byte contents of a file, plus its repo-relative
    /// path.  The caller owns the buffers, which a `git2::Patch` must borrow.
    fn head_and_work(&self, abs_path: &Path)
        -> Result<(PathBuf, Vec<u8>, Vec<u8>), String>
    {
        let rel = abs_path.strip_prefix(&self.workdir)
            .map_err(|e| format!("path outside workdir: {e}"))?
            .to_path_buf();
        let old = self.head_blob(&rel).unwrap_or_default();
        // Deleted-on-disk files read as empty (all removals).
        let new = std::fs::read(abs_path).unwrap_or_default();
        Ok((rel, old, new))
    }

    /// Unified diff of HEAD → working tree for one file, as renderable rows.
    /// Empty when there's no textual diff (identical, or binary).
    pub fn file_diff(&self, abs_path: &Path) -> Result<Vec<DiffRow>, String> {
        let (rel, old, new) = self.head_and_work(abs_path)?;
        let patch = git2::Patch::from_buffers(
            &old, Some(&rel), &new, Some(&rel), None)
            .map_err(|e| e.to_string())?;
        let mut rows = Vec::new();
        let n_hunks = patch.num_hunks();
        for h in 0..n_hunks {
            if let Ok((hunk, _)) = patch.hunk(h) {
                let header = String::from_utf8_lossy(hunk.header())
                    .trim_end().to_string();
                rows.push(DiffRow::Hunk(header));
            }
            let n_lines = patch.num_lines_in_hunk(h).unwrap_or(0);
            for l in 0..n_lines {
                let Ok(line) = patch.line_in_hunk(h, l) else { continue };
                let text = String::from_utf8_lossy(line.content())
                    .trim_end_matches(['\n', '\r']).to_string();
                match line.origin() {
                    '+' => rows.push(DiffRow::Add {
                        new: line.new_lineno().unwrap_or(0), text }),
                    '-' => rows.push(DiffRow::Del {
                        old: line.old_lineno().unwrap_or(0), text }),
                    _   => rows.push(DiffRow::Ctx {
                        old: line.old_lineno().unwrap_or(0),
                        new: line.new_lineno().unwrap_or(0), text }),
                }
            }
        }
        Ok(rows)
    }

    /// Per-line gutter markers (Added / Modified / Deleted) for the working-tree
    /// version of a file, keyed by 0-based line index.  Used to paint the diff
    /// bars in the editor gutter.
    pub fn gutter_marks(&self, abs_path: &Path)
        -> HashMap<usize, GutterMark>
    {
        let mut marks = HashMap::new();
        let Ok((rel, old, new)) = self.head_and_work(abs_path) else { return marks };
        let Ok(patch) = git2::Patch::from_buffers(
            &old, Some(&rel), &new, Some(&rel), None) else { return marks };
        let n_hunks = patch.num_hunks();
        for h in 0..n_hunks {
            // Collect the added/removed runs within this hunk, then classify
            // each contiguous change block the way VS Code does: overlapping
            // add+del = modified, pure add = added, pure del = a delete mark.
            let n_lines = patch.num_lines_in_hunk(h).unwrap_or(0);
            let mut adds: Vec<u32> = Vec::new(); // new line numbers
            let mut dels: usize = 0;
            // The new-side line a deletion sits against (next context/add line).
            let mut del_anchor: Option<u32> = None;
            let flush = |marks: &mut HashMap<usize, GutterMark>,
                         adds: &mut Vec<u32>, dels: &mut usize,
                         del_anchor: &mut Option<u32>| {
                for (i, &new_no) in adds.iter().enumerate() {
                    let idx = new_no.saturating_sub(1) as usize;
                    let mark = if i < *dels { GutterMark::Modified }
                               else         { GutterMark::Added };
                    marks.insert(idx, mark);
                }
                if *dels > adds.len() {
                    // Net deletion — mark the line the removed text sat above.
                    if let Some(anchor) = *del_anchor {
                        let idx = anchor.saturating_sub(1) as usize;
                        marks.entry(idx).or_insert(GutterMark::Deleted);
                    }
                }
                adds.clear();
                *dels = 0;
                *del_anchor = None;
            };
            for l in 0..n_lines {
                let Ok(line) = patch.line_in_hunk(h, l) else { continue };
                match line.origin() {
                    '+' => adds.push(line.new_lineno().unwrap_or(0)),
                    '-' => dels += 1,
                    _   => {
                        // Context line ends the current change block.
                        if del_anchor.is_none() {
                            del_anchor = line.new_lineno();
                        }
                        flush(&mut marks, &mut adds, &mut dels, &mut del_anchor);
                        del_anchor = None;
                    }
                }
            }
            flush(&mut marks, &mut adds, &mut dels, &mut del_anchor);
        }
        marks
    }
}

// ── Background status scanning ────────────────────────────────────────────────

fn base_status_opts() -> StatusOptions {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true)
        .exclude_submodules(true);
    opts
}

/// Walk the working tree and categorize every changed path.
///
/// Runs on a background thread, and opens its own `Repository` handle rather
/// than sharing the UI's — which keeps `GitState` free of synchronization.
fn scan_status(workdir: &Path) -> Option<StatusScan> {
    let repo = Repository::open(workdir).ok()?;

    // Pass 1 collapses untracked directories into one entry each, the way
    // plain `git status` does. This is the only variant that is cheap
    // unconditionally: in a `$HOME` repo with no `.gitignore` it is ~60ms,
    // where the recursive form is ~30s.
    let mut opts = base_status_opts();
    opts.recurse_untracked_dirs(false);
    let collapsed = repo.statuses(Some(&mut opts)).ok()?;

    // Recursing is what the file tree wants — individual files inside a newly
    // created folder get decorated — so do it whenever it is affordable.
    // libgit2 exposes no way to abort a status walk part-way, so affordability
    // has to be decided up front, by a probe walk that *can* stop early.
    let untracked_dirs: Vec<PathBuf> = collapsed.iter()
        .filter(|e| e.status().is_wt_new())
        .filter_map(|e| e.path().map(|p| workdir.join(p)))
        .filter(|p| p.is_dir())
        .collect();

    let mut truncated = false;
    let recursive = if untracked_dirs.is_empty() {
        None // nothing to expand; pass 1 is already exact
    } else if untracked_fits(&untracked_dirs, MAX_STATUS_ENTRIES) {
        opts.recurse_untracked_dirs(true);
        repo.statuses(Some(&mut opts)).ok()
    } else {
        truncated = true;
        None
    };
    let entries = recursive.as_ref().unwrap_or(&collapsed);

    let mut scan = StatusScan {
        statuses:   HashMap::new(),
        dirty_dirs: HashSet::new(),
        staged:     Vec::new(),
        unstaged:   Vec::new(),
        truncated,
    };

    for entry in entries.iter() {
        if scan.statuses.len() >= MAX_STATUS_ENTRIES {
            scan.truncated = true;
            break;
        }
        let Some(rel) = entry.path() else { continue };
        let abs = workdir.join(rel);
        let s = entry.status();

        // Per-side categorization for the SC panel.
        let staged_fs = if s.is_conflicted() {
            None // conflicts show on the unstaged side
        } else if s.is_index_renamed() {
            Some(FileStatus::Renamed)
        } else if s.is_index_new() {
            Some(FileStatus::Added)
        } else if s.is_index_deleted() {
            Some(FileStatus::Deleted)
        } else if s.is_index_modified() || s.is_index_typechange() {
            Some(FileStatus::Modified)
        } else { None };
        let unstaged_fs = if s.is_conflicted() {
            Some(FileStatus::Conflicted)
        } else if s.is_wt_renamed() {
            Some(FileStatus::Renamed)
        } else if s.is_wt_new() {
            Some(FileStatus::Untracked)
        } else if s.is_wt_deleted() {
            Some(FileStatus::Deleted)
        } else if s.is_wt_modified() || s.is_wt_typechange() {
            Some(FileStatus::Modified)
        } else { None };

        if let Some(st) = staged_fs   { scan.staged  .push((abs.clone(), st)); }
        if let Some(st) = unstaged_fs { scan.unstaged.push((abs.clone(), st)); }

        // Collapsed view for the file tree (any non-clean state).
        let fs = if s.is_conflicted() {
            FileStatus::Conflicted
        } else if s.is_index_renamed() || s.is_wt_renamed() {
            FileStatus::Renamed
        } else if s.is_index_new() {
            FileStatus::Added
        } else if s.is_wt_new() {
            FileStatus::Untracked
        } else if s.is_index_deleted() || s.is_wt_deleted() {
            FileStatus::Deleted
        } else if s.is_index_modified() || s.is_wt_modified() || s.is_wt_typechange() {
            FileStatus::Modified
        } else {
            continue;
        };
        scan.statuses.insert(abs.clone(), fs);
        // Propagate "dirty" up the directory chain so folders can show a hint
        // that something inside them has changed.
        let mut p = abs.parent();
        while let Some(dir) = p {
            if dir == workdir.parent().unwrap_or(Path::new("/")) { break; }
            scan.dirty_dirs.insert(dir.to_path_buf());
            if dir == workdir { break; }
            p = dir.parent();
        }
    }

    // Sort both lists by full path, case-insensitively — matches VSCode's
    // Source Control ordering (folder contents grouped, not git2's raw order).
    let by_path = |a: &(PathBuf, FileStatus), b: &(PathBuf, FileStatus)| {
        a.0.to_string_lossy().to_lowercase()
            .cmp(&b.0.to_string_lossy().to_lowercase())
    };
    scan.staged.sort_by(by_path);
    scan.unstaged.sort_by(by_path);
    Some(scan)
}

/// Probe: would recursing these untracked directories stay within `budget`
/// files? Stops the moment the budget is blown, which is the whole point —
/// this is the abortable stand-in for a status walk that can't be aborted.
///
/// Counts every file, including ones a nested `.gitignore` would exclude. That
/// only ever over-counts, so the answer errs toward the cheap collapsed scan.
fn untracked_fits(dirs: &[PathBuf], budget: usize) -> bool {
    let mut seen = 0usize;
    for dir in dirs {
        if !probe_walk(dir, 0, budget, &mut seen) { return false; }
    }
    true
}

/// Returns false as soon as `*seen` exceeds `budget`. Never follows symlinks —
/// `entry.file_type()` does not resolve them, so a link cycle cannot make this
/// recurse forever.
fn probe_walk(dir: &Path, depth: usize, budget: usize, seen: &mut usize) -> bool {
    if depth > MAX_PROBE_DEPTH { return true; }
    let Ok(iter) = std::fs::read_dir(dir) else { return true };
    for entry in iter.flatten() {
        if *seen > budget { return false; }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if !probe_walk(&entry.path(), depth + 1, budget, seen) { return false; }
        } else if ft.is_file() {
            *seen += 1;
        }
    }
    *seen <= budget
}

// ── Blame ───────────────────────────────────────────────────────────────

/// Per-line blame for a file, aligned to the *current* buffer text (so it stays
/// correct when the working copy has unsaved edits — those lines come back as
/// uncommitted). Indexed 0-based by line. Empty for untracked files or on error.
///
/// Only reachable through `spawn_blame`, deliberately: this walks the file's
/// history, which measured ~240ms on an 11k-line file in a five-commit repo and
/// scales with history depth. It used to run inline on every tab switch.
fn blame_with(repo: &Repository, workdir: &Path, abs_path: &Path, current: &str)
-> Vec<BlameLine>
{
    let Ok(rel) = abs_path.strip_prefix(&workdir) else { return vec![] };
    let mut opts = git2::BlameOptions::new();
    let Ok(base) = repo.blame_file(rel, Some(&mut opts)) else { return vec![] };
    // Map the committed blame onto the current buffer contents (falls back
    // to the raw file blame if the buffer remap fails).
    let buf_blame = base.blame_buffer(current.as_bytes());
    let blame: &git2::Blame = buf_blame.as_ref().unwrap_or(&base);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let n = current.lines().count().max(1);
    let mut out = Vec::with_capacity(n);
    for i in 1..=n {
        let line = match blame.get_line(i) {
            Some(h) => {
                let oid = h.final_commit_id();
                if oid.is_zero() {
                    BlameLine::uncommitted()
                } else if let Ok(commit) = repo.find_commit(oid) {
                    BlameLine {
                        short:     oid.to_string().chars().take(7).collect(),
                        author:    commit.author().name().unwrap_or("?").to_string(),
                        age:       relative_time(commit.time().seconds(), now),
                        summary:   commit.summary().unwrap_or("").to_string(),
                        committed: true,
                    }
                } else {
                    BlameLine::uncommitted()
                }
            }
            None => BlameLine::uncommitted(),
        };
        out.push(line);
    }
    out
}

/// Run a blame on a background thread. Opens its own `Repository` handle, so
/// nothing is shared with the UI's `GitState`. The path is echoed back so a
/// result that arrives after the user switched files can be discarded.
pub fn spawn_blame(workdir: PathBuf, abs_path: PathBuf, current: String)
    -> mpsc::Receiver<(PathBuf, Vec<BlameLine>)>
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let lines = match Repository::open(&workdir) {
            Ok(repo) => blame_with(&repo, &workdir, &abs_path, &current),
            Err(_)   => Vec::new(),
        };
        let _ = tx.send((abs_path, lines));
        crate::wake::wake();
    });
    rx
}

/// Blame info for a single line, ready to render in a hover tooltip.
#[derive(Clone, Debug)]
pub struct BlameLine {
    pub short:     String, // short commit hash
    pub author:    String,
    pub age:       String, // "3d ago"
    pub summary:   String, // commit subject line
    pub committed: bool,   // false = working-tree change not yet committed
}

impl BlameLine {
    fn uncommitted() -> Self {
        Self { short: String::new(), author: String::new(),
               age: String::new(), summary: String::new(), committed: false }
    }
}

/// Coarse "time ago" string from two epoch-second timestamps (no chrono dep).
fn relative_time(commit_secs: i64, now_secs: i64) -> String {
    let d = (now_secs - commit_secs).max(0);
    match d {
        _ if d < 60          => "just now".into(),
        _ if d < 3600        => format!("{}m ago", d / 60),
        _ if d < 86_400      => format!("{}h ago", d / 3600),
        _ if d < 86_400 * 30 => format!("{}d ago", d / 86_400),
        _ if d < 86_400 * 365 => format!("{}mo ago", d / (86_400 * 30)),
        _                    => format!("{}y ago", d / (86_400 * 365)),
    }
}

// ── Remote operations (run on a worker thread) ─────────────────────────────────
//
// git2's `Repository` isn't `Send`, so it can't cross a thread boundary.  The
// worker opens its OWN `Repository` from the (Send) workdir path instead.

/// Run a remote operation against "origin".  Returns a human-readable status
/// line on success.  Safe to call from a spawned thread.
pub fn run_remote_op(workdir: PathBuf, op: RemoteOp) -> Result<String, String> {
    let repo = Repository::open(&workdir).map_err(|e| e.to_string())?;
    match op {
        RemoteOp::Fetch => { fetch_origin(&repo)?; Ok("Fetched from origin".into()) }
        RemoteOp::Pull  => pull_fast_forward(&repo),
        RemoteOp::Push  => push_current_branch(&repo),
    }
}

/// Credential resolution shared by fetch and push: SSH agent first, then the
/// system git credential helper (macOS Keychain / token), then default.
fn auth_callbacks() -> git2::RemoteCallbacks<'static> {
    let mut cb = git2::RemoteCallbacks::new();
    cb.credentials(|url, username_from_url, allowed| {
        if allowed.contains(git2::CredentialType::SSH_KEY) {
            let user = username_from_url.unwrap_or("git");
            if let Ok(c) = git2::Cred::ssh_key_from_agent(user) { return Ok(c); }
        }
        if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
            if let Ok(cfg) = git2::Config::open_default() {
                if let Ok(c) = git2::Cred::credential_helper(&cfg, url, username_from_url) {
                    return Ok(c);
                }
            }
        }
        if allowed.contains(git2::CredentialType::DEFAULT) {
            return git2::Cred::default();
        }
        Err(git2::Error::from_str(
            "no usable credentials (configure an SSH agent key or a git credential helper)"))
    });
    cb
}

fn fetch_origin(repo: &Repository) -> Result<(), String> {
    let mut remote = repo.find_remote("origin")
        .map_err(|_| "no 'origin' remote configured".to_string())?;
    let mut fo = git2::FetchOptions::new();
    fo.remote_callbacks(auth_callbacks());
    // Empty refspec list → libgit2 uses the remote's configured refspecs.
    let empty: [&str; 0] = [];
    remote.fetch(&empty, Some(&mut fo), None).map_err(|e| e.to_string())?;
    Ok(())
}

fn pull_fast_forward(repo: &Repository) -> Result<String, String> {
    fetch_origin(repo)?;
    let head = repo.head().map_err(|e| e.to_string())?;
    if !head.is_branch() {
        return Err("HEAD is detached — checkout a branch to pull".into());
    }
    let refname = head.name().ok_or("invalid HEAD")?.to_string();
    let fetch_head = repo.find_reference("FETCH_HEAD")
        .map_err(|_| "nothing fetched to merge".to_string())?;
    let fetch_commit = repo.reference_to_annotated_commit(&fetch_head)
        .map_err(|e| e.to_string())?;
    let (analysis, _) = repo.merge_analysis(&[&fetch_commit])
        .map_err(|e| e.to_string())?;
    if analysis.is_up_to_date() {
        Ok("Already up to date".into())
    } else if analysis.is_fast_forward() {
        let mut reference = repo.find_reference(&refname).map_err(|e| e.to_string())?;
        reference.set_target(fetch_commit.id(), "pull: fast-forward")
            .map_err(|e| e.to_string())?;
        repo.set_head(&refname).map_err(|e| e.to_string())?;
        repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
            .map_err(|e| e.to_string())?;
        Ok("Pulled (fast-forward)".into())
    } else {
        Err("Pull needs a merge — only fast-forward is supported for now".into())
    }
}

// ── GitHub CLI integration (optional convenience layer) ────────────────────────
//
// libgit2 can talk to a remote but can't *create* a GitHub repo — that's a
// GitHub API operation.  Rather than implement OAuth ourselves, we lean on the
// `gh` CLI when it's installed: it already holds the user's token (from
// `gh auth login`), so we inherit their GitHub auth for free.

/// True only if `gh` is installed AND authenticated.
pub fn gh_ready() -> bool {
    std::process::Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a GitHub repo for `workdir`, wire up `origin`, and push.  `owner` is
/// the account/org to create under (empty = the active github.com account);
/// the host is pinned to github.com so a multi-host `gh` (e.g. a work
/// enterprise login) never grabs the wrong target.  Runs the `gh` CLI; safe to
/// call from a worker thread.
pub fn gh_publish(workdir: PathBuf, name: String, owner: String, private: bool)
    -> Result<String, String>
{
    let owner = owner.trim();
    let repo_arg = if owner.is_empty() { name.clone() }
                   else                { format!("{owner}/{name}") };
    let visibility = if private { "--private" } else { "--public" };
    let out = std::process::Command::new("gh")
        .args(["repo", "create", &repo_arg, visibility,
               "--source", ".", "--remote", "origin", "--push"])
        .env("GH_HOST", "github.com")
        .current_dir(&workdir)
        .output()
        .map_err(|e| format!("gh: {e}"))?;
    if out.status.success() {
        Ok(format!("Published “{repo_arg}” to GitHub"))
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(format!("gh: {}", err.trim()))
    }
}

fn push_current_branch(repo: &Repository) -> Result<String, String> {
    let mut remote = repo.find_remote("origin")
        .map_err(|_| "no 'origin' remote configured".to_string())?;
    let head = repo.head().map_err(|e| e.to_string())?;
    if !head.is_branch() {
        return Err("HEAD is detached — checkout a branch to push".into());
    }
    let refname = head.name().ok_or("invalid HEAD")?.to_string();
    let refspec = format!("{refname}:{refname}");
    let mut po = git2::PushOptions::new();
    po.remote_callbacks(auth_callbacks());
    remote.push(&[refspec.as_str()], Some(&mut po)).map_err(|e| e.to_string())?;
    Ok("Pushed to origin".into())
}

#[cfg(test)]
mod status_scan_tests {
    use super::{FileStatus, MAX_STATUS_ENTRIES, scan_status, untracked_fits};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-git-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `git init` with a committed baseline, so status has something to
    /// compare against.
    fn init_repo(dir: &PathBuf) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_oid).unwrap();
            let sig  = repo.signature().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        }
        repo
    }

    /// The probe is the abortable stand-in for libgit2's un-abortable status
    /// walk, so its only real job is bailing once the budget is blown.
    #[test]
    fn probe_rejects_a_tree_over_budget() {
        let root = scratch("probe-big");
        for i in 0..50 { std::fs::write(root.join(format!("f{i}")), "").unwrap(); }
        assert!(!untracked_fits(&[root.clone()], 10));
        assert!(untracked_fits(&[root.clone()], 500));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_counts_across_nested_directories() {
        let root = scratch("probe-nested");
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        for p in ["a/1", "a/b/2", "a/b/c/3"] {
            std::fs::write(root.join(p), "").unwrap();
        }
        assert!(untracked_fits(&[root.clone()], 3));
        assert!(!untracked_fits(&[root.clone()], 2));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlink cycle must not make the probe recurse forever — the same
    /// hazard the Quick Open walker had.
    #[test]
    #[cfg(unix)]
    fn probe_does_not_follow_symlinks() {
        let root = scratch("probe-cycle");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/real"), "").unwrap();
        std::os::unix::fs::symlink(&root, root.join("sub/loop")).unwrap();
        // Terminating at all is the assertion.
        assert!(untracked_fits(&[root.clone()], MAX_STATUS_ENTRIES));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A normal-sized repo takes the recursive path, so files inside a new
    /// untracked folder still get individual statuses for the file tree.
    #[test]
    fn small_repo_reports_files_inside_untracked_dirs() {
        let root = scratch("small");
        let _repo = init_repo(&root);
        std::fs::write(root.join("tracked.txt"), "one\ntwo\n").unwrap(); // modified
        std::fs::write(root.join("loose.txt"), "").unwrap();            // untracked file
        std::fs::create_dir(root.join("newdir")).unwrap();
        std::fs::write(root.join("newdir/inner.txt"), "").unwrap();     // inside untracked dir

        let scan = scan_status(&root).expect("scan");
        assert!(!scan.truncated);
        assert_eq!(scan.statuses.get(&root.join("tracked.txt")), Some(&FileStatus::Modified));
        assert_eq!(scan.statuses.get(&root.join("loose.txt")), Some(&FileStatus::Untracked));
        assert_eq!(scan.statuses.get(&root.join("newdir/inner.txt")),
                   Some(&FileStatus::Untracked),
                   "recursive pass should expand untracked directories");
        // Ancestors are marked dirty so the tree can hint at nested changes.
        assert!(scan.dirty_dirs.contains(&root.join("newdir")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no untracked directories there is nothing to expand, so pass 1 is
    /// already exact and the second libgit2 walk is skipped entirely.
    #[test]
    fn repo_without_untracked_dirs_is_exact() {
        let root = scratch("exact");
        let _repo = init_repo(&root);
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();

        let scan = scan_status(&root).expect("scan");
        assert!(!scan.truncated);
        assert_eq!(scan.statuses.len(), 1);
        assert_eq!(scan.unstaged.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Staged and worktree sides stay separated, as the SC panel renders them.
    #[test]
    fn staged_and_unstaged_sides_are_distinguished() {
        let root = scratch("sides");
        let repo = init_repo(&root);
        std::fs::write(root.join("staged.txt"), "new\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("staged.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(root.join("tracked.txt"), "edited\n").unwrap();

        let scan = scan_status(&root).expect("scan");
        assert_eq!(scan.staged, vec![(root.join("staged.txt"), FileStatus::Added)]);
        assert_eq!(scan.unstaged, vec![(root.join("tracked.txt"), FileStatus::Modified)]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Timing harness for the real `$HOME` repo — the case that froze window
/// creation. Ignored by default (machine-specific); run with:
///   cargo test -p forge-ide home_repo_scan_is_fast -- --ignored --nocapture
#[cfg(test)]
mod home_repo_bench {
    #[test]
    #[ignore]
    fn home_repo_scan_is_fast() {
        let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return };
        if git2::Repository::open(&home).is_err() {
            eprintln!("$HOME is not a git repo; nothing to measure");
            return;
        }
        let t = std::time::Instant::now();
        let scan = super::scan_status(&home).expect("scan");
        let elapsed = t.elapsed();
        eprintln!("scan_status($HOME): {:?}, {} entries, truncated={}",
                  elapsed, scan.statuses.len(), scan.truncated);
        assert!(elapsed < std::time::Duration::from_secs(5),
                "scan took {elapsed:?} — the bound is not holding");
        assert!(scan.statuses.len() <= super::MAX_STATUS_ENTRIES);
    }
}
