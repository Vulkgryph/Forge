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

pub fn load(root: &Path) -> Option<SessionState> {
    let path = session_path(root);
    // A missing file is the normal "no session yet" case and stays quiet. A
    // file that exists but won't parse is a real failure — a truncated write, a
    // schema change — and used to be swallowed by `.ok()?`, so a whole restored
    // session silently became "nothing to restore" with no way to tell why.
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(state) => Some(state),
        Err(e) => {
            eprintln!("session: ignoring unreadable {} ({e})", path.display());
            None
        }
    }
}

pub fn save(root: &Path, state: &SessionState) {
    let path = session_path(root);
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
