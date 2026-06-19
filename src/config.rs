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
    pub compaction_threshold: usize,
    #[serde(default)]
    pub subagents: SubagentConfig,
    /// Tool names to exclude from every agent turn. Internal tools are never affected.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    #[serde(default)]
    pub context_strategy: ContextStrategy,
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
}

fn default_max_concurrent() -> usize {
    4
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: 4,
            max_concurrent: 4,
            default_model: None,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            models: ModelsConfig {
                endpoints: vec![
                    ModelEndpoint {
                        name: "local-80b".to_string(),
                        base_url: "http://127.0.0.1:1234/v1".to_string(),
                        model_id: "default".to_string(),
                        api_key: None,
                        max_context_tokens: 262144,
                        max_output_tokens: 16384,
                        request_timeout_secs: 500,
                        endpoint_type: EndpointType::OpenAi,
                        reasoning: EndpointReasoningConfig::default(),
                    },
                    ModelEndpoint {
                        name: "local-30b".to_string(),
                        base_url: "http://127.0.0.1:1235/v1".to_string(),
                        model_id: "default".to_string(),
                        api_key: None,
                        max_context_tokens: 136069,
                        max_output_tokens: 16384,
                        request_timeout_secs: 500,
                        endpoint_type: EndpointType::OpenAi,
                        reasoning: EndpointReasoningConfig::default(),
                    },
                ],
                default: "local-80b".to_string(),
                web_tool_model: None,
            },
            agent: AgentConfig {
                auto_approve_reads: true,
                auto_approve_writes: false,
                thinking_mode: true,
                permission_mode: PermissionMode::Default,
                max_history_messages: 200,
                compaction_threshold: 150,
                subagents: SubagentConfig::default(),
                disabled_tools: vec![],
                context_strategy: ContextStrategy::Compaction,
            },
        }
    }
}

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
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, contents)?;
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
