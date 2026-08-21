// Forge agent integration — spawns `forge-agent --headless` and pumps the
// JSON-newline protocol into the IDE.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;

fn default_true() -> bool { true }

/// Resolves the `forge-agent` binary to run. Checked in order:
///
/// 1. Alongside this executable (`Contents/MacOS/forge-agent`, next to a
///    real shipped `Contents/MacOS/forge-ide`) — the bundled copy, resolved
///    from the *running* executable's actual path so it works wherever the
///    end user's `.app` actually lives, not baked in at compile time (same
///    reasoning as MoltenVK's own path resolution in `gfx.rs`).
/// 2. The monorepo's shared workspace `target/{release,debug}/forge-agent`,
///    relative to this crate's own manifest dir — dev convenience for
///    `cargo run`/`./target/debug/forge-ide` inside the source checkout.
/// 3. Bare `"forge-agent"`, resolved via `PATH` — for a separate install
///    (forge's own `install.sh`) with no bundled copy alongside this binary.
pub(crate) fn resolve_forge_agent_path() -> std::ffi::OsString {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("forge-agent"));
        }
    }

    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest.join("../target/release/forge-agent"));
    candidates.push(manifest.join("../target/debug/forge-agent"));

    candidates.into_iter()
        .find(|p| p.is_file())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| "forge-agent".into())
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutgoingMsg {
    SendMessage  { content: String },
    ApproveAction { tool_id: String },
    DenyAction    { tool_id: String, reason: String },
    SwitchModel(serde_json::Value),
    ToggleAutoMode,
    UpdateEndpointReasoning { endpoint_name: String, reasoning: serde_json::Value },
    AnswerQuestion { answer: String },
    ApprovePlan,
    RejectPlan { feedback: String },
    ClearAndApprovePlan,
    CancelRun,
    ProcessInput { content: String },
    BgProcessInput { bg_id: String, content: String },
    Rewind { checkpoint_id: Option<String> },
    RewindPreview { checkpoint_id: String },
    UpdateContextStrategy { strategy: String },
    UpdateOfflineMode { enabled: bool },
    UpdateXaiPriorityTier { endpoint_name: String, enabled: bool },
}

/// One question in a (possibly multi-question) `ask_question` tool call —
/// mirrors forge's `QuestionItemJson`/`QuestionOptionJson` (headless.rs).
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct QuestionItem {
    #[serde(default)] pub question: String,
    #[serde(default)] pub header: String,
    #[serde(default)] pub options: Vec<QuestionOption>,
    #[serde(default)] pub multi_select: bool,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct QuestionOption {
    #[serde(default)] pub label: String,
    #[serde(default)] pub description: String,
}

/// Live token/context usage, as reported by `usage`/`usage_update`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    #[serde(default)] pub last_prompt_tokens:      u64,
    #[serde(default)] pub last_completion_tokens:  u64,
    #[serde(default)] pub total_prompt_tokens:     u64,
    #[serde(default)] pub total_completion_tokens: u64,
    #[serde(default)] pub total_requests:          u64,
    #[serde(default)] pub max_context_tokens:      u64,
    #[serde(default)] pub history_messages:        u64,
}

/// Tab-level policy `poll`/`handle` need but don't themselves own — mirrors
/// `AgentTab` in app.rs, which is where the permission mode and the
/// session-scoped password actually live (the latter never lives on
/// `AgentSession`/`ChatItem` at all — see `ChatItem::InputNeeded`'s doc
/// comment on why).
#[derive(Clone, Copy)]
pub struct SessionPolicy<'a> {
    /// Whether a stored `session_password` (if any) should be auto-injected
    /// into detected password prompts without asking each time. Never
    /// implied by permission mode, including Dangerously Skip All — only
    /// ever set by the user explicitly typing "ALLOW" in a password card.
    pub password_auto_inject: bool,
    pub session_password: Option<&'a str>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    Init {
        // project_root is always the same cwd Forge IDE itself launched
        // forge-agent with (see forge's Executor::new — no independent
        // root-detection happens); model_id is already derivable via
        // `display_model_label` cross-referencing `endpoints`. Both true
        // duplicates, not gaps — deliberately not fields here.
        #[serde(default)] model_name:     String,
        #[serde(default)] endpoints:      Vec<serde_json::Value>,
        #[serde(default)] context_strategy: String,
        #[serde(default)] offline_mode: bool,
        /// forge-agent's own session id (its conversation-log filename) —
        /// distinct from Forge IDE's `conv_id`. Captured so a later respawn
        /// (session restore on reboot, reopening from history, a permission-
        /// mode change that requires a fresh subprocess) can pass it back via
        /// `--resume-session` and get the model's real prior context back,
        /// not just a replayed-looking transcript in the UI.
        #[serde(default)] session_id:     String,
    },
    Thinking,
    Reasoning,
    ReasoningToken { content: String },
    AssistantMessage { content: String },
    AssistantToken   { content: String },
    AssistantDone    { content: String },
    ToolRequest {
        tool_name: String,
        tool_args: String,
        #[serde(default)] tool_id: String,
        #[serde(default)] kind: String,
        #[serde(default)] subagent_id: Option<String>,
        // Fails safe (assume it does need approval) if a server ever omits
        // this — better to show an extra confirmation than to silently
        // auto-run something that should have waited.
        #[serde(default = "default_true")] needs_approval: bool,
    },
    ToolResult {
        tool_name: String,
        result: String,
        #[serde(default)] success: bool,
        #[serde(default)] subagent_id: Option<String>,
    },
    ToolOutput { tool_name: String, content: String },
    Error      { message: String },
    ApiRetry {
        #[serde(default)] attempt:      u32,
        #[serde(default)] max_attempts: u32,
        #[serde(default)] error:        String,
    },
    TurnDiscarded,
    Done,
    Cancelled,
    Usage       { #[serde(default)] snapshot: UsageSnapshot },
    UsageUpdate { #[serde(default)] snapshot: UsageSnapshot },
    ModelSwitched {
        name: String,
    },
    SessionCleared {
        #[serde(default)] session_id: String,
    },
    SubagentStarted {
        #[serde(default)] id:         String,
        #[serde(default)] agent_type: String,
        #[serde(default)] prompt:     String,
        #[serde(default)] parent_id:  Option<String>,
    },
    SubagentStatus {
        #[serde(default)] id:        String,
        #[serde(default)] tool_name: String,
        #[serde(default)] detail:    String,
    },
    SubagentFinished {
        // agent_type isn't needed here — the ChatItem::Subagent card already
        // carries it, set when SubagentStarted first created the card.
        #[serde(default)] id:         String,
        #[serde(default)] summary:    String,
    },
    QuestionRequest {
        #[serde(default)] question: String,
        #[serde(default)] tool_id: String,
        #[serde(default)] items: Vec<QuestionItem>,
    },
    ProcessInputNeeded {
        #[serde(default)] prompt: String,
    },
    BackgroundPromptNeeded {
        #[serde(default)] bg_id:   String,
        #[serde(default)] command: String,
        #[serde(default)] prompt:  String,
    },
    PlanModeEntered,
    PlanModeExited {
        #[serde(default)] reason: String,
    },
    PlanReady {
        #[serde(default)] plan_path: String,
        #[serde(default)] content:   String,
    },
    RewindCheckpoint {
        #[serde(default)] id:              String,
        #[serde(default)] preview:         String,
        #[serde(default)] message_count:   usize,
    },
    /// Response to a `rewind_preview` request (`AgentSession::rewind_preview`)
    /// — what rewinding to `checkpoint_id` would change, without doing it.
    RewindPreview {
        #[serde(default)] checkpoint_id: String,
        #[serde(default)] preview:       String,
        #[serde(default)] summary:       String,
    },
    SessionLoaded {
        #[serde(default)] title: String,
    },
    LoginStatus {
        #[serde(default)] message: String,
    },
    LoginComplete {
        #[serde(default)] success: bool,
        #[serde(default)] message: String,
    },
    EndpointsUpdated { #[serde(default)] endpoints: Vec<serde_json::Value> },
    #[serde(other)]
    Other,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChatItem {
    User(String),
    Assistant { text: String, done: bool },
    Reasoning { text: String, done: bool },
    ToolRequest  { name: String, args: String, id: String, kind: String, approval: ApprovalState,
                   #[serde(default)] expanded: bool },
    ToolResult   { name: String, content: String, success: bool, #[serde(default)] expanded: bool },
    Subagent {
        id: String, agent_type: String, prompt: String,
        #[serde(default)] current_tool: String,
        #[serde(default)] detail: String,
        #[serde(default)] finished: bool,
        #[serde(default)] summary: String,
        /// This subagent's own tool-call activity — kept separate from the
        /// main `items` list so its approvals/history live in their own
        /// docked strip entry instead of interleaving with the top-level
        /// conversation (see `AgentMsg::ToolRequest`/`ToolResult` handling).
        #[serde(default)] items: Vec<ChatItem>,
        /// Whether this subagent's docked-strip entry is expanded — user
        /// controlled, independent of whether it's still running.
        #[serde(default)] expanded: bool,
    },
    /// The agent is blocked waiting on `ask_question` — either a plain
    /// free-text question (`items` empty) or one-to-several structured
    /// multi-choice questions (matches Anthropic's own AskUserQuestion tool
    /// shape). Answering sends one combined text reply back — forge treats
    /// the whole thing as an opaque string handed to the model, so however
    /// this gets formatted on submit is entirely up to the client.
    Question {
        tool_id:  String,
        question: String,
        items:    Vec<QuestionItem>,
        /// Parallels `items` — selected option index/indices per question
        /// (0 or 1 entries for single-select, any number for multi-select).
        selected:   Vec<Vec<usize>>,
        /// Parallels `items` — free-text override for whichever question
        /// had its auto-appended "Other" option picked.
        other_text: Vec<String>,
        /// Free-text reply box, used only when `items` is empty.
        free_text: String,
        answered: bool,
    },
    /// A plan submitted via `exit_plan_mode`, awaiting (or already given)
    /// the user's decision. Unlike regular tool-call approval, forge-agent
    /// blocks on this unconditionally — it never auto-approves server-side
    /// regardless of trust settings, so the "nuances based on permission
    /// mode" live entirely on the client: see `AgentSession::handle`'s
    /// `PlanReady` arm, which auto-resolves this immediately (before it's
    /// ever drawn unresolved) unless the tab is in `AlwaysAsk` mode.
    Plan {
        plan_path: String,
        content:   String,
        resolved:  bool,
        /// Empty while unresolved; a short human-readable outcome once
        /// resolved (e.g. "Approved", "Rejected", "Approved automatically —
        /// Auto-Approve mode").
        #[serde(default)] resolution: String,
        /// Scratch input for the optional feedback box shown alongside
        /// "Reject" — only meaningful while unresolved.
        #[serde(default)] reject_feedback: String,
        /// Whether the full plan content is shown. Defaults to `true` so an
        /// unresolved plan always displays in full; collapses to a summary
        /// once resolved, like other historical cards.
        #[serde(default = "default_true")] expanded: bool,
    },
    /// A running shell command (foreground or backgrounded) is blocked on
    /// stdin. `is_password` is a client-side guess (the prompt text
    /// contains "password", covering the standard `sudo`/`su`/`ssh`/`mysql`
    /// convention) that decides whether this renders as a masked password
    /// card with the session-secret options, or a plain text-reply card.
    /// Whatever's actually typed here is sent via `ProcessInput`/
    /// `BgProcessInput` — never through `SendMessage` — so it's never added
    /// to `self.items` as visible text and never reaches the model.
    InputNeeded {
        /// `None` for the foreground command (the one directly blocking the
        /// current turn); `Some(bg_id)` for a backgrounded one, needed to
        /// route the reply via `BgProcessInput` instead of `ProcessInput`.
        bg_id: Option<String>,
        /// Best-effort command description; empty for the foreground case
        /// (the prompt text alone is the context there).
        command: String,
        prompt:  String,
        is_password: bool,
        resolved:    bool,
        /// Empty while unresolved; a short human-readable outcome once sent
        /// (e.g. "Sent.", "Auto-supplied saved password.", "Rejected.") —
        /// never the actual password value.
        #[serde(default)] resolution: String,
        /// Scratch input for the password/reply field and the "type ALLOW
        /// to remember" confirmation box. `#[serde(skip)]`, deliberately —
        /// conversations auto-save on every frame (see `save_conversation`'s
        /// call site), so anything typed here would otherwise land in a
        /// plaintext conversation history file on disk the instant it's
        /// typed, not just a tmp file. Skipping means it's never part of
        /// what gets serialized at all, regardless of resolution state.
        #[serde(skip)] text: String,
        #[serde(skip)] remember_confirm: String,
    },
    /// A rewind checkpoint forge-agent created automatically at the start
    /// of a turn (before that turn's own file/git changes) — not something
    /// the user requested, just a notification one now exists. Rewinding
    /// restores file/git state *and* truncates forge-agent's own message
    /// history back to this point, but has no way to tell Forge-IDE to
    /// retroactively remove the chat items shown after it — same
    /// limitation forge's own reference (TUI) client has, which can't erase
    /// already-printed terminal output either. The result shows up as a
    /// plain assistant message summarizing what changed; this card stays a
    /// permanent marker of where that point was, not a live/dead toggle.
    Checkpoint {
        id: String,
        preview: String,
        message_count: usize,
        /// "Are you sure" arm state for the Rewind button — transient UI
        /// state, not conversation content, so it's never persisted.
        #[serde(skip)] confirming: bool,
        /// Set once a `rewind_preview` request is sent, cleared when its
        /// response (`AgentMsg::RewindPreview`) arrives. Transient UI state.
        #[serde(skip)] preview_loading: bool,
        /// The response body once it arrives, if ever requested. Transient —
        /// re-requested fresh every time, never worth persisting since it can
        /// go stale the moment anything else changes file/git state.
        #[serde(skip)] preview_result: Option<String>,
    },
    Error(String),
    Status(String),
    /// The provider rejected a request because it's currently at capacity
    /// (xAI's "resource-exhausted" 429, or an equivalent from another
    /// OpenAI-compatible provider) — distinct from a plain `Error` so the UI
    /// can offer switching the affected endpoint to its paid priority tier,
    /// when one exists, right from the card instead of just showing text.
    ProviderBusy {
        message: String,
        /// The endpoint that was active when this happened — looked up
        /// against `AgentSession::endpoints` at render time (not cached
        /// here) so the offered action always reflects the endpoint's
        /// current state, not a stale snapshot.
        endpoint_name: String,
        resolved: bool,
        #[serde(default)] resolution: String,
    },
}

// ── Conversation persistence ──────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedConversation {
    pub id:    String,   // ISO-8601 timestamp used as filename
    pub title: String,   // first user message, truncated
    pub model: String,
    pub items: Vec<ChatItem>,
    /// forge-agent's own session id at save time — lets reopening this
    /// conversation (from history, or on reboot) resume the real subprocess
    /// state via `--resume-session` instead of just replaying `items` into a
    /// fresh, contextless one. `#[serde(default)]` so conversations saved
    /// before this field existed still load (falling back to the old,
    /// display-only replay behavior for those).
    #[serde(default)] pub forge_session_id: String,
    /// The workspace folder this conversation was created in (`IdeApp::cwd`
    /// at save time, as-is — same non-canonicalized convention `session.rs`
    /// already uses to key per-workspace state). `#[serde(default)]` so
    /// conversations saved before this field existed still load; those are
    /// treated as "unknown workspace" and shown regardless of which folder
    /// is open, rather than hidden — the first time one of them is reopened
    /// and re-saved, it gets stamped with whatever folder it's open in then,
    /// and becomes properly scoped to just that folder from that point on.
    #[serde(default)] pub workspace: String,
}

fn conversations_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("forge-ide")
        .join("conversations")
}

pub fn save_conversation(session: &AgentSession, id: &str, cwd: &std::path::Path) {
    let dir = conversations_dir();
    let _ = std::fs::create_dir_all(&dir);
    let title = session.items.iter().find_map(|i| {
        if let ChatItem::User(t) = i {
            let t = t.trim().to_string();
            Some(if t.len() > 60 { format!("{}…", &t[..60]) } else { t })
        } else { None }
    }).unwrap_or_else(|| "New conversation".into());
    let conv = SavedConversation {
        id: id.to_string(), title, model: session.model.clone(),
        items: session.items.clone(),
        forge_session_id: session.forge_session_id.clone(),
        workspace: cwd.to_string_lossy().into_owned(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&conv) {
        let _ = std::fs::write(dir.join(format!("{id}.json")), json);
    }
}

/// Loads conversations belonging to `cwd` — plus any legacy conversation
/// saved before workspace-scoping existed (empty `workspace`), so old
/// conversations aren't hidden just for predating this field. Most recent
/// first.
pub fn load_conversations(cwd: &std::path::Path) -> Vec<SavedConversation> {
    let dir = conversations_dir();
    let cwd_str = cwd.to_string_lossy();
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut convs: Vec<SavedConversation> = rd.flatten()
        .filter(|e| e.path().extension().map_or(false, |x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<SavedConversation>(&s).ok())
        .filter(|c| c.workspace.is_empty() || c.workspace == cwd_str)
        .collect();
    // Most recent first
    convs.sort_by(|a, b| b.id.cmp(&a.id));
    convs
}

enum SessionEvent {
    Msg(AgentMsg),
    Stderr(String),
    Exited,
}

/// Tool names whose successful result means a file on disk changed.
const WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "apply_patch"];

fn extract_path_arg(args_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args_json).ok()?;
    v.get("path").or_else(|| v.get("file_path"))?.as_str().map(str::to_string)
}

/// Finds the `ChatItem::Subagent` with this id anywhere in the tree —
/// searching each level's own nested `items` too, since a subagent can
/// itself nest another (`delegate_task` called from inside a subagent).
///
/// Searched newest-first, because ids are not unique across the life of a
/// transcript. The agent numbers subagents `sub_0`, `sub_1`, … from a counter
/// that starts again at zero in a new process — and a transcript outlives the
/// process: a resumed session, or a tab whose agent was respawned for a
/// permission-mode change or a window reload, keeps its items and gets a second
/// `sub_0`. Oldest-first found the *previous* `sub_0`, already finished, and
/// marked it finished again — so the live subagent never finished, its card
/// said "running" for good, and the docked strip kept it forever, which is
/// exactly what this looked like from the outside.
fn find_subagent_mut<'a>(items: &'a mut [ChatItem], id: &str) -> Option<&'a mut ChatItem> {
    for item in items.iter_mut().rev() {
        // Nested first: a match deeper in this item is newer than the item
        // itself, which was started before the subagent it delegated to.
        if let ChatItem::Subagent { items: nested, .. } = item {
            let deeper = find_subagent_mut(nested, id).is_some();
            if deeper {
                // Re-borrow: the recursive borrow above cannot be returned
                // through the `if let` while `item` is still live.
                let ChatItem::Subagent { items: nested, .. } = item else { unreachable!() };
                return find_subagent_mut(nested, id);
            }
        }
        if matches!(item, ChatItem::Subagent { id: sid, .. } if sid == id) {
            return Some(item);
        }
    }
    None
}

/// Close out any subagent still shown as running.
///
/// The parent turn cannot end while a subagent is live — `delegate_task` blocks
/// it until every subagent it started has returned — so a card still marked
/// running once the turn is over is stale by definition. Without this there was
/// no mechanism at all for clearing one: a finish that went missing (a crashed
/// agent, a dropped event, the id collision above) left a card running and a
/// docked strip entry that would not go away for the rest of the session.
fn finish_stale_subagents(items: &mut [ChatItem]) {
    for item in items {
        if let ChatItem::Subagent { finished, items: nested, .. } = item {
            finish_stale_subagents(nested);
            *finished = true;
        }
    }
}

/// Same search, returning the matching subagent's own nested `items` list —
/// used to route a nested subagent's `SubagentStarted` into its parent
/// instead of the top level.
fn find_subagent_items_mut<'a>(items: &'a mut [ChatItem], id: &str) -> Option<&'a mut Vec<ChatItem>> {
    match find_subagent_mut(items, id)? {
        ChatItem::Subagent { items, .. } => Some(items),
        _ => None,
    }
}

pub struct AgentSession {
    child:        Option<Child>,
    /// Where messages to the agent go. Boxed rather than a `ChildStdin`: the
    /// agent is a local subprocess when the workspace is local and an SSH exec
    /// channel when it is remote, and everything above this line is the same
    /// either way.
    stdin:        Option<Box<dyn Write + Send>>,
    rx:           mpsc::Receiver<SessionEvent>,
    pub items:    Vec<ChatItem>,
    pub model:    String,
    pub thinking: bool,
    pub input:    String,
    pub spawn_err: Option<String>,
    /// Set when there is deliberately no process yet, with what is being waited
    /// for. Distinct from `spawn_err`: nothing has gone wrong, so it must not be
    /// reported as a failure — a window reconnecting to its remote host is the
    /// case this exists for.
    pub pending: Option<String>,
    pub exited:   bool,
    /// Short, human-readable description of what the agent is doing right
    /// now — "Sending…", "Thinking…", "Responding…", "Running shell_exec…" —
    /// updated incrementally as events arrive. Only meaningful while
    /// `turn_active`; drives the live activity strip above the input box.
    pub activity:    String,
    /// Approximate token count streamed so far this turn — counted one per
    /// `AssistantToken`/`ReasoningToken` chunk (not exact tokenizer
    /// accounting, but real and live), reset at the start of each turn.
    pub turn_tokens: u64,
    /// True from the moment a user message is sent until `done`/`cancelled`
    /// comes back. While true, further sends are queued instead of firing
    /// immediately (not persisted — transient in-flight state).
    turn_active:  bool,
    /// Messages typed and sent while a turn was active; drained one at a time
    /// as each turn completes.
    pub queued:   Vec<String>,
    /// Paths written/edited by a successful tool call, drained each frame by
    /// the UI to live-reload any matching open buffer.
    pub changed_files: Vec<String>,
    /// Shell command output, drained each frame by the UI and mirrored into
    /// the integrated Output panel.
    pub shell_events:  Vec<String>,
    /// Set when forge-agent reports it just auto-initialized git for this
    /// project — drained each frame by the UI, which should re-check for a
    /// repo at that point (see the call site for why the panel wouldn't
    /// otherwise notice on its own).
    pub git_just_initialized: bool,
    last_write:   Option<(String, String)>, // (tool_name, path) awaiting its tool_result
    /// Available model endpoints, as reported by `init`/`endpoints_updated`.
    pub endpoints: Vec<serde_json::Value>,
    /// Set when `endpoints` describes what this machine lends over a tunnel
    /// rather than what the agent found in its own config.
    lent_endpoints: bool,
    /// "compaction" or "rolling_window" — mirrors `app_config.agent.context_strategy`,
    /// as reported by `init`. Updated optimistically by `update_context_strategy`
    /// (no ack broadcast for a runtime change, same as `auto_mode`).
    pub context_strategy: String,
    /// Mirrors `app_config.agent.offline_mode`, as reported by `init`.
    /// Updated optimistically by `update_offline_mode`.
    pub offline_mode: bool,
    /// forge-agent's own session id, as reported by `init` — see the field
    /// doc on `AgentMsg::Init::session_id`. Empty until the subprocess's
    /// first `init` message arrives.
    pub forge_session_id: String,
    /// True if this session was spawned with `--resume-session` (its session
    /// log may since have been deleted, or never actually written — e.g. a
    /// prior respawn whose subprocess never got as far as a first message —
    /// in which case forge-agent reports "Session not found" and exits
    /// immediately, before ever sending `init`). Used to recognize that
    /// specific failure below and ask the caller to fall back to a fresh,
    /// non-resumed spawn instead of leaving the tab permanently dead.
    resume_attempted: bool,
    /// Set when a resume attempt fails as described above; the caller
    /// (`AgentTab`) drains this each frame and respawns fresh, keeping the
    /// already-displayed `items` transcript as-is.
    pub needs_resume_fallback: bool,
    /// Latest token/context usage snapshot.
    pub usage:     UsageSnapshot,
    /// Incremented once per tool call the agent makes; drained each frame by
    /// the UI to pulse the anvil watermark's heat, like a hammer strike.
    tool_pulses:   u32,
    /// Forge IDE's own mirror of the subprocess's `auto_mode` flag (skips
    /// read/write/execute approval, but not unrecognized tool kinds). There's
    /// no state broadcast for it, so this is toggled optimistically in
    /// lockstep with the `toggle_auto_mode` messages we send — safe because
    /// headless mode has no other way to change it (no interactive
    /// keybinding of its own).
    pub auto_mode: bool,
    /// Set when a turn finishes — the UI should hand keyboard focus back to
    /// the input box so the next reply doesn't require re-clicking it. The
    /// input box has no built-in way to reclaim focus it never explicitly
    /// asked for (e.g. after the user was last interacting with a terminal
    /// panel elsewhere), so without this a reply typed right after the
    /// agent responds can silently go to whatever *did* last have focus
    /// instead of the chat.
    pub request_input_focus: bool,
}

/// Fold a streamed chunk of tool output into the card it belongs to.
///
/// Output arrives in chunks — often a line at a time — so a card per chunk
/// turned one `cargo build` into a column of hundreds of identical
/// "ok shell_exec" boxes each holding a single line.
fn append_tool_output(items: &mut Vec<ChatItem>, tool_name: String, content: String) {
    if let Some(ChatItem::ToolResult { name, content: existing, .. }) = items.last_mut() {
        if *name == tool_name {
            if !existing.is_empty() && !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push_str(&content);
            return;
        }
    }
    items.push(ChatItem::ToolResult {
        name: tool_name, content, success: true, expanded: false,
    });
}

impl AgentSession {
    /// True while this session is mid-turn (a request is in flight).
    pub fn is_active(&self) -> bool { self.turn_active }

    /// Takes and resets the tool-call pulse count accumulated since the last call.
    pub fn drain_tool_pulses(&mut self) -> u32 { std::mem::take(&mut self.tool_pulses) }

    /// True if the user hasn't sent a message in this tab yet — an untouched
    /// "new" conversation, safe to reuse instead of opening a duplicate.
    pub fn is_unused(&self) -> bool {
        !self.items.iter().any(|i| matches!(i, ChatItem::User(_)))
    }

    /// Spawns `forge-agent --headless` for a new session. `mode` controls how
    /// much it's allowed to do without asking first — see
    /// `settings::AgentPermissionMode`. `DangerouslySkipAll` is applied via
    /// the `--dangerously-allow-all` CLI flag (spawn-time only, forge-agent
    /// has no runtime toggle for it); `AutoApprove` is applied just after
    /// spawn via a `toggle_auto_mode` message, which *can* be toggled live —
    /// see `toggle_auto_mode` below. `resume` is forge-agent's own session id
    /// (see `AgentMsg::Init::session_id`) — when set, passed via
    /// `--resume-session` so the subprocess reconstructs its *real* prior
    /// conversation history for the model, not just whatever transcript the
    /// caller separately replays into `items` for display.
    /// A session that never started, carrying the reason.
    ///
    /// Used when a remote agent cannot be started. Falling back to a local one
    /// would be worse than failing: the agent would run on the wrong machine
    /// and edit files that merely happen to share a path, which is exactly the
    /// confusion this feature exists to end.
    /// A session waiting on something before it can start, carrying what.
    ///
    /// A window reloading a remote workspace has a transcript to show and a
    /// connection still being made. Spawning a local agent in the meantime would
    /// be wrong twice over — it would run on the wrong machine, and it would try
    /// to resume a session that lives on the other one — so the tab holds its
    /// transcript and no process until the connection resolves, then gets a real
    /// session either way.
    pub fn pending(note: String, resume: Option<&str>) -> Self {
        let mut s = Self::failed(String::new());
        s.spawn_err = None;
        s.pending = Some(note);
        s.forge_session_id = resume.unwrap_or_default().to_string();
        s
    }

    pub fn failed(reason: String) -> Self {
        let (_tx, rx) = mpsc::channel::<SessionEvent>();
        Self {
                child: None, stdin: None, rx,
                items: Vec::new(), model: String::new(),
                thinking: false, input: String::new(),
                spawn_err: Some(reason),
                pending: None,
                exited:   false,
                activity:    String::new(),
                turn_tokens: 0,
                turn_active: false,
                queued:      Vec::new(),
                tool_pulses: 0,
                changed_files: Vec::new(),
                shell_events:  Vec::new(),
                git_just_initialized: false,
                last_write:    None,
                endpoints: Vec::new(),
                lent_endpoints: false,
                context_strategy: String::new(),
                offline_mode: false,
                forge_session_id: String::new(),
                resume_attempted: false,
                needs_resume_fallback: false,
                usage:     UsageSnapshot::default(),
                auto_mode:   false,
                request_input_focus: false,
            }
    }

    /// A session driven over an already-open channel, for an agent running on
    /// another machine.
    ///
    /// There is no child to hold: the agent is a process on the remote, and the
    /// SSH channel closing is what ends it. `resume` is recorded the same way
    /// as for a local spawn so a failed resume still reports itself, but the
    /// flag that actually asks for it went into the command that opened this
    /// channel.
    pub fn over_channel(
        stdout: Box<dyn std::io::Read + Send>,
        stdin: Box<dyn Write + Send>,
        resume: Option<&str>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<SessionEvent>();
        Self::pump(stdout, None, tx);
        Self {
            child: None,
            stdin: Some(stdin),
                rx,
                items:    Vec::new(),
                model:    String::new(),
                thinking: false,
                input:    String::new(),
                spawn_err: None,
                pending: None,
                exited:   false,
                activity:    String::new(),
                turn_tokens: 0,
                changed_files: Vec::new(),
                shell_events:  Vec::new(),
                git_just_initialized: false,
                last_write:    None,
                endpoints: Vec::new(),
                lent_endpoints: false,
                context_strategy: String::new(),
                offline_mode: false,
                forge_session_id: String::new(),
                resume_attempted: resume.is_some(),
                needs_resume_fallback: false,
                usage:     UsageSnapshot::default(),
                turn_active: false,
                queued:      Vec::new(),
                tool_pulses: 0,
                auto_mode:   false,
                request_input_focus: false,
        }
    }

    /// Read the agent's protocol off `stdout` and its complaints off `stderr`,
    /// forwarding both to the session. Shared by the local and remote
    /// transports — the protocol does not care what it arrived over.
    fn pump(
        stdout: Box<dyn std::io::Read + Send>,
        stderr: Option<Box<dyn std::io::Read + Send>>,
        tx: mpsc::Sender<SessionEvent>,
    ) {
        let tx_out = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().is_empty() { continue; }
                if let Ok(msg) = serde_json::from_str::<AgentMsg>(&line) {
                    if tx_out.send(SessionEvent::Msg(msg)).is_err() { break; }
                }
            }
            let _ = tx_out.send(SessionEvent::Exited);
        });

        // A remote agent has no separate stderr: SSH gives one stream unless a
        // second is asked for, and its diagnostics arrive interleaved. Nothing
        // to pump in that case.
        if let Some(stderr) = stderr {
            let tx_err = tx;
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    if line.trim().is_empty() { continue; }
                    if tx_err.send(SessionEvent::Stderr(line)).is_err() { break; }
                }
            });
        }
    }

    pub fn spawn(cwd: &Path, mode: crate::settings::AgentPermissionMode, resume: Option<&str>) -> Self {
        let (tx, rx) = mpsc::channel::<SessionEvent>();

        let result: Result<(Child, ChildStdin), String> = (|| {
            let mut cmd = Command::new(resolve_forge_agent_path());
            cmd.arg("--headless").current_dir(cwd);
            if let Some(id) = resume {
                cmd.arg("--resume-session").arg(id);
            }
            if mode == crate::settings::AgentPermissionMode::DangerouslySkipAll {
                cmd.arg("--dangerously-allow-all");
            }
            let mut child = cmd
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("could not spawn `forge-agent`: {e}"))?;
            let stdin  = child.stdin.take().ok_or_else(|| "no stdin pipe".to_string())?;
            let stdout = child.stdout.take().ok_or_else(|| "no stdout pipe".to_string())?;
            let stderr = child.stderr.take().ok_or_else(|| "no stderr pipe".to_string())?;

            Self::pump(Box::new(stdout), Some(Box::new(stderr)), tx.clone());
            Ok((child, stdin))
        })();

        let mut session = match result {
            Ok((child, stdin)) => Self {
                child:    Some(child),
                stdin:    Some(Box::new(stdin)),
                rx,
                items:    Vec::new(),
                model:    String::new(),
                thinking: false,
                input:    String::new(),
                spawn_err: None,
                pending: None,
                exited:   false,
                activity:    String::new(),
                turn_tokens: 0,
                changed_files: Vec::new(),
                shell_events:  Vec::new(),
                git_just_initialized: false,
                last_write:    None,
                endpoints: Vec::new(),
                lent_endpoints: false,
                context_strategy: String::new(),
                offline_mode: false,
                forge_session_id: String::new(),
                resume_attempted: resume.is_some(),
                needs_resume_fallback: false,
                usage:     UsageSnapshot::default(),
                turn_active: false,
                queued:      Vec::new(),
                tool_pulses: 0,
                auto_mode:   false,
                request_input_focus: false,
            },
            Err(e) => Self {
                child: None, stdin: None, rx,
                items: Vec::new(), model: String::new(),
                thinking: false, input: String::new(),
                spawn_err: Some(e),
                pending: None,
                exited:   false,
                activity:    String::new(),
                turn_tokens: 0,
                turn_active: false,
                queued:      Vec::new(),
                tool_pulses: 0,
                changed_files: Vec::new(),
                shell_events:  Vec::new(),
                git_just_initialized: false,
                last_write:    None,
                endpoints: Vec::new(),
                lent_endpoints: false,
                context_strategy: String::new(),
                offline_mode: false,
                forge_session_id: String::new(),
                resume_attempted: resume.is_some(),
                needs_resume_fallback: false,
                usage:     UsageSnapshot::default(),
                auto_mode:   false,
                request_input_focus: false,
            },
        };
        if mode == crate::settings::AgentPermissionMode::AutoApprove {
            session.toggle_auto_mode();
        }
        session
    }

    pub fn poll(&mut self, policy: SessionPolicy) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                SessionEvent::Msg(msg) => self.handle(msg, policy),
                SessionEvent::Stderr(line) => {
                    self.items.push(ChatItem::Status(format!("[stderr] {line}")));
                }
                SessionEvent::Exited => {
                    if !self.exited {
                        self.thinking = false;
                        self.exited = true;
                        // Nothing it delegated survived the process.
                        finish_stale_subagents(&mut self.items);
                        self.items.push(ChatItem::Error(
                            "Agent process exited. Start a new conversation to continue.".into()));
                    }
                }
            }
        }
    }

    fn handle(&mut self, msg: AgentMsg, policy: SessionPolicy) {
        match msg {
            AgentMsg::Init { model_name, endpoints, context_strategy, offline_mode, session_id, .. } => {
                // Same reasoning as `EndpointsUpdated`: a lent list outranks the
                // remote's own. `init` can arrive after the lending, since one
                // is a reply and the other is sent at startup.
                if !self.lent_endpoints {
                    self.endpoints = endpoints;
                }
                self.model = if model_name.is_empty() { "unknown".into() } else { model_name };
                self.context_strategy = if context_strategy.is_empty() { "compaction".into() } else { context_strategy };
                self.offline_mode = offline_mode;
                self.forge_session_id = session_id;
                self.items.push(ChatItem::Status(format!("Agent ready ({})", self.model)));
            }
            AgentMsg::Thinking | AgentMsg::Reasoning => {
                self.thinking = true;
                self.activity = "Thinking…".into();
            }
            AgentMsg::ReasoningToken { content } => {
                self.thinking = false;
                self.activity = "Thinking…".into();
                self.turn_tokens += 1;
                if let Some(ChatItem::Reasoning { text, done }) = self.items.last_mut() {
                    if !*done { text.push_str(&content); return; }
                }
                self.items.push(ChatItem::Reasoning { text: content, done: false });
            }
            AgentMsg::AssistantToken { content } => {
                self.thinking = false;
                self.activity = "Responding…".into();
                self.turn_tokens += 1;
                if let Some(ChatItem::Reasoning { done, .. }) = self.items.last_mut() {
                    *done = true;
                }
                if let Some(ChatItem::Assistant { text, done }) = self.items.last_mut() {
                    if !*done { text.push_str(&content); return; }
                }
                self.items.push(ChatItem::Assistant { text: content, done: false });
            }
            AgentMsg::AssistantMessage { content } | AgentMsg::AssistantDone { content } => {
                self.thinking = false;
                if let Some(ChatItem::Reasoning { done, .. }) = self.items.last_mut() {
                    *done = true;
                }
                // forge-agent auto-initializes git the first time a turn
                // actually changes anything in a project that wasn't
                // version controlled yet (so rewind checkpoints have real
                // git backing), and says so with this exact message.
                // Forge IDE's own Source Control panel only ever checks
                // for a repo once, at folder-open time — flagged here so
                // the caller can refresh it right when this happens,
                // instead of the panel silently staying in its stale
                // "no repository" state until the next reload.
                if content.starts_with("[Initialized a git repository in this project") {
                    self.git_just_initialized = true;
                }
                if let Some(ChatItem::Assistant { text, done }) = self.items.last_mut() {
                    if !*done { *text = content; *done = true; return; }
                }
                self.items.push(ChatItem::Assistant { text: content, done: true });
            }
            AgentMsg::ToolRequest { tool_name, tool_args, tool_id, kind, subagent_id, needs_approval } => {
                self.tool_pulses += 1;
                if subagent_id.is_none() {
                    self.activity = format!("Running {tool_name}…");
                }
                if WRITE_TOOLS.contains(&tool_name.as_str()) {
                    if let Some(path) = extract_path_arg(&tool_args) {
                        self.last_write = Some((tool_name.clone(), path));
                    }
                }
                // Trust the server's own answer (computed from the session's
                // real trust settings — --dangerously-allow-all, auto-mode,
                // auto_approve_writes/_reads) instead of guessing from `kind`
                // alone, which had no way to see those settings. Guessing
                // wrong in either direction was a real bug: a write/execute
                // call under --dangerously-allow-all rendered as a
                // permanently "awaiting approval" card the agent was never
                // actually waiting on, and a read call under a config that
                // requires read approval got auto-approved when it shouldn't.
                let auto_approve = !needs_approval;
                let approval = if auto_approve { ApprovalState::Approved } else { ApprovalState::Pending };
                let item = ChatItem::ToolRequest {
                    name: tool_name, args: tool_args, id: tool_id.clone(), kind, approval,
                    expanded: false,
                };
                match subagent_id.and_then(|sid| self.subagent_items_mut(&sid)) {
                    Some(items) => items.push(item),
                    None => self.items.push(item),
                }
                if auto_approve {
                    let _ = self.write(&OutgoingMsg::ApproveAction { tool_id });
                }
            }
            AgentMsg::ToolResult { tool_name, result, success, subagent_id } => {
                if success {
                    if matches!(&self.last_write, Some((name, _)) if name == &tool_name) {
                        if let Some((_, path)) = self.last_write.take() {
                            self.changed_files.push(path);
                        }
                    }
                    if tool_name == "shell_exec" {
                        self.shell_events.push(result.clone());
                    }
                }
                let item = ChatItem::ToolResult {
                    name: tool_name, content: result, success, expanded: false,
                };
                match subagent_id.and_then(|sid| self.subagent_items_mut(&sid)) {
                    Some(items) => items.push(item),
                    None => { self.activity = "Thinking…".into(); self.items.push(item); }
                }
            }
            AgentMsg::ToolOutput { tool_name, content } => {
                // Not mirrored into shell_events here — the tool_result that follows
                // carries the same content already formatted with command + exit code,
                // so mirroring both would double-print every shell command.
                //
                // Appended to the card already streaming, not pushed as a new one.
                // Output arrives in chunks — often a line at a time — and a card per
                // chunk turned one `cargo build` into a column of hundreds of
                // identical "ok shell_exec" boxes, each holding a single line.
                append_tool_output(&mut self.items, tool_name, content);
            }
            AgentMsg::Error { message } => {
                // forge-agent reports this and exits immediately, before
                // ever sending `init`, when `--resume-session` names a
                // session whose log is missing or was never written (e.g. an
                // earlier respawn whose subprocess never got as far as a
                // first message). Recognized specifically so the caller can
                // fall back to a fresh, non-resumed spawn instead of leaving
                // the tab permanently dead with just this error shown.
                if self.resume_attempted && message.starts_with("Session not found:") {
                    self.needs_resume_fallback = true;
                    self.items.push(ChatItem::Status(
                        "Couldn't resume the prior session's context (its log is missing) — \
                         continuing as a new session instead. Earlier messages above are kept \
                         for reference, but the agent no longer remembers them.".into()));
                } else if message.starts_with("Provider at capacity:") {
                    self.items.push(ChatItem::ProviderBusy {
                        message, endpoint_name: self.model.clone(),
                        resolved: false, resolution: String::new(),
                    });
                } else {
                    self.items.push(ChatItem::Error(message));
                }
            }
            AgentMsg::ApiRetry { attempt, max_attempts, error } => {
                self.activity = format!("Retrying request ({attempt}/{max_attempts})…");
                self.items.push(ChatItem::Status(format!(
                    "Retrying… (attempt {attempt}/{max_attempts}): {error}")));
            }
            AgentMsg::Done => {
                self.thinking = false;
                self.turn_active = false;
                // The turn is over, so nothing it delegated is still running.
                finish_stale_subagents(&mut self.items);
                self.dispatch_next_queued();
                // Only if that didn't immediately kick off another queued
                // turn — genuinely idle now, so hand focus back to the
                // input box for whatever reply comes next. Without this,
                // typing a reply right after the agent responds can
                // silently go wherever the user last clicked instead (a
                // terminal panel open alongside, say), since the input box
                // never re-requests focus on its own.
                if !self.turn_active { self.request_input_focus = true; }
            }
            AgentMsg::Cancelled => {
                self.thinking = false;
                self.turn_active = false;
                // Cancelling aborts the subagents too; their cards should not
                // sit there claiming otherwise.
                finish_stale_subagents(&mut self.items);
                self.items.push(ChatItem::Status("Cancelled".into()));
                self.dispatch_next_queued();
                if !self.turn_active { self.request_input_focus = true; }
            }
            AgentMsg::ModelSwitched { name, .. } => {
                self.model = name.clone();
                self.items.push(ChatItem::Status(format!("Switched model to {name}")));
            }
            AgentMsg::SubagentStarted { id, agent_type, prompt, parent_id } => {
                let entry = ChatItem::Subagent {
                    id, agent_type, prompt,
                    current_tool: String::new(),
                    detail: "starting…".into(),
                    finished: false,
                    summary: String::new(),
                    items: Vec::new(),
                    expanded: true,
                };
                // A subagent nesting another (`delegate_task` called from
                // inside a subagent) carries its parent's id — nest it under
                // that subagent's own `items` instead of listing it as an
                // unrelated top-level entry, so its parent (which is
                // genuinely blocked waiting on it, not doing anything else
                // concurrently) is where it actually shows up.
                match parent_id.and_then(|pid| find_subagent_items_mut(&mut self.items, &pid)) {
                    Some(items) => items.push(entry),
                    None => self.items.push(entry),
                }
            }
            AgentMsg::SubagentStatus { id, tool_name, detail } => {
                if let Some(ChatItem::Subagent { current_tool, detail: d, .. }) = find_subagent_mut(&mut self.items, &id) {
                    *current_tool = tool_name;
                    *d = detail;
                }
            }
            AgentMsg::SubagentFinished { id, summary, .. } => {
                if let Some(ChatItem::Subagent { finished, summary: s, .. }) = find_subagent_mut(&mut self.items, &id) {
                    *finished = true;
                    *s = summary;
                }
            }
            AgentMsg::QuestionRequest { question, tool_id, items } => {
                let n = items.len();
                self.items.push(ChatItem::Question {
                    tool_id, question, items,
                    selected: vec![Vec::new(); n],
                    other_text: vec![String::new(); n],
                    free_text: String::new(),
                    answered: false,
                });
            }
            AgentMsg::ProcessInputNeeded { prompt } => {
                self.push_input_needed(None, String::new(), prompt, policy);
            }
            AgentMsg::BackgroundPromptNeeded { bg_id, command, prompt } => {
                self.push_input_needed(Some(bg_id), command, prompt, policy);
            }
            AgentMsg::PlanModeEntered => {
                self.items.push(ChatItem::Status("Entered plan mode.".into()));
            }
            AgentMsg::PlanModeExited { reason } => {
                let msg = match reason.as_str() {
                    "approved" => "Plan approved — proceeding with implementation.".to_string(),
                    "discuss"  => "Discussing the plan before proceeding.".to_string(),
                    other if !other.is_empty() => format!("Exited plan mode ({other})."),
                    _ => "Exited plan mode.".to_string(),
                };
                self.items.push(ChatItem::Status(msg));
            }
            AgentMsg::PlanReady { plan_path, content } => {
                // Always waits for an explicit human decision, regardless of
                // permission mode — including Auto-Approve and Dangerously
                // Skip All. A plan is a proposal for what to do, not an
                // individual action already covered by a trust setting; forge-
                // agent itself blocks on it unconditionally too (see
                // `handle_exit_plan_mode` in forge's core.rs), and the client
                // shouldn't second-guess that by auto-approving on the user's
                // behalf just because other tool calls are unattended in this
                // mode.
                self.items.push(ChatItem::Plan {
                    plan_path, content,
                    resolved: false, resolution: String::new(),
                    reject_feedback: String::new(), expanded: true,
                });
            }
            AgentMsg::RewindCheckpoint { id, preview, message_count } => {
                self.items.push(ChatItem::Checkpoint {
                    id, preview, message_count, confirming: false,
                    preview_loading: false, preview_result: None,
                });
            }
            AgentMsg::RewindPreview { checkpoint_id, preview, summary } => {
                if let Some(ChatItem::Checkpoint { preview_loading, preview_result, .. }) =
                    self.items.iter_mut().find_map(|i| match i {
                        ChatItem::Checkpoint { id, .. } if *id == checkpoint_id => Some(i),
                        _ => None,
                    })
                {
                    *preview_loading = false;
                    *preview_result = Some(match (summary.is_empty(), preview.is_empty()) {
                        (false, false) => format!("{summary}\n\n{preview}"),
                        (false, true)  => summary,
                        _              => preview,
                    });
                }
            }
            AgentMsg::SessionLoaded { title } => {
                self.items.push(ChatItem::Status(format!("Resumed session: {title}")));
            }
            AgentMsg::SessionCleared { session_id } => {
                // Clearing starts a genuinely new session log on forge-agent's
                // side (see `new_session_id` in forge's core.rs) — without
                // updating this, a later resume (app restart, reopening from
                // history) would pass back the stale *pre-clear* session id
                // and restore the wrong conversation.
                if !session_id.is_empty() { self.forge_session_id = session_id; }
                self.items.push(ChatItem::Status("Session cleared.".into()));
            }
            AgentMsg::LoginStatus { message } => {
                self.items.push(ChatItem::Status(message));
            }
            AgentMsg::LoginComplete { success, message } => {
                self.items.push(ChatItem::Status(format!(
                    "Login {}: {message}", if success { "succeeded" } else { "failed" })));
            }
            AgentMsg::EndpointsUpdated { endpoints } => {
                // Ignored when this machine is lending its own: the agent is
                // describing endpoints on its machine, which it has no
                // credentials for, and adopting them would replace a working
                // list with an unusable one.
                if !self.lent_endpoints {
                    self.endpoints = endpoints;
                    self.items.push(ChatItem::Status("Available endpoints updated.".into()));
                }
            }
            AgentMsg::TurnDiscarded => {
                self.items.push(ChatItem::Status("Turn discarded.".into()));
            }
            AgentMsg::Usage { snapshot } | AgentMsg::UsageUpdate { snapshot } => {
                self.usage = snapshot;
            }
            AgentMsg::Other => {}
        }
    }

    pub fn send_user(&mut self, content: String) {
        let trimmed = content.trim().to_string();
        if trimmed.is_empty() { return; }
        // No process yet, on purpose — a window reconnecting to its host. Held,
        // not sent: writing into a session with no agent behind it loses the
        // message and leaves the tab looking like it is thinking about it.
        if self.pending.is_some() {
            self.queued.push(trimmed);
            return;
        }
        if self.turn_active {
            self.queued.push(trimmed);
            return;
        }
        self.turn_active = true;
        self.activity = "Sending…".into();
        self.turn_tokens = 0;
        self.items.push(ChatItem::User(trimmed.clone()));
        let _ = self.write(&OutgoingMsg::SendMessage { content: trimmed });
    }

    pub(crate) fn dispatch_next_queued(&mut self) {
        if !self.queued.is_empty() {
            let next = self.queued.remove(0);
            self.send_user(next);
        }
    }

    /// Remove a queued message without sending it. No-op if `index` is out of bounds.
    pub fn revoke_queued(&mut self, index: usize) {
        if index < self.queued.len() {
            self.queued.remove(index);
        }
    }

    /// Interrupts the current turn and sends `index`'s queued message as
    /// soon as the cancellation actually completes — moves it to the front
    /// of the queue, since `dispatch_next_queued` (called from both the
    /// `Done` and `Cancelled` handlers) always sends whatever's there next.
    /// No-op if `index` is out of bounds; if there's no active turn to
    /// interrupt after all (a race with it finishing on its own), just
    /// dispatches immediately instead of waiting for a cancellation that
    /// was never coming.
    pub fn send_queued_now(&mut self, index: usize) {
        if index >= self.queued.len() { return; }
        let msg = self.queued.remove(index);
        self.queued.insert(0, msg);
        if self.turn_active {
            self.cancel_run();
        } else {
            self.dispatch_next_queued();
        }
    }

    /// The nested tool-call list belonging to the `ChatItem::Subagent` with
    /// this id, if it's still tracked (it always should be — `SubagentStarted`
    /// always arrives before any of that subagent's own tool calls).
    fn subagent_items_mut(&mut self, id: &str) -> Option<&mut Vec<ChatItem>> {
        find_subagent_items_mut(&mut self.items, id)
    }

    pub fn approve(&mut self, tool_id: String) {
        let _ = self.write(&OutgoingMsg::ApproveAction { tool_id });
    }

    pub fn deny(&mut self, tool_id: String, reason: String) {
        let _ = self.write(&OutgoingMsg::DenyAction { tool_id, reason });
    }

    /// Sends a combined answer back for a pending `ask_question` call.
    /// Forge treats the whole thing as one opaque string handed to the
    /// model — there's no structured per-question encoding on the wire.
    pub fn answer_question(&mut self, answer: String) {
        let _ = self.write(&OutgoingMsg::AnswerQuestion { answer });
    }

    /// Marks the last unresolved `ChatItem::Plan` resolved with `label` and
    /// sends `action` — the one place that actually talks to the plan-review
    /// wire messages, used both for a manual button click and for the
    /// client-side auto-approval in `handle`'s `PlanReady` arm.
    fn resolve_plan(&mut self, action: OutgoingMsg, label: &str) {
        if let Some(ChatItem::Plan { resolved, resolution, expanded, .. }) = self.items.iter_mut().rev()
            .find(|i| matches!(i, ChatItem::Plan { resolved: false, .. }))
        {
            *resolved   = true;
            *resolution = label.to_string();
            *expanded   = false;
        }
        let _ = self.write(&action);
    }

    /// Pushes an `InputNeeded` card for a command blocked on stdin — or, if
    /// this looks like a password prompt and the tab already has a stored,
    /// auto-inject-enabled session password, resolves and sends it
    /// immediately instead of ever showing an unresolved card at all. Either
    /// way, the password value itself never becomes part of `self.items` as
    /// visible text — only a status label like "Auto-supplied saved
    /// password." does.
    fn push_input_needed(&mut self, bg_id: Option<String>, command: String, prompt: String, policy: SessionPolicy) {
        let is_password = prompt.to_lowercase().contains("password");
        if is_password {
            if let (true, Some(pw)) = (policy.password_auto_inject, policy.session_password) {
                let content = pw.to_string();
                self.items.push(ChatItem::InputNeeded {
                    bg_id: bg_id.clone(), command, prompt, is_password,
                    resolved: true, resolution: "Auto-supplied saved password.".into(),
                    text: String::new(), remember_confirm: String::new(),
                });
                self.send_input(bg_id, content);
                return;
            }
        }
        self.items.push(ChatItem::InputNeeded {
            bg_id, command, prompt, is_password,
            resolved: false, resolution: String::new(),
            text: String::new(), remember_confirm: String::new(),
        });
    }

    fn send_input(&mut self, bg_id: Option<String>, content: String) {
        match bg_id {
            Some(bg_id) => { let _ = self.write(&OutgoingMsg::BgProcessInput { bg_id, content }); }
            None => { let _ = self.write(&OutgoingMsg::ProcessInput { content }); }
        }
    }

    /// Resolves the last unresolved `InputNeeded` card and sends `content`.
    /// Whether that's a one-time password, a saved session password, or a
    /// plain non-password reply is entirely app.rs's call (it owns the
    /// session-password storage) — this only ever moves bytes and marks the
    /// card resolved, and never records `content` anywhere else.
    pub fn answer_input(&mut self, content: String, label: &str) {
        let mut bg_id: Option<String> = None;
        if let Some(ChatItem::InputNeeded { resolved, resolution, bg_id: id, .. }) = self.items.iter_mut().rev()
            .find(|i| matches!(i, ChatItem::InputNeeded { resolved: false, .. }))
        {
            *resolved   = true;
            *resolution = label.to_string();
            bg_id = id.clone();
        }
        self.send_input(bg_id, content);
    }

    /// Declines to answer — sends a bare newline rather than leaving the
    /// process hung forever, so most prompts (password or otherwise) fail
    /// fast and the agent gets to react instead of stalling the whole turn.
    pub fn reject_input(&mut self) {
        self.answer_input(String::new(), "Rejected.");
    }

    pub fn approve_plan(&mut self) {
        self.resolve_plan(OutgoingMsg::ApprovePlan, "Approved.");
    }

    pub fn approve_plan_clear(&mut self) {
        self.resolve_plan(OutgoingMsg::ClearAndApprovePlan, "Approved (context cleared).");
    }

    pub fn reject_plan(&mut self, feedback: String) {
        let label = if feedback.trim().is_empty() {
            "Rejected.".to_string()
        } else {
            format!("Rejected — \"{}\"", feedback.trim())
        };
        self.resolve_plan(OutgoingMsg::RejectPlan { feedback }, &label);
    }

    /// Exits plan mode without approving or rejecting — the agent asks the
    /// user what to change instead of guessing from silence. Forge
    /// recognizes this via the literal feedback text "DISCUSS" (see
    /// `handle_exit_plan_mode` in forge's core.rs).
    pub fn discuss_plan(&mut self) {
        self.resolve_plan(
            OutgoingMsg::RejectPlan { feedback: "DISCUSS".to_string() },
            "Requested discussion.");
    }

    /// Interrupts the in-flight turn — forge handles `cancel_run` at every
    /// await point in its own agent loop (mid-completion, mid-tool-call,
    /// mid-shell-exec), so this is a real interrupt, not just a client-side
    /// "stop listening." `turn_active` clears itself once the matching
    /// `Cancelled` event comes back, same as it already does today.
    pub fn cancel_run(&mut self) {
        let _ = self.write(&OutgoingMsg::CancelRun);
    }

    /// Restores file/git state and forge-agent's own message history back
    /// to `checkpoint_id` (or the most recent checkpoint if `None`). The
    /// result comes back as a plain assistant message summarizing what
    /// changed — see `ChatItem::Checkpoint`'s doc comment for why this
    /// doesn't (and can't) also retroactively remove chat items shown after
    /// the checkpoint.
    pub fn rewind(&mut self, checkpoint_id: Option<String>) {
        let _ = self.write(&OutgoingMsg::Rewind { checkpoint_id });
    }

    /// Ask forge-agent what rewinding to `checkpoint_id` would actually
    /// change, without doing it — the response arrives as
    /// `AgentMsg::RewindPreview`.
    pub fn rewind_preview(&mut self, checkpoint_id: String) {
        let _ = self.write(&OutgoingMsg::RewindPreview { checkpoint_id });
    }

    /// Ask the agent to switch to a different model/endpoint. `endpoint` should
    /// be one of the objects from `self.endpoints`, sent back verbatim so we
    /// never have to reconstruct its (possibly provider-specific) fields.
    /// The endpoints this machine is lending to a remote agent.
    ///
    /// The model picker reads `endpoints`, which normally comes from the agent's
    /// own `init` — its machine's config. A remote agent's config is not the
    /// user's and may be empty, so the picker showed one model, or none.
    /// Overwritten here, once, with what is actually reachable through the
    /// tunnel; a later `EndpointsUpdated` from the agent would be about
    /// endpoints it cannot authenticate with, so this stands.
    pub fn set_lent_endpoints(&mut self, endpoints: Vec<serde_json::Value>) {
        self.endpoints = endpoints;
        self.lent_endpoints = true;
    }

    pub fn switch_model(&mut self, endpoint: serde_json::Value) {
        let _ = self.write(&OutgoingMsg::SwitchModel(endpoint));
    }

    /// Flips the subprocess's `auto_mode` (skips read/write/execute
    /// confirmation, not unrecognized tool kinds). Safe to call any time —
    /// headless mode has no other way to change it, so `self.auto_mode`
    /// always mirrors reality as long as this is the only caller.
    pub fn toggle_auto_mode(&mut self) {
        let _ = self.write(&OutgoingMsg::ToggleAutoMode);
        self.auto_mode = !self.auto_mode;
    }

    /// Switches forge's context-management strategy for long conversations:
    /// "compaction" (default — summarizes older messages via an LLM call once
    /// the context window fills) or "rolling_window" (just drops the oldest
    /// messages, no summarization call). Persisted into forge's own
    /// config.toml; no ack broadcast, so — same as `toggle_auto_mode` — this
    /// updates `self.context_strategy` optimistically.
    pub fn update_context_strategy(&mut self, strategy: String) {
        let _ = self.write(&OutgoingMsg::UpdateContextStrategy { strategy: strategy.clone() });
        self.context_strategy = strategy;
    }

    /// Toggles forge-agent's offline mode for this session: no network calls
    /// except the active model endpoint's own API — `web_search`/`web_fetch`
    /// disabled, ChatGPT Codex background catalog/version checks skipped.
    /// Persisted into forge's own config.toml; updates `self.offline_mode`
    /// optimistically, same as `update_context_strategy`.
    pub fn update_offline_mode(&mut self, enabled: bool) {
        let _ = self.write(&OutgoingMsg::UpdateOfflineMode { enabled });
        self.offline_mode = enabled;
    }

    /// Updates one endpoint's reasoning/thinking config. Persisted by
    /// forge-agent into its own config.toml (matched by `endpoint_name`) —
    /// there's no ack broadcast, so the caller should also update its own
    /// cached copy of `self.endpoints` to reflect the change immediately.
    pub fn update_endpoint_reasoning(&mut self, endpoint_name: String, reasoning: serde_json::Value) {
        let _ = self.write(&OutgoingMsg::UpdateEndpointReasoning { endpoint_name, reasoning });
    }

    /// Opts an xAI endpoint in or out of priority processing (`service_tier:
    /// "priority"` on every request — 2x xAI's standard per-token price, a
    /// charge from xAI itself, not Forge IDE). Same no-ack-broadcast caveat
    /// as `update_endpoint_reasoning` above.
    pub fn update_xai_priority_tier(&mut self, endpoint_name: String, enabled: bool) {
        let _ = self.write(&OutgoingMsg::UpdateXaiPriorityTier { endpoint_name, enabled });
    }

    fn write(&mut self, msg: &OutgoingMsg) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else { return Ok(()); };
        let json = serde_json::to_string(msg).map_err(std::io::Error::other)?;
        writeln!(stdin, "{}", json)?;
        stdin.flush()?;
        Ok(())
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child { let _ = child.kill(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(items: &[ChatItem]) -> (&str, &str) {
        match items.last().unwrap() {
            ChatItem::ToolResult { name, content, .. } => (name.as_str(), content.as_str()),
            _ => panic!("expected a tool card as the last item"),
        }
    }

    #[test]
    fn streamed_output_lands_in_one_card() {
        // A build streams a line at a time. Before this, each line got its own
        // "ok shell_exec" box, so `cargo build` produced a column of hundreds.
        let mut items = Vec::new();
        for line in ["Compiling a v0.1.0", "Compiling b v0.1.0", "Finished"] {
            append_tool_output(&mut items, "shell_exec".into(), line.into());
        }
        assert_eq!(items.len(), 1, "one running command should be one card");
        assert_eq!(
            card(&items),
            ("shell_exec", "Compiling a v0.1.0\nCompiling b v0.1.0\nFinished"),
        );
    }

    #[test]
    fn a_different_tool_starts_a_new_card() {
        let mut items = Vec::new();
        append_tool_output(&mut items, "shell_exec".into(), "building".into());
        append_tool_output(&mut items, "read_file".into(), "src/main.rs".into());
        assert_eq!(items.len(), 2);
        assert_eq!(card(&items), ("read_file", "src/main.rs"));
    }

    #[test]
    fn chunks_that_already_end_in_a_newline_are_not_double_spaced() {
        // The agent may send whole lines with their terminator, or fragments
        // without one; joining must not depend on which.
        let mut items = Vec::new();
        append_tool_output(&mut items, "shell_exec".into(), "one\n".into());
        append_tool_output(&mut items, "shell_exec".into(), "two".into());
        assert_eq!(card(&items).1, "one\ntwo");
    }

    #[test]
    fn output_after_a_finished_card_does_not_merge_into_it() {
        // A completed result and the next command's output are separate cards
        // even when both are shell_exec — the finished one is pushed by the
        // ToolResult arm and must not absorb the next command's stream.
        let mut items = vec![ChatItem::Status("between".into())];
        append_tool_output(&mut items, "shell_exec".into(), "after".into());
        assert_eq!(items.len(), 2);
        assert_eq!(card(&items), ("shell_exec", "after"));
    }
}

#[cfg(test)]
mod subagent_wire_tests {
    use super::*;

    /// The exact line forge-agent writes when a delegated task ends. Parsed
    /// here rather than trusted, because `pump` drops a line it cannot
    /// deserialize without a word — so a shape mismatch looks like a subagent
    /// that never finished, which is exactly what it looked like.
    #[test]
    fn a_finish_line_parses() {
        let line = r#"{"type":"subagent_finished","id":"sub-1","agent_type":"explore","summary":"a report"}"#;
        match serde_json::from_str::<AgentMsg>(line) {
            Ok(AgentMsg::SubagentFinished { id, summary }) => {
                assert_eq!(id, "sub-1");
                assert_eq!(summary, "a report");
            }
            other => panic!("did not parse as a finish: {other:?}"),
        }
    }

    #[test]
    fn a_start_line_parses() {
        let line = r#"{"type":"subagent_started","id":"sub-1","agent_type":"explore","prompt":"Explore"}"#;
        match serde_json::from_str::<AgentMsg>(line) {
            Ok(AgentMsg::SubagentStarted { id, agent_type, prompt, parent_id }) => {
                assert_eq!((id.as_str(), agent_type.as_str(), prompt.as_str()), ("sub-1", "explore", "Explore"));
                assert!(parent_id.is_none());
            }
            other => panic!("did not parse as a start: {other:?}"),
        }
    }

    fn running(id: &str) -> ChatItem {
        ChatItem::Subagent {
            id: id.into(), agent_type: "explore".into(), prompt: String::new(),
            current_tool: String::new(), detail: String::new(),
            finished: false, summary: String::new(), items: Vec::new(), expanded: true,
        }
    }

    fn finished_state(item: &ChatItem) -> bool {
        match item { ChatItem::Subagent { finished, .. } => *finished, _ => panic!("not a subagent") }
    }

    /// Ids repeat. The agent counts `sub_0`, `sub_1`, … from a counter that
    /// restarts at zero in a new process, and a transcript outlives the process
    /// — a resumed session, or a tab whose agent was respawned for a
    /// permission-mode change or a window reload, keeps its items and gets a
    /// second `sub_0`. The finish belongs to the newer one.
    #[test]
    fn a_finish_lands_on_the_live_card_not_an_older_one_with_the_same_id() {
        let mut items = vec![
            ChatItem::Subagent {
                id: "sub_0".into(), agent_type: "explore".into(), prompt: String::new(),
                current_tool: String::new(), detail: String::new(),
                finished: true, summary: "the first answer".into(),
                items: Vec::new(), expanded: false,
            },
            ChatItem::Status("… a resume, or a respawned agent …".into()),
            running("sub_0"),
        ];

        let found = find_subagent_mut(&mut items, "sub_0").expect("no card found");
        if let ChatItem::Subagent { finished, summary, .. } = found {
            *finished = true;
            *summary = "the second answer".into();
        }

        assert!(finished_state(&items[2]), "the live subagent is still running");
        // And the older one keeps the answer it actually returned.
        let ChatItem::Subagent { summary, .. } = &items[0] else { panic!() };
        assert_eq!(summary, "the first answer");
    }

    /// A turn cannot end with a subagent still running — `delegate_task` blocks
    /// the parent until they return. So anything still marked running once the
    /// turn is over is stale, and there was previously no way for it to clear:
    /// the card stayed, and so did the docked strip entry.
    #[test]
    fn the_turn_ending_clears_a_card_left_running() {
        let mut items = vec![running("sub_0"), running("sub_1")];
        if let ChatItem::Subagent { items: nested, .. } = &mut items[1] {
            nested.push(running("sub_1:call-9"));
        }

        finish_stale_subagents(&mut items);

        assert!(finished_state(&items[0]));
        assert!(finished_state(&items[1]));
        let ChatItem::Subagent { items: nested, .. } = &items[1] else { panic!() };
        assert!(finished_state(&nested[0]), "a nested subagent pins the strip too");
    }

    /// The card is found and flipped wherever it sits — including nested, where
    /// a subagent delegated to another.
    #[test]
    fn finishing_reaches_a_nested_card() {
        fn card(id: &str) -> ChatItem {
            ChatItem::Subagent {
                id: id.into(), agent_type: "explore".into(), prompt: String::new(),
                current_tool: String::new(), detail: String::new(),
                finished: false, summary: String::new(), items: Vec::new(), expanded: true,
            }
        }
        let mut items = vec![card("outer")];
        if let ChatItem::Subagent { items: nested, .. } = &mut items[0] {
            nested.push(card("inner"));
        }
        for id in ["outer", "inner"] {
            let found = find_subagent_mut(&mut items, id).expect("card not found");
            if let ChatItem::Subagent { finished, .. } = found { *finished = true; }
        }
        let ChatItem::Subagent { finished, items: nested, .. } = &items[0] else { panic!() };
        assert!(*finished);
        let ChatItem::Subagent { finished: inner, .. } = &nested[0] else { panic!() };
        assert!(*inner, "a nested subagent must be reachable too");
    }
}
