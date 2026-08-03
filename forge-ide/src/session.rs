//! Per-workspace session state — which files and terminals were open,
//! restored on the next launch if the user has that setting enabled.
//! Persisted at `<workspace>/.forge/session.json`, mirroring
//! `.forge/tasks.toml`'s existing per-project config convention.
//!
//! Terminals are recorded by *pty-host session id*, not just directory —
//! restoring one first checks whether that id is still alive in the local
//! pty-host daemon (it survives Forge IDE's own process restarting; see
//! the `ptyhost` module) and reattaches to the real running shell if so,
//! only falling back to a fresh shell in the same directory if that
//! session is genuinely gone (daemon never started, or was itself
//! restarted independently) — the same fallback VS Code's own terminal
//! restore has, since neither actually replays scrollback/history.

use std::path::{Path, PathBuf};

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TerminalState {
    /// The pty-host session id, if this terminal was daemon-backed —
    /// `None` for the direct-PTY fallback (daemon unreachable), which has
    /// no cross-restart identity to reattach to.
    pub pty_id: Option<u32>,
    pub cwd:    PathBuf,
    /// What was actually on screen at save time — restored onto the fresh
    /// `Grid` built for a reattached terminal so it shows what was running
    /// instead of sitting blank until new output arrives. `None` for the
    /// direct-PTY fallback (nothing to reattach to anyway).
    pub viewport: Option<crate::terminal::GridSnapshot>,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    pub open_files:  Vec<PathBuf>,
    pub active_file: usize,
    pub terminals:   Vec<TerminalState>,

    /// Whether the Forge Agent panel was open.
    pub agent_visible: bool,
    /// Conversation ids of open agent tabs, in tab order. Each is looked up
    /// in the on-disk saved-conversation store on restore (auto-saved
    /// continuously while a tab is active — see `agent_panel::save_conversation`)
    /// and replayed into a fresh tab exactly like manually reopening it from
    /// the history list — there's no live process to resume, just the
    /// transcript. A tab with no messages yet has nothing to save and isn't
    /// recorded here.
    pub agent_tabs:   Vec<String>,
    pub agent_active: usize,
}

fn session_path(root: &Path) -> PathBuf {
    root.join(".forge").join("session.json")
}

/// A stable identity for one window, so its state can be kept apart from every
/// other window's.
///
/// Nanoseconds since the epoch, plus a per-process counter for the case of two
/// windows opened inside the same nanosecond. Two *different* processes doing so
/// at the same nanosecond would collide, which on a desktop application is not a
/// case worth carrying machinery for.
pub fn new_window_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos.wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn sessions_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("forge-ide").join("sessions"))
}

/// The directory is a parameter throughout so the tests exercise these against a
/// temporary one instead of the user's real configuration.
fn window_session_path_in(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// This particular window's state.
///
/// Kept beside the window list rather than inside a workspace, because state
/// stored per folder cannot express either of the two cases that need it: a
/// window with no folder open has nowhere to put it, and two windows on the same
/// folder overwrite each other — the second one to close wins, and both come back
/// showing its files.
///
/// Falls back to the workspace's own `.forge/session.json`, which is where every
/// session lived before windows had identity (so existing state is not orphaned)
/// and remains the right answer for a window opening a folder it has no state of
/// its own for: where the project was last left beats nothing at all.
pub fn load_for_window(id: u64, root: Option<&Path>) -> Option<SessionState> {
    load_for_window_in(sessions_dir().as_deref(), id, root)
}

fn load_for_window_in(dir: Option<&Path>, id: u64, root: Option<&Path>) -> Option<SessionState> {
    if let Some(state) = dir.and_then(|d| read_state(&window_session_path_in(d, id))) {
        return Some(state);
    }
    root.and_then(load)
}

pub fn save_for_window(id: u64, root: Option<&Path>, state: &SessionState) {
    save_for_window_in(sessions_dir().as_deref(), id, root, state);
}

fn save_for_window_in(dir: Option<&Path>, id: u64, root: Option<&Path>, state: &SessionState) {
    if let Some(dir) = dir {
        write_state(&window_session_path_in(dir, id), state);
    }
    // Also left in the workspace, so opening that folder in a window that has no
    // state of its own still finds where the project was left.
    if let Some(root) = root {
        save(root, state);
    }
}

/// How long a window's state outlives the record that mentions it.
///
/// The recorded set is not a perfectly reliable census: a second Forge IDE
/// process writes its own window set over the first one's, so a record read at
/// startup can omit windows that are alive in another process. Deleting on the
/// strength of that alone would throw away state belonging to an open window, so
/// a file also has to be stale before it goes.
const SESSION_KEEP: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Forget the state of windows that are no longer in the recorded set, and have
/// not been touched for a week.
///
/// Without this, every window ever opened leaves a file behind for good. Only
/// ever called with a non-empty set: an empty one means the record could not be
/// read, which must not be taken as "no windows exist, delete everything".
pub fn prune_window_sessions(keep: &[WindowRecord]) {
    if let Some(dir) = sessions_dir() {
        prune_window_sessions_in(&dir, keep);
    }
}

fn prune_window_sessions_in(dir: &Path, keep: &[WindowRecord]) {
    if keep.is_empty() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let id = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok());
        let Some(id) = id else { continue };
        if keep.iter().any(|r| r.id == id) {
            continue;
        }
        let recently_used = path
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age < SESSION_KEEP);
        if !recently_used {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn load(root: &Path) -> Option<SessionState> {
    read_state(&session_path(root))
}

fn read_state(path: &Path) -> Option<SessionState> {
    // A missing file is the normal "no session yet" case and stays quiet. A
    // file that exists but won't parse is a real failure — a truncated write, a
    // schema change — and used to be swallowed by `.ok()?`, so a whole restored
    // session silently became "nothing to restore" with no way to tell why.
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(state) => Some(state),
        Err(e) => {
            eprintln!("session: ignoring unreadable {} ({e})", path.display());
            None
        }
    }
}

pub fn save(root: &Path, state: &SessionState) {
    write_state(&session_path(root), state);
}

fn write_state(path: &Path, state: &SessionState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Compact, not pretty: nothing reads this by hand, and pretty-printing the
    // terminal cell arrays roughly tripled an already-oversized file.
    let Ok(text) = serde_json::to_string(state) else { return };
    // Write to a sibling temp file and rename, so an interrupted or partial
    // write can't replace a good session with a truncated one — the failure
    // mode that loses everything, since a corrupt file restores nothing.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// A distinct path per test. These used to share the one global file, so they
    /// clobbered each other when run in parallel — and worse, wrote to the real
    /// configuration.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir()
            .join(format!("forge-windows-{}-{name}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// The bug this guards: quitting closes windows one at a time, so saving
    /// every change would end with an empty list on disk and nothing to reopen.
    #[test]
    fn an_empty_set_does_not_erase_what_was_saved() {
        let path = scratch("empty");
        let two = vec![
            WindowRecord { cwd: Some(PathBuf::from("/one")), ..Default::default() },
            WindowRecord { cwd: Some(PathBuf::from("/two")), ..Default::default() },
        ];
        save_windows_to(&path, &two);
        save_windows_to(&path, &[]);
        assert_eq!(
            load_windows_from(&path), two,
            "quitting must not wipe the set it is meant to restore",
        );
    }

    /// Several windows must survive a round trip, which is the whole point.
    #[test]
    fn several_windows_round_trip() {
        let path = scratch("round-trip");
        let records = vec![
            WindowRecord { cwd: Some(PathBuf::from("/project/one")), ..Default::default() },
            WindowRecord { cwd: None, ..Default::default() },
            WindowRecord { cwd: Some(PathBuf::from("/project/three")), ..Default::default() },
        ];
        save_windows_to(&path, &records);
        assert_eq!(load_windows_from(&path), records);
    }

    /// A folderless window records `None`, not the directory its terminal
    /// happened to be rooted at — reopening that would present a workspace the
    /// user never chose.
    #[test]
    fn a_folderless_window_records_no_folder() {
        let path = scratch("folderless");
        save_windows_to(&path, &[WindowRecord::default()]);
        assert_eq!(load_windows_from(&path), vec![WindowRecord::default()]);
    }

    /// Nothing saved yet is an empty list, not a panic, so a first run falls back
    /// to a single window.
    #[test]
    fn a_missing_file_loads_as_nothing() {
        assert!(load_windows_from(&scratch("missing")).is_empty());
    }

    /// A corrupt file must not stop the application opening.
    #[test]
    fn a_corrupt_file_loads_as_nothing() {
        let path = scratch("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_windows_from(&path).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("forge-sess-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A saved session with a busy terminal must stay small. This file used to
    /// reach 132 MB for one terminal, which made every save and restore a
    /// multi-second synchronous stall and risked truncation.
    #[test]
    fn saved_session_with_a_busy_terminal_stays_small() {
        let root = scratch("size");
        let mut grid = crate::terminal::Grid::with_size(50, 220);
        // Well past the snapshot cap, so trimming is what bounds the file.
        let mut feed = String::new();
        for i in 0..4000 {
            feed.push_str(&format!("line {i} ...........................\r\n"));
        }
        grid.process(&feed);
        let state = SessionState {
            open_files: vec![root.join("a.rs")],
            terminals: vec![TerminalState {
                pty_id: Some(1), cwd: root.clone(), viewport: Some(grid.snapshot()),
            }],
            ..Default::default()
        };
        save(&root, &state);
        let bytes = std::fs::metadata(session_path(&root)).unwrap().len();
        eprintln!("session.json for a 4000-line terminal: {} KB", bytes / 1024);
        assert!(bytes < 8 * 1024 * 1024,
                "session.json is {bytes} bytes — the scrollback cap regressed");
        // And it still round-trips.
        let back = load(&root).expect("should load");
        assert_eq!(back.open_files, state.open_files);
        assert_eq!(back.terminals.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A corrupt file must be reported and ignored, not panic.
    #[test]
    fn corrupt_session_is_ignored() {
        let root = scratch("corrupt");
        std::fs::create_dir_all(root.join(".forge")).unwrap();
        std::fs::write(session_path(&root), "{ truncated").unwrap();
        assert!(load(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_session_is_none() {
        let root = scratch("missing");
        assert!(load(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The temp-file dance must not leave debris behind.
    #[test]
    fn save_leaves_no_temp_file() {
        let root = scratch("tmp");
        save(&root, &SessionState::default());
        assert!(session_path(&root).exists());
        assert!(!session_path(&root).with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}

// ── Open windows ─────────────────────────────────────────────────────────────
//
// Which windows were open is *global* state, not per-workspace: it is a property
// of the application, and each window's own contents live in that workspace's
// own session file. Keeping it beside the settings rather than under any one
// project is what lets a fresh start reopen the set.
//
// Before this, the set only ever travelled as command-line arguments during
// "Reload Window" — one path per window. A genuine quit and relaunch had no such
// argument, so it always opened exactly one empty window and every other window
// was silently lost.

/// Where a window was on screen.
///
/// Physical pixels, not logical: the scale factor is per-monitor, so a logical
/// frame saved on a Retina display and reopened against a 1x one lands somewhere
/// else entirely. Physical coordinates reproduce the frame exactly whenever the
/// display arrangement is unchanged, which is the case a reload cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowFrame {
    /// Top-left of the whole window including its title bar — winit's *outer*
    /// position, which is what `WindowAttributes::with_position` takes, so this
    /// round-trips without an off-by-a-title-bar drift on every reload.
    pub x: i32,
    pub y: i32,
    /// The content area — winit's *inner* size, to match `with_inner_size`.
    pub w: u32,
    pub h: u32,
}

/// One window to reopen.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowRecord {
    /// The workspace folder, or `None` for a window opened without one.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Where it was. `None` for a record written before this was tracked, or a
    /// window whose position could not be read; the platform then places it.
    #[serde(default)]
    pub frame: Option<WindowFrame>,
    /// Zoomed/maximized windows are restored zoomed rather than at whatever
    /// frame they happened to occupy, since that is the state the user set.
    #[serde(default)]
    pub maximized: bool,
    /// Which stored session belongs to this window. `0` in a record written
    /// before windows had identity; such a window is given a fresh id and falls
    /// back to its workspace's own session file.
    #[serde(default)]
    pub id: u64,
}

// Only the folder is recorded, not a remote connection. `IdeApp` tracks a
// *pending* SSH host rather than an established one, so there is no reliable way
// to ask a connected window which host it is on — and reopening against a guess
// would connect somewhere the user did not ask for. A remote window therefore
// reopens as its folder; reconnecting stays manual until the app tracks the live
// host.

fn windows_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("forge-ide").join("windows.json"))
}

/// Record the currently open windows.
///
/// An empty list is deliberately *not* written. Quitting closes windows one at a
/// time, so saving each change would end with an empty list on disk and nothing
/// to restore — the exact failure this is meant to fix. The last non-empty set
/// therefore survives a quit, which is also what a user means by "reopen what I
/// had".
pub fn save_windows(records: &[WindowRecord]) {
    if let Some(path) = windows_path() {
        save_windows_to(&path, records);
    }
}

/// The windows open when the application last had any.
pub fn load_windows() -> Vec<WindowRecord> {
    windows_path().map(|p| load_windows_from(&p)).unwrap_or_default()
}

/// The path is a parameter so tests can exercise this without writing to the
/// user's real configuration — which the first version of these tests did.
fn save_windows_to(path: &Path, records: &[WindowRecord]) {
    if records.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string(records) else { return };

    // Written via a temporary and renamed, so a crash mid-write cannot leave a
    // truncated file that loses every window instead of one.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn load_windows_from(path: &Path) -> Vec<WindowRecord> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

#[cfg(test)]
mod window_session_tests {
    use super::*;

    fn dirs_for(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir()
            .join(format!("forge-sess-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions = base.join("sessions");
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        (sessions, workspace)
    }

    /// `set_file_times` is not on stable, so this goes through `utimes(2)`.
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let secs = when.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        #[repr(C)]
        struct TimeVal { sec: i64, usec: i64 }
        unsafe extern "C" {
            fn utimes(path: *const std::ffi::c_char, times: *const TimeVal) -> i32;
        }
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let times = [TimeVal { sec: secs, usec: 0 }, TimeVal { sec: secs, usec: 0 }];
        let rc = unsafe { utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {}", path.display());
    }

    fn state(files: &[&str]) -> SessionState {
        SessionState { open_files: files.iter().map(PathBuf::from).collect(), ..Default::default() }
    }

    fn files_of(s: &SessionState) -> Vec<String> {
        s.open_files.iter().map(|p| p.display().to_string()).collect()
    }

    /// The reported requirement: a window comes back with the state it had. Two
    /// windows on the *same* folder each need their own, which per-folder storage
    /// could not express — the second to close overwrote the first, and both
    /// reopened showing its files.
    #[test]
    fn two_windows_on_one_folder_keep_separate_state() {
        let (sessions, ws) = dirs_for("two");
        save_for_window_in(Some(&sessions), 1, Some(&ws), &state(["/a.rs"].as_ref()));
        save_for_window_in(Some(&sessions), 2, Some(&ws), &state(["/b.rs"].as_ref()));

        let first  = load_for_window_in(Some(&sessions), 1, Some(&ws)).unwrap();
        let second = load_for_window_in(Some(&sessions), 2, Some(&ws)).unwrap();
        assert_eq!(files_of(&first),  ["/a.rs"], "the first window kept its own files");
        assert_eq!(files_of(&second), ["/b.rs"], "and did not overwrite the second's");
    }

    /// A window with no folder open used to save nothing at all, so everything in
    /// it was lost on every restart.
    #[test]
    fn a_folderless_window_keeps_its_state() {
        let (sessions, _) = dirs_for("folderless");
        save_for_window_in(Some(&sessions), 7, None, &state(["/scratch.rs"].as_ref()));
        let got = load_for_window_in(Some(&sessions), 7, None)
            .expect("a window without a folder still has state");
        assert_eq!(files_of(&got), ["/scratch.rs"]);
    }

    /// State written before windows had identity is not orphaned: a window with no
    /// state of its own falls back to the workspace's.
    #[test]
    fn a_window_with_no_state_falls_back_to_the_workspace() {
        let (sessions, ws) = dirs_for("fallback");
        save(&ws, &state(["/legacy.rs"].as_ref()));
        let got = load_for_window_in(Some(&sessions), 999, Some(&ws))
            .expect("the workspace's own session is the fallback");
        assert_eq!(files_of(&got), ["/legacy.rs"]);
    }

    /// A window's own state wins over the workspace's.
    #[test]
    fn a_windows_own_state_wins() {
        let (sessions, ws) = dirs_for("wins");
        save(&ws, &state(["/project.rs"].as_ref()));
        save_for_window_in(Some(&sessions), 5, None, &state(["/mine.rs"].as_ref()));
        let got = load_for_window_in(Some(&sessions), 5, Some(&ws)).unwrap();
        assert_eq!(files_of(&got), ["/mine.rs"]);
    }

    /// Closed windows' state is forgotten once stale, or every window ever opened
    /// leaves a file behind for good.
    #[test]
    fn stale_state_for_windows_that_are_gone_is_pruned() {
        let (sessions, _) = dirs_for("prune");
        for id in [1u64, 2, 3] {
            save_for_window_in(Some(&sessions), id, None, &state(["/x.rs"].as_ref()));
        }
        // Backdate 1 and 3 past the keep window; 2 stays in the record.
        for id in [1u64, 3] {
            let old = std::time::SystemTime::now() - SESSION_KEEP - std::time::Duration::from_secs(60);
            set_mtime(&window_session_path_in(&sessions, id), old);
        }
        let keep = vec![WindowRecord { id: 2, ..Default::default() }];
        prune_window_sessions_in(&sessions, &keep);
        assert!(!window_session_path_in(&sessions, 1).exists(), "1 is gone and stale");
        assert!(window_session_path_in(&sessions, 2).exists(),  "2 is still open");
        assert!(!window_session_path_in(&sessions, 3).exists(), "3 is gone and stale");
    }

    /// The dangerous case: a second Forge IDE process writes its own window set
    /// over the first one's, so a record can omit windows that are alive
    /// elsewhere. Freshly-written state must survive being absent from it.
    #[test]
    fn state_a_live_window_just_wrote_is_not_pruned() {
        let (sessions, _) = dirs_for("live");
        save_for_window_in(Some(&sessions), 11, None, &state(["/open-right-now.rs"].as_ref()));
        // A record from another process that has never heard of window 11.
        let keep = vec![WindowRecord { id: 99, ..Default::default() }];
        prune_window_sessions_in(&sessions, &keep);
        assert!(
            window_session_path_in(&sessions, 11).exists(),
            "state written moments ago belongs to a window that is very likely open",
        );
    }

    /// An unreadable window list must not be read as "no windows exist".
    #[test]
    fn an_empty_set_prunes_nothing() {
        let (sessions, _) = dirs_for("empty");
        save_for_window_in(Some(&sessions), 42, None, &state(["/keep.rs"].as_ref()));
        prune_window_sessions_in(&sessions, &[]);
        assert!(window_session_path_in(&sessions, 42).exists(),
                "an empty set means the record could not be read, not that nothing is open");
    }
}
