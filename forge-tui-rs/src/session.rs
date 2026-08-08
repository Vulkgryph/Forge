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
    /// Approve edits without asking, but still ask before running anything.
    ///
    /// Distinct from `AllowAll`: the TypeScript client auto-approved calls whose
    /// kind is `write` and nothing else, so a command still stops for permission.
    /// This label used to sit on `AllowAll`, which claimed "auto-accept edits"
    /// while in fact approving commands too.
    AutoAccept,
    /// Approve everything without asking. Set by the agent's
    /// `--dangerously-allow-all`, or toggled at runtime.
    AllowAll,
    /// Read-only planning: anything that is not a read is refused here rather
    /// than being sent on, which is what the TypeScript client did.
    Plan,
}

impl PermissionMode {
    /// The next mode Shift-Tab moves to.
    ///
    /// Ask, auto-accept, plan — the three the TypeScript client cycled.
    /// `AllowAll` is deliberately not in the ring: it comes from
    /// `--dangerously-allow-all` or an explicit choice in the menu, and must not
    /// be something a stray keypress can turn on.
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AutoAccept,
            Self::AutoAccept => Self::Plan,
            Self::Plan | Self::AllowAll => Self::Ask,
        }
    }

    /// What the transcript says when the mode changes, as the TypeScript client
    /// worded it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Normal mode",
            Self::AutoAccept => "Auto-accept edits",
            Self::AllowAll => "Approving everything",
            Self::Plan => "Plan mode (read-only)",
        }
    }
}

/// Round a token count for reading rather than accounting: the difference between
/// 5,132 and 5,140 never matters at a glance, and the narrow line this sits on
/// does.
fn compact_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let thousands = n as f64 / 1000.0;
    if thousands < 10.0 {
        format!("{thousands:.1}k")
    } else {
        format!("{}k", thousands.round() as u64)
    }
}

/// What kind of line this is in the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// The footnote closing a finished turn: when it ended, how long it took, and
    /// what it cost.
    TurnSummary,
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
    /// The provider turned the request away because it is at capacity, and the
    /// endpoint in use has a priority tier that is not switched on.
    ///
    /// Its own pending state rather than an error line, because there is
    /// something the user can do about it — which was the whole reason the agent
    /// tags this rejection distinctly instead of reporting a generic failure.
    ProviderBusy {
        message:       String,
        endpoint_name: String,
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
    /// Restart the agent process, optionally resuming a saved session.
    ///
    /// Resuming is a restart because the agent has no runtime path for it:
    /// `ResumeSession` is accepted and ignored, and the only working route is the
    /// `--resume-session` flag at startup.
    Restart { resume: Option<String> },
    /// Leave.
    Quit,
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
    /// When the current turn began, and the token counters as they stood then.
    ///
    /// Kept so a finished turn can say when it ended and what it cost. The
    /// counters are cumulative, so a turn's own usage is the difference.
    turn_began_at: Option<std::time::Instant>,
    turn_tokens_at_start: (u64, u64),

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

/// How the agent marks a provider-capacity rejection, as opposed to any other
/// failure. Matching on a message prefix is not lovely, but it is the contract
/// the agent offers and the TypeScript client used the same one.
const PROVIDER_AT_CAPACITY: &str = "Provider at capacity:";

/// Why a tool call was refused in plan mode, as the model will read it.
///
/// The agent turns a denial into a tool result of `DENIED: <reason>` and hands
/// that to the model, so this string is the *only* thing telling it what
/// happened. Terse wording — the TypeScript client sent "Blocked by plan mode" —
/// leaves it to guess whether to retry, work around the refusal, or stop, and a
/// model that guesses "retry" will be refused again in a loop.
///
/// So it says the rule, that retrying will not help, and what the alternatives
/// are: draft the plan, or ask. Someone who has toggled plan mode by accident
/// gets told what happened by an agent that can explain it, rather than watching
/// it fail at the same edit repeatedly.
const PLAN_MODE_DENIAL: &str = "Plan mode is on, so only read-only tools are permitted. \
Retrying this call will be refused again. Either finish drafting the plan and present it \
for approval, or — if this change really is needed now — stop and ask the user to leave \
plan mode (Shift+Tab cycles permission modes) or to confirm how they want to proceed.";

/// The tools an approved plan is allowed to use without asking again.
///
/// The edit tools only. Approving a plan is not a blanket approval: running
/// commands still asks, since a plan describing an edit is not consent to execute
/// anything.
const PLAN_EDIT_TOOLS: &[&str] = &["apply_patch", "write_file", "edit_file"];

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
            turn_began_at: None,
            turn_tokens_at_start: (0, 0),
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

    /// The entry currently being appended to, if any.
    ///
    /// Committing this one would print half a message and then redraw the rest
    /// below it, so it is the one thing that must stay live.
    pub fn streaming_entry(&self) -> Option<usize> {
        self.streaming
    }

    /// Where the current turn's output begins, if a turn is in progress.
    ///
    /// The inline renderer needs it to decide what is settled: everything before
    /// this can be printed permanently, because only the current turn can still
    /// be rewritten or discarded.
    pub fn turn_start(&self) -> Option<usize> {
        self.turn_start
    }

    /// Record what the user sent, so it appears before the reply.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::new(EntryKind::User, text));
    }

    /// The text of the agent's most recent message — what "copy the last
    /// message" means. Reasoning, tool output and system notes are skipped:
    /// they are the machinery around the answer, not the answer.
    ///
    /// A message still being streamed counts. Waiting for it to finish would
    /// mean the command does nothing during the very turn the user is reading.
    pub fn last_agent_text(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| matches!(e.kind, EntryKind::Assistant) && !e.content.trim().is_empty())
            .map(|e| e.content.as_str())
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
            self.turn_began_at = Some(std::time::Instant::now());
            self.turn_tokens_at_start = self
                .usage
                .as_ref()
                .map(|u| (u.total_prompt_tokens, u.total_completion_tokens))
                .unwrap_or_default();
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
                self.push_turn_summary();
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
                self.activity = Activity::Idle;
                self.turn_start = None;

                // The agent tags a capacity rejection with this prefix precisely so
                // a client can offer the priority tier rather than printing red text
                // with no way forward. Only xAI has one to offer, and only when it
                // is not already on — anything else is an ordinary error.
                let busy = message.starts_with(PROVIDER_AT_CAPACITY);
                let endpoint = busy
                    .then(|| {
                        crate::model_display::active_endpoint(
                            &self.endpoints,
                            &self.model_name,
                            &self.model_id,
                            self.max_context_tokens,
                        )
                    })
                    .flatten();
                match endpoint {
                    Some(ep) if crate::model_display::is_xai(ep) && !ep.xai_priority_tier => {
                        let endpoint_name = ep.name.clone();
                        self.pending = Some(Pending::ProviderBusy { message, endpoint_name });
                    }
                    _ => {
                        self.entries.push(Entry::new(EntryKind::Error, message));
                        self.pending = None;
                    }
                }
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
                // Plan mode is read-only: refuse anything else here rather than
                // asking, which is how the TypeScript client enforced it.
                if self.permission_mode == PermissionMode::Plan && kind != "read" {
                    self.push_system(format!("Blocked by plan mode: {tool_name}"));
                    return vec![Effect::Send(ClientMessage::DenyAction {
                        tool_id,
                        reason: PLAN_MODE_DENIAL.into(),
                    })];
                }

                let pre_approved = self.permission_mode == PermissionMode::AllowAll
                    || (self.permission_mode == PermissionMode::AutoAccept && kind == "write")
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
            Some(Pending::Plan { .. }) => {
                // Approving a plan approves the edits it describes. Without this the
                // agent stopped for permission on every single write while working
                // through a plan the user had just accepted — which is not what
                // "approved" means, and is not what the TypeScript client did: it
                // added these three to its approved set on both approve paths.
                for tool in PLAN_EDIT_TOOLS {
                    self.approved_tools.insert((*tool).to_string());
                }
                vec![Effect::Send(if clear_context {
                    ClientMessage::ClearAndApprovePlan
                } else {
                    ClientMessage::ApprovePlan
                })]
            }
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    /// Turn the priority tier on for the endpoint that was turned away, and note
    /// it locally so the offer is not made again for the same endpoint.
    pub fn switch_to_priority_tier(&mut self) -> Vec<Effect> {
        match self.pending.take() {
            Some(Pending::ProviderBusy { endpoint_name, .. }) => {
                for ep in self.endpoints.iter_mut() {
                    if ep.name == endpoint_name {
                        ep.xai_priority_tier = true;
                    }
                }
                self.push_system(format!("Priority tier on for {endpoint_name}"));
                vec![Effect::Send(ClientMessage::UpdateXaiPriorityTier {
                    endpoint_name,
                    enabled: true,
                })]
            }
            other => {
                self.pending = other;
                Vec::new()
            }
        }
    }

    /// Leave the tier alone and keep the message as an ordinary error, so the
    /// transcript still records what happened.
    pub fn dismiss_provider_busy(&mut self) {
        if let Some(Pending::ProviderBusy { message, .. }) = self.pending.take() {
            self.entries.push(Entry::new(EntryKind::Error, message));
        }
    }

    /// Step to the next permission mode, saying so in the transcript.
    pub fn cycle_permission_mode(&mut self) {
        self.set_permission_mode(self.permission_mode.next());
    }

    /// Change the permission mode, saying so in the transcript.
    ///
    /// Takes effect at once and locally: the mode is this client's approval gate,
    /// checked as each tool request arrives, so the next request is judged by the
    /// new mode with no round trip to the agent.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        if self.permission_mode == mode {
            return;
        }
        self.permission_mode = mode;
        self.push_system(mode.label().to_string());
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

    /// Close a finished turn with when it ended, how long it took, and what it
    /// cost.
    ///
    /// The completion time is the point of it: a turn that ran while you were
    /// elsewhere leaves no other trace of when it actually finished, and
    /// scrollback has no timestamps. Local time, because the question being
    /// answered is "when was I away".
    fn push_turn_summary(&mut self) {
        let Some(began) = self.turn_began_at.take() else { return };
        let mut parts: Vec<String> = Vec::new();

        // A clock the platform declines to read is left out rather than guessed.
        if let Some(at) = crate::sys::local_time("%H:%M:%S") {
            parts.push(at);
        }
        parts.push(crate::app::format_duration(began.elapsed()));

        if let Some(usage) = self.usage.as_ref() {
            let (p0, c0) = self.turn_tokens_at_start;
            let sent = usage.total_prompt_tokens.saturating_sub(p0);
            let back = usage.total_completion_tokens.saturating_sub(c0);
            // Only when this turn actually accounted for something: a turn that
            // did no work should not claim "0 in".
            if sent > 0 || back > 0 {
                parts.push(format!("{} in", compact_count(sent)));
                parts.push(format!("{} out", compact_count(back)));
            }
        }

        if parts.is_empty() {
            return;
        }
        self.entries.push(Entry::new(EntryKind::TurnSummary, parts.join(" · ")));
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
    fn the_last_agent_message_is_the_newest_assistant_entry() {
        let mut s = Session::new();
        s.apply(AgentMessage::AssistantDone { content: "first answer".into() });
        s.push_user("and then?");
        s.apply(AgentMessage::AssistantDone { content: "second answer".into() });
        assert_eq!(s.last_agent_text(), Some("second answer"));
    }

    #[test]
    fn the_last_agent_message_ignores_everything_that_is_not_the_answer() {
        // What follows an answer is usually a system note — including the one
        // this very command pushes to say it copied. Copying that instead
        // would make a second press copy the receipt for the first.
        let mut s = Session::new();
        s.apply(AgentMessage::AssistantDone { content: "the answer".into() });
        s.push_system("Copied the last message (1 line) using pbcopy.");
        s.push_error("something went wrong");
        assert_eq!(s.last_agent_text(), Some("the answer"));
    }

    #[test]
    fn there_is_nothing_to_copy_before_the_agent_answers() {
        let mut s = Session::new();
        assert_eq!(s.last_agent_text(), None);
        s.push_user("hello");
        assert_eq!(s.last_agent_text(), None, "the user's own message is not the agent's");
    }

    #[test]
    fn a_message_still_streaming_can_be_copied() {
        let mut s = Session::new();
        s.apply(AgentMessage::AssistantToken { content: "half a th".into() });
        assert_eq!(s.last_agent_text(), Some("half a th"));
    }

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
    /// Approving a plan approves the edits it describes. Without this the agent
    /// stopped for permission on every write while working through a plan the user
    /// had just accepted.
    #[test]
    fn approving_a_plan_approves_the_edit_tools() {
        let mut session = Session::new();
        session.apply(AgentMessage::PlanReady {
            plan_path: "/tmp/plan.md".into(),
            content: "do the thing".into(),
        });
        let effects = session.approve_plan(false);
        assert!(matches!(effects.as_slice(), [Effect::Send(ClientMessage::ApprovePlan)]));

        for tool in ["apply_patch", "write_file", "edit_file"] {
            assert!(
                session.approved_tools.contains(tool),
                "{tool} still needs approval after the plan was approved",
            );
        }
    }

    /// It is not a blanket approval: a plan describing an edit is not consent to
    /// run commands.
    #[test]
    fn approving_a_plan_does_not_approve_running_commands() {
        let mut session = Session::new();
        session.apply(AgentMessage::PlanReady {
            plan_path: "/tmp/plan.md".into(),
            content: "do the thing".into(),
        });
        session.approve_plan(true);
        for tool in ["execute_command", "run_command", "bash"] {
            assert!(
                !session.approved_tools.contains(tool),
                "{tool} was approved by a plan, which it must not be",
            );
        }
    }

    /// Shift-Tab walks the three modes the TypeScript client cycled, and says which
    /// one it landed on.
    #[test]
    fn cycling_steps_ask_autoaccept_plan_and_back() {
        let mut session = Session::new();
        assert_eq!(session.permission_mode, PermissionMode::Ask);
        for want in [PermissionMode::AutoAccept, PermissionMode::Plan, PermissionMode::Ask] {
            session.cycle_permission_mode();
            assert_eq!(session.permission_mode, want);
            let last = session.entries().last().expect("a line saying so");
            assert_eq!(last.content, want.label());
        }
    }

    /// Approve-everything is not in the ring: a stray keypress must not be able to
    /// turn off every prompt. Cycling out of it lands on asking.
    #[test]
    fn cycling_never_reaches_approve_everything() {
        let mut mode = PermissionMode::Ask;
        for _ in 0..12 {
            mode = mode.next();
            assert_ne!(mode, PermissionMode::AllowAll);
        }
        assert_eq!(PermissionMode::AllowAll.next(), PermissionMode::Ask);
    }

    fn write_request(id: &str) -> AgentMessage {
        AgentMessage::ToolRequest {
            tool_name: "write_file".into(),
            tool_args: "{}".into(),
            tool_id: id.into(),
            kind: "write".into(),
            subagent_id: None,
            needs_approval: true,
        }
    }

    fn exec_request(id: &str) -> AgentMessage {
        AgentMessage::ToolRequest {
            tool_name: "shell_exec".into(),
            tool_args: "{}".into(),
            tool_id: id.into(),
            kind: "execute".into(),
            subagent_id: None,
            needs_approval: true,
        }
    }

    /// Auto-accept lets edits through and still asks before running anything —
    /// exactly the distinction the TypeScript client drew, and the reason it is not
    /// the same thing as approving everything.
    #[test]
    fn auto_accept_passes_edits_but_still_asks_to_run() {
        let mut session = Session::new();
        session.permission_mode = PermissionMode::AutoAccept;

        session.apply(write_request("t1"));
        assert!(session.pending.is_none(), "an edit should not stop for approval");

        session.apply(exec_request("t2"));
        assert!(
            matches!(session.pending, Some(Pending::Approval { .. })),
            "running a command still asks",
        );
    }

    /// The reason a plan-mode denial carries has to be usable by the model, since
    /// the agent hands it over as the tool's result. It is the only way an agent
    /// can tell that plan mode — rather than the tool itself — is what stopped it,
    /// and the only route to asking the user about it.
    #[test]
    fn a_plan_mode_denial_tells_the_model_what_to_do() {
        let mut session = Session::new();
        session.permission_mode = PermissionMode::Plan;
        let effects = session.apply(write_request("t1"));
        let reason = match effects.as_slice() {
            [Effect::Send(ClientMessage::DenyAction { reason, .. })] => reason.clone(),
            other => panic!("expected a denial, got {other:?}"),
        };
        assert!(reason.contains("Plan mode"), "names the cause: {reason:?}");
        assert!(reason.contains("read-only"), "states the rule: {reason:?}");
        assert!(
            reason.to_lowercase().contains("retrying"),
            "says retrying will not help, or a model will loop on it: {reason:?}",
        );
        assert!(
            reason.contains("ask the user"),
            "offers asking as the way out: {reason:?}",
        );
    }

    /// Plan mode is read-only: anything else is refused here rather than asked
    /// about, which is how the TypeScript client enforced it.
    #[test]
    fn plan_mode_refuses_anything_that_is_not_a_read() {
        let mut session = Session::new();
        session.permission_mode = PermissionMode::Plan;

        let effects = session.apply(write_request("t1"));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Send(ClientMessage::DenyAction { tool_id, .. })] if tool_id == "t1"
            ),
            "the write should be denied, got {effects:?}",
        );
        assert!(session.pending.is_none(), "and not queued as a question");
        assert!(
            session.entries().iter().any(|e| e.content.contains("Blocked by plan mode")),
            "the transcript says why",
        );

        let reads = session.apply(AgentMessage::ToolRequest {
            tool_name: "read_file".into(),
            tool_args: "{}".into(),
            tool_id: "t2".into(),
            kind: "read".into(),
            subagent_id: None,
            needs_approval: false,
        });
        assert!(
            !matches!(reads.as_slice(), [Effect::Send(ClientMessage::DenyAction { .. })]),
            "a read is allowed through: {reads:?}",
        );
    }

    fn xai_session(priority_on: bool) -> Session {
        let mut session = Session::new();
        session.model_name = "Grok".into();
        session.model_id = "grok-4".into();
        session.max_context_tokens = 200_000;
        session.endpoints = vec![EndpointInfo {
            name: "Grok".into(),
            base_url: "https://api.x.ai/v1".into(),
            model_id: "grok-4".into(),
            max_context_tokens: 200_000,
            max_output_tokens: 8192,
            endpoint_type: "openai".into(),
            reasoning: Default::default(),
            xai_priority_tier: priority_on,
        }];
        session
    }

    const BUSY: &str = "Provider at capacity: xAI is at capacity right now";

    /// A capacity rejection on an xAI endpoint without the tier on is an offer,
    /// not just red text — which is why the agent tags it distinctly.
    #[test]
    fn a_capacity_rejection_offers_the_priority_tier() {
        let mut session = xai_session(false);
        session.apply(AgentMessage::Error { message: BUSY.into() });
        assert!(
            matches!(session.pending, Some(Pending::ProviderBusy { .. })),
            "expected the offer, got {:?}", session.pending,
        );
        // Not also dumped into the transcript as an error; the dialog carries it.
        assert!(!session.entries().iter().any(|e| e.kind == EntryKind::Error));
    }

    /// Taking the offer turns the tier on for that endpoint and remembers it, so
    /// the same offer is not made again.
    #[test]
    fn switching_turns_the_tier_on_for_that_endpoint() {
        let mut session = xai_session(false);
        session.apply(AgentMessage::Error { message: BUSY.into() });
        let effects = session.switch_to_priority_tier();
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Send(ClientMessage::UpdateXaiPriorityTier { endpoint_name, enabled: true })]
                    if endpoint_name == "Grok"
            ),
            "got {effects:?}",
        );
        assert!(session.endpoints[0].xai_priority_tier, "noted locally too");
        assert!(session.pending.is_none());
    }

    /// Dismissing keeps the message: the turn still failed, and the transcript has
    /// to say so.
    #[test]
    fn dismissing_keeps_the_error_in_the_transcript() {
        let mut session = xai_session(false);
        session.apply(AgentMessage::Error { message: BUSY.into() });
        session.dismiss_provider_busy();
        assert!(session.pending.is_none());
        let last = session.entries().last().expect("an error line");
        assert_eq!(last.kind, EntryKind::Error);
        assert!(last.content.contains("at capacity"));
    }

    /// Already on the tier: there is nothing to offer, so it is an ordinary error.
    #[test]
    fn no_offer_when_the_tier_is_already_on() {
        let mut session = xai_session(true);
        session.apply(AgentMessage::Error { message: BUSY.into() });
        assert!(session.pending.is_none(), "nothing to offer");
        assert!(session.entries().iter().any(|e| e.kind == EntryKind::Error));
    }

    /// Another provider has no priority tier to switch to.
    #[test]
    fn no_offer_for_a_provider_that_has_no_such_tier() {
        let mut session = xai_session(false);
        session.endpoints[0].base_url = "https://api.openai.com/v1".into();
        session.apply(AgentMessage::Error { message: BUSY.into() });
        assert!(session.pending.is_none(), "not an xAI endpoint");
        assert!(session.entries().iter().any(|e| e.kind == EntryKind::Error));
    }

    /// And an ordinary failure stays an ordinary failure.
    #[test]
    fn an_unrelated_error_is_not_turned_into_an_offer() {
        let mut session = xai_session(false);
        session.apply(AgentMessage::Error { message: "connection reset".into() });
        assert!(session.pending.is_none());
        assert!(session.entries().iter().any(|e| e.kind == EntryKind::Error));
    }

    fn usage_after(prompt: u64, completion: u64) -> AgentMessage {
        AgentMessage::UsageUpdate {
            snapshot: UsageSnapshot {
                last_prompt_tokens: prompt as u32,
                last_completion_tokens: completion as u32,
                total_prompt_tokens: prompt,
                total_completion_tokens: completion,
                total_requests: 1,
                max_context_tokens: 200_000,
                history_messages: 4,
            },
        }
    }

    /// A finished turn says when it ended. Scrollback carries no timestamps, so a
    /// turn that ran while you were elsewhere otherwise leaves no trace of when it
    /// actually completed — which is the point of the line.
    #[test]
    fn a_finished_turn_records_when_it_ended() {
        let mut s = Session::new();
        s.apply(AgentMessage::Thinking);
        s.apply(AgentMessage::Done);

        let last = s.entries().last().expect("a summary");
        assert_eq!(last.kind, EntryKind::TurnSummary);
        // HH:MM:SS, then the duration.
        let clock = last.content.split(' ').next().unwrap_or("");
        assert_eq!(clock.len(), 8, "expected a wall-clock time, got {:?}", last.content);
        assert_eq!(clock.matches(':').count(), 2, "got {:?}", last.content);
    }

    /// And what the turn cost, as the difference between cumulative counters —
    /// not the totals, which would report the whole session every time.
    #[test]
    fn the_summary_reports_only_this_turns_tokens() {
        let mut s = Session::new();
        // An earlier turn already spent some.
        s.apply(usage_after(1000, 200));
        s.apply(AgentMessage::Thinking);
        s.apply(usage_after(6200, 540));
        s.apply(AgentMessage::Done);

        let summary = s.entries().last().expect("a summary").content.clone();
        assert!(summary.contains("5.2k in"), "this turn's prompt tokens: {summary:?}");
        assert!(summary.contains("340 out"), "this turn's completion tokens: {summary:?}");
    }

    /// A turn that accounted for nothing does not claim "0 in".
    #[test]
    fn a_turn_with_no_usage_reports_only_time() {
        let mut s = Session::new();
        s.apply(AgentMessage::Thinking);
        s.apply(AgentMessage::Done);
        let summary = s.entries().last().expect("a summary").content.clone();
        assert!(!summary.contains(" in"), "got {summary:?}");
    }

    /// Counts are rounded for reading; the exact figure is in /usage.
    #[test]
    fn token_counts_are_rounded_for_reading() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1000), "1.0k");
        assert_eq!(compact_count(5132), "5.1k");
        assert_eq!(compact_count(12_400), "12k");
    }

    /// Cancelling still closes the turn, but there is nothing to summarise: the
    /// line would claim a completion that did not happen.
    #[test]
    fn a_cancelled_turn_gets_no_summary() {
        let mut s = Session::new();
        s.apply(AgentMessage::Thinking);
        s.apply(AgentMessage::Cancelled);
        assert!(
            !s.entries().iter().any(|e| e.kind == EntryKind::TurnSummary),
            "a cancelled turn did not complete",
        );
    }

}
