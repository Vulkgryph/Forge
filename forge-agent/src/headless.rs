// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::log_types::SessionMeta;
use crate::agent::{
    AgentEvent, QuestionItem, QuestionOption, TokenUsageSnapshot, ToolKindEvent, UserAction,
};
use crate::config::ModelEndpoint;

// ── Agent → TUI (JSON on stdout) ──────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub enabled: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutgoingMessage {
    Init {
        project_root: String,
        model_name: String,
        model_id: String,
        max_context_tokens: usize,
        log_path: String,
        dangerously_allow_all: bool,
        agent_definitions: Vec<AgentDefInfo>,
        endpoints: Vec<EndpointInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        available_tools: Vec<ToolInfo>,
        context_strategy: String,
        chatgpt_logged_in: bool,
        offline_mode: bool,
    },
    Thinking,
    Reasoning,
    ReasoningToken {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    AssistantToken {
        content: String,
    },
    AssistantDone {
        content: String,
    },
    ToolRequest {
        tool_name: String,
        tool_args: String,
        tool_id: String,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subagent_id: Option<String>,
        needs_approval: bool,
    },
    ToolResult {
        tool_name: String,
        result: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        subagent_id: Option<String>,
    },
    ToolOutput {
        tool_name: String,
        content: String,
    },
    ProcessInputNeeded {
        prompt: String,
    },
    BackgroundPromptNeeded {
        bg_id: String,
        command: String,
        prompt: String,
    },
    Error {
        message: String,
    },
    ApiRetry {
        attempt: usize,
        max_attempts: usize,
        delay_secs: u64,
        error: String,
    },
    TurnDiscarded,
    Done,
    Cancelled,
    Usage {
        snapshot: UsageSnapshot,
    },
    UsageUpdate {
        snapshot: UsageSnapshot,
    },
    ModelSwitched {
        name: String,
        model_id: String,
        max_context_tokens: usize,
    },
    SessionCleared {
        session_id: String,
        log_path: String,
    },
    SubagentStarted {
        id: String,
        agent_type: String,
        prompt: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
    },
    SubagentStatus {
        id: String,
        tool_name: String,
        detail: String,
    },
    SubagentFinished {
        id: String,
        agent_type: String,
        summary: String,
    },
    QuestionRequest {
        question: String,
        tool_id: String,
        items: Vec<QuestionItemJson>,
    },
    PlanModeEntered {
        plan_path: String,
    },
    PlanModeExited {
        reason: String,
    },
    PlanReady {
        plan_path: String,
        content: String,
    },
    RewindCheckpoint {
        id: String,
        preview: String,
        message_count: usize,
        keep_on_restore: bool,
    },
    RewindPreview {
        checkpoint_id: String,
        preview: String,
        summary: String,
    },
    SessionLoaded {
        session_id: String,
        title: String,
        message_count: usize,
        compaction_count: usize,
        entries: Vec<ReplayEntryJson>,
        rewind_checkpoints: Vec<RewindCheckpointJson>,
    },
    LoginStatus {
        message: String,
    },
    LoginComplete {
        success: bool,
        message: String,
    },
    EndpointsUpdated {
        endpoints: Vec<EndpointInfo>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplayEntryJson {
    pub kind: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
}

/// Budget for the replayed transcript inside a `session_loaded` frame.
///
/// The frame itself is capped at 10 MB on both sides; this leaves room for
/// everything else it carries (endpoints, tools, rewind checkpoints) and for
/// JSON escaping, which can nearly double a run of newlines.
pub const REPLAY_BUDGET_BYTES: usize = 6 * 1024 * 1024;

/// Serialized size of one entry, as it will appear in the frame.
fn entry_bytes(entry: &ReplayEntryJson) -> usize {
    serde_json::to_string(entry).map(|s| s.len()).unwrap_or(0)
}

/// Cut `text` to at most `max` bytes, on a character boundary.
fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Trim a replayed transcript so the `session_loaded` frame stays inside the
/// protocol's cap.
///
/// Resuming a long-running session used to build one frame out of the entire
/// post-compaction log and write it uncapped — the agent's own cap applies to
/// what it reads, not what it writes. The client then refused the frame (it
/// caps what *it* reads, correctly, or a peer's output would be an allocation
/// budget), so the resumed session came back with no transcript at all and an
/// error in its place. Seen at 15 MB; session logs here reach 43 MB.
///
/// Two ways a transcript gets too big, and both have to be handled: many
/// entries, and one enormous one (a large file read is a single tool result).
/// So oversized entries are truncated individually first, then the oldest are
/// dropped until the rest fit — the newest is what a resumed session is for.
///
/// This is display only. The agent's own context comes from
/// `load_from_last_compaction`, a separate path, and is not affected by
/// anything here: what is dropped is the user's view of history, never the
/// model's.
pub fn fit_replay_entries(
    mut entries: Vec<ReplayEntryJson>,
    budget: usize,
) -> Vec<ReplayEntryJson> {
    // No single entry may take more than a quarter of the budget, so one huge
    // tool result cannot evict the whole conversation around it.
    let per_entry = (budget / 4).max(1024);
    let mut truncated = 0usize;
    for entry in &mut entries {
        if entry_bytes(entry) > per_entry {
            let kept = truncate_bytes(&entry.content, per_entry).to_string();
            let dropped = entry.content.len() - kept.len();
            entry.content = format!("{kept}\n… {dropped} more bytes not shown");
            truncated += 1;
        }
    }

    let mut total: usize = entries.iter().map(entry_bytes).sum();
    let mut dropped = 0usize;
    let mut first = 0usize;
    while total > budget && first < entries.len() {
        total -= entry_bytes(&entries[first]);
        first += 1;
        dropped += 1;
    }
    let mut entries: Vec<ReplayEntryJson> = entries.drain(first..).collect();

    if dropped > 0 || truncated > 0 {
        // An unrecognized kind renders as a system note in every client, so
        // saying this needs no protocol change.
        let mut what = Vec::new();
        if dropped > 0 {
            what.push(format!("{dropped} earlier messages are not shown"));
        }
        if truncated > 0 {
            what.push(format!("{truncated} were shortened"));
        }
        entries.insert(
            0,
            ReplayEntryJson {
                kind: "system".into(),
                content: format!(
                    "This conversation is too large to redisplay in full — {}. \
                     The agent still has its own context; only this transcript \
                     is abbreviated. The full log is on disk.",
                    what.join(" and "),
                ),
                tool_name: None,
                success: None,
            },
        );
    }
    entries
}

#[derive(Serialize, Clone)]
pub struct RewindCheckpointJson {
    pub id: String,
    pub preview: String,
    pub message_count: usize,
    pub display_index: usize,
    pub keep_on_restore: bool,
}

#[derive(Serialize)]
struct UsageSnapshot {
    last_prompt_tokens: u32,
    last_completion_tokens: u32,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    total_requests: u64,
    max_context_tokens: usize,
    history_messages: usize,
}

#[derive(Serialize)]
struct QuestionItemJson {
    question: String,
    header: String,
    options: Vec<QuestionOptionJson>,
    multi_select: bool,
}

#[derive(Serialize)]
struct QuestionOptionJson {
    label: String,
    description: String,
}

#[derive(Serialize)]
pub struct AgentDefInfo {
    name: String,
    description: String,
    model: String,
    max_turns: Option<usize>,
    tools: Vec<String>,
    source: String,
}

#[derive(Serialize, Clone)]
pub struct EndpointInfo {
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub max_context_tokens: usize,
    pub max_output_tokens: u32,
    pub endpoint_type: String,
    pub reasoning: crate::config::EndpointReasoningConfig,
    pub xai_priority_tier: bool,
}

impl From<&ModelEndpoint> for EndpointInfo {
    fn from(ep: &ModelEndpoint) -> Self {
        Self {
            name: ep.name.clone(),
            base_url: ep.base_url.clone(),
            model_id: ep.model_id.clone(),
            max_context_tokens: ep.max_context_tokens,
            max_output_tokens: ep.max_output_tokens,
            endpoint_type: match ep.endpoint_type {
                crate::config::EndpointType::Anthropic => "anthropic".to_string(),
                crate::config::EndpointType::ChatGptCodex => "chatgpt_codex".to_string(),
                crate::config::EndpointType::OpenAi => "open_ai".to_string(),
            },
            reasoning: ep.reasoning.clone(),
            xai_priority_tier: ep.xai_priority_tier,
        }
    }
}

impl From<&TokenUsageSnapshot> for UsageSnapshot {
    fn from(u: &TokenUsageSnapshot) -> Self {
        Self {
            last_prompt_tokens: u.last_prompt_tokens,
            last_completion_tokens: u.last_completion_tokens,
            total_prompt_tokens: u.total_prompt_tokens,
            total_completion_tokens: u.total_completion_tokens,
            total_requests: u.total_requests,
            max_context_tokens: u.max_context_tokens,
            history_messages: u.history_messages,
        }
    }
}

impl From<&QuestionItem> for QuestionItemJson {
    fn from(item: &QuestionItem) -> Self {
        Self {
            question: item.question.clone(),
            header: item.header.clone(),
            options: item
                .options
                .iter()
                .map(|o: &QuestionOption| QuestionOptionJson {
                    label: o.label.clone(),
                    description: o.description.clone(),
                })
                .collect(),
            multi_select: item.multi_select,
        }
    }
}

fn agent_event_to_json(event: &AgentEvent) -> OutgoingMessage {
    match event {
        AgentEvent::Thinking => OutgoingMessage::Thinking,
        AgentEvent::Reasoning => OutgoingMessage::Reasoning,
        AgentEvent::ReasoningToken(content) => OutgoingMessage::ReasoningToken {
            content: content.clone(),
        },
        AgentEvent::AssistantMessage(content) => OutgoingMessage::AssistantMessage {
            content: content.clone(),
        },
        AgentEvent::AssistantToken(content) => OutgoingMessage::AssistantToken {
            content: content.clone(),
        },
        AgentEvent::AssistantDone(content) => OutgoingMessage::AssistantDone {
            content: content.clone(),
        },
        AgentEvent::ToolRequest {
            tool_name,
            tool_args,
            tool_id,
            kind,
            subagent_id,
            needs_approval,
        } => {
            let kind_str = match kind {
                ToolKindEvent::Read => "read",
                ToolKindEvent::Write => "write",
                ToolKindEvent::Execute => "execute",
            };
            OutgoingMessage::ToolRequest {
                tool_name: tool_name.clone(),
                tool_args: tool_args.clone(),
                tool_id: tool_id.clone(),
                kind: kind_str.to_string(),
                subagent_id: subagent_id.clone(),
                needs_approval: *needs_approval,
            }
        }
        AgentEvent::ToolResult {
            tool_name,
            result,
            success,
            subagent_id,
        } => OutgoingMessage::ToolResult {
            tool_name: tool_name.clone(),
            result: result.clone(),
            success: *success,
            subagent_id: subagent_id.clone(),
        },
        AgentEvent::ToolOutput { tool_name, content } => OutgoingMessage::ToolOutput {
            tool_name: tool_name.clone(),
            content: content.clone(),
        },
        AgentEvent::ProcessInputNeeded { prompt } => OutgoingMessage::ProcessInputNeeded {
            prompt: prompt.clone(),
        },
        AgentEvent::BackgroundPromptNeeded {
            bg_id,
            command,
            prompt,
        } => OutgoingMessage::BackgroundPromptNeeded {
            bg_id: bg_id.clone(),
            command: command.clone(),
            prompt: prompt.clone(),
        },
        AgentEvent::Error(msg) => OutgoingMessage::Error {
            message: msg.clone(),
        },
        AgentEvent::ApiRetry {
            attempt,
            max_attempts,
            delay_secs,
            error,
        } => OutgoingMessage::ApiRetry {
            attempt: *attempt,
            max_attempts: *max_attempts,
            delay_secs: *delay_secs,
            error: error.clone(),
        },
        AgentEvent::TurnDiscarded => OutgoingMessage::TurnDiscarded,
        AgentEvent::Done => OutgoingMessage::Done,
        AgentEvent::Cancelled => OutgoingMessage::Cancelled,
        AgentEvent::Usage(u) => OutgoingMessage::Usage { snapshot: u.into() },
        AgentEvent::UsageUpdate(u) => OutgoingMessage::UsageUpdate { snapshot: u.into() },
        AgentEvent::ModelSwitched {
            name,
            model_id,
            max_context_tokens,
        } => OutgoingMessage::ModelSwitched {
            name: name.clone(),
            model_id: model_id.clone(),
            max_context_tokens: *max_context_tokens,
        },
        AgentEvent::SessionCleared {
            session_id,
            log_path,
        } => OutgoingMessage::SessionCleared {
            session_id: session_id.clone(),
            log_path: log_path.clone(),
        },
        AgentEvent::SubagentStarted {
            id,
            agent_type,
            prompt,
            parent_id,
        } => OutgoingMessage::SubagentStarted {
            id: id.clone(),
            agent_type: agent_type.clone(),
            prompt: prompt.clone(),
            parent_id: parent_id.clone(),
        },
        AgentEvent::SubagentStatus {
            id,
            tool_name,
            detail,
        } => OutgoingMessage::SubagentStatus {
            id: id.clone(),
            tool_name: tool_name.clone(),
            detail: detail.clone(),
        },
        AgentEvent::SubagentFinished {
            id,
            agent_type,
            summary,
        } => OutgoingMessage::SubagentFinished {
            id: id.clone(),
            agent_type: agent_type.clone(),
            summary: summary.clone(),
        },
        AgentEvent::QuestionRequest {
            question,
            tool_id,
            items,
        } => OutgoingMessage::QuestionRequest {
            question: question.clone(),
            tool_id: tool_id.clone(),
            items: items.iter().map(|i| i.into()).collect(),
        },
        AgentEvent::PlanModeEntered { plan_path } => OutgoingMessage::PlanModeEntered {
            plan_path: plan_path.clone(),
        },
        AgentEvent::PlanModeExited { reason } => OutgoingMessage::PlanModeExited {
            reason: reason.to_string(),
        },
        AgentEvent::PlanReady { plan_path, content } => OutgoingMessage::PlanReady {
            plan_path: plan_path.clone(),
            content: content.clone(),
        },
        AgentEvent::RewindCheckpoint {
            id,
            preview,
            message_count,
            keep_on_restore,
        } => OutgoingMessage::RewindCheckpoint {
            id: id.clone(),
            preview: preview.clone(),
            message_count: *message_count,
            keep_on_restore: *keep_on_restore,
        },
        AgentEvent::RewindPreview {
            checkpoint_id,
            preview,
            summary,
        } => OutgoingMessage::RewindPreview {
            checkpoint_id: checkpoint_id.clone(),
            preview: preview.clone(),
            summary: summary.clone(),
        },
    }
}

// ── TUI → Agent (JSON on stdin) ───────────────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IncomingMessage {
    SendMessage {
        content: String,
    },
    ApproveAction {
        #[serde(default)]
        tool_id: String,
    },
    DenyAction {
        #[serde(default)]
        tool_id: String,
        #[serde(default)]
        reason: String,
    },
    ToggleAutoMode,
    SwitchModel {
        name: String,
        base_url: String,
        model_id: String,
        max_context_tokens: usize,
        max_output_tokens: u32,
        #[serde(default)]
        endpoint_type: String,
        #[serde(default)]
        reasoning: crate::config::EndpointReasoningConfig,
        /// A key supplied by the client for this switch alone.
        ///
        /// For an agent running somewhere its user's credentials are not, and
        /// should not be: the client holds them, hands one over when it selects
        /// an endpoint, and this process keeps it in memory for as long as it
        /// runs. `switch_model` writes nothing to disk — see the handler in
        /// `agent/core.rs`, which builds a new client and stops there — so
        /// nothing here leaves a key behind on that machine.
        ///
        /// Omitted, the key comes from this machine's own configuration as
        /// before, which is what a local agent wants.
        #[serde(default)]
        api_key: Option<String>,
        /// Use this endpoint, do not remember it.
        ///
        /// For an address that is only meaningful for this session: a client
        /// proxying model calls for an agent that has no credentials sends a
        /// tunnel address, and a tunnel is a different port every time. Written
        /// to the config it would be an endpoint that no longer answers.
        ///
        /// A key supplied by the client implies this too — it is not this
        /// machine's to keep — but the reverse does not hold, since a proxied
        /// endpoint carries no key at all.
        #[serde(default)]
        ephemeral: bool,
    },
    UpdateSubagentConfig {
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        max_concurrent: Option<usize>,
        #[serde(default)]
        max_depth: Option<usize>,
        #[serde(default)]
        default_model: Option<String>,
        /// Set to true to clear default_model (set to inherit)
        #[serde(default)]
        clear_default_model: bool,
        /// Wall-clock ceiling for a parent delegate_task batch (seconds).
        #[serde(default)]
        max_delegate_secs: Option<u64>,
    },
    UpdateWebModel {
        /// Endpoint name, or empty/"" to inherit
        model: String,
    },
    UpdateContextStrategy {
        strategy: String,
    },
    UpdateToolConfig {
        tool: String,
        enabled: bool,
    },
    UpdateEndpointReasoning {
        endpoint_name: String,
        reasoning: crate::config::EndpointReasoningConfig,
    },
    UpdateXaiPriorityTier {
        endpoint_name: String,
        enabled: bool,
    },
    UpdateOfflineMode {
        enabled: bool,
    },
    LoginChatgpt,
    ListSessions,
    ResumeSession {
        // Accepted and ignored. Resuming happens by restarting the agent with
        // `--resume-session <id>`, which is what both clients do; the id is read
        // here so a client that sends this message is not met with a parse
        // error, and nothing else is done with it.
        #[allow(dead_code)]
        session_id: String,
    },
    Compact,
    Rewind {
        #[serde(default)]
        checkpoint_id: Option<String>,
    },
    Revert {
        #[serde(default)]
        checkpoint_id: Option<String>,
    },
    RewindPreview {
        checkpoint_id: String,
    },
    RevertPreview {
        checkpoint_id: String,
    },
    RequestUsage,
    EnterPlanMode,
    ApprovePlan,
    RejectPlan {
        #[serde(default)]
        feedback: String,
    },
    AnswerQuestion {
        answer: String,
    },
    ClearAndApprovePlan,
    ClearSession,
    ProcessInput {
        content: String,
    },
    BgProcessInput {
        bg_id: String,
        content: String,
    },
    CancelRun,
    Quit,
}

/// Convert incoming message to UserAction.
/// Returns None for messages handled directly by the headless loop (config changes).
fn json_to_user_action(
    msg: IncomingMessage,
    app_config: &mut crate::config::AppConfig,
    action_tx: &mpsc::UnboundedSender<UserAction>,
) -> Option<UserAction> {
    match msg {
        IncomingMessage::SendMessage { content } => Some(UserAction::SendMessage(content)),
        IncomingMessage::ApproveAction { tool_id } => Some(UserAction::ApproveAction(tool_id)),
        IncomingMessage::DenyAction { tool_id, reason } => {
            Some(UserAction::DenyAction { tool_id, reason })
        }
        IncomingMessage::ToggleAutoMode => Some(UserAction::ToggleAutoMode),
        IncomingMessage::SwitchModel {
            name,
            base_url,
            model_id,
            max_context_tokens,
            max_output_tokens,
            endpoint_type,
            reasoning,
            api_key,
            ephemeral,
        } => {
            let ep_type = match endpoint_type.as_str() {
                "anthropic" => crate::config::EndpointType::Anthropic,
                "chatgpt_codex" => crate::config::EndpointType::ChatGptCodex,
                _ => crate::config::EndpointType::OpenAi,
            };
            // The client only ever sees `EndpointInfo` (no `api_key` field —
            // that never leaves the server), so look the real key up from
            // our own config by name instead of trusting the incoming
            // message. This used to always be `None`, meaning switching to
            // an authenticated endpoint silently dropped its key for the
            // rest of the session.
            let existing = app_config.models.endpoints.iter().find(|e| e.name == name);
            // The client's key wins when it sent one: it is the machine that
            // has them. Falling back to the stored key keeps a local agent, and
            // a remote one whose host is already configured, working unchanged.
            let client_key = api_key.as_deref().is_some_and(|k| !k.is_empty());
            let api_key = api_key
                .filter(|k| !k.is_empty())
                .or_else(|| existing.and_then(|e| e.api_key.clone()));
            // Same reasoning as `api_key` above — not part of what the client
            // sends in `switch_model`, so carried over from our own stored
            // config instead of defaulting to off on every switch.
            let xai_priority_tier = existing.map(|e| e.xai_priority_tier).unwrap_or(false);
            // A key the client supplied is the client's, not this machine's.
            let persist = if client_key || ephemeral {
                crate::agent::PersistEndpoint::No
            } else {
                crate::agent::PersistEndpoint::Yes
            };
            Some(UserAction::SwitchModel(ModelEndpoint {
                name,
                base_url,
                model_id,
                api_key,
                max_context_tokens,
                max_output_tokens,
                request_timeout_secs: crate::config::default_request_timeout_secs(),
                endpoint_type: ep_type,
                reasoning,
                xai_priority_tier,
            }, persist))
        }
        IncomingMessage::UpdateSubagentConfig {
            enabled,
            max_concurrent,
            max_depth,
            default_model,
            clear_default_model,
            max_delegate_secs,
        } => {
            if let Some(v) = enabled {
                app_config.agent.subagents.enabled = v;
            }
            if let Some(v) = max_concurrent {
                app_config.agent.subagents.max_concurrent = v;
            }
            if let Some(v) = max_depth {
                app_config.agent.subagents.max_depth = v;
            }
            if clear_default_model {
                app_config.agent.subagents.default_model = None;
            } else if let Some(v) = default_model {
                app_config.agent.subagents.default_model = Some(v);
            }
            if let Some(v) = max_delegate_secs {
                app_config.agent.subagents.max_delegate_secs = v.max(1);
            }
            let _ = app_config.save();
            let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            None
        }
        IncomingMessage::UpdateWebModel { model } => {
            if model.is_empty() {
                app_config.models.web_tool_model = None;
            } else {
                app_config.models.web_tool_model = Some(model);
            }
            let _ = app_config.save();
            let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            None
        }
        IncomingMessage::UpdateContextStrategy { strategy } => {
            let parsed = match strategy.as_str() {
                "rolling_window" => crate::config::ContextStrategy::RollingWindow,
                _ => crate::config::ContextStrategy::Compaction,
            };
            app_config.agent.context_strategy = parsed;
            let _ = app_config.save();
            let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            None
        }
        IncomingMessage::UpdateOfflineMode { enabled } => {
            app_config.agent.offline_mode = enabled;
            let _ = app_config.save();
            let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            None
        }
        IncomingMessage::UpdateToolConfig { tool, enabled } => {
            if enabled {
                app_config.agent.disabled_tools.retain(|t| t != &tool);
            } else if !app_config.agent.disabled_tools.contains(&tool) {
                app_config.agent.disabled_tools.push(tool);
            }
            let _ = app_config.save();
            let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            None
        }
        IncomingMessage::UpdateEndpointReasoning {
            endpoint_name,
            reasoning,
        } => {
            if let Some(endpoint) = app_config
                .models
                .endpoints
                .iter_mut()
                .find(|ep| ep.name == endpoint_name)
            {
                endpoint.reasoning = reasoning;
                let _ = app_config.save();
                let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            }
            None
        }
        IncomingMessage::UpdateXaiPriorityTier {
            endpoint_name,
            enabled,
        } => {
            if let Some(endpoint) = app_config
                .models
                .endpoints
                .iter_mut()
                .find(|ep| ep.name == endpoint_name)
            {
                endpoint.xai_priority_tier = enabled;
                let _ = app_config.save();
                let _ = action_tx.send(UserAction::UpdateConfig(app_config.clone()));
            }
            None
        }
        IncomingMessage::ListSessions | IncomingMessage::ResumeSession { .. } => {
            // Handled directly in the headless loop, not forwarded
            None
        }
        IncomingMessage::Compact => Some(UserAction::Compact),
        IncomingMessage::Rewind { checkpoint_id } => Some(UserAction::Rewind(checkpoint_id)),
        IncomingMessage::Revert { checkpoint_id } => Some(UserAction::Rewind(checkpoint_id)),
        IncomingMessage::RewindPreview { checkpoint_id } => {
            Some(UserAction::RewindPreview(checkpoint_id))
        }
        IncomingMessage::RevertPreview { checkpoint_id } => {
            Some(UserAction::RewindPreview(checkpoint_id))
        }
        IncomingMessage::RequestUsage => Some(UserAction::RequestUsage),
        IncomingMessage::EnterPlanMode => Some(UserAction::EnterPlanMode),
        IncomingMessage::ApprovePlan => Some(UserAction::ApprovePlan),
        IncomingMessage::RejectPlan { feedback } => Some(UserAction::RejectPlan(feedback)),
        IncomingMessage::AnswerQuestion { answer } => Some(UserAction::AnswerQuestion(answer)),
        IncomingMessage::ClearAndApprovePlan => Some(UserAction::ClearAndApprovePlan),
        IncomingMessage::ClearSession => Some(UserAction::ClearSession),
        IncomingMessage::ProcessInput { content } => Some(UserAction::ProcessInput(content)),
        IncomingMessage::BgProcessInput { bg_id, content } => Some(UserAction::BgProcessInput {
            bg_id,
            text: content,
        }),
        IncomingMessage::CancelRun => Some(UserAction::CancelRun),
        IncomingMessage::Quit => Some(UserAction::Quit),
        // Handled directly in the headless loop before this function is called
        IncomingMessage::LoginChatgpt => None,
    }
}

// ── Headless event loop ───────────────────────────────────────────────

pub async fn run_headless(
    mut event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    action_tx: mpsc::UnboundedSender<UserAction>,
    init_info: HeadlessInit,
    mut app_config: crate::config::AppConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = tokio::io::stdout();
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);

    // Send init message
    let init_msg = OutgoingMessage::Init {
        project_root: init_info.project_root,
        model_name: init_info.model_name,
        model_id: init_info.model_id,
        max_context_tokens: init_info.max_context_tokens,
        log_path: init_info.log_path,
        dangerously_allow_all: init_info.dangerously_allow_all,
        agent_definitions: init_info.agent_definitions,
        endpoints: init_info.endpoints,
        session_id: init_info.session_id,
        available_tools: init_info.available_tools,
        context_strategy: init_info.context_strategy,
        chatgpt_logged_in: init_info.chatgpt_logged_in,
        offline_mode: init_info.offline_mode,
    };
    let json = serde_json::to_string(&init_msg)?;
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;

    // If resuming, send session_loaded with replay entries
    if let Some(ref meta) = init_info.resume_meta {
        let loaded_msg = OutgoingMessage::SessionLoaded {
            session_id: meta.id.clone(),
            title: meta.title.clone(),
            message_count: meta.message_count,
            compaction_count: meta.compaction_count,
            entries: init_info.replay_entries,
            rewind_checkpoints: init_info.rewind_checkpoints,
        };
        let json = serde_json::to_string(&loaded_msg)?;
        stdout.write_all(json.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    let mut line_buf = String::new();
    // Per-message size cap. A single JSON-newline frame must fit in 10 MB; any
    // longer is treated as a protocol error rather than an unbounded allocation.
    const MAX_HEADLESS_LINE_BYTES: usize = 10 * 1024 * 1024;
    // Channel for OAuth login background task to send JSON strings back to stdout
    let (login_tx, mut login_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    loop {
        tokio::select! {
            biased;

            // Always check stdin first so cancel/quit are never starved by output floods
            result = reader.read_line(&mut line_buf) => {
                match result {
                    Ok(0) => {
                        // EOF — TUI closed
                        let _ = action_tx.send(UserAction::Quit);
                        break;
                    }
                    Ok(_) => {
                        // Guard against unbounded growth: a partial line that
                        // grows past the cap without a newline is dropped and
                        // we resync on the next newline.
                        if line_buf.len() > MAX_HEADLESS_LINE_BYTES {
                            eprintln!(
                                "headless: dropping oversized message ({} bytes > {} cap)",
                                line_buf.len(),
                                MAX_HEADLESS_LINE_BYTES
                            );
                            line_buf.clear();
                            continue;
                        }
                        let trimmed = line_buf.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<IncomingMessage>(trimmed) {
                                Ok(IncomingMessage::LoginChatgpt) => {
                                    let tx = login_tx.clone();
                                    tokio::spawn(async move {
                                        let status = OutgoingMessage::LoginStatus {
                                            message: "Opening browser for ChatGPT Codex authorization...".to_string(),
                                        };
                                        if let Ok(s) = serde_json::to_string(&status) {
                                            let _ = tx.send(s);
                                        }
                                        match crate::auth::login_chatgpt(false).await {
                                            Ok(()) => {
                                                let mut models =
                                                    crate::auth::fetch_chatgpt_codex_models().await;
                                                if models.is_empty() {
                                                    models.push(crate::auth::ChatGptCodexModel {
                                                        id: "gpt-5.4".to_string(),
                                                        display_name: "gpt-5.4".to_string(),
                                                        context_window: 258_400,
                                                        max_output_tokens: 16_384,
                                                    });
                                                }
                                                let endpoints = models
                                                    .into_iter()
                                                    .map(|model| EndpointInfo {
                                                        name: model.display_name,
                                                        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
                                                        model_id: model.id,
                                                        max_context_tokens: model.context_window,
                                                        max_output_tokens: model.max_output_tokens,
                                                        endpoint_type: "chatgpt_codex".to_string(),
                                                        reasoning: crate::config::EndpointReasoningConfig::default(),
                                                        xai_priority_tier: false,
                                                    })
                                                    .collect();
                                                let msg = OutgoingMessage::EndpointsUpdated { endpoints };
                                                if let Ok(s) = serde_json::to_string(&msg) { let _ = tx.send(s); }
                                                let msg = OutgoingMessage::LoginComplete {
                                                    success: true,
                                                    message: "Logged in to ChatGPT Codex successfully.".to_string(),
                                                };
                                                if let Ok(s) = serde_json::to_string(&msg) { let _ = tx.send(s); }
                                            }
                                            Err(e) => {
                                                let msg = OutgoingMessage::LoginComplete {
                                                    success: false,
                                                    message: format!("Login failed: {}", e),
                                                };
                                                if let Ok(s) = serde_json::to_string(&msg) { let _ = tx.send(s); }
                                            }
                                        }
                                    });
                                }
                                Ok(msg) => {
                                    if let Some(action) = json_to_user_action(msg, &mut app_config, &action_tx) {
                                        let is_quit = matches!(action, UserAction::Quit);
                                        let _ = action_tx.send(action);
                                        if is_quit {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("headless: failed to parse incoming JSON: {}", e);
                                    eprintln!("headless: line was: {}", trimmed);
                                }
                            }
                        }
                        line_buf.clear();
                    }
                    Err(e) => {
                        eprintln!("headless: stdin read error: {}", e);
                        let _ = action_tx.send(UserAction::Quit);
                        break;
                    }
                }
            }

            // Login task results
            Some(json) = login_rx.recv() => {
                stdout.write_all(json.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
            event = event_rx.recv() => {
                match event {
                    Some(ev) => {
                        let msg = agent_event_to_json(&ev);
                        let json = serde_json::to_string(&msg)?;
                        stdout.write_all(json.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    None => {
                        // Agent channel closed
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

pub struct HeadlessInit {
    pub project_root: String,
    pub model_name: String,
    pub model_id: String,
    pub max_context_tokens: usize,
    pub log_path: String,
    pub dangerously_allow_all: bool,
    pub agent_definitions: Vec<AgentDefInfo>,
    pub endpoints: Vec<EndpointInfo>,
    pub session_id: Option<String>,
    pub resume_meta: Option<SessionMeta>,
    pub replay_entries: Vec<ReplayEntryJson>,
    pub rewind_checkpoints: Vec<RewindCheckpointJson>,
    pub available_tools: Vec<ToolInfo>,
    pub context_strategy: String,
    pub chatgpt_logged_in: bool,
    pub offline_mode: bool,
}

impl HeadlessInit {
    pub fn make_agent_def_info(
        defs: &[crate::agent::agent_def::AgentDefinition],
    ) -> Vec<AgentDefInfo> {
        defs.iter()
            .map(|def| {
                let model = match &def.model {
                    crate::agent::agent_def::AgentModel::Inherit => "inherit".to_string(),
                    crate::agent::agent_def::AgentModel::Named(n) => n.clone(),
                };
                let source = match &def.source {
                    crate::agent::agent_def::AgentDefSource::BuiltIn => "built-in".to_string(),
                    crate::agent::agent_def::AgentDefSource::ProjectFile(p) => {
                        format!("project:{}", p.display())
                    }
                    crate::agent::agent_def::AgentDefSource::GlobalFile(p) => {
                        format!("global:{}", p.display())
                    }
                };
                AgentDefInfo {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    model,
                    max_turns: def.max_turns,
                    tools: def.tools.clone(),
                    source,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod replay_fit_tests {
    use super::*;

    fn entry(kind: &str, content: &str) -> ReplayEntryJson {
        ReplayEntryJson {
            kind: kind.into(),
            content: content.into(),
            tool_name: None,
            success: None,
        }
    }

    fn frame_bytes(entries: &[ReplayEntryJson]) -> usize {
        serde_json::to_string(entries).unwrap().len()
    }

    #[test]
    fn a_transcript_that_fits_is_left_exactly_as_it_was() {
        let entries = vec![entry("user", "hello"), entry("assistant", "hi")];
        let out = fit_replay_entries(entries.clone(), REPLAY_BUDGET_BYTES);
        assert_eq!(out.len(), entries.len(), "no note should be added");
        assert_eq!(out[0].content, "hello");
    }

    #[test]
    fn many_entries_are_trimmed_from_the_oldest() {
        // The newest is what a resumed session is for.
        let budget = 20_000;
        let entries: Vec<_> = (0..200)
            .map(|i| entry("user", &format!("message {i} ").repeat(20)))
            .collect();
        let out = fit_replay_entries(entries, budget);
        assert!(frame_bytes(&out) <= budget * 2, "still enormous: {}", frame_bytes(&out));
        assert!(out.len() < 200, "nothing was dropped");
        assert!(out[0].kind == "system", "the user should be told: {:?}", out[0].kind);
        let last = out.last().unwrap();
        assert!(last.content.contains("message 199"), "the newest must survive: {}", last.content);
    }

    #[test]
    fn one_enormous_entry_does_not_evict_the_conversation_around_it() {
        // A large file read is a single tool result. Dropping oldest alone
        // would throw away everything else to make room for it.
        let budget = 20_000;
        let mut entries = vec![entry("user", "before")];
        entries.push(entry("tool_result", &"x".repeat(500_000)));
        entries.push(entry("assistant", "after"));
        let out = fit_replay_entries(entries, budget);
        assert!(
            out.iter().any(|e| e.content == "after"),
            "the newest entries should survive: {:?}",
            out.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        let big = out.iter().find(|e| e.kind == "tool_result").expect("kept, shortened");
        assert!(big.content.contains("not shown"), "and say it was shortened");
        assert!(big.content.len() < 500_000);
    }

    #[test]
    fn the_note_says_the_agent_still_remembers() {
        // The transcript is display only; the model's context comes from a
        // different path. Someone reading this must not think context was lost.
        let out = fit_replay_entries(
            (0..500).map(|i| entry("user", &format!("m{i} ").repeat(50))).collect(),
            10_000,
        );
        assert!(out[0].content.contains("still has its own context"), "{:?}", out[0].content);
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        // A multi-byte character cut down the middle is invalid UTF-8, and the
        // frame is JSON — it would fail to serialize at all.
        let budget = 5_000;
        let out = fit_replay_entries(vec![entry("assistant", &"日本語".repeat(100_000))], budget);
        let kept = &out.last().unwrap().content;
        assert!(kept.starts_with('日'), "got {:?}", &kept[..12.min(kept.len())]);
        assert!(serde_json::to_string(&out).is_ok());
    }

    #[test]
    fn an_empty_transcript_stays_empty() {
        assert!(fit_replay_entries(Vec::new(), REPLAY_BUDGET_BYTES).is_empty());
    }
}
