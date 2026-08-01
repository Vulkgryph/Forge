// SPDX-License-Identifier: Apache-2.0
//! Session state: everything the agent has told us, and what it is waiting on.
//!
//! A pure state machine. [`Session::apply`] takes one [`AgentMessage`] and
//! returns the [`Effect`]s the caller should carry out; nothing here spawns a
//! process, touches a terminal, or reads a clock the caller cannot control. That
//! makes the parts most likely to be subtly wrong — the tool-approval gate, token
//! accumulation, turn teardown — testable without an agent or a tty.
//!
//! Two things it does *not* carry over from the TypeScript version:
//!
//!  * **No scrollback/transient split.** That existed to feed ink's `<Static>`,
//!    which needs committed output separated from in-progress output — and it was
//!    the direct cause of the doubled-messages bug, because the split index could
//!    move backwards and `<Static>` reprints when its list shrinks. There is one
//!    [`Session::entries`] list here, and the renderer draws a window onto it.
//!  * **No id counter.** Entries were keyed by a monotonic `e-N` string so React
//!    could reconcile them. A renderer that diffs cells needs no keys.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use forge_agent_proto::{
    AgentDefInfo, AgentMessage, ClientMessage, EndpointInfo, QuestionItem, ReplayEntry,
    RewindCheckpoint, ToolInfo, UsageSnapshot,
};

/// What the agent is doing right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Idle,
    /// Working, with nothing emitted yet.
    Thinking,
    /// Emitting a reasoning block.
    Reasoning,
    /// Emitting an assistant message.
    Streaming,
    /// Running a tool, named for the status line.
    RunningTool(String),
    /// Retrying after a transient API failure.
    Retrying {
        attempt: usize,
        max_attempts: usize,
    },
}

impl Activity {
    /// Text for the status line, or `None` when idle.
    pub fn label(&self) -> Option<String> {
        match self {
            Activity::Idle => None,
            Activity::Thinking => Some("thinking".into()),
            Activity::Reasoning => Some("reasoning".into()),
            Activity::Streaming => Some("responding".into()),
            Activity::RunningTool(name) => Some(format!("running {name}")),
            Activity::Retrying { attempt, max_attempts } => {
                Some(format!("retrying ({attempt}/{max_attempts})"))
            }
        }
    }

    pub fn is_busy(&self) -> bool {
        *self != Activity::Idle
    }
}

/// How much the user has to be asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Ask before every tool that requests approval.
    #[default]
    Ask,
    /// Approve everything without asking. Set by the agent's
    /// `--dangerously-allow-all`, or toggled at runtime.
    AllowAll,
    /// Read-only planning; the agent enforces this, we only display it.
    Plan,
}

/// What kind of line this is in the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    /// A reasoning block still being emitted.
    Reasoning,
    /// A finished reasoning block, collapsed to a summary.
    Thought,
    ToolCall,
    ToolResult,
    ToolOutput,
    System,
    Error,
    PlanContent,
    PlanStatus,
    SubagentHeader,
}

/// One line of the transcript.
#[derive(Clone, Debug)]
pub struct Entry {
    pub kind:    EntryKind,
    pub content: String,
    /// Set for tool entries.
    pub tool_name: Option<String>,
    pub tool_id:   Option<String>,
    pub success:   Option<bool>,
    /// How long a `Thought` took.
    pub duration:  Option<Duration>,
    /// Which subagent produced this, if any.
    pub subagent_id: Option<String>,
}

impl Entry {
    fn new(kind: EntryKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
            tool_name: None,
            tool_id: None,
            success: None,
            duration: None,
            subagent_id: None,
        }
    }
}

/// Something the agent is blocked on, needing a reply from the user.
///
/// One at a time by construction: the agent does not ask two things at once, and
/// modelling it as an `Option` rather than six independent fields means the UI
/// cannot end up showing two dialogs stacked.
#[derive(Clone, Debug)]
pub enum Pending {
    Approval {
        tool_name: String,
        tool_id:   String,
        tool_args: String,
        kind:      String,
    },
    Plan {
        plan_path: String,
        content:   String,
    },
    Question {
        question: String,
        tool_id:  String,
        items:    Vec<QuestionItem>,
    },
    /// A foreground process wants stdin.
    ProcessInput {
        prompt: String,
    },
    /// A background process wants stdin.
    BackgroundInput {
        bg_id:   String,
        command: String,
        prompt:  String,
    },
    /// A rewind is awaiting confirmation.
    Rewind {
        checkpoint_id: String,
        preview:       String,
        summary:       String,
    },
}

#[derive(Clone, Debug)]
pub struct Subagent {
    pub id:         String,
    pub agent_type: String,
    pub prompt:     String,
    pub parent_id:  Option<String>,
    pub detail:     String,
}

/// What the caller should do as a result of applying a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Send this to the agent.
    Send(ClientMessage),
    /// The turn finished; ring the terminal bell if the user is not watching.
    TurnComplete,
}

/// Everything known about the current session.
pub struct Session {
    // ── From init ─────────────────────────────────────────────────────────
    pub connected:      bool,
    pub session_id:     Option<String>,
    pub project_root:   String,
    pub model_name:     String,
    pub model_id:       String,
    pub max_context_tokens: usize,
    pub log_path:       String,
    pub agent_defs:     Vec<AgentDefInfo>,
    pub endpoints:      Vec<EndpointInfo>,
    pub available_tools: Vec<ToolInfo>,
    pub context_strategy: String,
    pub offline_mode:   bool,
    pub chatgpt_logged_in: bool,

    // ── Turn state ────────────────────────────────────────────────────────
    pub activity:        Activity,
    pub permission_mode: PermissionMode,
    pub plan_mode:       bool,
    pub login_in_progress: bool,

    // ── Transcript ────────────────────────────────────────────────────────
    entries: Vec<Entry>,
    /// Index of the assistant entry being streamed into.
    streaming: Option<usize>,
    /// Index of the reasoning entry being streamed into, and when it began.
    reasoning: Option<(usize, Instant)>,
    /// Where this turn's output starts, so a discarded turn can be removed
    /// whole. Tracking only the *open* entries is not enough: a reasoning block
    /// that already closed is still part of the turn being thrown away.
    turn_start: Option<usize>,

    // ── Interaction ───────────────────────────────────────────────────────
    pub pending:     Option<Pending>,
    pub usage:       Option<UsageSnapshot>,
    pub subagents:   Vec<Subagent>,
    pub checkpoints: Vec<RewindCheckpoint>,

    /// Tools the user approved for the rest of the session, by name.
    approved_tools: HashSet<String>,
    /// Whether finished reasoning blocks show their full text.
    ///
    /// Collapsed by default, matching the TypeScript client: a transcript that
    /// prints every thought in full is mostly grey text, and the summary line is
    /// enough unless you want to read it.
    pub reasoning_expanded: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            connected: false,
            session_id: None,
            project_root: String::new(),
            model_name: String::new(),
            model_id: String::new(),
            max_context_tokens: 0,
            log_path: String::new(),
            agent_defs: Vec::new(),
            endpoints: Vec::new(),
            available_tools: Vec::new(),
            context_strategy: String::new(),
            offline_mode: false,
            chatgpt_logged_in: false,
            activity: Activity::Idle,
            permission_mode: PermissionMode::Ask,
            plan_mode: false,
            login_in_progress: false,
            entries: Vec::new(),
            streaming: None,
            reasoning: None,
            turn_start: None,
            pending: None,
            usage: None,
            subagents: Vec::new(),
            checkpoints: Vec::new(),
            approved_tools: HashSet::new(),
            reasoning_expanded: false,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Record what the user sent, so it appears before the reply.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::User, text));
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::System, text));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::Error, text));
    }

    /// Apply one message from the agent.
    pub fn apply(&mut self, msg: AgentMessage) -> Vec<Effect> {
        let mut effects = Vec::new();

        // The first output of a turn fixes the point a discard would rewind to.
        if self.turn_start.is_none() && starts_a_turn(&msg) {
            self.turn_start = Some(self.entries.len());
        }

        match msg {
            AgentMessage::Init(init) => {
                self.connected = true;
                self.project_root = init.project_root;
                self.model_name = init.model_name;
                self.model_id = init.model_id;
                self.max_context_tokens = init.max_context_tokens;
                self.log_path = init.log_path;
                self.agent_defs = init.agent_definitions;
                self.endpoints = init.endpoints;
                self.available_tools = init.available_tools;
                self.context_strategy = init.context_strategy;
                self.offline_mode = init.offline_mode;
                self.chatgpt_logged_in = init.chatgpt_logged_in;
                self.session_id = init.session_id;
                if init.dangerously_allow_all {
                    self.permission_mode = PermissionMode::AllowAll;
                }
                self.entries.push(Entry::new(
                    EntryKind::System,
                    format!("◆ {} ({})", self.model_name, self.model_id),
                ));
            }

            // ── Turn lifecycle ────────────────────────────────────────────
            AgentMessage::Thinking => self.activity = Activity::Thinking,

            AgentMessage::Reasoning => {
                self.activity = Activity::Reasoning;
                // Only open a block if one is not already open: the agent can
                // send `reasoning` more than once in a turn.
                if self.reasoning.is_none() {
                    self.entries.push(Entry::new(EntryKind::Reasoning, String::new()));
                    self.reasoning = Some((self.entries.len() - 1, Instant::now()));
                }
            }

            AgentMessage::ReasoningToken { content } => {
                self.activity = Activity::Reasoning;
                let idx = match self.reasoning {
                    Some((i, _)) => i,
                    None => {
                        // Tokens without an opening `reasoning`; start a block
                        // rather than dropping the text.
                        self.entries.push(Entry::new(EntryKind::Reasoning, String::new()));
                        let i = self.entries.len() - 1;
                        self.reasoning = Some((i, Instant::now()));
                        i
                    }
                };
                self.entries[idx].content.push_str(&content);
            }

            AgentMessage::AssistantToken { content } => {
                self.activity = Activity::Streaming;
                self.finish_reasoning();
                let idx = match self.streaming {
                    Some(i) => i,
                    None => {
                        self.entries.push(Entry::new(EntryKind::Assistant, String::new()));
                        let i = self.entries.len() - 1;
                        self.streaming = Some(i);
                        i
                    }
                };
                self.entries[idx].content.push_str(&content);
            }

            AgentMessage::AssistantMessage { content } => {
                self.finish_reasoning();
                // A complete message arriving mid-stream replaces what was
                // accumulated rather than appending to it.
                match self.streaming.take() {
                    Some(i) => self.entries[i].content = content,
                    None => self.entries.push(Entry::new(EntryKind::Assistant, content)),
                }
                self.activity = Activity::Idle;
            }

            AgentMessage::AssistantDone { content } => {
                self.finish_reasoning();
                // The authoritative text. Prefer it over the accumulation,
                // which can differ if a token was dropped.
                match self.streaming.take() {
                    Some(i) => {
                        if !content.is_empty() {
                            self.entries[i].content = content;
                        }
                    }
                    None if !content.is_empty() => {
                        self.entries.push(Entry::new(EntryKind::Assistant, content));
                    }
                    None => {}
                }
                self.activity = Activity::Idle;
            }

            AgentMessage::Done => {
                self.end_turn();
                effects.push(Effect::TurnComplete);
            }

            AgentMessage::Cancelled => {
                self.end_turn();
                self.entries.push(Entry::new(EntryKind::System, "cancelled"));
            }

            AgentMessage::TurnDiscarded => {
                // A rewind landed mid-turn: the whole turn's output goes, not
                // just whatever happened to still be open.
                self.discard_turn();
                self.end_turn();
            }

            AgentMessage::Error { message } => {
                self.finish_reasoning();
                self.streaming = None;
                self.entries.push(Entry::new(EntryKind::Error, message));
                self.activity = Activity::Idle;
                self.pending = None;
                self.turn_start = None;
            }

            AgentMessage::ApiRetry { attempt, max_attempts, delay_secs, error } => {
                self.activity = Activity::Retrying { attempt, max_attempts };
                self.entries.push(Entry::new(
                    EntryKind::System,
                    format!(
                        "retrying in {delay_secs}s ({attempt}/{max_attempts}): {}",
                        first_line(&error),
                    ),
                ));
            }

            // ── Tools ─────────────────────────────────────────────────────
            AgentMessage::ToolRequest {
                tool_name, tool_args, tool_id, kind, subagent_id, needs_approval,
            } => {
                self.finish_reasoning();
                self.streaming = None;
                self.activity = Activity::RunningTool(tool_name.clone());

                let mut entry = Entry::new(EntryKind::ToolCall, summarize_tool(&tool_name, &tool_args));
                entry.tool_name = Some(tool_name.clone());
                entry.tool_id = Some(tool_id.clone());
                entry.subagent_id = subagent_id;
                self.entries.push(entry);

                // The gate. Approval is skipped when the whole session allows
                // it, or when this tool was approved earlier — matching the
                // agent's own `needs_approval` rather than second-guessing it.
                let pre_approved = self.permission_mode == PermissionMode::AllowAll
                    || self.approved_tools.contains(&tool_name);
                if needs_approval && !pre_approved {
                    self.pending = Some(Pending::Approval {
                        tool_name, tool_id, tool_args, kind,
                    });
                } else if needs_approval {
                    effects.push(Effect::Send(ClientMessage::ApproveAction { tool_id }));
                }
            }

            AgentMessage::ToolResult { tool_name, result, success, subagent_id } => {
                let mut entry = Entry::new(EntryKind::ToolResult, result);
                entry.tool_name = Some(tool_name);
                entry.success = Some(success);
                entry.subagent_id = subagent_id;
                self.entries.push(entry);
                // Back to whatever the model does next; `done` or the next
                // request will correct this.
                self.activity = Activity::Thinking;
            }

            AgentMessage::ToolOutput { tool_name, content } => {
                // Incremental output from a running tool: append to the tail
                // entry when it is that tool's output, so a long build log does
                // not become one entry per chunk.
                match self.entries.last_mut() {
                    Some(e)
                        if e.kind == EntryKind::ToolOutput
                            && e.tool_name.as_deref() == Some(tool_name.as_str()) =>
                    {
                        e.content.push_str(&content);
                    }
                    _ => {
                        let mut entry = Entry::new(EntryKind::ToolOutput, content);
                        entry.tool_name = Some(tool_name);
                        self.entries.push(entry);
                    }
                }
            }

            AgentMessage::ProcessInputNeeded { prompt } => {
                self.pending = Some(Pending::ProcessInput { prompt });
            }

            AgentMessage::BackgroundPromptNeeded { bg_id, command, prompt } => {
                self.pending = Some(Pending::BackgroundInput { bg_id, command, prompt });
            }

            // ── Usage and model ───────────────────────────────────────────
            AgentMessage::Usage { snapshot } | AgentMessage::UsageUpdate { snapshot } => {
                self.usage = Some(snapshot);
            }

            AgentMessage::ModelSwitched { name, model_id, max_context_tokens } => {
                self.model_name = name;
                self.model_id = model_id;
                self.max_context_tokens = max_context_tokens;
                self.entries.push(Entry::new(
                    EntryKind::System,
                    format!("◆ switched to {} ({})", self.model_name, self.model_id),
                ));
            }

            AgentMessage::EndpointsUpdated { endpoints } => self.endpoints = endpoints,

            // ── Sessions ──────────────────────────────────────────────────
            AgentMessage::SessionCleared { session_id, log_path } => {
                self.session_id = Some(session_id);
                self.log_path = log_path;
                self.entries.clear();
                self.streaming = None;
                self.reasoning = None;
                self.turn_start = None;
                self.pending = None;
                self.subagents.clear();
                self.checkpoints.clear();
                self.usage = None;
                self.activity = Activity::Idle;
                self.entries.push(Entry::new(EntryKind::System, "session cleared"));
            }

            AgentMessage::SessionLoaded {
                session_id, title, message_count, entries, rewind_checkpoints, ..
            } => {
                self.session_id = Some(session_id);
                self.checkpoints = rewind_checkpoints;
                self.entries.push(Entry::new(
                    EntryKind::System,
                    format!("resumed \"{title}\" — {message_count} messages"),
                ));
                for replay in entries {
                    self.entries.push(replay_to_entry(replay));
                }
            }

            // ── Subagents ─────────────────────────────────────────────────
            AgentMessage::SubagentStarted { id, agent_type, prompt, parent_id } => {
                self.entries.push(Entry::new(
                    EntryKind::SubagentHeader,
                    format!("▸ {agent_type}: {}", first_line(&prompt)),
                ));
                self.subagents.push(Subagent {
                    id, agent_type, prompt, parent_id, detail: String::new(),
                });
            }

            AgentMessage::SubagentStatus { id, tool_name, detail } => {
                if let Some(sub) = self.subagents.iter_mut().find(|s| s.id == id) {
                    sub.detail = if detail.is_empty() { tool_name } else { detail };
                }
            }

            AgentMessage::SubagentFinished { id, agent_type, summary } => {
                self.subagents.retain(|s| s.id != id);
                self.entries.push(Entry::new(
                    EntryKind::SubagentHeader,
                    format!("▸ {agent_type} finished: {}", first_line(&summary)),
                ));
            }

            // ── Questions ─────────────────────────────────────────────────
            AgentMessage::QuestionRequest { question, tool_id, items } => {
                self.pending = Some(Pending::Question { question, tool_id, items });
            }

            // ── Plan mode ─────────────────────────────────────────────────
            AgentMessage::PlanModeEntered { plan_path } => {
                self.plan_mode = true;
                self.permission_mode = PermissionMode::Plan;
                self.entries.push(Entry::new(
                    EntryKind::PlanStatus,
                    format!("plan mode — {plan_path}"),
                ));
            }

            AgentMessage::PlanModeExited { reason } => {
                self.plan_mode = false;
                self.permission_mode = PermissionMode::Ask;
                self.entries.push(Entry::new(
                    EntryKind::PlanStatus,
                    format!("left plan mode: {reason}"),
                ));
            }

            AgentMessage::PlanReady { plan_path, content } => {
                self.pending = Some(Pending::Plan { plan_path, content });
            }

            // ── Rewind ────────────────────────────────────────────────────
            AgentMessage::RewindCheckpoint { id, preview, message_count, keep_on_restore } => {
                // The event carries no display index; number them in arrival
                // order, which is what the checkpoint list shows.
                let display_index = self.checkpoints.len() + 1;
                self.checkpoints.push(RewindCheckpoint {
                    id, preview, message_count, display_index, keep_on_restore,
                });
            }

            AgentMessage::RewindPreview { checkpoint_id, preview, summary } => {
                self.pending = Some(Pending::Rewind { checkpoint_id, preview, summary });
            }

            // ── Login ─────────────────────────────────────────────────────
            AgentMessage::LoginStatus { message } => {
                self.login_in_progress = true;
                self.entries.push(Entry::new(EntryKind::System, message));
            }

            AgentMessage::LoginComplete { success, message } => {
                self.login_in_progress = false;
                if success {
                    self.chatgpt_logged_in = true;
                }
                self.entries.push(Entry::new(
                    if success { EntryKind::System } else { EntryKind::Error },
                    message,
                ));
            }
        }

        effects
    }

    // ── User decisions ────────────────────────────────────────────────────

    /// Approve the pending tool. `remember` approves that tool for the session.
    pub fn approve(&mut self, remember: bool) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::Approval { tool_id, tool_name, .. }) => {
                if remember {
                    self.approved_tools.insert(tool_name);
                }
                vec![Effect::Send(ClientMessage::ApproveAction { tool_id })]
            }
            other => {
                // Not an approval; put it back rather than silently dropping
                // whatever the agent was actually waiting on.
                self.pending = other;
                Vec::new()
            }
        }
    }

    pub fn deny(&mut self, reason: impl Into<String>) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::Approval { tool_id, .. }) => {
                vec![Effect::Send(ClientMessage::DenyAction {
                    tool_id,
                    reason: reason.into(),
                })]
            }
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    /// Answer whatever the agent is waiting on, when it takes free text.
    pub fn reply(&mut self, text: impl Into<String>) -> Vec<Effect> {
        let text = text.into();
        match self.pending.take() {
            Some(Pending::Question { .. }) => {
                vec![Effect::Send(ClientMessage::AnswerQuestion { answer: text })]
            }
            Some(Pending::ProcessInput { .. }) => {
                vec![Effect::Send(ClientMessage::ProcessInput { content: text })]
            }
            Some(Pending::BackgroundInput { bg_id, .. }) => {
                vec![Effect::Send(ClientMessage::BgProcessInput { bg_id, content: text })]
            }
            Some(Pending::Plan { .. }) => {
                vec![Effect::Send(ClientMessage::RejectPlan { feedback: text })]
            }
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    pub fn approve_plan(&mut self, clear_context: bool) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::Plan { .. }) => vec![Effect::Send(if clear_context {
                ClientMessage::ClearAndApprovePlan
            } else {
                ClientMessage::ApprovePlan
            })],
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    /// Confirm a pending rewind.
    ///
    /// Separate from [`Session::approve`] because a rewind is not a tool call:
    /// it answers with the checkpoint id rather than a tool id, and approving
    /// the wrong one would rewind to the wrong place.
    pub fn confirm_rewind(&mut self) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::Rewind { checkpoint_id, .. }) => {
                vec![Effect::Send(ClientMessage::Rewind {
                    checkpoint_id: Some(checkpoint_id),
                })]
            }
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    /// Show or hide the full text of finished reasoning blocks.
    pub fn toggle_reasoning(&mut self) {
        self.reasoning_expanded = !self.reasoning_expanded;
    }

    /// Dismiss whatever is pending without answering.
    ///
    /// Only correct where the agent is not blocked on the reply; the dialog
    /// layer decides which prompts may be cancelled at all.
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    /// Flip between asking and approving everything.
    pub fn toggle_permission_mode(&mut self) -> Vec<Effect> {
        self.permission_mode = match self.permission_mode {
            PermissionMode::AllowAll => PermissionMode::Ask,
            // Plan mode is the agent's to leave, so treat it as Ask here.
            _ => PermissionMode::AllowAll,
        };
        vec![Effect::Send(ClientMessage::ToggleAutoMode)]
    }

    /// Fraction of the context window in use, for the context bar.
    pub fn context_fraction(&self) -> f32 {
        self.usage.map(|u| u.context_fraction()).unwrap_or(0.0)
    }

    // ── Internals ─────────────────────────────────────────────────────────

    /// Collapse an open reasoning block into a `Thought` with its duration.
    fn finish_reasoning(&mut self) {
        if let Some((idx, started)) = self.reasoning.take() {
            let elapsed = started.elapsed();
            let entry = &mut self.entries[idx];
            entry.kind = EntryKind::Thought;
            entry.duration = Some(elapsed);
            // An empty block is noise; drop it rather than leaving a bare
            // "thought for 0s" in the transcript.
            if entry.content.trim().is_empty() {
                self.entries.remove(idx);
                self.shift_indices_after(idx);
            }
        }
    }

    fn end_turn(&mut self) {
        self.finish_reasoning();
        self.streaming = None;
        self.turn_start = None;
        self.activity = Activity::Idle;
        // A turn that ends with something still pending would leave a dialog on
        // screen that the agent is no longer waiting on.
        self.pending = None;
        self.subagents.clear();
    }

    /// Drop everything this turn produced.
    fn discard_turn(&mut self) {
        self.streaming = None;
        self.reasoning = None;
        if let Some(start) = self.turn_start.take() {
            self.entries.truncate(start.min(self.entries.len()));
        }
    }

    /// Keep the streaming/reasoning cursors valid after removing an entry.
    fn shift_indices_after(&mut self, removed: usize) {
        if let Some(i) = self.streaming.as_mut() {
            if *i > removed {
                *i -= 1;
            }
        }
        if let Some((i, _)) = self.reasoning.as_mut() {
            if *i > removed {
                *i -= 1;
            }
        }
    }
}

/// Whether this message is the first sign of a turn producing output.
///
/// Deliberately excludes bookkeeping the agent can send at any time (usage,
/// checkpoints, endpoint updates), which must not be mistaken for the start of a
/// turn and so must not be swallowed by a discard.
fn starts_a_turn(msg: &AgentMessage) -> bool {
    matches!(
        msg,
        AgentMessage::Thinking
            | AgentMessage::Reasoning
            | AgentMessage::ReasoningToken { .. }
            | AgentMessage::AssistantToken { .. }
            | AgentMessage::AssistantMessage { .. }
            | AgentMessage::ToolRequest { .. }
    )
}

/// First non-empty line, trimmed — for one-line summaries of long text.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// A short description of a tool call for the transcript.
///
/// The full arguments are JSON and often large; the transcript shows the
/// interesting field per tool, mirroring what the TypeScript client displayed.
fn summarize_tool(name: &str, args_json: &str) -> String {
    let args = forge_agent_proto::json::parse(args_json)
        .unwrap_or(forge_agent_proto::json::Json::Null);
    let field = |k: &str| args.str_or_empty(k);

    let detail = match name {
        "read_file" | "write_file" | "edit_file" | "apply_patch" => field("path"),
        "shell_exec" => field("command"),
        "search_code" => field("pattern"),
        "glob_files" => field("glob"),
        "list_directory" => field("path"),
        "delegate_task" => field("description"),
        _ => String::new(),
    };

    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}({})", first_line(&detail))
    }
}

fn replay_to_entry(replay: ReplayEntry) -> Entry {
    let kind = match replay.kind.as_str() {
        "user" => EntryKind::User,
        "assistant" => EntryKind::Assistant,
        "tool_call" => EntryKind::ToolCall,
        "tool_result" => EntryKind::ToolResult,
        "error" => EntryKind::Error,
        "reasoning" | "thought" => EntryKind::Thought,
        _ => EntryKind::System,
    };
    let mut entry = Entry::new(kind, replay.content);
    entry.tool_name = replay.tool_name;
    entry.success = replay.success;
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent_proto::Init;

    fn init_msg() -> AgentMessage {
        AgentMessage::Init(Box::new(Init {
            project_root: "/p".into(),
            model_name: "Model".into(),
            model_id: "m-1".into(),
            max_context_tokens: 1000,
            log_path: "/l".into(),
            dangerously_allow_all: false,
            context_strategy: "compact".into(),
            ..Default::default()
        }))
    }

    fn session() -> Session {
        let mut s = Session::new();
        s.apply(init_msg());
        s
    }

    fn tool_request(name: &str, id: &str, needs_approval: bool) -> AgentMessage {
        AgentMessage::ToolRequest {
            tool_name: name.into(),
            tool_args: "{}".into(),
            tool_id: id.into(),
            kind: "read".into(),
            subagent_id: None,
            needs_approval,
        }
    }

    fn kinds(s: &Session) -> Vec<EntryKind> {
        s.entries().iter().map(|e| e.kind).collect()
    }

    fn last(s: &Session) -> &Entry {
        s.entries().last().expect("an entry")
    }

    // ── Init ──────────────────────────────────────────────────────────────

    #[test]
    fn init_populates_the_session() {
        let s = session();
        assert!(s.connected);
        assert_eq!(s.model_name, "Model");
        assert_eq!(s.max_context_tokens, 1000);
        assert_eq!(s.permission_mode, PermissionMode::Ask);
    }

    /// The agent's own flag must set the mode, or the client would prompt for
    /// approvals the agent has already decided to skip.
    #[test]
    fn dangerously_allow_all_from_init_sets_the_mode() {
        let mut s = Session::new();
        s.apply(AgentMessage::Init(Box::new(Init {
            dangerously_allow_all: true,
            ..Default::default()
        })));
        assert_eq!(s.permission_mode, PermissionMode::AllowAll);
    }

    // ── Streaming ─────────────────────────────────────────────────────────

    #[test]
    fn assistant_tokens_accumulate_into_one_entry() {
        let mut s = session();
        for tok in ["Hel", "lo ", "world"] {
            s.apply(AgentMessage::AssistantToken { content: tok.into() });
        }
        assert_eq!(last(&s).content, "Hello world");
        assert_eq!(
            kinds(&s).iter().filter(|k| **k == EntryKind::Assistant).count(),
            1,
            "one entry, not one per token",
        );
    }

    /// `assistant_done` is authoritative — a dropped token must not leave the
    /// transcript permanently wrong.
    #[test]
    fn assistant_done_replaces_the_accumulated_text() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "parti".into() });
        s.apply(AgentMessage::AssistantDone { content: "partial then complete".into() });
        assert_eq!(last(&s).content, "partial then complete");
    }

    /// An empty `assistant_done` must not blank out what was streamed.
    #[test]
    fn an_empty_assistant_done_keeps_the_streamed_text() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "kept".into() });
        s.apply(AgentMessage::AssistantDone { content: String::new() });
        assert_eq!(last(&s).content, "kept");
    }

    #[test]
    fn a_second_turn_starts_a_new_entry() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "first".into() });
        s.apply(AgentMessage::Done);
        s.apply(AgentMessage::AssistantToken { content: "second".into() });
        let assistants: Vec<_> = s.entries().iter()
            .filter(|e| e.kind == EntryKind::Assistant)
            .map(|e| e.content.clone())
            .collect();
        assert_eq!(assistants, vec!["first", "second"]);
    }

    // ── Reasoning ─────────────────────────────────────────────────────────

    #[test]
    fn reasoning_collapses_to_a_thought_with_a_duration() {
        let mut s = session();
        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::ReasoningToken { content: "considering".into() });
        s.apply(AgentMessage::AssistantToken { content: "answer".into() });

        let thought = s.entries().iter().find(|e| e.kind == EntryKind::Thought)
            .expect("reasoning became a thought");
        assert_eq!(thought.content, "considering");
        assert!(thought.duration.is_some(), "duration recorded");
    }

    /// An empty reasoning block is noise and must not leave "thought for 0s".
    #[test]
    fn an_empty_reasoning_block_is_dropped() {
        let mut s = session();
        let before = s.entries().len();
        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::AssistantToken { content: "answer".into() });
        assert!(
            !kinds(&s).contains(&EntryKind::Thought),
            "no empty thought retained",
        );
        assert_eq!(s.entries().len(), before + 1, "just the assistant entry");
    }

    /// Dropping an entry must not corrupt the streaming cursor, or subsequent
    /// tokens land in the wrong entry.
    #[test]
    fn dropping_an_empty_thought_keeps_the_streaming_cursor_valid() {
        let mut s = session();
        s.apply(AgentMessage::Reasoning);          // empty, will be dropped
        s.apply(AgentMessage::AssistantToken { content: "one ".into() });
        s.apply(AgentMessage::AssistantToken { content: "two".into() });
        assert_eq!(last(&s).content, "one two", "tokens stayed in one entry");
    }

    #[test]
    fn reasoning_tokens_without_an_opening_event_still_land_somewhere() {
        let mut s = session();
        s.apply(AgentMessage::ReasoningToken { content: "orphan".into() });
        assert_eq!(last(&s).content, "orphan");
        assert_eq!(last(&s).kind, EntryKind::Reasoning);
    }

    #[test]
    fn a_repeated_reasoning_event_does_not_open_a_second_block() {
        let mut s = session();
        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::ReasoningToken { content: "a".into() });
        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::ReasoningToken { content: "b".into() });
        s.apply(AgentMessage::Done);
        let thoughts: Vec<_> = s.entries().iter()
            .filter(|e| e.kind == EntryKind::Thought).collect();
        assert_eq!(thoughts.len(), 1, "one block");
        assert_eq!(thoughts[0].content, "ab");
    }

    // ── The approval gate ─────────────────────────────────────────────────

    /// The security-relevant path: a tool needing approval must block and must
    /// not be auto-approved.
    #[test]
    fn a_tool_needing_approval_blocks_and_sends_nothing() {
        let mut s = session();
        let effects = s.apply(tool_request("shell_exec", "t1", true));
        assert!(effects.is_empty(), "nothing sent without the user's say-so");
        assert!(matches!(s.pending, Some(Pending::Approval { .. })));
    }

    #[test]
    fn a_tool_not_needing_approval_does_not_block() {
        let mut s = session();
        let effects = s.apply(tool_request("read_file", "t1", false));
        assert!(effects.is_empty());
        assert!(s.pending.is_none(), "no dialog for an unguarded tool");
    }

    #[test]
    fn allow_all_auto_approves_with_the_right_tool_id() {
        let mut s = session();
        s.permission_mode = PermissionMode::AllowAll;
        let effects = s.apply(tool_request("shell_exec", "t-42", true));
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t-42".into() })],
        );
        assert!(s.pending.is_none());
    }

    #[test]
    fn approving_sends_the_matching_tool_id() {
        let mut s = session();
        s.apply(tool_request("shell_exec", "t-7", true));
        let effects = s.approve(false);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t-7".into() })],
        );
        assert!(s.pending.is_none());
    }

    /// "Approve and remember" must apply to later calls of the same tool, and
    /// only that tool.
    #[test]
    fn remembering_an_approval_covers_later_calls_of_that_tool_only() {
        let mut s = session();
        s.apply(tool_request("shell_exec", "t1", true));
        s.approve(true);

        let effects = s.apply(tool_request("shell_exec", "t2", true));
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t2".into() })],
            "same tool is now pre-approved",
        );

        let effects = s.apply(tool_request("write_file", "t3", true));
        assert!(effects.is_empty(), "a different tool still asks");
        assert!(matches!(s.pending, Some(Pending::Approval { .. })));
    }

    #[test]
    fn not_remembering_means_asking_again() {
        let mut s = session();
        s.apply(tool_request("shell_exec", "t1", true));
        s.approve(false);
        s.apply(tool_request("shell_exec", "t2", true));
        assert!(matches!(s.pending, Some(Pending::Approval { .. })), "asks again");
    }

    #[test]
    fn denying_sends_the_reason() {
        let mut s = session();
        s.apply(tool_request("shell_exec", "t1", true));
        let effects = s.deny("not safe");
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::DenyAction {
                tool_id: "t1".into(),
                reason: "not safe".into(),
            })],
        );
    }

    /// Approving when the agent is waiting on something else must not fabricate
    /// an approval for a tool id that was never requested.
    #[test]
    fn approving_the_wrong_kind_of_pending_does_nothing() {
        let mut s = session();
        s.apply(AgentMessage::QuestionRequest {
            question: "which?".into(),
            tool_id: "q1".into(),
            items: Vec::new(),
        });
        let effects = s.approve(false);
        assert!(effects.is_empty(), "no approval invented");
        assert!(matches!(s.pending, Some(Pending::Question { .. })), "question kept");
    }

    // ── Turn teardown ─────────────────────────────────────────────────────

    #[test]
    fn done_ends_the_turn_and_reports_completion() {
        let mut s = session();
        s.apply(AgentMessage::Thinking);
        let effects = s.apply(AgentMessage::Done);
        assert_eq!(s.activity, Activity::Idle);
        assert!(effects.contains(&Effect::TurnComplete));
    }

    /// A dialog left on screen after the turn ends would be unanswerable.
    #[test]
    fn ending_a_turn_clears_a_stale_pending_dialog() {
        let mut s = session();
        s.apply(tool_request("shell_exec", "t1", true));
        assert!(s.pending.is_some());
        s.apply(AgentMessage::Done);
        assert!(s.pending.is_none(), "no orphaned dialog");
    }

    #[test]
    fn cancelling_ends_the_turn_and_notes_it() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "partial".into() });
        s.apply(AgentMessage::Cancelled);
        assert_eq!(s.activity, Activity::Idle);
        assert_eq!(last(&s).content, "cancelled");
    }

    /// A rewind mid-turn discards what the turn produced.
    #[test]
    fn a_discarded_turn_drops_its_open_entries() {
        let mut s = session();
        let before = s.entries().len();
        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::ReasoningToken { content: "thinking".into() });
        s.apply(AgentMessage::AssistantToken { content: "half an answer".into() });
        s.apply(AgentMessage::TurnDiscarded);
        assert_eq!(s.entries().len(), before, "the turn left nothing behind");
        assert_eq!(s.activity, Activity::Idle);
    }

    /// A discard must remove the *whole* turn, including parts that already
    /// closed. Dropping only what was still open left a stale thought and tool
    /// call in the transcript for a turn that never happened.
    #[test]
    fn a_discarded_turn_also_removes_its_completed_parts() {
        let mut s = session();
        s.push_user("do something");
        let before = s.entries().len();

        s.apply(AgentMessage::Reasoning);
        s.apply(AgentMessage::ReasoningToken { content: "planning".into() });
        s.apply(tool_request("read_file", "t1", false));   // closes the reasoning
        s.apply(AgentMessage::ToolResult {
            tool_name: "read_file".into(), result: "contents".into(),
            success: true, subagent_id: None,
        });
        s.apply(AgentMessage::AssistantToken { content: "here is".into() });
        assert!(s.entries().len() > before, "the turn produced output");

        s.apply(AgentMessage::TurnDiscarded);
        assert_eq!(
            s.entries().len(), before,
            "everything the turn produced is gone, including the thought, \
             the tool call and its result",
        );
        // The user's own message must survive — they did send it.
        assert_eq!(last(&s).kind, EntryKind::User);
    }

    /// A second turn after a discard must be discardable too, or the marker
    /// would still point at the first turn.
    #[test]
    fn the_turn_marker_resets_after_a_discard() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "first".into() });
        s.apply(AgentMessage::TurnDiscarded);
        let after_first = s.entries().len();

        s.apply(AgentMessage::AssistantToken { content: "second".into() });
        s.apply(AgentMessage::TurnDiscarded);
        assert_eq!(s.entries().len(), after_first, "the second turn also went");
    }

    /// Bookkeeping the agent can send at any moment must not be mistaken for
    /// the start of a turn, or a later discard would rewind past real output.
    #[test]
    fn bookkeeping_messages_do_not_open_a_turn() {
        let mut s = session();
        s.apply(AgentMessage::UsageUpdate { snapshot: UsageSnapshot::default() });
        s.apply(AgentMessage::RewindCheckpoint {
            id: "c1".into(), preview: "p".into(),
            message_count: 1, keep_on_restore: false,
        });
        s.push_user("a question");
        let before = s.entries().len();

        s.apply(AgentMessage::AssistantToken { content: "an answer".into() });
        s.apply(AgentMessage::TurnDiscarded);
        assert_eq!(
            s.entries().len(), before,
            "the discard rewound to the turn, not to the earlier bookkeeping",
        );
    }

    /// A completed turn cannot be retroactively discarded by a later stray
    /// message — the marker is cleared when the turn ends.
    #[test]
    fn a_discard_after_a_completed_turn_removes_nothing() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "kept".into() });
        s.apply(AgentMessage::Done);
        let after_done = s.entries().len();

        s.apply(AgentMessage::TurnDiscarded);
        assert_eq!(
            s.entries().len(), after_done,
            "a finished turn stays finished",
        );
    }

    #[test]
    fn an_error_stops_the_turn_and_records_it() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "partial".into() });
        s.apply(AgentMessage::Error { message: "boom".into() });
        assert_eq!(s.activity, Activity::Idle);
        assert_eq!(last(&s).kind, EntryKind::Error);
        assert_eq!(last(&s).content, "boom");
    }

    #[test]
    fn a_retry_is_reported_without_ending_the_turn() {
        let mut s = session();
        s.apply(AgentMessage::ApiRetry {
            attempt: 2, max_attempts: 5, delay_secs: 3,
            error: "overloaded\nsecond line".into(),
        });
        assert_eq!(s.activity, Activity::Retrying { attempt: 2, max_attempts: 5 });
        assert!(last(&s).content.contains("2/5"));
        assert!(!last(&s).content.contains("second line"), "summarised to one line");
    }

    // ── Tool output ───────────────────────────────────────────────────────

    /// A long build log arrives in many chunks; one entry per chunk would
    /// swamp the transcript.
    #[test]
    fn tool_output_chunks_coalesce_into_one_entry() {
        let mut s = session();
        for chunk in ["line one\n", "line two\n", "line three\n"] {
            s.apply(AgentMessage::ToolOutput {
                tool_name: "shell_exec".into(),
                content: chunk.into(),
            });
        }
        let outputs: Vec<_> = s.entries().iter()
            .filter(|e| e.kind == EntryKind::ToolOutput).collect();
        assert_eq!(outputs.len(), 1, "coalesced");
        assert_eq!(outputs[0].content, "line one\nline two\nline three\n");
    }

    #[test]
    fn output_from_a_different_tool_starts_a_new_entry() {
        let mut s = session();
        s.apply(AgentMessage::ToolOutput { tool_name: "a".into(), content: "x".into() });
        s.apply(AgentMessage::ToolOutput { tool_name: "b".into(), content: "y".into() });
        let outputs: Vec<_> = s.entries().iter()
            .filter(|e| e.kind == EntryKind::ToolOutput).collect();
        assert_eq!(outputs.len(), 2);
    }

    #[test]
    fn a_tool_call_is_summarised_with_its_interesting_argument() {
        let mut s = session();
        s.apply(AgentMessage::ToolRequest {
            tool_name: "read_file".into(),
            tool_args: r#"{"path":"src/main.rs"}"#.into(),
            tool_id: "t1".into(),
            kind: "read".into(),
            subagent_id: None,
            needs_approval: false,
        });
        assert_eq!(last(&s).content, "read_file(src/main.rs)");
    }

    /// Malformed arguments must not panic or lose the tool name.
    #[test]
    fn a_tool_call_with_unparseable_arguments_still_shows_its_name() {
        let mut s = session();
        s.apply(AgentMessage::ToolRequest {
            tool_name: "mystery".into(),
            tool_args: "not json".into(),
            tool_id: "t1".into(),
            kind: "read".into(),
            subagent_id: None,
            needs_approval: false,
        });
        assert_eq!(last(&s).content, "mystery");
    }

    // ── Sessions ──────────────────────────────────────────────────────────

    #[test]
    fn clearing_a_session_empties_the_transcript_and_state() {
        let mut s = session();
        s.apply(AgentMessage::AssistantToken { content: "history".into() });
        s.apply(tool_request("shell_exec", "t1", true));
        s.apply(AgentMessage::SessionCleared {
            session_id: "new".into(),
            log_path: "/new".into(),
        });
        assert_eq!(s.session_id.as_deref(), Some("new"));
        assert!(s.pending.is_none());
        assert!(s.checkpoints.is_empty());
        assert_eq!(kinds(&s), vec![EntryKind::System], "just the notice");
    }

    #[test]
    fn a_loaded_session_replays_its_history() {
        let mut s = session();
        s.apply(AgentMessage::SessionLoaded {
            session_id: "s".into(),
            title: "Earlier work".into(),
            message_count: 2,
            compaction_count: 0,
            entries: vec![
                ReplayEntry { kind: "user".into(), content: "hi".into(), tool_name: None, success: None },
                ReplayEntry { kind: "assistant".into(), content: "hello".into(), tool_name: None, success: None },
            ],
            rewind_checkpoints: Vec::new(),
        });
        let replayed: Vec<_> = s.entries().iter()
            .filter(|e| matches!(e.kind, EntryKind::User | EntryKind::Assistant))
            .map(|e| e.content.clone())
            .collect();
        assert_eq!(replayed, vec!["hi", "hello"]);
    }

    #[test]
    fn an_unknown_replay_kind_becomes_a_system_entry() {
        let entry = replay_to_entry(ReplayEntry {
            kind: "something_new".into(),
            content: "text".into(),
            tool_name: None,
            success: None,
        });
        assert_eq!(entry.kind, EntryKind::System);
    }

    // ── Checkpoints ───────────────────────────────────────────────────────

    /// The event carries no display index, so it has to be assigned here.
    #[test]
    fn checkpoints_are_numbered_in_arrival_order() {
        let mut s = session();
        for id in ["c1", "c2", "c3"] {
            s.apply(AgentMessage::RewindCheckpoint {
                id: id.into(),
                preview: "p".into(),
                message_count: 1,
                keep_on_restore: false,
            });
        }
        let indices: Vec<_> = s.checkpoints.iter().map(|c| c.display_index).collect();
        assert_eq!(indices, vec![1, 2, 3]);
    }

    // ── Plan mode ─────────────────────────────────────────────────────────

    #[test]
    fn entering_and_leaving_plan_mode_tracks_the_permission_mode() {
        let mut s = session();
        s.apply(AgentMessage::PlanModeEntered { plan_path: "/plan.md".into() });
        assert!(s.plan_mode);
        assert_eq!(s.permission_mode, PermissionMode::Plan);
        s.apply(AgentMessage::PlanModeExited { reason: "approved".into() });
        assert!(!s.plan_mode);
        assert_eq!(s.permission_mode, PermissionMode::Ask);
    }

    #[test]
    fn approving_a_plan_can_clear_the_context() {
        let mut s = session();
        s.apply(AgentMessage::PlanReady { plan_path: "/p".into(), content: "steps".into() });
        assert_eq!(
            s.approve_plan(true),
            vec![Effect::Send(ClientMessage::ClearAndApprovePlan)],
        );

        s.apply(AgentMessage::PlanReady { plan_path: "/p".into(), content: "steps".into() });
        assert_eq!(s.approve_plan(false), vec![Effect::Send(ClientMessage::ApprovePlan)]);
    }

    #[test]
    fn replying_to_a_plan_rejects_it_with_feedback() {
        let mut s = session();
        s.apply(AgentMessage::PlanReady { plan_path: "/p".into(), content: "steps".into() });
        assert_eq!(
            s.reply("needs more detail"),
            vec![Effect::Send(ClientMessage::RejectPlan {
                feedback: "needs more detail".into(),
            })],
        );
    }

    // ── Replies routed by what is pending ─────────────────────────────────

    /// One `reply` entry point, routed by what the agent asked. Sending the
    /// wrong message type here would hang the agent.
    #[test]
    fn a_reply_is_routed_to_whatever_is_pending() {
        let mut s = session();

        s.apply(AgentMessage::QuestionRequest {
            question: "q".into(), tool_id: "t".into(), items: Vec::new(),
        });
        assert_eq!(
            s.reply("answer"),
            vec![Effect::Send(ClientMessage::AnswerQuestion { answer: "answer".into() })],
        );

        s.apply(AgentMessage::ProcessInputNeeded { prompt: "password:".into() });
        assert_eq!(
            s.reply("secret"),
            vec![Effect::Send(ClientMessage::ProcessInput { content: "secret".into() })],
        );

        s.apply(AgentMessage::BackgroundPromptNeeded {
            bg_id: "b1".into(), command: "cmd".into(), prompt: "?".into(),
        });
        assert_eq!(
            s.reply("yes"),
            vec![Effect::Send(ClientMessage::BgProcessInput {
                bg_id: "b1".into(), content: "yes".into(),
            })],
        );
    }

    #[test]
    fn a_reply_with_nothing_pending_does_nothing() {
        let mut s = session();
        assert!(s.reply("into the void").is_empty());
    }

    // ── Subagents ─────────────────────────────────────────────────────────

    #[test]
    fn subagents_are_tracked_then_cleared_when_they_finish() {
        let mut s = session();
        s.apply(AgentMessage::SubagentStarted {
            id: "s1".into(), agent_type: "Explore".into(),
            prompt: "look around\nmore".into(), parent_id: None,
        });
        assert_eq!(s.subagents.len(), 1);
        assert!(last(&s).content.contains("look around"));
        assert!(!last(&s).content.contains("more"), "one-line summary");

        s.apply(AgentMessage::SubagentStatus {
            id: "s1".into(), tool_name: "read".into(), detail: "reading main.rs".into(),
        });
        assert_eq!(s.subagents[0].detail, "reading main.rs");

        s.apply(AgentMessage::SubagentFinished {
            id: "s1".into(), agent_type: "Explore".into(), summary: "found it".into(),
        });
        assert!(s.subagents.is_empty());
    }

    /// A turn ending with subagents still listed would show ghosts in the
    /// status area.
    #[test]
    fn ending_a_turn_clears_lingering_subagents() {
        let mut s = session();
        s.apply(AgentMessage::SubagentStarted {
            id: "s1".into(), agent_type: "Explore".into(),
            prompt: "p".into(), parent_id: None,
        });
        s.apply(AgentMessage::Done);
        assert!(s.subagents.is_empty());
    }

    // ── Usage ─────────────────────────────────────────────────────────────

    #[test]
    fn usage_updates_drive_the_context_fraction() {
        let mut s = session();
        assert_eq!(s.context_fraction(), 0.0, "no usage yet");
        s.apply(AgentMessage::UsageUpdate {
            snapshot: UsageSnapshot {
                last_prompt_tokens: 500,
                last_completion_tokens: 0,
                max_context_tokens: 1000,
                ..Default::default()
            },
        });
        assert!((s.context_fraction() - 0.5).abs() < f32::EPSILON);
    }

    // ── Model and login ───────────────────────────────────────────────────

    #[test]
    fn switching_model_updates_the_header_and_notes_it() {
        let mut s = session();
        s.apply(AgentMessage::ModelSwitched {
            name: "Other".into(), model_id: "o-1".into(), max_context_tokens: 2000,
        });
        assert_eq!(s.model_name, "Other");
        assert_eq!(s.max_context_tokens, 2000);
        assert!(last(&s).content.contains("Other"));
    }

    #[test]
    fn login_success_and_failure_are_distinguished() {
        let mut s = session();
        s.apply(AgentMessage::LoginStatus { message: "open this url".into() });
        assert!(s.login_in_progress);

        s.apply(AgentMessage::LoginComplete { success: true, message: "done".into() });
        assert!(!s.login_in_progress);
        assert!(s.chatgpt_logged_in);
        assert_eq!(last(&s).kind, EntryKind::System);

        s.apply(AgentMessage::LoginComplete { success: false, message: "failed".into() });
        assert_eq!(last(&s).kind, EntryKind::Error, "a failure reads as an error");
    }

    // ── Permission toggle ─────────────────────────────────────────────────

    #[test]
    fn toggling_permission_mode_round_trips_and_tells_the_agent() {
        let mut s = session();
        assert_eq!(s.permission_mode, PermissionMode::Ask);
        let effects = s.toggle_permission_mode();
        assert_eq!(s.permission_mode, PermissionMode::AllowAll);
        assert_eq!(effects, vec![Effect::Send(ClientMessage::ToggleAutoMode)]);
        s.toggle_permission_mode();
        assert_eq!(s.permission_mode, PermissionMode::Ask);
    }

    // ── Activity labels ───────────────────────────────────────────────────

    #[test]
    fn activity_labels_describe_what_is_happening() {
        assert_eq!(Activity::Idle.label(), None);
        assert!(!Activity::Idle.is_busy());
        assert_eq!(Activity::Thinking.label().unwrap(), "thinking");
        assert_eq!(
            Activity::RunningTool("shell_exec".into()).label().unwrap(),
            "running shell_exec",
        );
        assert!(Activity::Streaming.is_busy());
    }
}
