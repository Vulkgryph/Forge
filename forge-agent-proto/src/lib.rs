// SPDX-License-Identifier: Apache-2.0
//! Wire types for the `forge-agent --headless` protocol.
//!
//! One newline-delimited JSON message per line, in both directions:
//! [`AgentMessage`] on the agent's stdout, [`ClientMessage`] on its stdin. Both
//! are internally tagged on a `"type"` field whose value is the variant name in
//! snake_case.
//!
//! **Why this crate exists.** The protocol had three definitions: the agent's
//! own (private to its binary, so unusable by anyone else), forge-ide's Rust
//! mirror in `agent_panel.rs`, and forge-tui's zod schemas in `protocol.ts`.
//! Three hand-maintained copies of one contract, with nothing but review
//! catching drift between them — the README says as much: "a protocol change has
//! to be applied by hand in each client that needs it". These types are meant to
//! become the single definition all three use.
//!
//! **Forward compatibility.** A client built against an older protocol than the
//! agent it launches must not fall over when it sees an event it has never heard
//! of. [`AgentLine`] makes that explicit: anything unrecognised arrives as
//! [`AgentLine::Unknown`] carrying the raw JSON, so a client can log or ignore
//! it and carry on. The zod schemas threw instead, which turned a new event into
//! a dead session.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Agent → client ────────────────────────────────────────────────────────────

/// One line of the agent's stdout, tolerant of unrecognised messages.
///
/// `untagged` tries [`AgentMessage`] first and falls back to raw JSON, so a
/// protocol addition degrades to "ignored" rather than "fatal".
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum AgentLine {
    Known(AgentMessage),
    Unknown(Value),
}

impl AgentLine {
    /// Parse one line of the agent's stdout.
    ///
    /// Returns `Err` only for input that is not JSON at all — a well-formed
    /// message this build does not recognise becomes [`AgentLine::Unknown`].
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// The `"type"` tag, whether or not the variant is recognised. Useful for
    /// logging what was skipped.
    pub fn tag(&self) -> Option<&str> {
        match self {
            AgentLine::Known(m) => Some(m.tag()),
            AgentLine::Unknown(v) => v.get("type").and_then(Value::as_str),
        }
    }
}

/// A message from the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// Always the first message. Everything the client needs to render a
    /// session before any turn happens.
    Init(Box<Init>),

    // ── Turn lifecycle ────────────────────────────────────────────────────
    /// The model is working but has produced no output yet.
    Thinking,
    /// A reasoning block has begun; `ReasoningToken`s follow.
    Reasoning,
    ReasoningToken {
        content: String,
    },
    /// A complete assistant message, sent when not streaming.
    AssistantMessage {
        content: String,
    },
    /// One streamed chunk. Append to the message under construction.
    AssistantToken {
        content: String,
    },
    /// End of a streamed message, carrying the full text. Clients that
    /// accumulated tokens should prefer this as the authoritative version.
    AssistantDone {
        content: String,
    },
    /// The turn ended normally.
    Done,
    /// The turn was cancelled at the user's request.
    Cancelled,
    /// The turn's output was thrown away (a rewind landed mid-turn).
    TurnDiscarded,
    Error {
        message: String,
    },
    /// A transient API failure is being retried; purely informational.
    ApiRetry {
        attempt: usize,
        max_attempts: usize,
        delay_secs: u64,
        error: String,
    },

    // ── Tools ─────────────────────────────────────────────────────────────
    /// The model wants to run a tool. When `needs_approval`, nothing happens
    /// until the client sends `ApproveAction`/`DenyAction` with this `tool_id`.
    ToolRequest {
        tool_name: String,
        tool_args: String,
        tool_id: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_id: Option<String>,
        needs_approval: bool,
    },
    ToolResult {
        tool_name: String,
        result: String,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_id: Option<String>,
    },
    /// Incremental output from a still-running tool.
    ToolOutput {
        tool_name: String,
        content: String,
    },
    /// A foreground process is waiting on stdin; reply with `ProcessInput`.
    ProcessInputNeeded {
        prompt: String,
    },
    /// A background process wants input; reply with `BgProcessInput`.
    BackgroundPromptNeeded {
        bg_id: String,
        command: String,
        prompt: String,
    },

    // ── Usage and model ───────────────────────────────────────────────────
    /// Token accounting in reply to `RequestUsage`.
    Usage {
        snapshot: UsageSnapshot,
    },
    /// Unsolicited token accounting, pushed as a turn progresses.
    UsageUpdate {
        snapshot: UsageSnapshot,
    },
    ModelSwitched {
        name: String,
        model_id: String,
        max_context_tokens: usize,
    },
    EndpointsUpdated {
        endpoints: Vec<EndpointInfo>,
    },

    // ── Sessions ──────────────────────────────────────────────────────────
    SessionCleared {
        session_id: String,
        log_path: String,
    },
    /// A resumed session's full history, for replay into the transcript.
    SessionLoaded {
        session_id: String,
        title: String,
        message_count: usize,
        compaction_count: usize,
        entries: Vec<ReplayEntry>,
        rewind_checkpoints: Vec<RewindCheckpoint>,
    },

    // ── Subagents ─────────────────────────────────────────────────────────
    SubagentStarted {
        id: String,
        agent_type: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
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

    // ── Questions ─────────────────────────────────────────────────────────
    /// The agent is blocked on a structured question; reply with
    /// `AnswerQuestion`.
    QuestionRequest {
        question: String,
        tool_id: String,
        items: Vec<QuestionItem>,
    },

    // ── Plan mode ─────────────────────────────────────────────────────────
    PlanModeEntered {
        plan_path: String,
    },
    PlanModeExited {
        reason: String,
    },
    /// A plan is awaiting approval; reply with `ApprovePlan`,
    /// `ClearAndApprovePlan`, or `RejectPlan`.
    PlanReady {
        plan_path: String,
        content: String,
    },

    // ── Rewind ────────────────────────────────────────────────────────────
    /// A new checkpoint the user could rewind to.
    RewindCheckpoint {
        id: String,
        preview: String,
        message_count: usize,
        keep_on_restore: bool,
    },
    /// What rewinding to a checkpoint would do, in reply to `RewindPreview`
    /// or `RevertPreview`.
    RewindPreview {
        checkpoint_id: String,
        preview: String,
        summary: String,
    },

    // ── Login ─────────────────────────────────────────────────────────────
    LoginStatus {
        message: String,
    },
    LoginComplete {
        success: bool,
        message: String,
    },
}

impl AgentMessage {
    /// The wire tag for this variant, matching what serde emits.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Thinking => "thinking",
            Self::Reasoning => "reasoning",
            Self::ReasoningToken { .. } => "reasoning_token",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::AssistantToken { .. } => "assistant_token",
            Self::AssistantDone { .. } => "assistant_done",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::TurnDiscarded => "turn_discarded",
            Self::Error { .. } => "error",
            Self::ApiRetry { .. } => "api_retry",
            Self::ToolRequest { .. } => "tool_request",
            Self::ToolResult { .. } => "tool_result",
            Self::ToolOutput { .. } => "tool_output",
            Self::ProcessInputNeeded { .. } => "process_input_needed",
            Self::BackgroundPromptNeeded { .. } => "background_prompt_needed",
            Self::Usage { .. } => "usage",
            Self::UsageUpdate { .. } => "usage_update",
            Self::ModelSwitched { .. } => "model_switched",
            Self::EndpointsUpdated { .. } => "endpoints_updated",
            Self::SessionCleared { .. } => "session_cleared",
            Self::SessionLoaded { .. } => "session_loaded",
            Self::SubagentStarted { .. } => "subagent_started",
            Self::SubagentStatus { .. } => "subagent_status",
            Self::SubagentFinished { .. } => "subagent_finished",
            Self::QuestionRequest { .. } => "question_request",
            Self::PlanModeEntered { .. } => "plan_mode_entered",
            Self::PlanModeExited { .. } => "plan_mode_exited",
            Self::PlanReady { .. } => "plan_ready",
            Self::RewindCheckpoint { .. } => "rewind_checkpoint",
            Self::RewindPreview { .. } => "rewind_preview",
            Self::LoginStatus { .. } => "login_status",
            Self::LoginComplete { .. } => "login_complete",
        }
    }
}

/// The opening handshake.
///
/// Boxed inside [`AgentMessage`] because it is far larger than every other
/// variant, and an enum is sized by its largest member.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Init {
    pub project_root: String,
    pub model_name: String,
    pub model_id: String,
    pub max_context_tokens: usize,
    pub log_path: String,
    pub dangerously_allow_all: bool,
    #[serde(default)]
    pub agent_definitions: Vec<AgentDefInfo>,
    #[serde(default)]
    pub endpoints: Vec<EndpointInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub available_tools: Vec<ToolInfo>,
    pub context_strategy: String,
    pub chatgpt_logged_in: bool,
    pub offline_mode: bool,
}

// ── Shared payloads ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub enabled: bool,
}

/// One entry of a resumed session's history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub kind: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
}

/// A point the conversation can be rewound to.
///
/// Note this carries `display_index`, which the `RewindCheckpoint` *event* does
/// not — the event announces a new checkpoint, while this form appears in
/// `SessionLoaded` where checkpoints are numbered for display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RewindCheckpoint {
    pub id: String,
    pub preview: String,
    pub message_count: usize,
    pub display_index: usize,
    pub keep_on_restore: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub last_prompt_tokens: u32,
    pub last_completion_tokens: u32,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_requests: u64,
    pub max_context_tokens: usize,
    pub history_messages: usize,
}

impl UsageSnapshot {
    /// Fraction of the context window in use, clamped to `0.0..=1.0`.
    ///
    /// Guards the zero denominator: `max_context_tokens` is 0 for an endpoint
    /// that never reported one, and a NaN here would propagate into layout.
    pub fn context_fraction(&self) -> f32 {
        if self.max_context_tokens == 0 {
            return 0.0;
        }
        let used = self.last_prompt_tokens as f32 + self.last_completion_tokens as f32;
        (used / self.max_context_tokens as f32).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionItem {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentDefInfo {
    pub name: String,
    pub description: String,
    pub model: String,
    /// Serialized as `null` rather than omitted, so no `skip_serializing_if`.
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default)]
    pub tools: Vec<String>,
    pub source: String,
}

/// A configured model endpoint.
///
/// Deliberately carries no `api_key`: the agent owns credentials and never
/// discloses them, which is why `SwitchModel` echoes an endpoint's shape back
/// rather than the client holding secrets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointInfo {
    pub name: String,
    pub base_url: String,
    pub model_id: String,
    pub max_context_tokens: usize,
    pub max_output_tokens: u32,
    pub endpoint_type: String,
    #[serde(default)]
    pub reasoning: EndpointReasoningConfig,
    #[serde(default)]
    pub xai_priority_tier: bool,
}

// ── Reasoning configuration ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToggle {
    #[default]
    ProviderDefault,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompatibleReasoningConfig {
    #[serde(default)]
    pub thinking: ProviderToggle,
    #[serde(default)]
    pub preserve_thinking: ProviderToggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicReasoningConfig {
    #[serde(default)]
    pub thinking: ProviderToggle,
    #[serde(default = "default_anthropic_budget_tokens")]
    pub budget_tokens: u32,
}

fn default_anthropic_budget_tokens() -> u32 {
    8192
}

impl Default for AnthropicReasoningConfig {
    fn default() -> Self {
        Self {
            thinking: ProviderToggle::On,
            budget_tokens: default_anthropic_budget_tokens(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptCodexReasoningConfig {
    #[serde(default)]
    pub effort: ChatGptReasoningEffort,
}

impl Default for ChatGptCodexReasoningConfig {
    fn default() -> Self {
        Self { effort: ChatGptReasoningEffort::Medium }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointReasoningConfig {
    #[serde(default)]
    pub open_ai_compatible: OpenAiCompatibleReasoningConfig,
    #[serde(default)]
    pub anthropic: AnthropicReasoningConfig,
    #[serde(default)]
    pub chatgpt_codex: ChatGptCodexReasoningConfig,
}

// ── Client → agent ────────────────────────────────────────────────────────────

/// A message to the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    SendMessage {
        content: String,
    },
    /// Allow a pending `ToolRequest`. `tool_id` must match the request.
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
    /// Flip "approve everything from now on".
    ToggleAutoMode,
    CancelRun,
    Quit,

    // ── Configuration ─────────────────────────────────────────────────────
    SwitchModel {
        name: String,
        base_url: String,
        model_id: String,
        max_context_tokens: usize,
        max_output_tokens: u32,
        #[serde(default)]
        endpoint_type: String,
        #[serde(default)]
        reasoning: EndpointReasoningConfig,
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
        /// True clears `default_model` back to inherit.
        #[serde(default)]
        clear_default_model: bool,
        /// Wall-clock ceiling for a parent delegate_task batch (seconds).
        #[serde(default)]
        max_delegate_secs: Option<u64>,
    },
    /// Endpoint name, or empty to inherit.
    UpdateWebModel {
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
        reasoning: EndpointReasoningConfig,
    },
    UpdateXaiPriorityTier {
        endpoint_name: String,
        enabled: bool,
    },
    UpdateOfflineMode {
        enabled: bool,
    },

    // ── Sessions ──────────────────────────────────────────────────────────
    LoginChatgpt,
    ListSessions,
    ResumeSession {
        session_id: String,
    },
    ClearSession,
    Compact,
    RequestUsage,

    // ── Rewind ────────────────────────────────────────────────────────────
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

    // ── Plan mode ─────────────────────────────────────────────────────────
    EnterPlanMode,
    ApprovePlan,
    ClearAndApprovePlan,
    RejectPlan {
        #[serde(default)]
        feedback: String,
    },

    // ── Replies to agent requests ─────────────────────────────────────────
    AnswerQuestion {
        answer: String,
    },
    ProcessInput {
        content: String,
    },
    BgProcessInput {
        bg_id: String,
        content: String,
    },
}

impl ClientMessage {
    /// Serialize as one protocol line, newline included.
    ///
    /// The framing lives here so no caller has to remember it; a message
    /// written without its newline would hang the agent's `read_line`.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).unwrap_or_else(|_| {
            // Every variant is plain data with derived Serialize, so this is
            // unreachable; a panic in the send path is not worth the risk.
            String::from(r#"{"type":"request_usage"}"#)
        });
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tag agreement ─────────────────────────────────────────────────────

    /// `tag()` is hand-written, so it can drift from what serde emits. Checking
    /// them against each other is the point: a mismatch would mean logs and
    /// metrics naming events differently from the wire.
    #[test]
    fn hand_written_tags_match_what_serde_emits() {
        let samples = vec![
            AgentMessage::Init(Box::default()),
            AgentMessage::Thinking,
            AgentMessage::Reasoning,
            AgentMessage::ReasoningToken { content: String::new() },
            AgentMessage::AssistantMessage { content: String::new() },
            AgentMessage::AssistantToken { content: String::new() },
            AgentMessage::AssistantDone { content: String::new() },
            AgentMessage::Done,
            AgentMessage::Cancelled,
            AgentMessage::TurnDiscarded,
            AgentMessage::Error { message: String::new() },
            AgentMessage::ApiRetry {
                attempt: 1, max_attempts: 3, delay_secs: 1, error: String::new(),
            },
            AgentMessage::ToolRequest {
                tool_name: String::new(), tool_args: String::new(),
                tool_id: String::new(), kind: String::new(),
                subagent_id: None, needs_approval: false,
            },
            AgentMessage::ToolResult {
                tool_name: String::new(), result: String::new(),
                success: true, subagent_id: None,
            },
            AgentMessage::ToolOutput { tool_name: String::new(), content: String::new() },
            AgentMessage::ProcessInputNeeded { prompt: String::new() },
            AgentMessage::BackgroundPromptNeeded {
                bg_id: String::new(), command: String::new(), prompt: String::new(),
            },
            AgentMessage::Usage { snapshot: UsageSnapshot::default() },
            AgentMessage::UsageUpdate { snapshot: UsageSnapshot::default() },
            AgentMessage::ModelSwitched {
                name: String::new(), model_id: String::new(), max_context_tokens: 0,
            },
            AgentMessage::EndpointsUpdated { endpoints: Vec::new() },
            AgentMessage::SessionCleared { session_id: String::new(), log_path: String::new() },
            AgentMessage::SessionLoaded {
                session_id: String::new(), title: String::new(), message_count: 0,
                compaction_count: 0, entries: Vec::new(), rewind_checkpoints: Vec::new(),
            },
            AgentMessage::SubagentStarted {
                id: String::new(), agent_type: String::new(),
                prompt: String::new(), parent_id: None,
            },
            AgentMessage::SubagentStatus {
                id: String::new(), tool_name: String::new(), detail: String::new(),
            },
            AgentMessage::SubagentFinished {
                id: String::new(), agent_type: String::new(), summary: String::new(),
            },
            AgentMessage::QuestionRequest {
                question: String::new(), tool_id: String::new(), items: Vec::new(),
            },
            AgentMessage::PlanModeEntered { plan_path: String::new() },
            AgentMessage::PlanModeExited { reason: String::new() },
            AgentMessage::PlanReady { plan_path: String::new(), content: String::new() },
            AgentMessage::RewindCheckpoint {
                id: String::new(), preview: String::new(),
                message_count: 0, keep_on_restore: false,
            },
            AgentMessage::RewindPreview {
                checkpoint_id: String::new(), preview: String::new(), summary: String::new(),
            },
            AgentMessage::LoginStatus { message: String::new() },
            AgentMessage::LoginComplete { success: true, message: String::new() },
        ];

        // Every variant is represented, so adding one without a `tag()` arm
        // fails to compile and adding one without a sample fails here.
        assert_eq!(samples.len(), 34, "one sample per AgentMessage variant");

        for msg in &samples {
            let json: Value = serde_json::to_value(msg).expect("serializes");
            let wire = json.get("type").and_then(Value::as_str).expect("has a type tag");
            assert_eq!(wire, msg.tag(), "tag() disagrees with the wire for {msg:?}");
        }
    }

    // ── Exact wire shapes ─────────────────────────────────────────────────

    /// Unit variants must be a bare tag object. If serde emitted
    /// `{"type":"done","done":null}` the agent's matching would break.
    #[test]
    fn unit_variants_are_a_bare_tag() {
        assert_eq!(
            serde_json::to_string(&AgentMessage::Done).unwrap(),
            r#"{"type":"done"}"#,
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::CancelRun).unwrap(),
            r#"{"type":"cancel_run"}"#,
        );
    }

    /// Multi-word variants must be snake_case, not camelCase — the agent
    /// matches on the exact string.
    #[test]
    fn multi_word_tags_are_snake_case() {
        for (msg, expected) in [
            (AgentMessage::TurnDiscarded, "turn_discarded"),
            (AgentMessage::Thinking, "thinking"),
        ] {
            let json: Value = serde_json::to_value(&msg).unwrap();
            assert_eq!(json["type"], expected);
        }
        let json: Value = serde_json::to_value(ClientMessage::ToggleAutoMode).unwrap();
        assert_eq!(json["type"], "toggle_auto_mode");
        let json: Value = serde_json::to_value(ClientMessage::ClearAndApprovePlan).unwrap();
        assert_eq!(json["type"], "clear_and_approve_plan");
    }

    /// Absent optionals must deserialize, since the agent omits them entirely
    /// via `skip_serializing_if`.
    #[test]
    fn omitted_optional_fields_deserialize() {
        let line = r#"{"type":"tool_request","tool_name":"read","tool_args":"{}",
                       "tool_id":"t1","kind":"read","needs_approval":false}"#;
        let msg = match AgentLine::parse(line).unwrap() {
            AgentLine::Known(m) => m,
            AgentLine::Unknown(v) => panic!("should be recognised, got {v}"),
        };
        match msg {
            AgentMessage::ToolRequest { subagent_id, tool_id, .. } => {
                assert_eq!(subagent_id, None, "omitted subagent_id is None");
                assert_eq!(tool_id, "t1");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn init_round_trips() {
        let init = Init {
            project_root: "/tmp/p".into(),
            model_name: "Claude".into(),
            model_id: "claude-opus-5".into(),
            max_context_tokens: 200_000,
            log_path: "/tmp/l".into(),
            dangerously_allow_all: false,
            agent_definitions: vec![AgentDefInfo {
                name: "Explore".into(),
                description: "d".into(),
                model: "inherit".into(),
                max_turns: None,
                tools: vec!["read".into()],
                source: "built-in".into(),
            }],
            endpoints: vec![EndpointInfo {
                name: "e".into(),
                base_url: "https://x".into(),
                model_id: "m".into(),
                max_context_tokens: 1000,
                max_output_tokens: 100,
                endpoint_type: "anthropic".into(),
                reasoning: EndpointReasoningConfig::default(),
                xai_priority_tier: false,
            }],
            session_id: Some("s1".into()),
            available_tools: vec![ToolInfo { name: "read".into(), enabled: true }],
            context_strategy: "compact".into(),
            chatgpt_logged_in: false,
            offline_mode: false,
        };
        let line = serde_json::to_string(&AgentMessage::Init(Box::new(init))).unwrap();
        match AgentLine::parse(&line).unwrap() {
            AgentLine::Known(AgentMessage::Init(back)) => {
                assert_eq!(back.model_id, "claude-opus-5");
                assert_eq!(back.session_id.as_deref(), Some("s1"));
                assert_eq!(back.available_tools.len(), 1);
                assert_eq!(back.agent_definitions[0].name, "Explore");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ── Forward compatibility ─────────────────────────────────────────────

    /// The property the zod schemas lacked: an event from a newer agent must be
    /// survivable. Throwing here turned a protocol addition into a dead session.
    #[test]
    fn an_unrecognised_event_is_survivable() {
        let line = r#"{"type":"telepathy_engaged","strength":11}"#;
        match AgentLine::parse(line).unwrap() {
            AgentLine::Unknown(v) => {
                assert_eq!(v["type"], "telepathy_engaged");
                assert_eq!(v["strength"], 11, "raw payload is retained for logging");
            }
            AgentLine::Known(m) => panic!("should not have matched: {m:?}"),
        }
    }

    #[test]
    fn the_tag_is_readable_for_both_known_and_unknown() {
        assert_eq!(AgentLine::parse(r#"{"type":"done"}"#).unwrap().tag(), Some("done"));
        assert_eq!(AgentLine::parse(r#"{"type":"nope"}"#).unwrap().tag(), Some("nope"));
    }

    /// A known tag carrying the wrong shape must not be mistaken for that
    /// variant — it falls through to Unknown rather than deserializing to
    /// something wrong.
    #[test]
    fn a_known_tag_with_a_bad_payload_falls_through() {
        // `content` should be a string.
        let line = r#"{"type":"assistant_token","content":42}"#;
        assert!(matches!(AgentLine::parse(line).unwrap(), AgentLine::Unknown(_)));
    }

    /// Genuinely malformed input is an error, not silently Unknown.
    #[test]
    fn non_json_is_an_error() {
        assert!(AgentLine::parse("not json at all").is_err());
        assert!(AgentLine::parse("").is_err());
    }

    // ── Framing ───────────────────────────────────────────────────────────

    /// A line without its newline would leave the agent's `read_line` waiting
    /// forever, so framing is the type's job, not the caller's.
    #[test]
    fn client_lines_are_newline_terminated() {
        let line = ClientMessage::SendMessage { content: "hi".into() }.to_line();
        assert!(line.ends_with('\n'), "framing is included");
        assert_eq!(line.matches('\n').count(), 1, "exactly one");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["type"], "send_message");
        assert_eq!(parsed["content"], "hi");
    }

    /// Message content is arbitrary user text; newlines in it must be escaped
    /// by the JSON encoder rather than splitting the frame.
    #[test]
    fn newlines_in_content_do_not_break_framing() {
        let line = ClientMessage::SendMessage { content: "one\ntwo\nthree".into() }.to_line();
        assert_eq!(line.matches('\n').count(), 1, "only the frame terminator");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["content"], "one\ntwo\nthree");
    }

    #[test]
    fn client_messages_round_trip() {
        let messages = vec![
            ClientMessage::SendMessage { content: "x".into() },
            ClientMessage::ApproveAction { tool_id: "t".into() },
            ClientMessage::DenyAction { tool_id: "t".into(), reason: "no".into() },
            ClientMessage::Rewind { checkpoint_id: None },
            ClientMessage::RejectPlan { feedback: "redo".into() },
            ClientMessage::UpdateSubagentConfig {
                enabled: Some(true), max_concurrent: Some(2), max_depth: None,
                default_model: None, clear_default_model: false,
                max_delegate_secs: Some(600),
            },
            ClientMessage::Quit,
        ];
        for msg in messages {
            let line = msg.to_line();
            let back: ClientMessage = serde_json::from_str(line.trim_end())
                .unwrap_or_else(|e| panic!("{msg:?} failed to round-trip: {e}"));
            assert_eq!(
                serde_json::to_value(&msg).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }

    /// Your in-flight agent change added this field; it must survive the wire.
    #[test]
    fn max_delegate_secs_is_carried() {
        let line = ClientMessage::UpdateSubagentConfig {
            enabled: None, max_concurrent: None, max_depth: None,
            default_model: None, clear_default_model: false,
            max_delegate_secs: Some(900),
        }.to_line();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["max_delegate_secs"], 900);
    }

    // ── Reasoning config ──────────────────────────────────────────────────

    /// These defaults are not all zero-ish, and the agent relies on them: an
    /// endpoint that omits `reasoning` must land on Anthropic thinking On with
    /// an 8192 budget and Codex effort Medium.
    #[test]
    fn reasoning_defaults_match_the_agent() {
        let cfg = EndpointReasoningConfig::default();
        assert_eq!(cfg.anthropic.thinking, ProviderToggle::On);
        assert_eq!(cfg.anthropic.budget_tokens, 8192);
        assert_eq!(cfg.chatgpt_codex.effort, ChatGptReasoningEffort::Medium);
        assert_eq!(cfg.open_ai_compatible.thinking, ProviderToggle::ProviderDefault);
    }

    #[test]
    fn reasoning_enums_use_snake_case_on_the_wire() {
        let json = serde_json::to_string(&ProviderToggle::ProviderDefault).unwrap();
        assert_eq!(json, r#""provider_default""#);
        let json = serde_json::to_string(&ChatGptReasoningEffort::Xhigh).unwrap();
        assert_eq!(json, r#""xhigh""#);
    }

    #[test]
    fn an_endpoint_without_reasoning_deserializes_to_defaults() {
        let line = r#"{"name":"e","base_url":"u","model_id":"m",
                       "max_context_tokens":1,"max_output_tokens":2,
                       "endpoint_type":"anthropic"}"#;
        let ep: EndpointInfo = serde_json::from_str(line).unwrap();
        assert_eq!(ep.reasoning.anthropic.budget_tokens, 8192);
        assert!(!ep.xai_priority_tier);
    }

    // ── Usage ─────────────────────────────────────────────────────────────

    #[test]
    fn context_fraction_handles_a_missing_window() {
        let snap = UsageSnapshot { max_context_tokens: 0, ..Default::default() };
        assert_eq!(snap.context_fraction(), 0.0, "no NaN into layout");
    }

    #[test]
    fn context_fraction_is_clamped() {
        let snap = UsageSnapshot {
            last_prompt_tokens: 900, last_completion_tokens: 200,
            max_context_tokens: 1000, ..Default::default()
        };
        assert_eq!(snap.context_fraction(), 1.0, "over-full clamps to 1");
    }

    #[test]
    fn context_fraction_is_proportional() {
        let snap = UsageSnapshot {
            last_prompt_tokens: 250, last_completion_tokens: 250,
            max_context_tokens: 1000, ..Default::default()
        };
        assert!((snap.context_fraction() - 0.5).abs() < f32::EPSILON);
    }
}
