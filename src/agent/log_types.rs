// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lightweight session metadata stored alongside the JSONL conversation log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
    pub compaction_count: usize,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_window_plan: Option<String>,
}

/// Every line in the JSONL conversation log is one of these record types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LogRecord {
    /// A chat message (user, assistant, system, or tool result).
    #[serde(rename = "message")]
    Message(MessageRecord),

    /// A tool was proposed by the model (before approval).
    #[serde(rename = "tool_proposed")]
    ToolProposed(ToolProposedRecord),

    /// A tool was approved by the user or auto-approved.
    #[serde(rename = "tool_approved")]
    ToolApproved(ToolApprovedRecord),

    /// A tool was denied by the user.
    #[serde(rename = "tool_denied")]
    ToolDenied(ToolDeniedRecord),

    /// A tool finished executing and produced a result.
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultRecord),

    /// Marks the beginning of a compaction operation.
    #[serde(rename = "compaction_start")]
    CompactionStart(CompactionStartRecord),

    /// The LLM-generated structured summary for compaction.
    #[serde(rename = "compaction_summary")]
    CompactionSummary(CompactionSummaryRecord),

    /// Marks the successful completion of a compaction.
    /// On reload, we scan backwards for this marker.
    #[serde(rename = "compaction_commit")]
    CompactionCommit(CompactionCommitRecord),

    /// A snapshot of the agent's run state.
    #[serde(rename = "run_state")]
    RunState(RunStateRecord),

    /// A rewind checkpoint captured before a user turn starts.
    #[serde(rename = "rewind_checkpoint")]
    RewindCheckpoint(RewindCheckpointRecord),

    /// A hidden Git snapshot captured after a turn reaches a boundary.
    #[serde(rename = "rewind_snapshot")]
    RewindSnapshot(RewindSnapshotRecord),

    /// A rewind operation restored a previous checkpoint.
    #[serde(rename = "rewind_restore")]
    RewindRestore(RewindRestoreRecord),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub ts: DateTime<Utc>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LogToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogToolCall {
    pub id: String,
    pub function_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolProposedRecord {
    pub ts: DateTime<Utc>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolApprovedRecord {
    pub ts: DateTime<Utc>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub auto_approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeniedRecord {
    pub ts: DateTime<Utc>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub ts: DateTime<Utc>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionStartRecord {
    pub ts: DateTime<Utc>,
    pub messages_before: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSummaryRecord {
    pub ts: DateTime<Utc>,
    pub summary: CompactionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSummary {
    pub goal: String,
    pub repo_map: Vec<String>,
    pub work_completed: Vec<String>,
    pub current_state: String,
    pub commands_run: Vec<String>,
    pub decisions: Vec<String>,
    pub next_actions: Vec<String>,
    pub pitfalls: Vec<String>,
}

impl CompactionSummary {
    pub fn to_context_string(&self) -> String {
        let mut out = String::from("[Compaction Summary]\n");

        out.push_str(&format!("Goal: {}\n", self.goal));

        if !self.repo_map.is_empty() {
            out.push_str("Repo map:\n");
            for item in &self.repo_map {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        if !self.work_completed.is_empty() {
            out.push_str("Work completed:\n");
            for item in &self.work_completed {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        out.push_str(&format!("Current state: {}\n", self.current_state));

        if !self.commands_run.is_empty() {
            out.push_str("Commands run:\n");
            for item in &self.commands_run {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        if !self.decisions.is_empty() {
            out.push_str("Decisions:\n");
            for item in &self.decisions {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        if !self.next_actions.is_empty() {
            out.push_str("Next actions:\n");
            for item in &self.next_actions {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        if !self.pitfalls.is_empty() {
            out.push_str("Known pitfalls:\n");
            for item in &self.pitfalls {
                out.push_str(&format!("  - {}\n", item));
            }
        }

        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionCommitRecord {
    pub ts: DateTime<Utc>,
    pub messages_after: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Idle,
    WaitingUser,
    AwaitingApproval,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStateRecord {
    pub ts: DateTime<Utc>,
    pub state: RunState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindCheckpointRecord {
    pub ts: DateTime<Utc>,
    pub id: String,
    pub message_count: usize,
    pub history_len: usize,
    pub log_offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_base_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_stash_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindRestoreRecord {
    pub ts: DateTime<Utc>,
    pub checkpoint_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindSnapshotRecord {
    pub ts: DateTime<Utc>,
    pub id: String,
    pub preview: String,
    pub message_count: usize,
    pub history_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_snapshot_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worktree_snapshots: Vec<RewindWorktreeSnapshotRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_snapshots: Vec<RewindFileSnapshotRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindWorktreeSnapshotRecord {
    pub root: String,
    pub snapshot_commit: String,
    pub snapshot_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindFileSnapshotRecord {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_content: Option<String>,
}
