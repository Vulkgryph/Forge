// SPDX-License-Identifier: Apache-2.0
use crate::api::types::*;
use crate::config::{
    AgentConfig, ChatGptReasoningEffort, EndpointReasoningConfig, EndpointType, ModelEndpoint,
    ProviderToggle,
};
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc;

// Beta features required for OAuth-based Claude Code subscriptions
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

enum Backend {
    OpenAi {
        base_url: String,
    },
    Anthropic {
        base_url: String,
        auth_token: String,
    },
    ChatGptCodex {
        base_url: String,
        access_token: String,
        account_id: Option<String>,
    },
}

#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    backend: std::sync::Arc<Backend>,
    max_output_tokens: u32,
    reasoning: EndpointReasoningConfig,
    /// Forge session ID sent as X-Forge-Session header to Oxide, allowing the
    /// inference server to scope its KV cache to one conversation at a time.
    pub forge_session_id: Option<String>,
}

impl Clone for Backend {
    fn clone(&self) -> Self {
        match self {
            Backend::OpenAi { base_url } => Backend::OpenAi {
                base_url: base_url.clone(),
            },
            Backend::Anthropic {
                base_url,
                auth_token,
            } => Backend::Anthropic {
                base_url: base_url.clone(),
                auth_token: auth_token.clone(),
            },
            Backend::ChatGptCodex {
                base_url,
                access_token,
                account_id,
            } => Backend::ChatGptCodex {
                base_url: base_url.clone(),
                access_token: access_token.clone(),
                account_id: account_id.clone(),
            },
        }
    }
}

/// Events emitted by the streaming API methods.
pub enum StreamEvent {
    /// A text token from the model.
    Token(String),
    /// Hidden provider reasoning was observed and intentionally not forwarded.
    Reasoning,
    /// A complete tool call (assembled from streaming deltas).
    ToolCall(ToolCall),
    /// Stream finished successfully.
    Done { usage: Option<Usage> },
    /// Stream ended with an error.
    Error(String),
}

struct ThinkBlockFilter {
    in_think: bool,
    pending: String,
}

impl ThinkBlockFilter {
    fn new() -> Self {
        Self {
            in_think: false,
            pending: String::new(),
        }
    }

    fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);
        let mut output = String::new();

        loop {
            if self.in_think {
                if let Some(end) = find_ci(&self.pending, "</think>") {
                    self.pending.drain(..end + "</think>".len());
                    self.in_think = false;
                    continue;
                }
                let keep = self.pending.len().min("</think>".len() - 1);
                let drop_len = self.pending.len().saturating_sub(keep);
                self.pending.drain(..drop_len);
                break;
            }

            if let Some(start) = find_ci(&self.pending, "<think>") {
                output.push_str(&self.pending[..start]);
                self.pending.drain(..start + "<think>".len());
                self.in_think = true;
                continue;
            }

            let keep = longest_suffix_prefix(&self.pending, "<think>");
            let emit_len = self.pending.len().saturating_sub(keep);
            output.push_str(&self.pending[..emit_len]);
            self.pending.drain(..emit_len);
            break;
        }

        output
    }

    fn finish(mut self) -> String {
        if self.in_think {
            String::new()
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

fn longest_suffix_prefix(value: &str, pattern: &str) -> usize {
    let value_bytes = value.as_bytes();
    let pattern_bytes = pattern.as_bytes();
    let max = value_bytes.len().min(pattern_bytes.len().saturating_sub(1));
    for len in (1..=max).rev() {
        let suffix = &value_bytes[value_bytes.len() - len..];
        if pattern_bytes[..len].eq_ignore_ascii_case(suffix) {
            return len;
        }
    }
    0
}

fn strip_think_blocks(text: &str) -> String {
    let mut filter = ThinkBlockFilter::new();
    let mut out = filter.push(text);
    out.push_str(&filter.finish());
    out
}

impl ApiClient {
    /// Build an ApiClient from a config endpoint + optional OAuth token.
    /// Pass `auth_token` for Anthropic endpoints (loaded from auth.json at startup).
    pub fn from_endpoint(endpoint: &ModelEndpoint, auth_token: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(endpoint.request_timeout_secs))
            .build()
            .expect("Failed to build HTTP client");

        let backend = match endpoint.endpoint_type {
            EndpointType::Anthropic => {
                let token = auth_token
                    .or_else(|| endpoint.api_key.clone())
                    .unwrap_or_default();
                Backend::Anthropic {
                    base_url: endpoint.base_url.clone(),
                    auth_token: token,
                }
            }
            EndpointType::ChatGptCodex => {
                let mut tokens =
                    crate::auth::load_chatgpt_tokens().unwrap_or(crate::auth::ChatGptTokens {
                        id_token: String::new(),
                        access_token: endpoint.api_key.clone().unwrap_or_default(),
                        refresh_token: String::new(),
                        api_key: endpoint.api_key.clone(),
                        account_id: None,
                        plan_type: None,
                        expires_at: None,
                    });
                if let Some(access_token) = auth_token {
                    tokens.access_token = access_token;
                }
                Backend::ChatGptCodex {
                    base_url: CHATGPT_CODEX_BASE_URL.to_string(),
                    access_token: tokens.access_token,
                    account_id: tokens.account_id,
                }
            }
            EndpointType::OpenAi => Backend::OpenAi {
                base_url: endpoint.base_url.clone(),
            },
        };

        Self {
            client,
            backend: std::sync::Arc::new(backend),
            max_output_tokens: endpoint.max_output_tokens,
            reasoning: endpoint.reasoning.clone(),
            forge_session_id: None,
        }
    }

    pub fn apply_agent_reasoning_defaults(&mut self, agent_config: &AgentConfig) {
        if !agent_config.thinking_mode
            && self.reasoning.open_ai_compatible.thinking == ProviderToggle::ProviderDefault
        {
            self.reasoning.open_ai_compatible.thinking = ProviderToggle::Off;
        }
    }

    pub fn without_forge_session(mut self) -> Self {
        self.forge_session_id = None;
        self
    }

    pub fn with_forge_session_suffix(mut self, suffix: &str) -> Self {
        if let Some(base) = &self.forge_session_id {
            self.forge_session_id = Some(format!("{base}:{suffix}"));
        }
        self
    }

    /// Resolve "auto" model IDs: query /v1/models and return the first model's ID.
    /// Returns None if the endpoint is unreachable or returns no models.
    /// Only applies to OpenAI-compatible endpoints (Anthropic uses its own detection).
    pub async fn resolve_auto_model_id(&self) -> Option<String> {
        let base_url = match self.backend.as_ref() {
            Backend::OpenAi { base_url } => base_url,
            _ => return None,
        };
        let url = format!("{}/models", base_url);
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.client.get(&url).send(),
        )
        .await
        .ok()?
        .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        let first = json.get("data")?.as_array()?.first()?;
        first.get("id")?.as_str().map(|s| s.to_string())
    }

    /// Non-streaming chat — used by compaction, web summarizer, subagents.
    pub async fn chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, String> {
        match self.backend.as_ref() {
            Backend::OpenAi { base_url } => {
                self.chat_openai(base_url, model, messages, tools).await
            }
            Backend::Anthropic {
                base_url,
                auth_token,
            } => {
                self.chat_anthropic(base_url, auth_token, model, messages, tools)
                    .await
            }
            Backend::ChatGptCodex {
                base_url,
                access_token,
                account_id,
            } => {
                self.chat_responses(
                    base_url,
                    access_token,
                    account_id.as_deref(),
                    model,
                    messages,
                    tools,
                )
                .await
            }
        }
    }

    /// Streaming chat — sends `StreamEvent`s over `tx` as tokens arrive.
    /// Used by the main agent loop for the primary response.
    pub async fn chat_stream(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) {
        match self.backend.as_ref() {
            Backend::OpenAi { base_url } => {
                self.chat_stream_openai(base_url, model, messages, tools, tx)
                    .await;
            }
            Backend::Anthropic {
                base_url,
                auth_token,
            } => {
                self.chat_stream_anthropic(base_url, auth_token, model, messages, tools, tx)
                    .await;
            }
            Backend::ChatGptCodex {
                base_url,
                access_token,
                account_id,
            } => {
                self.chat_stream_responses(
                    base_url,
                    access_token,
                    account_id.as_deref(),
                    model,
                    messages,
                    tools,
                    tx,
                )
                .await;
            }
        }
    }

    fn apply_openai_compatible_reasoning(&self, body: &mut serde_json::Value) {
        let cfg = &self.reasoning.open_ai_compatible;
        let mut kwargs = serde_json::Map::new();

        match cfg.thinking {
            ProviderToggle::On => {
                kwargs.insert("enable_thinking".to_string(), serde_json::Value::Bool(true));
            }
            ProviderToggle::Off => {
                kwargs.insert(
                    "enable_thinking".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            ProviderToggle::ProviderDefault => {}
        }

        match cfg.preserve_thinking {
            ProviderToggle::On => {
                kwargs.insert(
                    "preserve_thinking".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            ProviderToggle::Off => {
                kwargs.insert(
                    "preserve_thinking".to_string(),
                    serde_json::Value::Bool(false),
                );
            }
            ProviderToggle::ProviderDefault => {}
        }

        if !kwargs.is_empty() {
            body["chat_template_kwargs"] = serde_json::Value::Object(kwargs);
        }
    }

    fn apply_anthropic_reasoning(&self, body: &mut serde_json::Value) {
        let cfg = &self.reasoning.anthropic;
        if cfg.thinking == ProviderToggle::On {
            if self.max_output_tokens <= 1024 {
                return;
            }
            let budget_tokens = cfg.budget_tokens.min(self.max_output_tokens - 1);
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget_tokens.max(1024),
            });
        }
    }

    // ── OpenAI-compatible backend ────────────────────────────────────────────

    async fn chat_openai(
        &self,
        base_url: &str,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, String> {
        let mut request = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": 0.7,
            "max_tokens": self.max_output_tokens,
            "stream": false,
        });
        if !tools.is_empty() {
            request["tools"] = serde_json::json!(tools);
            request["parallel_tool_calls"] = serde_json::json!(true);
        }
        self.apply_openai_compatible_reasoning(&mut request);

        let mut req = self
            .client
            .post(format!("{}/chat/completions", base_url))
            .json(&request);
        if let Some(sid) = &self.forge_session_id {
            req = req.header("x-forge-session", sid.as_str());
        }
        let response = req
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("API error: {}", response.status()));
        }

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("JSON error: {}", e))?;
        let mut chat_response: ChatResponse =
            serde_json::from_value(json).map_err(|e| format!("Parse error: {}", e))?;
        if self.reasoning.open_ai_compatible.preserve_thinking != ProviderToggle::On {
            for choice in &mut chat_response.choices {
                if let Some(content) = choice.message.content.as_mut() {
                    *content = strip_think_blocks(content);
                }
            }
        }

        Ok(chat_response)
    }

    async fn chat_stream_openai(
        &self,
        base_url: &str,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) {
        use futures_util::StreamExt;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "temperature": 0.7,
            "max_tokens": self.max_output_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
            body["parallel_tool_calls"] = serde_json::json!(true);
        }
        self.apply_openai_compatible_reasoning(&mut body);

        let mut req = self
            .client
            .post(format!("{}/chat/completions", base_url))
            .header("accept", "text/event-stream")
            .header("accept-encoding", "identity")
            .json(&body);
        if let Some(sid) = &self.forge_session_id {
            req = req.header("x-forge-session", sid.as_str());
        }
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(format!("Network error: {}", e)));
                return;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            let _ = tx.send(StreamEvent::Error(format!(
                "API error ({}): {}",
                status, body_text
            )));
            return;
        }

        // index → (id, name, accumulated_args)
        let mut tool_call_map: std::collections::HashMap<u32, (String, String, String)> =
            std::collections::HashMap::new();
        let mut usage: Option<Usage> = None;
        let mut think_filter = (self.reasoning.open_ai_compatible.preserve_thinking
            != ProviderToggle::On)
            .then(ThinkBlockFilter::new);
        let mut reasoning_emitted = false;

        let stream = tokio_util::io::StreamReader::new(
            response
                .bytes_stream()
                .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        );
        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!(
                        "OpenAI-compatible stream read error: {}",
                        e
                    )));
                    break;
                }
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let data = match trimmed.strip_prefix("data: ") {
                Some(d) => d,
                None => continue,
            };
            if data == "[DONE]" {
                break;
            }

            let chunk: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Capture streaming usage if present
            if let Some(u) = chunk.get("usage").and_then(|v| v.as_object()) {
                usage = Some(Usage {
                    prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                        as u32,
                    completion_tokens: u
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                        as u32,
                });
            }

            if let Some(choices) = chunk.get("choices").and_then(|v| v.as_array()) {
                for choice in choices {
                    let delta = &choice["delta"];

                    // Some local OpenAI-compatible servers stream internal
                    // reasoning in separate fields. Forge deliberately ignores
                    // those fields and only forwards visible assistant content.
                    let ignored_reasoning = delta
                        .get("reasoning_content")
                        .or_else(|| delta.get("reasoning"))
                        .or_else(|| delta.get("thinking"));
                    if !reasoning_emitted
                        && ignored_reasoning
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                    {
                        reasoning_emitted = true;
                        let _ = tx.send(StreamEvent::Reasoning);
                    }

                    // Text token
                    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                        let visible = if let Some(filter) = think_filter.as_mut() {
                            filter.push(text)
                        } else {
                            text.to_string()
                        };
                        if !visible.is_empty() {
                            let _ = tx.send(StreamEvent::Token(visible));
                        }
                    }

                    // Tool call deltas
                    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tcs {
                            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let entry = tool_call_map
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                entry.0 = id.to_string();
                            }
                            if let Some(name) =
                                tc.pointer("/function/name").and_then(|v| v.as_str())
                            {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) =
                                tc.pointer("/function/arguments").and_then(|v| v.as_str())
                            {
                                entry.2.push_str(args);
                            }
                        }
                    }

                    // When finish_reason arrives, emit accumulated tool calls
                    if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                        if reason == "tool_calls" || reason == "stop" {
                            let mut indices: Vec<u32> = tool_call_map.keys().copied().collect();
                            indices.sort();
                            for i in indices {
                                if let Some((id, name, args)) = tool_call_map.remove(&i) {
                                    let _ = tx.send(StreamEvent::ToolCall(ToolCall {
                                        id,
                                        call_type: "function".to_string(),
                                        function: FunctionCall {
                                            name,
                                            arguments: args,
                                        },
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(filter) = think_filter {
            let visible = filter.finish();
            if !visible.is_empty() {
                let _ = tx.send(StreamEvent::Token(visible));
            }
        }

        // Emit any tool calls not yet emitted (safety net)
        let mut indices: Vec<u32> = tool_call_map.keys().copied().collect();
        indices.sort();
        for i in indices {
            if let Some((id, name, args)) = tool_call_map.remove(&i) {
                let _ = tx.send(StreamEvent::ToolCall(ToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: args,
                    },
                }));
            }
        }

        let _ = tx.send(StreamEvent::Done { usage });
    }

    // ── Anthropic backend ────────────────────────────────────────────────────

    async fn chat_anthropic(
        &self,
        base_url: &str,
        auth_token: &str,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, String> {
        let (system_prompt, anthropic_messages) = convert_messages_to_anthropic(messages);
        let anthropic_tools = convert_tools_to_anthropic(tools);
        let is_oauth = auth_token.contains("sk-ant-oat");

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": self.max_output_tokens,
            "messages": anthropic_messages,
        });

        if is_oauth {
            let mut system_blocks = vec![
                serde_json::json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
            ];
            if let Some(sys) = system_prompt {
                system_blocks.push(serde_json::json!({"type": "text", "text": sys}));
            }
            body["system"] = serde_json::Value::Array(system_blocks);
        } else if let Some(sys) = system_prompt {
            body["system"] = serde_json::Value::String(sys);
        }
        if !anthropic_tools.is_empty() {
            body["tools"] = serde_json::Value::Array(anthropic_tools);
        }
        self.apply_anthropic_reasoning(&mut body);

        // On 401 with an OAuth token, refresh and retry once — Anthropic invalidates
        // tokens server-side before our local expiry.
        let mut current_token = auth_token.to_string();
        let mut retry = is_oauth;

        loop {
            let response = self
                .anthropic_request(base_url, &current_token, &body)
                .await
                .map_err(|e| format!("Network error: {}", e))?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && retry {
                retry = false;
                let http = reqwest::Client::new();
                match crate::auth::get_valid_token_force_refresh(&http).await {
                    Ok(new_token) => { current_token = new_token; continue; }
                    Err(e) => return Err(format!("Auth error (token refresh failed: {}). Run /login --anthropic to re-authenticate.", e)),
                }
            }

            if !response.status().is_success() {
                let status = response.status();
                let err_body = response.text().await.unwrap_or_default();
                return Err(format!("Anthropic API error ({}): {}", status, err_body));
            }

            let json: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("JSON error: {}", e))?;

            return convert_anthropic_response(json);
        }
    }

    async fn chat_stream_anthropic(
        &self,
        base_url: &str,
        auth_token: &str,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) {
        use futures_util::StreamExt;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let (system_prompt, anthropic_messages) = convert_messages_to_anthropic(messages);
        let anthropic_tools = convert_tools_to_anthropic(tools);
        let is_oauth = auth_token.contains("sk-ant-oat");

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": self.max_output_tokens,
            "messages": anthropic_messages,
            "stream": true,
        });

        if is_oauth {
            let mut system_blocks = vec![
                serde_json::json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}),
            ];
            if let Some(sys) = system_prompt {
                system_blocks.push(serde_json::json!({"type": "text", "text": sys}));
            }
            body["system"] = serde_json::Value::Array(system_blocks);
        } else if let Some(sys) = system_prompt {
            body["system"] = serde_json::Value::String(sys);
        }
        if !anthropic_tools.is_empty() {
            body["tools"] = serde_json::Value::Array(anthropic_tools);
        }
        self.apply_anthropic_reasoning(&mut body);

        // Build request with token refresh on 401
        let mut current_token = auth_token.to_string();
        let mut retry = is_oauth;

        let response = loop {
            let is_oauth_now = current_token.contains("sk-ant-oat");
            let mut req = self
                .client
                .post(format!("{}/v1/messages", base_url))
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("accept-encoding", "identity");

            if is_oauth_now {
                req = req
                    .header("authorization", format!("Bearer {}", current_token))
                    .header("anthropic-beta", ANTHROPIC_BETA)
                    .header("user-agent", format!("claude-cli/{}", crate::auth::claude_client_version().await))
                    .header("x-app", "cli")
                    .header("anthropic-dangerous-direct-browser-access", "true");
            } else {
                req = req.header("x-api-key", &current_token);
            }

            let resp = match req.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!("Network error: {}", e)));
                    return;
                }
            };

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && retry {
                retry = false;
                let http = reqwest::Client::new();
                match crate::auth::get_valid_token_force_refresh(&http).await {
                    Ok(new_token) => {
                        current_token = new_token;
                        continue;
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(format!("Auth error: {}", e)));
                        return;
                    }
                }
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let err_body = resp.text().await.unwrap_or_default();
                let _ = tx.send(StreamEvent::Error(format!(
                    "Anthropic API error ({}): {}",
                    status, err_body
                )));
                return;
            }

            break resp;
        };

        // Parse Anthropic SSE stream
        let stream = tokio_util::io::StreamReader::new(
            response
                .bytes_stream()
                .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        );
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut event_type = String::new();

        // content_block_index → (id, name, accumulated_json_args)
        let mut tool_blocks: std::collections::HashMap<u64, (String, String, String)> =
            std::collections::HashMap::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut reasoning_emitted = false;

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!(
                        "Anthropic stream read error: {}",
                        e
                    )));
                    break;
                }
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                event_type.clear();
                continue;
            }
            if let Some(ev) = trimmed.strip_prefix("event: ") {
                event_type = ev.to_string();
                continue;
            }
            let data = match trimmed.strip_prefix("data: ") {
                Some(d) => d,
                None => continue,
            };
            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event_type.as_str() {
                "message_start" => {
                    if let Some(tok) = json
                        .pointer("/message/usage/input_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        input_tokens = tok as u32;
                    }
                }
                "content_block_start" => {
                    let idx = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    let block = &json["content_block"];
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        tool_blocks.insert(idx, (id, name, String::new()));
                    }
                }
                "content_block_delta" => {
                    let idx = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    let delta = &json["delta"];
                    match delta.get("type").and_then(|v| v.as_str()) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    let _ = tx.send(StreamEvent::Token(text.to_string()));
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial) =
                                delta.get("partial_json").and_then(|v| v.as_str())
                            {
                                if let Some(block) = tool_blocks.get_mut(&idx) {
                                    block.2.push_str(partial);
                                }
                            }
                        }
                        Some("thinking_delta") | Some("signature_delta") => {
                            if !reasoning_emitted {
                                reasoning_emitted = true;
                                let _ = tx.send(StreamEvent::Reasoning);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let idx = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                    if let Some((id, name, args)) = tool_blocks.remove(&idx) {
                        // Only emit if this was a tool_use block (has a name)
                        if !name.is_empty() {
                            let _ = tx.send(StreamEvent::ToolCall(ToolCall {
                                id,
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name,
                                    arguments: args,
                                },
                            }));
                        }
                    }
                }
                "message_delta" => {
                    if let Some(tok) = json
                        .pointer("/usage/output_tokens")
                        .and_then(|v| v.as_u64())
                    {
                        output_tokens = tok as u32;
                    }
                }
                "message_stop" => {
                    break;
                }
                _ => {}
            }
        }

        let usage = Some(Usage {
            prompt_tokens: input_tokens,
            completion_tokens: output_tokens,
            total_tokens: input_tokens + output_tokens,
        });
        let _ = tx.send(StreamEvent::Done { usage });
    }

    async fn anthropic_request(
        &self,
        base_url: &str,
        token: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let is_oauth = token.contains("sk-ant-oat");
        let mut req = self
            .client
            .post(format!("{}/v1/messages", base_url))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "application/json");

        if is_oauth {
            let ua = format!("claude-cli/{}", crate::auth::claude_client_version().await);
            req = req
                .header("authorization", format!("Bearer {}", token))
                .header("anthropic-beta", ANTHROPIC_BETA)
                .header("user-agent", ua)
                .header("x-app", "cli")
                .header("anthropic-dangerous-direct-browser-access", "true");
        } else {
            req = req.header("x-api-key", token);
        }

        req.json(body).send().await
    }

    // ── Responses API backend (ChatGPT/Codex subscription auth) ──────────────

    async fn chat_responses(
        &self,
        base_url: &str,
        access_token: &str,
        account_id: Option<&str>,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.chat_stream_responses(
            base_url,
            access_token,
            account_id,
            model,
            messages,
            tools,
            tx,
        )
        .await;

        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = None;

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(text) => content.push_str(&text),
                StreamEvent::Reasoning => {}
                StreamEvent::ToolCall(tc) => tool_calls.push(tc),
                StreamEvent::Done { usage: done_usage } => {
                    usage = done_usage;
                    break;
                }
                StreamEvent::Error(err) => return Err(err),
            }
        }

        let message = if tool_calls.is_empty() {
            Message::assistant(&content)
        } else {
            Message::assistant_with_tools(
                if content.is_empty() {
                    None
                } else {
                    Some(content)
                },
                tool_calls,
            )
        };

        Ok(ChatResponse {
            id: String::new(),
            choices: vec![Choice {
                index: 0,
                message,
                finish_reason: Some("stop".to_string()),
            }],
            usage,
        })
    }

    async fn chat_stream_responses(
        &self,
        base_url: &str,
        access_token: &str,
        account_id: Option<&str>,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) {
        use futures_util::StreamExt;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let body = build_responses_body(
            model,
            messages,
            tools,
            self.max_output_tokens,
            true,
            &self.reasoning,
        );
        let mut current = access_token.to_string();
        let mut account = account_id.map(str::to_string);
        let mut auth_retry = true;
        let mut server_retries: u8 = 3; // retry transient 5xx errors up to 3 times

        let response = loop {
            let resp = match self
                .responses_request(base_url, &current, account.as_deref(), &body, true)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!("Network error: {}", e)));
                    return;
                }
            };

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && auth_retry {
                auth_retry = false;
                let http = reqwest::Client::new();
                match crate::auth::get_valid_chatgpt_token_force_refresh(&http).await {
                    Ok(tokens) => {
                        current = tokens.access_token;
                        account = tokens.account_id;
                        continue;
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(format!("ChatGPT auth error: {}", e)));
                        return;
                    }
                }
            }

            // Retry transient server errors (500, 502, 503, 504)
            if resp.status().is_server_error() && server_retries > 0 {
                server_retries -= 1;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }

            if !resp.status().is_success() {
                let status = resp.status();
                let err_body = resp.text().await.unwrap_or_default();
                let _ = tx.send(StreamEvent::Error(format!(
                    "Responses API error ({}): {}",
                    status, err_body
                )));
                return;
            }

            break resp;
        };

        let stream = tokio_util::io::StreamReader::new(
            response
                .bytes_stream()
                .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        );
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut event_type = String::new();
        let mut usage: Option<Usage> = None;
        let mut reasoning_emitted = false;
        let mut streamed_text = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(format!(
                        "Responses API stream read error: {}",
                        e
                    )));
                    break;
                }
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                event_type.clear();
                continue;
            }
            if let Some(ev) = trimmed.strip_prefix("event: ") {
                event_type = ev.to_string();
                continue;
            }
            let data = match trimmed.strip_prefix("data: ") {
                Some(d) => d,
                None => continue,
            };
            if data == "[DONE]" {
                break;
            }
            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            match event_type.as_str() {
                "response.output_text.delta" => {
                    if let Some(delta) = json.get("delta").and_then(|v| v.as_str()) {
                        if !delta.is_empty() {
                            streamed_text.push_str(delta);
                            let _ = tx.send(StreamEvent::Token(delta.to_string()));
                        }
                    }
                }
                "response.output_text.done" => {
                    if let Some(final_text) = json.get("text").and_then(|v| v.as_str()) {
                        if let Some(missing) =
                            missing_responses_text_suffix(&streamed_text, final_text)
                        {
                            streamed_text.push_str(&missing);
                            let _ = tx.send(StreamEvent::Token(missing));
                        }
                    }
                }
                "response.output_item.done" => {
                    let item = json.get("item").unwrap_or(&json);
                    if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                        if let Some(tc) = response_item_to_tool_call(item) {
                            let _ = tx.send(StreamEvent::ToolCall(tc));
                        }
                    }
                }
                event if event.contains("reasoning") => {
                    if !reasoning_emitted {
                        reasoning_emitted = true;
                        let _ = tx.send(StreamEvent::Reasoning);
                    }
                }
                "response.completed" => {
                    let response = json.get("response").unwrap_or(&json);
                    usage = parse_responses_usage(response.get("usage"));
                    if let Some(final_text) = extract_responses_text(response) {
                        if let Some(missing) =
                            missing_responses_text_suffix(&streamed_text, &final_text)
                        {
                            streamed_text.push_str(&missing);
                            let _ = tx.send(StreamEvent::Token(missing));
                        }
                    }
                    let incomplete_details =
                        response.get("incomplete_details").filter(|v| !v.is_null());
                    if response.get("status").and_then(|v| v.as_str()) == Some("incomplete")
                        || incomplete_details.is_some()
                        || responses_output_has_incomplete_item(response)
                    {
                        let details = response
                            .get("incomplete_details")
                            .filter(|v| !v.is_null())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "status=incomplete".to_string());
                        let _ = tx.send(StreamEvent::Error(format!(
                            "Responses API response incomplete: {}",
                            details
                        )));
                        return;
                    }
                    break;
                }
                "response.failed" | "response.incomplete" => {
                    let _ = tx.send(StreamEvent::Error(format!(
                        "Responses API stream ended with event {}: {}",
                        event_type, data
                    )));
                    return;
                }
                _ => {}
            }
        }

        let _ = tx.send(StreamEvent::Done { usage });
    }

    fn responses_request(
        &self,
        base_url: &str,
        token: &str,
        account_id: Option<&str>,
        body: &serde_json::Value,
        stream: bool,
    ) -> impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + '_ {
        let url = format!("{}/responses", base_url.trim_end_matches('/'));
        let mut req = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("content-type", "application/json");
        if stream {
            req = req
                .header("accept", "text/event-stream")
                .header("accept-encoding", "identity");
        }
        if let Some(account_id) = account_id {
            req = req.header("ChatGPT-Account-ID", account_id);
            req = req.header("chatgpt-account-id", account_id);
        }
        req.json(body).send()
    }

    // ── Context length probe ─────────────────────────────────────────────────

    /// Query the server's /models endpoint to detect the context window size.
    pub async fn fetch_context_length(&self, model_id: &str) -> Option<usize> {
        match self.backend.as_ref() {
            Backend::Anthropic {
                base_url,
                auth_token,
            } => {
                return self
                    .fetch_anthropic_context_length(base_url, auth_token, model_id)
                    .await;
            }
            Backend::ChatGptCodex { .. } => {
                return None;
            }
            Backend::OpenAi { .. } => {}
        }

        let base_url = match self.backend.as_ref() {
            Backend::OpenAi { base_url } => base_url,
            Backend::ChatGptCodex { .. } => unreachable!(),
            Backend::Anthropic { .. } => unreachable!(),
        };

        let url = format!("{}/models", base_url);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.client.get(&url).send(),
        )
        .await
        .ok()?
        .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let json: serde_json::Value = response.json().await.ok()?;
        let data = json.get("data")?.as_array()?;

        let model_entry = data
            .iter()
            .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model_id));
        let entry = model_entry.or_else(|| data.first())?;

        for field in &[
            "loaded_context_length",
            "max_context_length",
            "max_model_len",
            "context_length",
        ] {
            if let Some(val) = entry.get(field).and_then(|v| v.as_u64()) {
                if val > 0 {
                    return Some(val as usize);
                }
            }
        }

        None
    }

    async fn fetch_anthropic_context_length(
        &self,
        base_url: &str,
        auth_token: &str,
        model_id: &str,
    ) -> Option<usize> {
        let url = format!("{}/v1/models/{}", base_url, model_id);
        let is_oauth = auth_token.contains("sk-ant-oat");
        let mut req = self
            .client
            .get(&url)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if is_oauth {
            req = req
                .header("authorization", format!("Bearer {}", auth_token))
                .header("anthropic-beta", ANTHROPIC_BETA)
                .header("user-agent", format!("claude-cli/{}", crate::auth::claude_client_version().await))
                .header("x-app", "cli")
                .header("anthropic-dangerous-direct-browser-access", "true");
        } else {
            req = req.header("x-api-key", auth_token);
        }
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), req.send())
            .await
            .ok()?
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let json: serde_json::Value = response.json().await.ok()?;
        // Anthropic model info: { "context_window": 200000, ... }
        json.get("context_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
    }

    pub async fn chat_simple(&self, model: &str, user_message: &str) -> Result<String, String> {
        let messages = vec![
            Message::system("You are a helpful assistant."),
            Message::user(user_message),
        ];

        let response = self.chat(model, &messages, &[]).await?;

        match response.choices.first() {
            Some(choice) => {
                if let Some(content) = &choice.message.content {
                    Ok(content.clone())
                } else {
                    Err("No content in response".to_string())
                }
            }
            None => Err("Empty response".to_string()),
        }
    }
}

// ── Format conversion: Forge ↔ Responses API ────────────────────────────────

fn build_responses_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
    _max_output_tokens: u32,
    stream: bool,
    reasoning: &EndpointReasoningConfig,
) -> serde_json::Value {
    let instructions = messages
        .iter()
        .filter(|m| m.role == "system")
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");

    let input = convert_messages_to_responses_input(messages);
    let response_tools = convert_tools_to_responses(tools);

    let mut body = serde_json::json!({
        "model": model,
        "input": input,
        "stream": stream,
        "store": false,
        "parallel_tool_calls": true,
    });
    if !instructions.is_empty() {
        body["instructions"] = serde_json::Value::String(instructions);
    }
    if !response_tools.is_empty() {
        body["tools"] = serde_json::Value::Array(response_tools);
    }
    let effort = match reasoning.chatgpt_codex.effort {
        ChatGptReasoningEffort::ProviderDefault => None,
        ChatGptReasoningEffort::None => Some("none"),
        ChatGptReasoningEffort::Minimal => Some("minimal"),
        ChatGptReasoningEffort::Low => Some("low"),
        ChatGptReasoningEffort::Medium => Some("medium"),
        ChatGptReasoningEffort::High => Some("high"),
        ChatGptReasoningEffort::Xhigh => Some("xhigh"),
    };
    if let Some(effort) = effort {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    body
}

fn convert_tools_to_responses(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": tool.function.parameters,
            })
        })
        .collect()
}

fn extract_responses_text(json: &serde_json::Value) -> Option<String> {
    if let Some(text) = json.get("output_text").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let mut out = String::new();
    if let Some(items) = json.get("output").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                for block in content {
                    let kind = block.get("type").and_then(|v| v.as_str());
                    if matches!(kind, Some("output_text") | Some("text")) {
                        if let Some(text) = block
                            .get("text")
                            .or_else(|| block.get("output_text"))
                            .and_then(|v| v.as_str())
                        {
                            out.push_str(text);
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn missing_responses_text_suffix(current: &str, final_text: &str) -> Option<String> {
    if final_text.is_empty() || final_text == current {
        return None;
    }
    if current.is_empty() {
        return Some(final_text.to_string());
    }
    if let Some(missing) = final_text.strip_prefix(current) {
        return (!missing.is_empty()).then(|| missing.to_string());
    }

    // If a provider dropped or reordered a delta near the boundary, append only
    // the non-overlapping tail instead of duplicating the entire final text.
    let max_overlap = current.len().min(final_text.len());
    for overlap in (1..=max_overlap).rev() {
        if current.is_char_boundary(current.len() - overlap)
            && final_text.is_char_boundary(overlap)
            && current[current.len() - overlap..] == final_text[..overlap]
        {
            let missing = &final_text[overlap..];
            return (!missing.is_empty()).then(|| missing.to_string());
        }
    }

    None
}

fn responses_output_has_incomplete_item(response: &serde_json::Value) -> bool {
    response
        .get("output")
        .and_then(|v| v.as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("status").and_then(|v| v.as_str()) == Some("incomplete")
                    || item
                        .get("content")
                        .and_then(|v| v.as_array())
                        .is_some_and(|content| {
                            content.iter().any(|block| {
                                block.get("status").and_then(|v| v.as_str()) == Some("incomplete")
                            })
                        })
            })
        })
}

fn convert_messages_to_responses_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != "system").collect();
    let mut emitted_function_call_ids = std::collections::HashSet::new();
    let mut input = Vec::new();
    for (idx, msg) in non_system.iter().enumerate() {
        match msg.role.as_str() {
            "user" => input.push(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": msg.content.as_deref().unwrap_or("")}],
            })),
            "assistant" => {
                if let Some(content) = msg.content.as_deref().filter(|c| !c.is_empty()) {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content}],
                    }));
                }
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        if has_later_tool_output(&non_system, idx, &tc.id) {
                            emitted_function_call_ids.insert(tc.id.clone());
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": tc.id,
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }));
                        } else {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": "user",
                                "content": [{
                                    "type": "input_text",
                                    "text": format!(
                                        "[Prior assistant tool call preserved after context trimming]\nTool: {}\nCall ID: {}\nArguments:\n{}",
                                        tc.function.name,
                                        tc.id,
                                        tc.function.arguments
                                    ),
                                }],
                            }));
                        }
                    }
                }
            }
            "tool" => {
                let call_id = msg.tool_call_id.as_deref().unwrap_or("");
                if emitted_function_call_ids.contains(call_id) {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": msg.content.as_deref().unwrap_or(""),
                    }));
                } else {
                    let tool_name = msg.name.as_deref().unwrap_or("unknown");
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": format!(
                                "[Prior tool result preserved after context trimming]\nTool: {}\nCall ID: {}\nOutput:\n{}",
                                tool_name,
                                call_id,
                                msg.content.as_deref().unwrap_or("")
                            ),
                        }],
                    }));
                }
            }
            _ => {}
        }
    }
    input
}

fn has_later_tool_output(messages: &[&Message], current_idx: usize, call_id: &str) -> bool {
    messages
        .iter()
        .skip(current_idx + 1)
        .any(|msg| msg.role == "tool" && msg.tool_call_id.as_deref() == Some(call_id))
}

fn response_item_to_tool_call(item: &serde_json::Value) -> Option<ToolCall> {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let name = item.get("name").and_then(|v| v.as_str())?.to_string();
    let arguments = match item.get("arguments") {
        Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
        Some(v) => v.to_string(),
        None => "{}".to_string(),
    };
    Some(ToolCall {
        id: call_id,
        call_type: "function".to_string(),
        function: FunctionCall { name, arguments },
    })
}

fn parse_responses_usage(value: Option<&serde_json::Value>) -> Option<Usage> {
    let u = value?;
    let input = u
        .get("input_tokens")
        .or_else(|| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let total = u
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or((input + output) as u64) as u32;
    Some(Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
    })
}

// ── Format conversion: Forge ↔ Anthropic ────────────────────────────────────

/// Split out system messages; convert the rest to Anthropic message format.
/// Consecutive tool results are batched into a single user message.
fn convert_messages_to_anthropic(messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
    let system = messages
        .iter()
        .filter(|m| m.role == "system")
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    let system = if system.is_empty() {
        None
    } else {
        Some(system)
    };

    let mut result: Vec<serde_json::Value> = Vec::new();
    let mut i = 0;
    let non_system = sanitize_anthropic_message_sequence(messages);

    while i < non_system.len() {
        let msg = &non_system[i];

        if msg.role == "tool" {
            // Batch consecutive tool results into one user message
            let mut tool_results = vec![forge_tool_result_to_anthropic(msg)];
            i += 1;
            while i < non_system.len() && non_system[i].role == "tool" {
                tool_results.push(forge_tool_result_to_anthropic(&non_system[i]));
                i += 1;
            }
            result.push(serde_json::json!({
                "role": "user",
                "content": tool_results,
            }));
        } else if msg.role == "assistant" {
            result.push(forge_assistant_to_anthropic(msg));
            i += 1;
        } else {
            // user message
            result.push(serde_json::json!({
                "role": "user",
                "content": msg.content.as_deref().unwrap_or(""),
            }));
            i += 1;
        }
    }

    if result.is_empty() {
        result.push(serde_json::json!({
            "role": "user",
            "content": "Continue from the retained context.",
        }));
    } else if result
        .last()
        .and_then(|msg| msg.get("role"))
        .and_then(|role| role.as_str())
        == Some("assistant")
    {
        result.push(serde_json::json!({
            "role": "user",
            "content": "Continue from the retained context.",
        }));
    }

    (system, result)
}

fn sanitize_anthropic_message_sequence(messages: &[Message]) -> Vec<Message> {
    let non_system: Vec<&Message> = messages.iter().filter(|m| m.role != "system").collect();
    let mut sanitized: Vec<Message> = Vec::new();
    let mut i = 0;

    while i < non_system.len() {
        let msg = non_system[i];

        if msg.role == "tool" {
            i += 1;
            continue;
        }

        if let Some(tool_calls) = msg.tool_calls.as_ref().filter(|calls| !calls.is_empty()) {
            let mut j = i + 1;
            let mut result_ids = std::collections::HashSet::new();
            while j < non_system.len() && non_system[j].role == "tool" {
                if let Some(id) = non_system[j].tool_call_id.as_deref() {
                    result_ids.insert(id);
                }
                j += 1;
            }

            let has_all_results = tool_calls
                .iter()
                .all(|tc| result_ids.contains(tc.id.as_str()));
            if has_all_results {
                sanitized.push(msg.clone());
                sanitized.extend(non_system[i + 1..j].iter().map(|m| (*m).clone()));
                i = j;
                continue;
            }

            let mut text_only = msg.clone();
            text_only.tool_calls = None;
            if text_only
                .content
                .as_deref()
                .is_some_and(|content| !content.is_empty())
            {
                sanitized.push(text_only);
            }
            i += 1;
            continue;
        }

        sanitized.push(msg.clone());
        i += 1;
    }

    sanitized
}

fn forge_tool_result_to_anthropic(msg: &Message) -> serde_json::Value {
    serde_json::json!({
        "type": "tool_result",
        "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
        "content": msg.content.as_deref().unwrap_or(""),
    })
}

fn forge_assistant_to_anthropic(msg: &Message) -> serde_json::Value {
    let mut content: Vec<serde_json::Value> = Vec::new();

    if let Some(text) = &msg.content {
        if !text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text}));
        }
    }

    if let Some(tool_calls) = &msg.tool_calls {
        for tc in tool_calls {
            let input: serde_json::Value =
                serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::json!({}));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.function.name,
                "input": input,
            }));
        }
    }

    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }

    serde_json::json!({ "role": "assistant", "content": content })
}

fn convert_tools_to_anthropic(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.function.name,
                "description": t.function.description,
                "input_schema": t.function.parameters,
            })
        })
        .collect()
}

/// Convert an Anthropic /v1/messages response into Forge's ChatResponse.
fn convert_anthropic_response(json: serde_json::Value) -> Result<ChatResponse, String> {
    let id = json["id"].as_str().unwrap_or("").to_string();

    // Collect text content
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(content) = json["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    let tc_id = block["id"].as_str().unwrap_or("").to_string();
                    let name = block["name"].as_str().unwrap_or("").to_string();
                    let input = &block["input"];
                    let arguments =
                        serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                    tool_calls.push(ToolCall {
                        id: tc_id,
                        call_type: "function".to_string(),
                        function: FunctionCall { name, arguments },
                    });
                }
                _ => {}
            }
        }
    }

    let text_content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    let stop_reason = json["stop_reason"].as_str().map(|s| s.to_string());

    let message = if tool_calls.is_empty() {
        Message {
            role: "assistant".to_string(),
            content: text_content,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    } else {
        Message {
            role: "assistant".to_string(),
            content: text_content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        }
    };

    let usage = json["usage"].as_object().map(|u| Usage {
        prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        total_tokens: (u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
            + u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0))
            as u32,
    });

    let finish_reason = stop_reason.map(|r| match r.as_str() {
        "tool_use" => "tool_calls".to_string(),
        "end_turn" => "stop".to_string(),
        other => other.to_string(),
    });

    Ok(ChatResponse {
        id,
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason,
        }],
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn strip_think_blocks_removes_local_reasoning() {
        assert_eq!(
            strip_think_blocks("visible <think>private reasoning</think> answer"),
            "visible  answer"
        );
        assert_eq!(
            strip_think_blocks("<THINK>private</THINK>visible"),
            "visible"
        );
    }

    #[test]
    fn think_block_filter_handles_split_tags_and_unicode() {
        let mut filter = ThinkBlockFilter::new();
        let mut visible = String::new();
        for chunk in ["hello é <thi", "nk>secret", "</thi", "nk> world"] {
            visible.push_str(&filter.push(chunk));
        }
        visible.push_str(&filter.finish());

        assert_eq!(visible, "hello é  world");
    }

    #[test]
    fn think_block_filter_drops_unclosed_reasoning() {
        let mut filter = ThinkBlockFilter::new();
        let mut visible = filter.push("shown <think>hidden forever");
        visible.push_str(&filter.finish());

        assert_eq!(visible, "shown ");
    }

    #[test]
    fn responses_final_text_reconciliation_appends_missing_tail() {
        assert_eq!(
            missing_responses_text_suffix("Scope", "Scope notes:\n- Done").as_deref(),
            Some(" notes:\n- Done")
        );
        assert_eq!(
            missing_responses_text_suffix("abcXYZ", "XYZ123").as_deref(),
            Some("123")
        );
        assert!(missing_responses_text_suffix("complete", "complete").is_none());
    }

    #[test]
    fn responses_incomplete_output_item_is_detected() {
        let response = serde_json::json!({
            "status": "completed",
            "incomplete_details": null,
            "output": [{
                "type": "message",
                "status": "incomplete",
                "content": [{ "type": "output_text", "text": "partial" }]
            }]
        });

        assert!(responses_output_has_incomplete_item(&response));
    }

    #[test]
    fn anthropic_sanitizer_drops_orphan_tool_result() {
        let messages = vec![
            Message::system("system"),
            Message::tool_result("missing-tool-use", "search", "orphan"),
            Message::user("continue"),
        ];

        let sanitized = sanitize_anthropic_message_sequence(&messages);

        assert_eq!(sanitized.len(), 1);
        assert_eq!(sanitized[0].role, "user");
    }

    #[test]
    fn anthropic_conversion_inserts_fallback_when_history_has_no_valid_messages() {
        let messages = vec![
            Message::system("system"),
            Message::tool_result("missing-tool-use", "search", "orphan"),
        ];

        let (_system, anthropic_messages) = convert_messages_to_anthropic(&messages);

        assert_eq!(anthropic_messages.len(), 1);
        assert_eq!(anthropic_messages[0]["role"].as_str(), Some("user"));
    }

    #[test]
    fn anthropic_conversion_appends_user_when_history_ends_with_assistant() {
        let messages = vec![
            Message::user("do the task"),
            Message::assistant("partial answer"),
        ];

        let (_system, anthropic_messages) = convert_messages_to_anthropic(&messages);

        assert_eq!(anthropic_messages.len(), 3);
        assert_eq!(anthropic_messages[0]["role"].as_str(), Some("user"));
        assert_eq!(anthropic_messages[1]["role"].as_str(), Some("assistant"));
        assert_eq!(anthropic_messages[2]["role"].as_str(), Some("user"));
    }

    #[test]
    fn anthropic_sanitizer_preserves_complete_tool_exchange() {
        let messages = vec![
            Message::assistant_with_tools(None, vec![tool_call("tool-1")]),
            Message::tool_result("tool-1", "search", "result"),
        ];

        let sanitized = sanitize_anthropic_message_sequence(&messages);

        assert_eq!(sanitized.len(), 2);
        assert_eq!(sanitized[0].role, "assistant");
        assert_eq!(sanitized[1].role, "tool");
    }

    #[test]
    fn anthropic_sanitizer_removes_incomplete_tool_call() {
        let messages = vec![
            Message::assistant_with_tools(Some("text".to_string()), vec![tool_call("tool-1")]),
            Message::user("continue"),
        ];

        let sanitized = sanitize_anthropic_message_sequence(&messages);

        assert_eq!(sanitized.len(), 2);
        assert_eq!(sanitized[0].role, "assistant");
        assert!(sanitized[0].tool_calls.is_none());
        assert_eq!(sanitized[0].content.as_deref(), Some("text"));
    }
}
