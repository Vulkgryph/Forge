// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub models: ModelsConfig,
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub endpoints: Vec<ModelEndpoint>,
    pub default: String,
    /// Model endpoint for web_fetch content summarization. None = use main model.
    #[serde(default)]
    pub web_tool_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EndpointType {
    #[serde(rename = "open_ai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "chatgpt_codex")]
    ChatGptCodex,
}

impl Default for EndpointType {
    fn default() -> Self {
        EndpointType::OpenAi
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToggle {
    #[default]
    ProviderDefault,
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChatGptReasoningEffort {
    #[default]
    ProviderDefault,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OpenAiCompatibleReasoningConfig {
    #[serde(default)]
    pub thinking: ProviderToggle,
    #[serde(default)]
    pub preserve_thinking: ProviderToggle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnthropicReasoningConfig {
    #[serde(default)]
    pub thinking: ProviderToggle,
    #[serde(default = "default_anthropic_budget_tokens")]
    pub budget_tokens: u32,
}

impl Default for AnthropicReasoningConfig {
    fn default() -> Self {
        Self {
            thinking: ProviderToggle::On,
            budget_tokens: default_anthropic_budget_tokens(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatGptCodexReasoningConfig {
    #[serde(default)]
    pub effort: ChatGptReasoningEffort,
}

impl Default for ChatGptCodexReasoningConfig {
    fn default() -> Self {
        Self {
            effort: ChatGptReasoningEffort::Medium,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EndpointReasoningConfig {
    #[serde(default)]
    pub open_ai_compatible: OpenAiCompatibleReasoningConfig,
    #[serde(default)]
    pub anthropic: AnthropicReasoningConfig,
    #[serde(default)]
    pub chatgpt_codex: ChatGptCodexReasoningConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEndpoint {
    pub name: String,
    pub base_url: String,
    /// Model ID to send in API requests.
    /// Set to "auto" (or omit entirely) to have Forge query the endpoint's
    /// /v1/models at startup and use the first model it finds — useful for
    /// local servers like Oxide where the loaded model changes.
    #[serde(default = "default_model_id")]
    pub model_id: String,
    pub api_key: Option<String>,
    pub max_context_tokens: usize,
    /// Max tokens the model can output per response. Default: 16384.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    /// API request timeout in seconds. Default: 500.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// API wire format. Default: openai (OpenAI-compatible). Use "anthropic" for Claude API.
    #[serde(default)]
    pub endpoint_type: EndpointType,
    /// Provider-specific reasoning / thinking controls for this endpoint.
    #[serde(default)]
    pub reasoning: EndpointReasoningConfig,
    /// Opt in to xAI's priority processing tier (`service_tier: "priority"`
    /// on every request to this endpoint) — higher scheduling priority
    /// during high demand, at a 2x per-token premium over standard pricing.
    /// Only meaningful for genuine xAI endpoints (`base_url` containing
    /// `x.ai`); harmless no-op otherwise, since the API client only ever
    /// sends the parameter when both this flag is set *and* the base URL
    /// actually looks like xAI's.
    #[serde(default)]
    pub xai_priority_tier: bool,
}

fn default_model_id() -> String {
    "auto".to_string()
}

fn default_max_output_tokens() -> u32 {
    16384
}

pub fn default_request_timeout_secs() -> u64 {
    500
}

fn default_anthropic_budget_tokens() -> u32 {
    8192
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    DontAsk,
    Plan,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::Default
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    Compaction,
    RollingWindow,
}

impl Default for ContextStrategy {
    fn default() -> Self {
        ContextStrategy::Compaction
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub auto_approve_reads: bool,
    pub auto_approve_writes: bool,
    /// Legacy global switch for providers that expose thinking controls.
    /// false maps OpenAI-compatible endpoints to enable_thinking=false unless
    /// that endpoint has an explicit reasoning override.
    #[serde(default = "default_thinking_mode")]
    pub thinking_mode: bool,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    pub max_history_messages: usize,
    /// Dead since the trigger became token-based; kept only so existing config
    /// files that name it still parse. `compact_at_percent` is the live knob.
    pub compaction_threshold: usize,
    /// Fraction of the context window at which the conversation is compacted,
    /// as a percentage. The trigger used to be `>= 100`, which is not a
    /// threshold but an overflow: compaction only ran once a request had
    /// already been built too large, so the turn that paid for it was the one
    /// that had already gone wrong. Compacting with headroom left means the
    /// summarizer call itself still fits.
    #[serde(default = "default_compact_at_percent")]
    pub compact_at_percent: u8,
    #[serde(default)]
    pub subagents: SubagentConfig,
    /// Tool names to exclude from every agent turn. Internal tools are never affected.
    ///
    /// `web_search` is in here by default. It works by scraping DuckDuckGo's HTML,
    /// which is not a search API and does not reliably answer — a tool that
    /// usually returns nothing is worse than one that is absent, because the
    /// model spends a turn on it and then reasons about the emptiness. Off until
    /// there is a real search behind it. Turn it on from the tools menu in either
    /// client if you want it as it stands; that choice is written here and kept.
    ///
    /// `web_fetch` stays on: it retrieves a URL it has been given and summarises
    /// it, which does not depend on search working.
    #[serde(default = "default_disabled_tools")]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub context_strategy: ContextStrategy,
    /// Floor on `shell_exec`'s `timeout_secs` (with `wait=true`) — the
    /// model chooses that value per call and can simply guess wrong for a
    /// task that runs longer than it expected (a long build, a data
    /// pipeline, a simulation run), which gets the command killed with a
    /// timeout error partway through. `0` (default) applies no floor —
    /// today's behavior, entirely up to the model's own per-call choice.
    /// Set higher to guarantee at least this many seconds regardless of
    /// what the model requests for that project.
    #[serde(default)]
    pub min_shell_timeout_secs: u64,
    /// Absolute ceiling (seconds) after which **any** still-running top-level
    /// `shell_exec` is force-moved to the background — including when the
    /// model set `wait=true`, picked a huge `timeout_secs`, or the interactive
    /// prompt heuristic paused the normal timer. The command keeps running as
    /// `bg-N`; the agent can poll it with `background_id` / kill it with
    /// `background_action=kill`, and gets a `BgDone` delivery when it finishes.
    /// Default **300** (5 minutes). Set to `0` to disable the forced ceiling
    /// (not recommended — a stuck wait=true shell can hold the turn again).
    #[serde(default = "default_forced_shell_background_secs")]
    pub forced_shell_background_secs: u64,
    /// When true, forces off the network touchpoints that are not part of
    /// making a model call: the `web_search`/`web_fetch` tools, the weekly
    /// GitHub version self-check, and the ChatGPT Codex model-catalog fetch
    /// unless Codex is the endpoint actually in use.
    ///
    /// What it does not turn off, because a model call cannot happen without
    /// it: refreshing the ChatGPT Codex OAuth token when Codex is the active
    /// endpoint. That reaches the provider, the same as the request it
    /// authenticates. Anything else — a local endpoint, an API key — makes no
    /// such call.
    ///
    /// Off by default — nothing changes unless a user opts in.
    #[serde(default)]
    pub offline_mode: bool,
}

fn default_forced_shell_background_secs() -> u64 {
    300
}

fn default_compact_at_percent() -> u8 {
    80
}

/// Tools off unless asked for. See `AgentConfig::disabled_tools`.
fn default_disabled_tools() -> Vec<String> {
    vec!["web_search".to_string()]
}

fn default_thinking_mode() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub max_depth: usize,
    /// Maximum number of subagents that can run concurrently when the LLM
    /// returns multiple delegate_task calls in a single response.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Default model endpoint name for subagents. None = inherit parent's model.
    pub default_model: Option<String>,
    /// Wall-clock ceiling (seconds) for a single parent `delegate_task` batch.
    /// When exceeded, unfinished subagents are aborted and the parent receives
    /// a timeout summary instead of hanging forever. Default 1800s (30 min).
    #[serde(default = "default_max_delegate_secs")]
    pub max_delegate_secs: u64,
}

fn default_max_concurrent() -> usize {
    4
}

fn default_max_delegate_secs() -> u64 {
    1800
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: 4,
            max_concurrent: 4,
            default_model: None,
            max_delegate_secs: default_max_delegate_secs(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        // This default is only reached when no ~/.config/forge/config.toml
        // exists yet. install.sh's wizard always writes a real config, so in
        // practice users never see these values. They're here as a sensible
        // fallback (loopback localhost defaults matching LM Studio's port).
        Self {
            models: ModelsConfig {
                endpoints: vec![ModelEndpoint {
                    name: "local".to_string(),
                    base_url: "http://127.0.0.1:1234/v1".to_string(),
                    model_id: "auto".to_string(),
                    api_key: None,
                    max_context_tokens: 32768,
                    max_output_tokens: 16384,
                    request_timeout_secs: 500,
                    endpoint_type: EndpointType::OpenAi,
                    reasoning: EndpointReasoningConfig::default(),
                    xai_priority_tier: false,
                }],
                default: "local".to_string(),
                web_tool_model: None,
            },
            agent: AgentConfig {
                auto_approve_reads: true,
                auto_approve_writes: false,
                thinking_mode: true,
                permission_mode: PermissionMode::Default,
                max_history_messages: 200,
                compaction_threshold: 150,
                compact_at_percent: default_compact_at_percent(),
                subagents: SubagentConfig::default(),
                disabled_tools: default_disabled_tools(),
                context_strategy: ContextStrategy::Compaction,
                min_shell_timeout_secs: 0,
                forced_shell_background_secs: default_forced_shell_background_secs(),
                offline_mode: false,
            },
        }
    }
}

/// Narrow `path` to its owner. Best effort: a filesystem that cannot express
/// this is not a reason to refuse to save, and Windows has its own ACL model
/// where the user profile is already private.
#[cfg(unix)]
fn restrict_to_owner(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &std::path::Path, _mode: u32) {}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;
        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)
                .with_context(|| format!("Failed to read config at {}", config_path.display()))?;
            let config: AppConfig =
                toml::from_str(&contents).with_context(|| "Failed to parse config")?;
            config.validate_endpoints()?;
            Ok(config)
        } else {
            let config = AppConfig::default();
            config.save()?;
            Ok(config)
        }
    }

    /// Reject endpoints whose `base_url` isn't an http(s) URL. Without this
    /// check, a config could point at `file://`, `gopher://`, etc., and the
    /// HTTP client would happily attach bearer tokens to whatever it dialed.
    fn validate_endpoints(&self) -> Result<()> {
        for endpoint in &self.models.endpoints {
            let url = reqwest::Url::parse(&endpoint.base_url).with_context(|| {
                format!(
                    "endpoint '{}' has an invalid base_url: {}",
                    endpoint.name, endpoint.base_url
                )
            })?;
            match url.scheme() {
                "http" | "https" => {}
                other => anyhow::bail!(
                    "endpoint '{}' uses unsupported scheme '{}' in base_url (only http/https allowed): {}",
                    endpoint.name,
                    other,
                    endpoint.base_url
                ),
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
            // The directory too: a 0755 directory holding a 0600 file still
            // tells every account on the machine that the file is there.
            restrict_to_owner(parent, 0o700);
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, contents)?;
        // This file holds API keys. Written with the process umask it lands at
        // 0644 on a stock machine — readable by every account on it — which on
        // a shared or multi-user box means the keys are, too. Set after the
        // write rather than before, so it applies to a file that already
        // existed as well as one just created.
        restrict_to_owner(&config_path, 0o600);
        Ok(())
    }

    pub fn config_path() -> Result<PathBuf> {
        let home = dirs::home_dir().context("Could not find home directory")?;
        Ok(home.join(".config").join("forge").join("config.toml"))
    }

    pub fn get_endpoint(&self, name: &str) -> Option<&ModelEndpoint> {
        self.models.endpoints.iter().find(|e| e.name == name)
    }

    pub fn default_endpoint(&self) -> Option<&ModelEndpoint> {
        self.get_endpoint(&self.models.default)
    }
}

#[cfg(test)]
mod permission_tests {
    /// The config holds API keys, so it must not be readable by other accounts
    /// on the machine. Written with the process umask it lands at 0644 on a
    /// stock box — which is how a key ended up world-readable on a remote host
    /// during testing.
    #[test]
    #[cfg(unix)]
    fn a_saved_config_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("forge-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // As `save` does it: write, then narrow.
        std::fs::write(&path, "key = \"secret\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::restrict_to_owner(&path, 0o600);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod default_tools_tests {
    use super::*;

    /// `web_search` is off unless asked for.
    ///
    /// It scrapes DuckDuckGo's HTML rather than using a search API, and usually
    /// comes back empty. A tool that usually returns nothing is worse than one
    /// that is absent: the model spends a turn on it and then reasons about the
    /// emptiness as if it meant something.
    #[test]
    fn web_search_is_off_by_default() {
        let cfg = AppConfig::default();
        assert!(cfg.agent.disabled_tools.iter().any(|t| t == "web_search"));
    }

    /// `web_fetch` stays on. It retrieves a URL it has been handed and summarises
    /// it, which does not depend on search working — and a pasted link is the
    /// common way anyone wants a page read.
    #[test]
    fn web_fetch_is_left_alone() {
        assert!(!AppConfig::default().agent.disabled_tools.iter().any(|t| t == "web_fetch"));
    }

    /// A real config file, as the app writes them, with one line changed. The
    /// minimal hand-written TOML this used to try does not parse — most of
    /// `[agent]` has no default — and testing against a shape nobody has on disk
    /// would prove nothing about the upgrade path.
    fn config_with(disabled_line: Option<&str>) -> AppConfig {
        let full = toml::to_string_pretty(&AppConfig::default()).expect("serialise");
        let mut out = String::new();
        for line in full.lines() {
            if line.starts_with("disabled_tools") {
                match disabled_line {
                    Some(replacement) => out.push_str(replacement),
                    // Omitted entirely: an existing file from before the setting.
                    None => continue,
                }
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        toml::from_str(&out).expect("parse")
    }

    /// A config that does not mention the setting gets the new default, so the
    /// change reaches everybody who has never touched it.
    #[test]
    fn a_config_without_the_setting_gets_the_default() {
        assert_eq!(config_with(None).agent.disabled_tools, vec!["web_search".to_string()]);
    }

    /// And a config that *does* mention it is obeyed, including an empty list —
    /// somebody who turned web search back on keeps it, which is the whole point
    /// of it being a default rather than a removal.
    #[test]
    fn an_explicit_choice_is_obeyed() {
        let on = config_with(Some("disabled_tools = []"));
        assert!(on.agent.disabled_tools.is_empty(), "an explicit opt-in was overridden");

        let other = config_with(Some("disabled_tools = [\"shell_exec\"]"));
        assert_eq!(other.agent.disabled_tools, vec!["shell_exec".to_string()],
                   "somebody else\'s disabled list was rewritten");
    }
}
