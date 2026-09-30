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
    /// Whether a re-crawl may echo back the `ETag` a site issued.
    ///
    /// Off by default, and the asymmetry is the point. `Last-Modified` is a
    /// property of the content — every visitor gets the same value, so
    /// returning it tells a site nothing about who is asking. An `ETag` is
    /// chosen by the server and *can* be made unique per visitor, which is a
    /// documented tracking technique. Forge sends the harmless one always and
    /// the correlatable one only if asked.
    ///
    /// Turning this on saves bandwidth on sites that issue no `Last-Modified`
    /// — many CDNs — at the cost of handing that site a token it minted for
    /// you.
    #[serde(default)]
    pub send_etag: bool,

    /// Path to a PKCS#8 Ed25519 private key used to sign outbound crawler
    /// requests (Web Bot Auth, RFC 9421). Absent means requests are not
    /// signed, which is the default and behaves exactly as before.
    ///
    /// Signing is only useful once the matching public key is published at
    /// `<web_bot_auth_directory>/.well-known/http-message-signatures-directory`.
    /// A signature nobody can look up is a header nobody can check.
    #[serde(default)]
    pub web_bot_auth_key: Option<String>,
    /// The origin publishing that key. Required when `web_bot_auth_key` is set.
    #[serde(default)]
    pub web_bot_auth_directory: Option<String>,
    /// Defaulted because `[agent]` is a table a user writes by hand. Without
    /// one, naming the table at all made this mandatory, so a snippet that set
    /// only `disabled_tools` failed to parse — which is what the offline setup
    /// in the README told people to write.
    #[serde(default = "default_auto_approve_reads")]
    pub auto_approve_reads: bool,
    #[serde(default)]
    pub auto_approve_writes: bool,
    /// Legacy global switch for providers that expose thinking controls.
    /// false maps OpenAI-compatible endpoints to enable_thinking=false unless
    /// that endpoint has an explicit reasoning override.
    #[serde(default = "default_thinking_mode")]
    pub thinking_mode: bool,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default = "default_max_history_messages")]
    pub max_history_messages: usize,
    /// Dead since the trigger became token-based; kept only so existing config
    /// files that name it still parse. `compact_at_percent` is the live knob.
    ///
    /// Defaulted for the same reason as the fields above, and more sharply: a
    /// key that does nothing has no business being mandatory.
    #[serde(default = "default_compaction_threshold")]
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
    #[serde(default)]
    pub scratchpad: ScratchpadConfig,
    /// Tool names to exclude from every agent turn. Internal tools are never affected.
    ///
    /// Empty by default. `web_search` used to be in here, because it scraped
    /// DuckDuckGo's HTML and usually came back empty, and a tool that usually
    /// returns nothing is worse than one that is absent — the model spends a
    /// turn on it and then reasons about the emptiness. The note said "off
    /// until there is a real search behind it"; there is one now, an index
    /// Forge crawls itself, so the condition is met and the default is off the
    /// list.
    ///
    /// That only reaches installs that have never written a config. The app
    /// serialises this field, so anyone who has run Forge before has
    /// `disabled_tools = ["web_search"]` on disk already, and it is not
    /// distinguishable from somebody who chose it — so it is kept. The tools
    /// menu in either client turns it back on.
    ///
    /// `web_fetch` and `search_papers` are on for the same reason: both
    /// retrieve something they have been asked for rather than depending on a
    /// search working.
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

fn default_auto_approve_reads() -> bool {
    true
}

fn default_max_history_messages() -> usize {
    200
}

fn default_compaction_threshold() -> usize {
    150
}

fn default_compact_at_percent() -> u8 {
    80
}

/// Tools off unless asked for. See `AgentConfig::disabled_tools`.
/// Tools that reach the network and are not part of making a model call.
///
/// `offline_mode` forces these off. `search_papers` belongs here and was
/// missing: it is enabled by default and reaches Europe PMC at
/// `www.ebi.ac.uk`, so "no outgoing request except to the endpoint you
/// configured" was false for anyone who left it on.
pub const NETWORK_TOOLS: [&str; 3] = ["web_search", "web_fetch", "search_papers"];

impl AgentConfig {
    /// Every tool this session may not use: the operator's own list, plus
    /// the network tools when offline.
    ///
    /// Centralised because it was applied in one place and needed in two. The
    /// top-level agent filtered its own tool list and subagents did not, so a
    /// delegated `general` agent — whose definition lists `web_search` and
    /// `web_fetch` — kept live web tools with `offline_mode = true`, and
    /// ignored `disabled_tools` as well. A guarantee that holds for the agent
    /// you can see and not the ones it spawns is not a guarantee.
    pub fn effective_disabled_tools(&self) -> Vec<String> {
        let mut disabled = self.disabled_tools.clone();
        if self.offline_mode {
            for t in NETWORK_TOOLS {
                if !disabled.iter().any(|d| d == t) {
                    disabled.push(t.to_string());
                }
            }
        }
        disabled
    }
}

fn default_disabled_tools() -> Vec<String> {
    Vec::new()
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

/// The agent's own working area — see `tools::scratchpad`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchpadConfig {
    /// Whether the agent is given a lab at all. Off means it behaves exactly as
    /// it did before there was one.
    #[serde(default = "default_scratchpad_on")]
    pub enabled: bool,
    /// Whether writing inside the lab skips the approval prompt.
    ///
    /// On by default: nothing in the lab is the user's, and a scratch area that
    /// asks permission for every throwaway file is not a scratch area. Turning
    /// this off keeps the lab and its cleanup, and approves writes to it the
    /// same way as writes anywhere else.
    ///
    /// It only ever exempts writes *into* the lab. Copying a file out of it
    /// into a real directory is an ordinary write and is approved like one, and
    /// `shell_exec` is unaffected either way.
    #[serde(default = "default_scratchpad_on")]
    pub auto_approve_writes: bool,
    /// How long a lab survives without being touched.
    #[serde(default = "default_scratchpad_keep_days")]
    pub keep_days: u64,
}

fn default_scratchpad_keep_days() -> u64 {
    7
}

/// For scratchpad switches that are on unless a config says otherwise.
fn default_scratchpad_on() -> bool {
    true
}

impl Default for ScratchpadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_approve_writes: true,
            keep_days: default_scratchpad_keep_days(),
        }
    }
}

impl ScratchpadConfig {
    /// The age at which an untouched lab is swept.
    ///
    /// `0` would mean "delete every lab the moment it is looked at", including
    /// the one in use, so it is read as "never sweep" instead.
    pub fn keep(&self) -> Option<std::time::Duration> {
        (self.keep_days > 0)
            .then(|| std::time::Duration::from_secs(self.keep_days * 24 * 60 * 60))
    }
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
                send_etag: false,
                web_bot_auth_key: None,
                web_bot_auth_directory: None,
                auto_approve_reads: true,
                auto_approve_writes: false,
                thinking_mode: true,
                permission_mode: PermissionMode::Default,
                max_history_messages: 200,
                compaction_threshold: 150,
                compact_at_percent: default_compact_at_percent(),
                subagents: SubagentConfig::default(),
                scratchpad: ScratchpadConfig::default(),
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
        // Explicit profiles also allow failure tests to stay isolated on Windows,
        // where Known Folder APIs intentionally ignore HOME/USERPROFILE overrides.
        if let Some(path) = std::env::var_os("FORGE_CONFIG_FILE").filter(|p| !p.is_empty()) {
            return Ok(PathBuf::from(path));
        }
        let home = dirs::home_dir().context("Could not find home directory")?;
        Ok(home.join(".config").join("forge").join("config.toml"))
    }

    pub fn get_endpoint(&self, name: &str) -> Option<&ModelEndpoint> {
        self.models.endpoints.iter().find(|e| e.name == name)
    }

    pub fn default_endpoint(&self) -> Option<&ModelEndpoint> {
        self.get_endpoint(&self.models.default)
    }

    /// The endpoint to actually use, and a note when it is not the one named.
    ///
    /// `default` is a name written to this file, and the endpoint list is
    /// discovered — the ChatGPT Codex pass adds newly available models and
    /// prunes ones that have gone, persisting both. A stored name against a
    /// list that changes underneath it will dangle sooner or later, so that is
    /// a case to handle rather than an accident to report.
    ///
    /// Observed: a config naming `GPT-6-Astra` as its default against a list
    /// of thirty-seven endpoints containing `gpt-6-astra-wm` and no exact
    /// match. Forge refused to start at all, having written that state itself.
    ///
    /// The ladder is deliberately short. An exact match, then the same name in
    /// a different case — which is the same endpoint, spelled differently.
    /// Then any endpoint at all, with a loud note, because a working model the
    /// user did not pick is better than an agent that will not launch and it
    /// is one line to correct.
    ///
    /// It does *not* guess by prefix. `gpt-6-astra` is a prefix of
    /// `gpt-6-astra-wm` and might well be the intended model, but it might
    /// equally be a different tier at a different price, and silently billing
    /// somebody for a model they did not choose is worse than telling them to
    /// choose.
    ///
    /// Nothing is written back. The file is the user's, and a config that
    /// silently repairs itself hides the thing they need to fix.
    pub fn resolve_default(&self) -> Option<(&ModelEndpoint, Option<String>)> {
        let wanted = &self.models.default;
        if let Some(exact) = self.get_endpoint(wanted) {
            return Some((exact, None));
        }
        if let Some(cased) = self
            .models
            .endpoints
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(wanted))
        {
            return Some((
                cased,
                Some(format!(
                    "config names the default endpoint '{wanted}'; using '{}', which differs \
                     only in case",
                    cased.name,
                )),
            ));
        }
        let picked = pick_newest(&self.models.endpoints)?;
        let named = if wanted.is_empty() {
            "no default endpoint is set".to_string()
        } else {
            format!("the default endpoint '{wanted}' is not in the config")
        };
        Some((
            picked,
            Some(format!(
                "{named}; using '{}' — the highest version number available. \
                 Change it with /model, or set `default` in ~/.config/forge/config.toml.",
                picked.name,
            )),
        ))
    }
}

/// The endpoint with the highest version number in its name, ties broken
/// alphabetically.
///
/// Installation does not have to choose a default and a stale one does not have
/// to stop anything: there is always a defensible pick, and the user changes it
/// with `/model` whenever they like.
///
/// Numbers are compared component by component rather than by taking the
/// largest in the name, which matters because a name can carry a number that is
/// not a version: `Claude Opus 4.6 (200k)` reads as 4, then 6, then 200 — a
/// context size, and taking the maximum would rank it above every GPT-6. First
/// component first makes `gpt-6-pro` beat `gpt-5-6-mini`, which is the ordering
/// anyone would expect.
///
/// A name with no digits sorts last, since there is nothing to claim it is new.
///
/// Ties go to the alphabetically lower name, compared without case so `Grok`
/// and `grok` order the same way. That is not a claim about which is better —
/// it is a claim that the choice must be the same on every run, because a
/// default that moves around is worse than one that is merely arbitrary.
fn pick_newest(endpoints: &[ModelEndpoint]) -> Option<&ModelEndpoint> {
    let mut ranked: Vec<&ModelEndpoint> = endpoints.iter().collect();
    ranked.sort_by(|a, b| {
        version_of(&b.name)
            .cmp(&version_of(&a.name))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    ranked.first().copied()
}

/// The runs of digits in a name, in order — `gpt-5-6-mini` is `[5, 6]`.
fn version_of(name: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut digits = String::new();
    for ch in name.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !digits.is_empty() {
            out.push(digits.parse().unwrap_or(0));
            digits.clear();
        }
    }
    if !digits.is_empty() {
        out.push(digits.parse().unwrap_or(0));
    }
    out
}

#[cfg(test)]
mod default_endpoint_tests {
    use super::*;

    fn endpoints(names: &[&str]) -> Vec<ModelEndpoint> {
        names
            .iter()
            .map(|n| ModelEndpoint {
                name: (*n).to_string(),
                base_url: "http://localhost:1/v1".to_string(),
                model_id: "auto".to_string(),
                api_key: None,
                max_context_tokens: 8192,
                max_output_tokens: 1024,
                request_timeout_secs: 60,
                endpoint_type: EndpointType::default(),
                reasoning: EndpointReasoningConfig::default(),
                xai_priority_tier: false,
            })
            .collect()
    }

    fn config(default: &str, names: &[&str]) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.models.default = default.to_string();
        cfg.models.endpoints = endpoints(names);
        cfg
    }

    #[test]
    fn an_exact_name_is_used_without_comment() {
        let cfg = config("gpt-6-pro", &["gpt-5-5", "gpt-6-pro"]);
        let (ep, note) = cfg.resolve_default().unwrap();
        assert_eq!(ep.name, "gpt-6-pro");
        assert!(note.is_none());
    }

    /// The same endpoint spelled in a different case is the same endpoint.
    #[test]
    fn a_case_difference_still_resolves() {
        let cfg = config("GPT-6-Pro", &["gpt-6-pro"]);
        let (ep, note) = cfg.resolve_default().unwrap();
        assert_eq!(ep.name, "gpt-6-pro");
        assert!(note.unwrap().contains("differs only in case"));
    }

    /// The case that stopped Forge starting: a default naming an endpoint that
    /// discovery has since pruned, against a list of thirty-seven others.
    #[test]
    fn a_dangling_default_falls_back_to_the_highest_version() {
        let cfg = config(
            "GPT-6-Astra",
            &["gpt-5-5", "gpt-5-6-mini", "gpt-6-astra-wm", "o3-pro", "research"],
        );
        let (ep, note) = cfg.resolve_default().unwrap();
        assert_eq!(ep.name, "gpt-6-astra-wm");
        let note = note.unwrap();
        assert!(note.contains("not in the config"), "{note}");
        assert!(note.contains("/model"), "it should say how to change it: {note}");
    }

    /// Installation does not have to pick one.
    #[test]
    fn no_default_at_all_is_not_an_error() {
        let cfg = config("", &["gpt-5-5", "gpt-6-pro"]);
        let (ep, note) = cfg.resolve_default().unwrap();
        assert_eq!(ep.name, "gpt-6-pro");
        assert!(note.unwrap().contains("no default endpoint is set"));
    }

    /// Version numbers compare component by component. Taking the largest
    /// number in the name would rank a context size above a version: the
    /// `(200k)` in `Claude Opus 4.6 (200k)` is not a version 200.
    #[test]
    fn a_context_size_is_not_mistaken_for_a_version() {
        let cfg = config("", &["Claude Opus 4.6 (200k)", "gpt-6-pro"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "gpt-6-pro");
    }

    #[test]
    fn the_first_component_decides_before_the_second() {
        let cfg = config("", &["gpt-5-6-mini", "gpt-6-pro"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "gpt-6-pro");
    }

    /// The same thing in the naming that actually occurs, where the version is
    /// written with a dot: a 6 must beat a 5.6, not lose to it for having
    /// fewer digits.
    #[test]
    fn a_major_version_beats_a_higher_minor_of_a_lower_major() {
        let cfg = config(
            "",
            &[
                "GPT-5.5",
                "GPT-5.6-Luna",
                "GPT-5.6-Sol",
                "GPT-5.6-Terra",
                "GPT-6-Astra",
                "Grok 4.20 Reasoning",
                "Claude Opus 4.6 (200k)",
                "GPT-Reserve",
            ],
        );
        assert_eq!(cfg.resolve_default().unwrap().0.name, "GPT-6-Astra");
    }

    /// And a release number is not a decimal: 4.20 is the twentieth release
    /// after 4, so it is newer than 4.6 rather than older.
    #[test]
    fn a_release_number_is_not_a_decimal() {
        let cfg = config("", &["Grok 4.6", "Grok 4.20 Reasoning"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "Grok 4.20 Reasoning");
    }

    /// A wrinkle worth pinning rather than pretending away. Component-wise
    /// comparison stops a context size outranking a major version — the `200`
    /// in `Claude Opus 4.6 (200k)` cannot beat a 5 or a 6 — but between two
    /// endpoints whose versions are otherwise identical it still decides, so
    /// the 200k variant sorts above the plain one. That is a defensible
    /// outcome (more context) reached for an undignified reason, and it is
    /// here so a later change notices it.
    #[test]
    fn a_trailing_number_still_breaks_a_tie_between_equal_versions() {
        let cfg = config("", &["Claude Sonnet 4.6", "Claude Sonnet 4.6 (200k)"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "Claude Sonnet 4.6 (200k)");
        // But it cannot climb past a genuinely higher version.
        let beaten = config("", &["Claude Sonnet 4.6 (200k)", "GPT-6-Astra"]);
        assert_eq!(beaten.resolve_default().unwrap().0.name, "GPT-6-Astra");
    }

    /// Ties go alphabetically, without case, so the pick is the same on every
    /// run — a default that moves around is worse than an arbitrary one.
    #[test]
    fn a_tie_is_broken_by_the_lower_name() {
        let cfg = config("", &["zeta-6", "Alpha-6", "middle-6"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "Alpha-6");
        // And the same answer whatever order they arrive in.
        let other = config("", &["middle-6", "zeta-6", "Alpha-6"]);
        assert_eq!(other.resolve_default().unwrap().0.name, "Alpha-6");
    }

    /// A name with no digits has nothing to claim it is new, so it sorts last
    /// — but is still picked when it is all there is.
    #[test]
    fn a_name_without_digits_sorts_last_but_is_still_usable() {
        let cfg = config("", &["research", "gpt-5-5"]);
        assert_eq!(cfg.resolve_default().unwrap().0.name, "gpt-5-5");
        let only = config("", &["research"]);
        assert_eq!(only.resolve_default().unwrap().0.name, "research");
    }

    /// Nothing configured is the one case no fallback can rescue.
    #[test]
    fn no_endpoints_at_all_resolves_to_nothing() {
        let cfg = config("anything", &[]);
        assert!(cfg.resolve_default().is_none());
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
    /// `web_search` was disabled by default for as long as it scraped
    /// DuckDuckGo and usually came back empty. It is now an index Forge crawls
    /// itself, which answers, so the reason is gone and so is the default.
    #[test]
    fn web_search_is_on_now_that_it_is_a_real_index() {
        let cfg = AppConfig::default();
        assert!(
            !cfg.agent.disabled_tools.iter().any(|t| t == "web_search"),
            "disabled by default: {:?}",
            cfg.agent.disabled_tools,
        );
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

    /// A config that does not mention the setting gets the current default.
    ///
    /// Worth being precise about what this does and does not reach: the app
    /// serialises `disabled_tools`, so a file written by any previous version
    /// names `web_search` explicitly and keeps it. Only a config from before
    /// the field existed — or a fresh install — takes the default. Changing a
    /// default is not a migration, and treating it as one would be the wrong
    /// claim to make here.
    #[test]
    fn a_config_without_the_setting_gets_the_default() {
        assert!(config_with(None).agent.disabled_tools.is_empty());
    }

    /// And the case above, stated as the limitation it is: an existing file
    /// that disables web search still disables it.
    #[test]
    fn an_existing_file_keeps_web_search_disabled() {
        let old = config_with(Some("disabled_tools = [\"web_search\"]"));
        assert_eq!(old.agent.disabled_tools, vec!["web_search".to_string()],
                   "a choice already on disk must not be quietly reversed");
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

#[cfg(test)]
mod scratchpad_config_tests {
    use super::{AgentConfig, ScratchpadConfig};

    #[test]
    fn the_defaults_are_on_and_a_week() {
        let c = ScratchpadConfig::default();
        assert!(c.enabled);
        assert!(c.auto_approve_writes);
        assert_eq!(c.keep_days, 7);
        assert_eq!(c.keep(), Some(std::time::Duration::from_secs(7 * 86400)));
    }

    /// Zero would otherwise mean "sweep every lab on sight", including the one
    /// currently in use.
    #[test]
    fn zero_days_means_never_sweep() {
        let c = ScratchpadConfig { keep_days: 0, ..Default::default() };
        assert_eq!(c.keep(), None);
    }

    /// A config file written before the scratchpad existed must still parse,
    /// and must come up with the feature in its default state.
    #[test]
    fn an_older_config_file_still_loads() {
        let toml = r#"
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 50
compaction_threshold = 100
"#;
        let c: AgentConfig = toml::from_str(toml).expect("older config should still parse");
        assert!(c.scratchpad.enabled);
        assert_eq!(c.scratchpad.keep_days, 7);
    }
}
#[cfg(test)]
mod offline_tool_tests {
    use super::*;

    fn cfg(offline: bool, disabled: &[&str]) -> AgentConfig {
        let mut a = AppConfig::default().agent;
        a.offline_mode = offline;
        a.disabled_tools = disabled.iter().map(|s| s.to_string()).collect();
        a
    }

    /// Every tool that reaches the network goes off when offline.
    ///
    /// `search_papers` was missing from this list. It is enabled by default
    /// and reaches Europe PMC at `www.ebi.ac.uk`, so the README's "no
    /// outgoing request except to the endpoint you configured" was false for
    /// anyone who left it on — which is everyone, since the default disabled
    /// list is empty.
    #[test]
    fn offline_mode_stops_every_tool_that_leaves_the_machine() {
        let disabled = cfg(true, &[]).effective_disabled_tools();
        for tool in ["web_search", "web_fetch", "search_papers"] {
            assert!(
                disabled.iter().any(|d| d == tool),
                "{tool} reaches the network and survives offline mode: {disabled:?}"
            );
        }
    }

    #[test]
    fn online_is_left_alone() {
        assert!(cfg(false, &[]).effective_disabled_tools().is_empty());
        // And the operator's own list is preserved either way.
        assert_eq!(cfg(false, &["shell_exec"]).effective_disabled_tools(), vec!["shell_exec"]);
        let both = cfg(true, &["shell_exec"]).effective_disabled_tools();
        assert!(both.iter().any(|d| d == "shell_exec"));
        assert!(both.iter().any(|d| d == "web_search"));
    }

    /// A tool named twice is disabled once.
    #[test]
    fn an_explicitly_disabled_network_tool_is_not_listed_twice() {
        let disabled = cfg(true, &["web_search"]).effective_disabled_tools();
        assert_eq!(
            disabled.iter().filter(|d| *d == "web_search").count(),
            1,
            "{disabled:?}"
        );
    }
}
#[cfg(test)]
mod documented_config_parses {
    use super::*;

    /// Every complete `toml` block in the README that looks like a Forge
    /// config must actually load.
    ///
    /// Two of them did not. `[agent]` had four members with no serde default,
    /// so naming the table at all made all four mandatory: the "Multiple
    /// endpoints" example failed with `missing field max_history_messages`,
    /// and the offline-setup snippet — two lines, `[agent]` and
    /// `disabled_tools` — failed with `missing field auto_approve_reads`.
    /// Both were presented as things to copy.
    ///
    /// Reads the README rather than restating its examples, so the two cannot
    /// drift apart.
    #[test]
    fn every_config_example_in_the_readme_loads() {
        let readme = include_str!("../README.md");
        let mut checked = 0;
        let mut failures = Vec::new();

        for block in readme.split("```toml").skip(1) {
            let Some(body) = block.split("```").next() else { continue };

            // Only whole-file examples. A fragment showing one key under a
            // header it does not include is prose, not a file.
            if !body.contains("[[models.endpoints]]") && !body.contains("[agent]") {
                continue;
            }
            // Snippets written as an edit to an existing table are explicitly
            // not standalone files; the README says so where it shows them.
            let standalone = body.contains("[models]") || body.contains("[[models.endpoints]]");
            if !standalone {
                continue;
            }

            checked += 1;
            if let Err(e) = toml::from_str::<AppConfig>(body) {
                failures.push(format!("{e}\n--- block ---\n{body}"));
            }
        }

        assert!(checked >= 2, "expected to find the README's config examples, found {checked}");
        assert!(
            failures.is_empty(),
            "{} of {checked} documented config examples do not parse:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }

    /// The offline setup the README tells people to write.
    ///
    /// Presented as an edit to an existing `[agent]` table, but it has to work
    /// on its own too — a reader who creates the file from it should get a
    /// working config, not a parse error naming a field they were never shown.
    #[test]
    fn the_offline_snippet_loads_on_its_own() {
        let c: AgentConfig = toml::from_str(
            "disabled_tools = [\"web_search\", \"web_fetch\", \"search_papers\"]",
        )
        .expect("the documented offline snippet should load on its own");
        assert_eq!(c.disabled_tools.len(), 3);
        // And the defaults that filled in are the ones the table documents.
        assert!(c.auto_approve_reads);
        assert!(!c.auto_approve_writes);
        assert_eq!(c.max_history_messages, 200);
        assert_eq!(c.compact_at_percent, 80);
    }
}


