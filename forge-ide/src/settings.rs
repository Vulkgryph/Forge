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

    // Updates
    /// Check GitHub Releases for a newer Forge IDE version on startup. Off
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size:          14.0,
            tab_width:          4,
            insert_spaces:      true,
            word_wrap:           false,
            auto_close_brackets: true,
            minimap:            true,
            terminal_font_size: 14.0,
            theme:              "Dark+".into(),
            restore_session:    true,
            restore_windows:    true,
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
        let _ = std::fs::write(path, text);
    }
}
