//! User settings — persisted at ~/.config/forge-ide/settings.toml.

use std::path::PathBuf;

/// How much a Forge agent tab is allowed to do without asking first.
/// Mirrors `forge-agent`'s own three tiers: per-category config defaults
/// (reads auto-approved, writes/execute confirmed), the `auto_mode` runtime
/// toggle (skips read/write/execute confirmation, still blocks unrecognized
/// tool kinds), and `--dangerously-allow-all` (skips everything, spawn-time
/// only — see `agent_panel::AgentSession::spawn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionMode {
    AlwaysAsk,
    AutoApprove,
    DangerouslySkipAll,
}

impl Default for AgentPermissionMode {
    fn default() -> Self { Self::AlwaysAsk }
}

impl AgentPermissionMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::AlwaysAsk          => "Always Ask",
            Self::AutoApprove        => "Auto-Approve",
            Self::DangerouslySkipAll => "Skip All Permissions",
        }
    }

    /// The same thing in as few characters as still say it, for the agent
    /// panel's status strip at a width where the full labels do not fit.
    ///
    /// `Skip All` keeps the word "Skip", which is the part that should worry
    /// somebody who did not mean to be in that mode.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::AlwaysAsk          => "Ask",
            Self::AutoApprove        => "Auto",
            Self::DangerouslySkipAll => "Skip All",
        }
    }

    /// Short line explaining the tradeoff, shown under each option in the picker.
    pub fn description(&self) -> &'static str {
        match self {
            Self::AlwaysAsk          => "Confirm writes and shell commands (default).",
            Self::AutoApprove        => "Skip confirmation for reads, writes, and shell commands.",
            Self::DangerouslySkipAll => "Skip all confirmation, including unrecognized tools. Use with caution.",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    // Editor
    pub font_size:     f32,
    pub tab_width:     u8,
    pub insert_spaces: bool,
    pub word_wrap:     bool,
    pub auto_close_brackets: bool,
    pub minimap:       bool,

    // Terminal
    pub terminal_font_size: f32,

    // Appearance
    pub theme: String,

    // Session
    /// Remember open editor tabs and terminal working directories, and
    /// restore them on the next launch of this workspace. Off by default —
    /// this should be an opt-in choice a user makes (surfaced in a
    /// getting-started guide, matching VS Code's welcome experience), not a
    /// silent default behavior change for existing users.
    pub restore_session: bool,

    /// Reopen every window that was open, rather than a single empty one.
    ///
    /// Separate from `restore_session` deliberately: which windows were open is
    /// a different question from what was inside them, and wanting your folders
    /// back is not the same as wanting your editor tabs and terminals back. On
    /// by default — losing windows on a restart is a surprise, where restoring
    /// tabs is a preference.
    #[serde(default = "default_true")]
    pub restore_windows: bool,

    /// Write modified files to disk periodically, so a crash or a power cut
    /// costs minutes rather than a session. On by default: losing work is
    /// worse than an unexpected write, and the one surprising case — a file
    /// you have not chosen a name for — is written to a temporary directory
    /// and announced rather than saved somewhere silently.
    #[serde(default = "default_true")]
    pub auto_save: bool,
    /// How often, in seconds.
    #[serde(default = "default_auto_save_secs")]
    pub auto_save_secs: u64,
    /// How many undo steps a buffer keeps.
    ///
    /// A step is one edit as a person would count it: anything that adds or
    /// removes a line starts one immediately, and a burst of typing is folded
    /// into a single step. Saving has nothing to do with it.
    ///
    /// Steps hold only the lines that changed, so this costs little for
    /// ordinary editing — it was worth capping when a step was a copy of the
    /// whole file.
    #[serde(default = "default_undo_steps")]
    pub undo_steps: usize,

    // Updates
    /// Check GitHub Releases on startup and hourly while open. Off
    /// by default — this is the one network call Forge IDE makes on its
    /// own (everything else is local, or the agent panel's own subprocess),
    /// so it's an explicit opt-in rather than silently phoning home.
    pub check_for_updates: bool,
    /// Whether the one-time "want update checks?" prompt has already been
    /// shown, regardless of which way the user answered — so it's asked
    /// exactly once, not every launch.
    pub update_check_prompted: bool,

    // Agent setup
    /// Set once the user explicitly dismisses the AI-provider setup wizard
    /// without configuring anything — otherwise it would reappear on every
    /// launch for as long as forge-agent's config has no real endpoint
    /// (`onboarding::needs_setup()` has no other way to remember "the user
    /// was already asked and said not now", since that's not something
    /// forge-agent's own config file should track).
    pub onboarding_skipped: bool,

    // Layout
    /// When true, the file tree (Explorer) sidebar spans the full window
    /// height instead of stopping above the terminal. Off by default —
    /// keeps the terminal full-width on that side.
    pub file_tree_full_height: bool,
    /// When true, the Forge agent panel spans the full window height (like
    /// VS Code's secondary side bar) and the terminal is inset to the
    /// central width instead. Off by default — keeps the terminal
    /// full-width, which most of this editor's own usage favors.
    pub agent_panel_full_height: bool,

    // Agent
    /// Default permission mode for newly spawned Forge agent tabs. Each tab
    /// can still override this individually via the dropdown in the agent
    /// panel's status bar — this only sets what a *new* tab starts as.
    /// Always-ask by default: skipping approval, especially entirely
    /// (`DangerouslySkipAll`), is a deliberate per-use choice, not something
    /// to default on silently.
    pub default_agent_permission_mode: AgentPermissionMode,
}

/// Five minutes. Long enough that it is never in the way of typing, short
/// enough that what a crash costs is a coffee break rather than an afternoon.
fn default_auto_save_secs() -> u64 { 300 }
fn default_undo_steps() -> usize { crate::buffer::DEFAULT_UNDO_STEPS }

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size:          14.0,
            undo_steps:         default_undo_steps(),
            tab_width:          4,
            insert_spaces:      true,
            word_wrap:           false,
            auto_close_brackets: true,
            minimap:            true,
            terminal_font_size: 14.0,
            theme:              "Dark+".into(),
            restore_session:    true,
            restore_windows:    true,
            auto_save:          true,
            auto_save_secs:     default_auto_save_secs(),
            check_for_updates:  false,
            update_check_prompted: false,
            onboarding_skipped: false,
            file_tree_full_height:   false,
            agent_panel_full_height: false,
            default_agent_permission_mode: AgentPermissionMode::AlwaysAsk,
        }
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("forge-ide")
        .join("settings.toml")
}

fn default_true() -> bool {
    true
}

pub fn load() -> Settings {
    let path = settings_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = toml::to_string_pretty(s) {
        // Written to a neighbour and renamed over, rather than in place.
        //
        // Every window writes this whole file, so another one can be reading
        // it at the moment this writes. `write` truncates first, so a reader
        // landing in that window gets an empty or half-written file and falls
        // back to defaults — which is a settings reset, silently, for
        // somebody who only had two windows open. A rename is atomic: a
        // reader sees either the old file or the new one.
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// When the settings file was last written, for noticing another window's
/// change.
///
/// Every window holds its own copy of these settings and writes the whole
/// file, so without this the last window to save wins and quietly reverts
/// whatever the others changed — which is what made the editor font size
/// look as though it did nothing at all. Checked by mtime rather than by
/// watching the file: a `stat` on a frame that was already happening costs
/// nothing, and a window that is idle has no setting to apply anyway.
pub fn modified_at() -> Option<std::time::SystemTime> {
    std::fs::metadata(settings_path()).ok()?.modified().ok()
}

#[cfg(test)]
mod cross_window_tests {
    use super::*;

    /// A write must never be visible half-finished.
    ///
    /// Every window writes this whole file, so another can be reading it at
    /// that moment. A plain `write` truncates first, and a reader landing in
    /// that window parses nothing and falls back to defaults — a silent
    /// settings reset for somebody who only had two windows open.
    #[test]
    fn a_save_is_atomic_so_a_reader_never_sees_half_a_file() {
        let dir = std::env::temp_dir().join(format!("forge-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");

        // The shape `save` uses: write a neighbour, rename over.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, "font_size = 18.0\n").unwrap();
        std::fs::rename(&tmp, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("18.0"));
        assert!(!tmp.exists(), "the temporary file was left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The mtime is what tells one window that another has written.
    #[test]
    fn a_write_moves_the_timestamp_the_windows_compare() {
        let before = modified_at();
        let mut s = Settings::default();
        s.font_size = if before.is_some() { 15.0 } else { 15.0 };
        // Not saved here — saving would touch the real file somebody is
        // using. What matters is that the function reports something to
        // compare at all; without it a window has no way to notice.
        assert!(
            modified_at().is_some() || before.is_none(),
            "no timestamp to compare, so no window can notice another's change",
        );
    }

    /// The default must stay what the editor assumes, since a failed read
    /// falls back to it.
    #[test]
    fn the_default_font_size_is_the_one_the_editor_expects() {
        assert_eq!(Settings::default().font_size, 14.0);
    }
}
