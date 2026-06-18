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
        anthropic_logged_in: bool,
        chatgpt_logged_in: bool,
    },
    Thinking,
    Reasoning,
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
    },
    ToolResult {
        tool_name: String,
        result: String,
        success: bool,
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

#[derive(Serialize)]
pub struct ReplayEntryJson {
    pub kind: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
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
            }
        }
        AgentEvent::ToolResult {
            tool_name,
            result,
            success,
        } => OutgoingMessage::ToolResult {
            tool_name: tool_name.clone(),
            result: result.clone(),
            success: *success,
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
        } => OutgoingMessage::SubagentStarted {
            id: id.clone(),
            agent_type: agent_type.clone(),
            prompt: prompt.clone(),
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
    LoginAnthropic,
    LoginChatgpt,
    ListSessions,
    ResumeSession {
        #[allow(dead_code)] // resume is wired via --resume-session CLI flag; runtime resume path not yet implemented
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
        IncomingMessage::DenyAction { reason } => Some(UserAction::DenyAction(reason)),
        IncomingMessage::ToggleAutoMode => Some(UserAction::ToggleAutoMode),
        IncomingMessage::SwitchModel {
            name,
            base_url,
            model_id,
            max_context_tokens,
            max_output_tokens,
            endpoint_type,
            reasoning,
        } => {
            let ep_type = match endpoint_type.as_str() {
                "anthropic" => crate::config::EndpointType::Anthropic,
                "chatgpt_codex" => crate::config::EndpointType::ChatGptCodex,
                _ => crate::config::EndpointType::OpenAi,
            };
            Some(UserAction::SwitchModel(ModelEndpoint {
                name,
                base_url,
                model_id,
                api_key: None,
                max_context_tokens,
                max_output_tokens,
                request_timeout_secs: crate::config::default_request_timeout_secs(),
                endpoint_type: ep_type,
                reasoning,
            }))
        }
        IncomingMessage::UpdateSubagentConfig {
            enabled,
            max_concurrent,
            max_depth,
            default_model,
            clear_default_model,
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
        IncomingMessage::LoginAnthropic | IncomingMessage::LoginChatgpt => None,
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
        anthropic_logged_in: init_info.anthropic_logged_in,
        chatgpt_logged_in: init_info.chatgpt_logged_in,
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
                        let trimmed = line_buf.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<IncomingMessage>(trimmed) {
                                Ok(IncomingMessage::LoginAnthropic) => {
                                    let tx = login_tx.clone();
                                    tokio::spawn(async move {
                                        let status = OutgoingMessage::LoginStatus {
                                            message: "Opening browser for Claude authorization...".to_string(),
                                        };
                                        if let Ok(s) = serde_json::to_string(&status) {
                                            let _ = tx.send(s);
                                        }
                                        // Inside the headless protocol — stdin is owned by the
                                        // message reader, so the paste fallback isn't available.
                                        // login() will bail with a clear error if the callback
                                        // port is busy, pointing the user at `forge --login`.
                                        match crate::auth::login(false).await {
                                            Ok(()) => {
                                                // Fetch available models and push them to the UI
                                                let http = reqwest::Client::new();
                                                if let Ok(token) = crate::auth::get_valid_token(&http).await {
                                                    let models = crate::auth::fetch_anthropic_models(&http, &token).await;
                                                    let endpoints: Vec<EndpointInfo> = models.iter().map(|m| EndpointInfo {
                                                        name: m.display_name.clone(),
                                                        base_url: "https://api.anthropic.com".to_string(),
                                                        model_id: m.id.clone(),
                                                        max_context_tokens: m.context_window,
                                                        max_output_tokens: m.max_output_tokens,
                                                        endpoint_type: "anthropic".to_string(),
                                                        reasoning: crate::config::EndpointReasoningConfig::default(),
                                                    }).collect();
                                                    if !endpoints.is_empty() {
                                                        let msg = OutgoingMessage::EndpointsUpdated { endpoints };
                                                        if let Ok(s) = serde_json::to_string(&msg) { let _ = tx.send(s); }
                                                    }
                                                }
                                                let msg = OutgoingMessage::LoginComplete {
                                                    success: true,
                                                    message: "Logged in to Claude successfully.".to_string(),
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
    pub anthropic_logged_in: bool,
    pub chatgpt_logged_in: bool,
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
