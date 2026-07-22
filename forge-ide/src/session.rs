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
    let text = std::fs::read_to_string(session_path(root)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(root: &Path, state: &SessionState) {
    let path = session_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, text);
    }
}
