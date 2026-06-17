// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use chrono::Utc;
use std::io::{BufRead, BufReader, Read as IoRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::log_types::*;
use super::rewind::{FileSnapshot, GitWorktreeSnapshot, RewindCheckpoint};
use crate::api::Message;

/// Append-only JSONL conversation log with smart reload from last compaction.
pub struct ConversationLog {
    path: PathBuf,
    file: std::fs::File,
}

impl ConversationLog {
    /// Open or create a conversation log at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open conversation log: {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    /// Append a single record to the log.
    pub fn append(&mut self, record: &LogRecord) -> Result<()> {
        let line = serde_json::to_string(record).context("Failed to serialize log record")?;
        writeln!(self.file, "{}", line).context("Failed to write to conversation log")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn current_offset(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn truncate_at(&mut self, offset: u64) -> Result<()> {
        self.file.flush()?;
        self.file
            .set_len(offset)
            .context("Failed to truncate conversation log")?;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| {
                format!("Failed to reopen conversation log: {}", self.path.display())
            })?;
        Ok(())
    }

    /// Log a chat message.
    pub fn log_message(&mut self, msg: &Message) -> Result<()> {
        let tool_calls = msg.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| LogToolCall {
                    id: tc.id.clone(),
                    function_name: tc.function.name.clone(),
                    arguments: tc.function.arguments.clone(),
                })
                .collect()
        });

        self.append(&LogRecord::Message(MessageRecord {
            ts: Utc::now(),
            role: msg.role.clone(),
            content: msg.content.clone(),
            tool_calls,
            tool_call_id: msg.tool_call_id.clone(),
            name: msg.name.clone(),
        }))
    }

    /// Log a tool proposal (model wants to call a tool).
    pub fn log_tool_proposed(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &str,
    ) -> Result<()> {
        self.append(&LogRecord::ToolProposed(ToolProposedRecord {
            ts: Utc::now(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments: arguments.to_string(),
        }))
    }

    /// Log a tool approval.
    pub fn log_tool_approved(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        auto_approved: bool,
    ) -> Result<()> {
        self.append(&LogRecord::ToolApproved(ToolApprovedRecord {
            ts: Utc::now(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            auto_approved,
        }))
    }

    /// Log a tool denial.
    pub fn log_tool_denied(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        reason: &str,
    ) -> Result<()> {
        self.append(&LogRecord::ToolDenied(ToolDeniedRecord {
            ts: Utc::now(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            reason: reason.to_string(),
        }))
    }

    /// Log a tool execution result.
    pub fn log_tool_result(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        success: bool,
        output: &str,
    ) -> Result<()> {
        self.append(&LogRecord::ToolResult(ToolResultRecord {
            ts: Utc::now(),
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            success,
            output: output.to_string(),
        }))
    }

    /// Log a run state transition.
    pub fn log_run_state(&mut self, state: RunState) -> Result<()> {
        self.append(&LogRecord::RunState(RunStateRecord {
            ts: Utc::now(),
            state,
        }))
    }

    pub fn log_rewind_restore(&mut self, checkpoint_id: String) -> Result<()> {
        self.append(&LogRecord::RewindRestore(RewindRestoreRecord {
            ts: Utc::now(),
            checkpoint_id,
        }))
    }

    pub fn log_rewind_snapshot(
        &mut self,
        id: String,
        preview: String,
        message_count: usize,
        history_len: usize,
        snapshot_commit: Option<String>,
        snapshot_ref: Option<String>,
        parent_snapshot_commit: Option<String>,
        worktree_snapshots: Vec<GitWorktreeSnapshot>,
        file_snapshots: Vec<FileSnapshot>,
    ) -> Result<u64> {
        self.append(&LogRecord::RewindSnapshot(RewindSnapshotRecord {
            ts: Utc::now(),
            id,
            preview,
            message_count,
            history_len,
            snapshot_commit,
            snapshot_ref,
            parent_snapshot_commit,
            worktree_snapshots: worktree_snapshots
                .into_iter()
                .map(|snapshot| RewindWorktreeSnapshotRecord {
                    root: snapshot.root.to_string_lossy().to_string(),
                    snapshot_commit: snapshot.commit,
                    snapshot_ref: snapshot.ref_name,
                })
                .collect(),
            file_snapshots: file_snapshots
                .into_iter()
                .map(|snapshot| RewindFileSnapshotRecord {
                    path: snapshot.path.to_string_lossy().to_string(),
                    before_content: snapshot.before_content,
                    after_content: snapshot.after_content,
                })
                .collect(),
        }))?;
        self.current_offset()
    }

    /// Log compaction start marker.
    pub fn log_compaction_start(&mut self, messages_before: usize) -> Result<()> {
        self.append(&LogRecord::CompactionStart(CompactionStartRecord {
            ts: Utc::now(),
            messages_before,
        }))
    }

    /// Log the compaction summary.
    pub fn log_compaction_summary(&mut self, summary: CompactionSummary) -> Result<()> {
        self.append(&LogRecord::CompactionSummary(CompactionSummaryRecord {
            ts: Utc::now(),
            summary,
        }))
    }

    /// Log compaction commit marker (compaction is complete).
    pub fn log_compaction_commit(&mut self, messages_after: usize) -> Result<()> {
        self.append(&LogRecord::CompactionCommit(CompactionCommitRecord {
            ts: Utc::now(),
            messages_after,
        }))
    }

    /// Load conversation context using reverse-seek from end of file.
    /// Only reads from the last compaction_commit forward, avoiding OOM on large logs.
    pub fn load_from_last_compaction(&self) -> Result<LoadedContext> {
        let mut file = std::fs::File::open(&self.path)
            .with_context(|| format!("Failed to open log for reading: {}", self.path.display()))?;

        let file_len = file.metadata()?.len();
        if file_len == 0 {
            return Ok(LoadedContext {
                summary: None,
                messages: Vec::new(),
            });
        }

        // Reverse-seek: find the last compaction_commit line by scanning backward.
        // We read chunks from the end and look for complete JSON lines containing "compaction_commit".
        let commit_offset = reverse_find_record_offset(&mut file, file_len, "compaction_commit")?;

        match commit_offset {
            Some(commit_off) => {
                // Find the compaction_summary by scanning backward from the commit line
                let summary_offset = reverse_find_record_offset_before(
                    &mut file,
                    commit_off,
                    "compaction_summary",
                    "compaction_start",
                )?;

                let mut summary: Option<CompactionSummary> = None;
                if let Some(sum_off) = summary_offset {
                    file.seek(SeekFrom::Start(sum_off))?;
                    let mut reader = BufReader::new(&file);
                    let mut line = String::new();
                    if reader.read_line(&mut line)? > 0 {
                        if let Ok(LogRecord::CompactionSummary(s)) =
                            serde_json::from_str::<LogRecord>(line.trim())
                        {
                            summary = Some(s.summary);
                        }
                    }
                }

                // Read from commit line forward to get the rolling window
                file.seek(SeekFrom::Start(commit_off))?;
                let reader = BufReader::new(&file);
                let mut messages: Vec<Message> = Vec::new();
                let mut first_line = true;

                for line in reader.lines() {
                    let line = line.context("Failed to read line")?;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Skip the compaction_commit line itself
                    if first_line {
                        first_line = false;
                        continue;
                    }
                    if let Ok(LogRecord::Message(mr)) = serde_json::from_str::<LogRecord>(trimmed) {
                        messages.push(message_record_to_api_message(&mr));
                    }
                }

                Ok(LoadedContext { summary, messages })
            }
            None => {
                // No compaction found — read entire file (small log)
                file.seek(SeekFrom::Start(0))?;
                let reader = BufReader::new(file);
                let mut messages: Vec<Message> = Vec::new();

                for line in reader.lines() {
                    let line = line.context("Failed to read line")?;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(LogRecord::Message(mr)) = serde_json::from_str::<LogRecord>(trimmed) {
                        messages.push(message_record_to_api_message(&mr));
                    }
                }

                Ok(LoadedContext {
                    summary: None,
                    messages,
                })
            }
        }
    }

    /// Replay log entries for TUI display on resume.
    /// Returns (entries, message_count, compaction_count) — entries are only from
    /// the post-compaction window so we don't replay the entire history.
    pub fn replay_for_display(&self) -> Result<(Vec<DisplayEntry>, usize, usize)> {
        let mut file = std::fs::File::open(&self.path).with_context(|| {
            format!(
                "Failed to open log for display replay: {}",
                self.path.display()
            )
        })?;

        let file_len = file.metadata()?.len();
        if file_len == 0 {
            return Ok((Vec::new(), 0, 0));
        }

        // Count total messages and compactions by scanning full file (just type tags)
        file.seek(SeekFrom::Start(0))?;
        let reader = BufReader::new(&file);
        let mut total_messages = 0usize;
        let mut total_compactions = 0usize;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Quick string check before full parse
            if trimmed.contains("\"message\"") && trimmed.contains("\"role\"") {
                total_messages += 1;
            } else if trimmed.contains("\"compaction_commit\"") {
                total_compactions += 1;
            }
        }

        // Now load just the post-compaction window for display
        let _loaded = self.load_from_last_compaction()?;
        let mut entries = Vec::new();

        // Also replay tool results from the same window — re-read from commit offset
        let commit_offset = {
            let mut f = std::fs::File::open(&self.path)?;
            let flen = f.metadata()?.len();
            reverse_find_record_offset(&mut f, flen, "compaction_commit")?
        };

        let start_offset = commit_offset.map(|o| o).unwrap_or(0);
        file.seek(SeekFrom::Start(start_offset))?;
        let reader = BufReader::new(&file);
        let mut skip_first = commit_offset.is_some();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if skip_first {
                skip_first = false;
                continue;
            }

            if let Ok(record) = serde_json::from_str::<LogRecord>(trimmed) {
                match record {
                    LogRecord::Message(mr) => match mr.role.as_str() {
                        "user" => {
                            if let Some(content) = &mr.content {
                                entries.push(DisplayEntry::User(content.clone()));
                            }
                        }
                        "assistant" => {
                            if let Some(content) = &mr.content {
                                entries.push(DisplayEntry::Assistant(content.clone()));
                            }
                        }
                        _ => {}
                    },
                    LogRecord::ToolResult(tr) => {
                        entries.push(DisplayEntry::ToolResult {
                            tool_name: tr.tool_name,
                            output: tr.output,
                            success: tr.success,
                        });
                    }
                    LogRecord::ToolApproved(ta) => {
                        entries.push(DisplayEntry::ToolCall(ta.tool_name));
                    }
                    _ => {}
                }
            }
        }

        Ok((entries, total_messages, total_compactions))
    }

    pub fn rewind_checkpoints_for_resume(&self) -> Result<Vec<ResumeRewindCheckpoint>> {
        let mut file = std::fs::File::open(&self.path).with_context(|| {
            format!(
                "Failed to open log for rewind checkpoint replay: {}",
                self.path.display()
            )
        })?;

        let file_len = file.metadata()?.len();
        if file_len == 0 {
            return Ok(Vec::new());
        }

        let commit_offset = reverse_find_record_offset(&mut file, file_len, "compaction_commit")?;
        let start_offset = commit_offset.unwrap_or(0);
        file.seek(SeekFrom::Start(start_offset))?;
        let reader = BufReader::new(&file);
        let mut skip_first = commit_offset.is_some();
        let mut display_index = 0usize;
        let mut log_offset = start_offset;
        let mut pending: Vec<(RewindCheckpointRecord, usize)> = Vec::new();
        let mut checkpoints = Vec::new();

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let next_log_offset = log_offset + line.len() as u64 + 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                log_offset = next_log_offset;
                continue;
            }
            if skip_first {
                skip_first = false;
                log_offset = next_log_offset;
                continue;
            }

            if let Ok(record) = serde_json::from_str::<LogRecord>(trimmed) {
                match record {
                    LogRecord::RewindSnapshot(record) => {
                        checkpoints.push(ResumeRewindCheckpoint {
                            checkpoint: RewindCheckpoint {
                                id: record.id,
                                preview: record.preview,
                                message_count: record.message_count,
                                history_len: record.history_len,
                                log_offset: next_log_offset,
                                keep_on_restore: true,
                                snapshot_commit: record.snapshot_commit,
                                snapshot_ref: record.snapshot_ref,
                                git_base_head: None,
                                git_stash_sha: None,
                                worktree_snapshots: record
                                    .worktree_snapshots
                                    .into_iter()
                                    .map(|snapshot| GitWorktreeSnapshot {
                                        root: std::path::PathBuf::from(snapshot.root),
                                        commit: snapshot.snapshot_commit,
                                        ref_name: snapshot.snapshot_ref,
                                    })
                                    .collect(),
                                file_snapshots: record
                                    .file_snapshots
                                    .into_iter()
                                    .map(|snapshot| FileSnapshot {
                                        path: std::path::PathBuf::from(snapshot.path),
                                        before_content: snapshot.before_content,
                                        after_content: snapshot.after_content,
                                    })
                                    .collect(),
                            },
                            display_index,
                        });
                    }
                    LogRecord::RewindCheckpoint(record) => {
                        pending.push((record, display_index));
                    }
                    LogRecord::Message(mr) => {
                        if mr.role == "user" {
                            if !pending.is_empty() {
                                let (record, checkpoint_display_index) = pending.remove(0);
                                let preview = mr
                                    .content
                                    .as_deref()
                                    .map(preview_text_for_rewind)
                                    .unwrap_or_else(|| "(empty message)".to_string());
                                checkpoints.push(ResumeRewindCheckpoint {
                                    checkpoint: RewindCheckpoint {
                                        id: record.id,
                                        preview,
                                        message_count: record.message_count,
                                        history_len: record.history_len,
                                        log_offset: record.log_offset,
                                        keep_on_restore: false,
                                        snapshot_commit: None,
                                        snapshot_ref: None,
                                        git_base_head: record.git_base_head,
                                        git_stash_sha: record.git_stash_sha,
                                        worktree_snapshots: Vec::new(),
                                        file_snapshots: Vec::new(),
                                    },
                                    display_index: checkpoint_display_index,
                                });
                            }
                        }
                        if mr.role == "user" || mr.role == "assistant" {
                            display_index += 1;
                        }
                    }
                    LogRecord::ToolApproved(_) | LogRecord::ToolResult(_) => {
                        display_index += 1;
                    }
                    _ => {}
                }
            }
            log_offset = next_log_offset;
        }

        Ok(checkpoints)
    }

}

/// Entry types for TUI replay on resume.
pub enum DisplayEntry {
    User(String),
    Assistant(String),
    ToolCall(String),
    ToolResult {
        tool_name: String,
        output: String,
        success: bool,
    },
}

pub struct ResumeRewindCheckpoint {
    pub checkpoint: RewindCheckpoint,
    pub display_index: usize,
}

fn preview_text_for_rewind(text: &str) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() > 72 {
        format!("{}...", &single_line[..69])
    } else {
        single_line
    }
}

/// The result of loading context from a conversation log.
pub struct LoadedContext {
    pub summary: Option<CompactionSummary>,
    pub messages: Vec<Message>,
}

/// Scan `.forge/sessions/*/meta.json` and return sorted session metadata.
pub fn scan_sessions(workspace_root: &Path) -> Result<Vec<SessionMeta>> {
    let sessions_dir = workspace_root.join(".forge").join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut metas = Vec::new();
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let meta_path = entry.path().join("meta.json");
        if meta_path.exists() {
            match std::fs::read_to_string(&meta_path) {
                Ok(content) => {
                    if let Ok(meta) = serde_json::from_str::<SessionMeta>(&content) {
                        metas.push(meta);
                    }
                }
                Err(_) => continue,
            }
        }
    }

    // Sort by updated_at descending (most recent first)
    metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(metas)
}

/// Write a new meta.json for a session.
pub fn write_meta(workspace_root: &Path, meta: &SessionMeta) -> Result<()> {
    let meta_path = workspace_root
        .join(".forge")
        .join("sessions")
        .join(&meta.id)
        .join("meta.json");
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(&meta_path, json)?;
    Ok(())
}

/// Generate a session ID: YYYYMMDD_HHMMSS_{3-char hex}
pub fn generate_session_id() -> String {
    let now = Utc::now();
    let hex = format!("{:03x}", rand_u16() & 0xFFF);
    now.format("%Y%m%d_%H%M%S_").to_string() + &hex
}

/// Simple random u16 using std (no extra crate dependency).
fn rand_u16() -> u16 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u8(0);
    h.finish() as u16
}

/// Get the session log path for a given session ID.
pub fn session_log_path(workspace_root: &Path, session_id: &str) -> PathBuf {
    workspace_root
        .join(".forge")
        .join("sessions")
        .join(session_id)
        .join("conversation.jsonl")
}

/// Convert a MessageRecord back to an API Message.
fn message_record_to_api_message(mr: &MessageRecord) -> Message {
    use crate::api::types::{FunctionCall, ToolCall};

    let tool_calls = mr.tool_calls.as_ref().map(|tcs| {
        tcs.iter()
            .map(|tc| ToolCall {
                id: tc.id.clone(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: tc.function_name.clone(),
                    arguments: tc.arguments.clone(),
                },
            })
            .collect()
    });

    Message {
        role: mr.role.clone(),
        content: mr.content.clone(),
        tool_calls,
        tool_call_id: mr.tool_call_id.clone(),
        name: mr.name.clone(),
    }
}

/// Reverse-seek through a file to find the byte offset of the last line containing `marker`.
/// Returns the byte offset of the start of that line, or None.
fn reverse_find_record_offset(
    file: &mut std::fs::File,
    file_len: u64,
    marker: &str,
) -> Result<Option<u64>> {
    const CHUNK: u64 = 64 * 1024; // 64KB chunks
    let marker_bytes = format!("\"{}\"", marker);

    let mut pos = file_len;
    let mut leftover = Vec::<u8>::new();

    while pos > 0 {
        let read_start = pos.saturating_sub(CHUNK);
        let read_len = (pos - read_start) as usize;
        file.seek(SeekFrom::Start(read_start))?;

        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf)?;

        // Prepend to leftover from previous chunk
        buf.extend_from_slice(&leftover);
        leftover.clear();

        // Scan lines in this chunk from bottom to top
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.split('\n').collect();

        // The first "line" might be partial (split at chunk boundary) — save as leftover
        if lines.len() > 1 && read_start > 0 {
            leftover = lines[0].as_bytes().to_vec();
        }

        let start_idx = if read_start > 0 && lines.len() > 1 {
            1
        } else {
            0
        };

        // Calculate byte offsets for each line within the chunk
        let mut line_offset = if start_idx == 1 {
            lines[0].len() + 1
        } else {
            0
        };

        // Build (offset_in_buf, line_content) pairs
        let mut line_entries: Vec<(u64, &str)> = Vec::new();
        for i in start_idx..lines.len() {
            let abs_offset = read_start + line_offset as u64;
            line_entries.push((abs_offset, lines[i]));
            line_offset += lines[i].len() + 1; // +1 for \n
        }

        // Scan from bottom to top
        for (offset, line) in line_entries.iter().rev() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && trimmed.contains(&marker_bytes) {
                // Verify it's actually the right record type
                if let Ok(record) = serde_json::from_str::<LogRecord>(trimmed) {
                    let is_match = match (&record, marker) {
                        (LogRecord::CompactionCommit(_), "compaction_commit") => true,
                        (LogRecord::CompactionSummary(_), "compaction_summary") => true,
                        (LogRecord::CompactionStart(_), "compaction_start") => true,
                        _ => false,
                    };
                    if is_match {
                        return Ok(Some(*offset));
                    }
                }
            }
        }

        pos = read_start;
    }

    Ok(None)
}

/// Reverse-seek for a marker, but only before a given byte offset.
/// Stops if it encounters `stop_marker` first.
fn reverse_find_record_offset_before(
    file: &mut std::fs::File,
    before_offset: u64,
    marker: &str,
    stop_marker: &str,
) -> Result<Option<u64>> {
    const CHUNK: u64 = 64 * 1024;
    let marker_bytes = format!("\"{}\"", marker);
    let stop_bytes = format!("\"{}\"", stop_marker);

    let mut pos = before_offset;
    let mut leftover = Vec::<u8>::new();

    while pos > 0 {
        let read_start = pos.saturating_sub(CHUNK);
        let read_len = (pos - read_start) as usize;
        file.seek(SeekFrom::Start(read_start))?;

        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf)?;

        buf.extend_from_slice(&leftover);
        leftover.clear();

        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.split('\n').collect();

        if lines.len() > 1 && read_start > 0 {
            leftover = lines[0].as_bytes().to_vec();
        }

        let start_idx = if read_start > 0 && lines.len() > 1 {
            1
        } else {
            0
        };
        let mut line_offset = if start_idx == 1 {
            lines[0].len() + 1
        } else {
            0
        };

        let mut line_entries: Vec<(u64, &str)> = Vec::new();
        for i in start_idx..lines.len() {
            let abs_offset = read_start + line_offset as u64;
            line_entries.push((abs_offset, lines[i]));
            line_offset += lines[i].len() + 1;
        }

        for (offset, line) in line_entries.iter().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed.contains(&stop_bytes) {
                return Ok(None); // Hit stop marker first
            }

            if trimmed.contains(&marker_bytes) {
                if let Ok(record) = serde_json::from_str::<LogRecord>(trimmed) {
                    let is_match = match (&record, marker) {
                        (LogRecord::CompactionSummary(_), "compaction_summary") => true,
                        _ => false,
                    };
                    if is_match {
                        return Ok(Some(*offset));
                    }
                }
            }
        }

        pos = read_start;
    }

    Ok(None)
}
