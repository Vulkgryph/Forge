// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;

use super::conversation_log::ConversationLog;
use super::log_types::CompactionSummary;
use crate::api::{ApiClient, Message};

/// Default number of recent messages to keep after compaction.
const ROLLING_WINDOW_SIZE: usize = 20;
const ROLLING_PLAN_MARKER: &str = "[Forge rolling-window approved plan]";

/// Perform compaction: call the LLM to summarize messages, write JSONL markers,
/// and return the new in-memory history (system + policy + summary + rolling window).
///
/// When `keep_rolling_window` is true (default for same-model compaction), the
/// last ROLLING_WINDOW_SIZE messages are appended after the summary. When false
/// (used when switching to a small-context model), only the system prompt and
/// compaction summary are kept so the result fits tight context budgets.
pub async fn perform_compaction(
    client: &ApiClient,
    model_id: &str,
    history: &[Message],
    system_prompt: &str,
    log: &mut ConversationLog,
    keep_rolling_window: bool,
) -> Result<Vec<Message>> {
    let messages_before = history.len();

    // Write compaction_start marker
    log.log_compaction_start(messages_before)?;

    // Build the summarizer prompt from the messages being compacted
    let summary = generate_structured_summary(client, model_id, history).await?;

    // Write the summary to the log
    log.log_compaction_summary(summary.clone())?;

    // Build the new history
    let mut new_history = Vec::new();

    // 1. System prompt (static)
    new_history.push(Message::system(system_prompt));

    // 2. Compaction summary as an assistant message
    new_history.push(Message::assistant(&summary.to_context_string()));

    // 3. Rolling window of recent messages (skip system prompt)
    if keep_rolling_window {
        let non_system: Vec<&Message> = history.iter().filter(|m| m.role != "system").collect();
        new_history.extend(valid_recent_window(&non_system, ROLLING_WINDOW_SIZE));
    }

    let messages_after = new_history.len();

    // Write compaction_commit marker
    log.log_compaction_commit(messages_after)?;

    Ok(new_history)
}

/// Use the LLM to generate a structured summary of the conversation so far.
/// This calls the model with tools disabled and a specific summarizer prompt.
pub(crate) async fn generate_structured_summary(
    client: &ApiClient,
    model_id: &str,
    history: &[Message],
) -> Result<CompactionSummary> {
    // Build a condensed transcript of the conversation for the summarizer
    let transcript = build_transcript(history);

    let summarizer_prompt = format!(
        r#"You are a conversation summarizer for a coding agent. Below is a transcript of a coding session.
Your job is to produce a structured JSON summary that will replace the old messages in the agent's context.

The summary MUST be valid JSON with exactly these fields:
{{
  "goal": "what the user is trying to accomplish",
  "repo_map": ["key files and what they do"],
  "work_completed": ["list of completed work items"],
  "current_state": "what's working, what's failing, what's next",
  "commands_run": ["commands and their outcomes"],
  "decisions": ["architectural or implementation decisions made"],
  "next_actions": ["what should happen next"],
  "pitfalls": ["things to avoid or known issues"]
}}

Respond with ONLY the JSON object, no markdown fences, no explanation.

TRANSCRIPT:
{}"#,
        transcript
    );

    // Call the model with no tools (pure text generation)
    let response = client
        .chat_simple(model_id, &summarizer_prompt)
        .await
        .map_err(|e| anyhow::anyhow!("Summarizer LLM call failed: {}", e))?;

    // Parse the JSON response
    parse_summary_response(&response)
}

/// Build a condensed text transcript from the message history.
fn build_transcript(history: &[Message]) -> String {
    let mut transcript = String::new();
    let max_content_len = 500;

    for msg in history {
        let role = &msg.role;
        let content = msg.content.as_deref().unwrap_or("");

        match role.as_str() {
            "system" => {
                // Skip system prompt in transcript (it's static)
                continue;
            }
            "user" => {
                let truncated: String = content.chars().take(max_content_len).collect();
                transcript.push_str(&format!("[USER]: {}\n", truncated));
            }
            "assistant" => {
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let args: String = tc.function.arguments.chars().take(200).collect();
                        transcript.push_str(&format!(
                            "[ASSISTANT calls {}]: {}\n",
                            tc.function.name, args
                        ));
                    }
                }
                if !content.is_empty() {
                    let truncated: String = content.chars().take(max_content_len).collect();
                    transcript.push_str(&format!("[ASSISTANT]: {}\n", truncated));
                }
            }
            "tool" => {
                let name = msg.name.as_deref().unwrap_or("unknown");
                let truncated: String = content.chars().take(300).collect();
                transcript.push_str(&format!("[TOOL {}]: {}\n", name, truncated));
            }
            _ => {}
        }
    }

    // If transcript is very long, truncate the middle and keep start + end
    const MAX_TRANSCRIPT: usize = 12_000;
    if transcript.len() > MAX_TRANSCRIPT {
        let keep_each = MAX_TRANSCRIPT / 2;
        let start: String = transcript.chars().take(keep_each).collect();
        let end: String = transcript
            .chars()
            .rev()
            .take(keep_each)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!(
            "{}\n\n... [{} characters omitted] ...\n\n{}",
            start,
            transcript.len() - MAX_TRANSCRIPT,
            end
        )
    } else {
        transcript
    }
}

/// Parse the LLM's JSON response into a CompactionSummary.
/// Handles cases where the model wraps JSON in markdown fences.
fn parse_summary_response(response: &str) -> Result<CompactionSummary> {
    let cleaned = response.trim();

    // Strip markdown code fences if present
    let json_str = if cleaned.starts_with("```") {
        let inner = cleaned
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        inner
    } else {
        cleaned
    };

    // Try to parse as CompactionSummary
    match serde_json::from_str::<CompactionSummary>(json_str) {
        Ok(summary) => Ok(summary),
        Err(e) => {
            // Fallback: create a minimal summary from the raw text
            Ok(CompactionSummary {
                goal: "Unable to parse structured summary".to_string(),
                repo_map: Vec::new(),
                work_completed: Vec::new(),
                current_state: format!(
                    "Summary parse error: {}. Raw: {}",
                    e,
                    &json_str[..json_str.len().min(500)]
                ),
                commands_run: Vec::new(),
                decisions: Vec::new(),
                next_actions: Vec::new(),
                pitfalls: vec!["Previous compaction summary failed to parse".to_string()],
            })
        }
    }
}

/// Check if compaction should be triggered based on context saturation.
/// Triggers when prompt tokens reach 100% of max context window.
/// The user is warned at 85% and encouraged to manually /compact before this.
pub fn should_compact(last_prompt_tokens: u32, max_context_tokens: usize) -> bool {
    if max_context_tokens == 0 {
        return false;
    }
    last_prompt_tokens >= max_context_tokens as u32
}

/// Emergency rolling window: drop oldest non-system messages until the estimated
/// token count is under the target. Uses the actual reported token count as the
/// starting point so it handles sudden jumps above the limit correctly.
/// Returns the number of messages dropped.
pub fn apply_rolling_window(
    history: &mut Vec<Message>,
    max_context_tokens: usize,
    actual_prompt_tokens: u32,
    tokens_per_message: u32,
) -> usize {
    let target_tokens = (max_context_tokens as f64 * 0.80) as usize;
    let mut dropped = 0;

    // Use server-reported token count as ground truth; fall back to char estimate
    let mut current_tokens = if actual_prompt_tokens > 0 {
        actual_prompt_tokens as usize
    } else {
        history
            .iter()
            .map(|m| m.content.as_ref().map(|c| c.len() / 4).unwrap_or(100))
            .sum()
    };

    let per_msg = tokens_per_message.max(50) as usize;

    loop {
        if current_tokens <= target_tokens || history.len() <= 2 {
            break;
        }
        if let Some(idx) = history.iter().position(|m| m.role != "system") {
            if history[idx].role != "tool" && conversational_anchor_count(history) <= 1 {
                break;
            }
            let end = context_unit_end(history, idx);
            let removed = end.saturating_sub(idx).max(1);
            history.drain(idx..end);
            current_tokens = current_tokens.saturating_sub(per_msg * removed);
            dropped += removed;
        } else {
            break;
        }
    }

    dropped
}

/// Retain the user-approved plan as non-droppable rolling-window context.
///
/// Unlike the old heuristic working-state anchor, this preserves only the plan
/// the model wrote and the user approved. Completed checklist/task lines are
/// pruned so the retained plan stays focused as the transcript rolls forward.
pub fn ensure_rolling_plan_context(
    history: &mut Vec<Message>,
    plan: &str,
    plan_completed_todo_index: Option<usize>,
) {
    let plan = prune_completed_plan_lines(plan, history);
    if plan.trim().is_empty() {
        remove_rolling_plan_context(history);
        return;
    }

    let completion_instruction = plan_completed_todo_index
        .map(|idx| {
            format!(
                "When every remaining plan task is complete, call todo_write to update todo index {idx} to done. \
                 That todo is named \"plan completed\" and marks this rolling-window session complete."
            )
        })
        .unwrap_or_else(|| {
            "When every remaining plan task is complete, mark the todo named \"plan completed\" as done. \
             That marks this rolling-window session complete."
                .to_string()
        });

    let state = format!(
        "{ROLLING_PLAN_MARKER}\n\
         Rolling-window continuity is driven by this approved plan. Follow the remaining tasks until complete or blocked. \
         Completed tasks may be omitted from this retained copy.\n\
         {completion_instruction}\n\n\
         Plan:\n{plan}"
    );

    if let Some(existing_idx) = history.iter().position(is_rolling_plan_context) {
        history[existing_idx] = Message::system(&state);
        return;
    }
    let insert_at = history
        .iter()
        .rposition(|msg| msg.role == "system")
        .map(|idx| idx + 1)
        .unwrap_or(0);
    history.insert(insert_at, Message::system(&state));
}

pub fn remove_rolling_plan_context(history: &mut Vec<Message>) {
    history.retain(|msg| !is_rolling_plan_context(msg));
}

pub fn extract_rolling_plan_context(history: &[Message]) -> Option<String> {
    let content = history
        .iter()
        .find(|msg| is_rolling_plan_context(msg))?
        .content
        .as_deref()?;
    content
        .split_once("\nPlan:\n")
        .map(|(_, plan)| plan.trim().to_string())
        .filter(|plan| !plan.is_empty())
}

fn is_rolling_plan_context(msg: &Message) -> bool {
    msg.role == "system"
        && msg
            .content
            .as_deref()
            .is_some_and(|content| content.starts_with(ROLLING_PLAN_MARKER))
}

fn prune_completed_plan_lines(plan: &str, history: &[Message]) -> String {
    let completed = completed_task_texts(history);
    plan.lines()
        .filter(|line| !is_completed_plan_line(line, &completed))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_completed_plan_line(line: &str, completed: &[String]) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("- [x]")
        || lower.starts_with("* [x]")
        || lower.starts_with("+ [x]")
        || lower.contains(" status: done")
        || lower.contains(" status: completed")
    {
        return true;
    }

    let normalized_line = normalize_task_text(trimmed);
    completed.iter().any(|task| {
        !task.is_empty() && normalized_line.len() >= task.len() && normalized_line.contains(task)
    })
}

fn completed_task_texts(history: &[Message]) -> Vec<String> {
    let mut completed = Vec::new();
    for msg in history {
        if msg.role != "tool" || msg.name.as_deref() != Some("todo_write") {
            continue;
        }
        let Some(content) = msg.content.as_deref() else {
            continue;
        };
        for line in content.lines() {
            if let Some(task) = completed_todo_line_text(line) {
                let task = normalize_task_text(&task);
                if !task.is_empty() && !completed.contains(&task) {
                    completed.push(task);
                }
            }
        }
    }
    completed
}

fn completed_todo_line_text(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    if !(lower.contains(" done") || lower.contains("done:") || lower.contains("→ done")) {
        return None;
    }
    if let Some((_, task)) = line.split_once("done:") {
        return Some(task.trim().to_string());
    }
    if let Some((task, _)) = line.split_once("→ done") {
        return Some(task.trim().to_string());
    }
    Some(line.trim().to_string())
}

fn normalize_task_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn conversational_anchor_count(history: &[Message]) -> usize {
    history
        .iter()
        .filter(|msg| msg.role != "system" && msg.role != "tool")
        .count()
}

fn valid_recent_window(non_system: &[&Message], max_messages: usize) -> Vec<Message> {
    let mut start = non_system.len().saturating_sub(max_messages);

    // Anthropic requires tool_result blocks to directly follow their matching
    // assistant tool_use blocks. If the rolling window starts in the middle of
    // that exchange, drop the orphaned tool results from the window.
    while start < non_system.len() && non_system[start].role == "tool" {
        start += 1;
    }

    non_system[start..]
        .iter()
        .map(|msg| (*msg).clone())
        .collect()
}

fn context_unit_end(history: &[Message], start: usize) -> usize {
    let mut end = start + 1;
    if history
        .get(start)
        .and_then(|msg| msg.tool_calls.as_ref())
        .is_some_and(|tool_calls| !tool_calls.is_empty())
    {
        while end < history.len() && history[end].role == "tool" {
            end += 1;
        }
    } else if history.get(start).is_some_and(|msg| msg.role == "tool") {
        while end < history.len() && history[end].role == "tool" {
            end += 1;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{FunctionCall, ToolCall};

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
    fn rolling_window_drops_tool_exchange_as_a_unit() {
        let mut history = vec![
            Message::system("system"),
            Message::assistant_with_tools(None, vec![tool_call("tool-1")]),
            Message::tool_result("tool-1", "search", "result"),
            Message::user("next"),
        ];

        let dropped = apply_rolling_window(&mut history, 100, 1_000, 100);

        assert_eq!(dropped, 2);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, "user");
    }

    #[test]
    fn rolling_window_preserves_last_user_anchor() {
        let mut history = vec![Message::system("system"), Message::user("current request")];

        let dropped = apply_rolling_window(&mut history, 100, 1_000, 100);

        assert_eq!(dropped, 0);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content.as_deref(), Some("current request"));
    }

    #[test]
    fn rolling_window_can_drop_leading_orphan_tools_before_last_user() {
        let mut history = vec![
            Message::system("system"),
            Message::tool_result("orphan", "search", "result"),
            Message::user("current request"),
        ];

        let dropped = apply_rolling_window(&mut history, 100, 1_000, 100);

        assert_eq!(dropped, 1);
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, "user");
    }

    #[test]
    fn valid_recent_window_skips_leading_orphan_tool_results() {
        let messages = [
            Message::assistant_with_tools(None, vec![tool_call("tool-1")]),
            Message::tool_result("tool-1", "search", "result"),
            Message::user("next"),
        ];
        let refs: Vec<&Message> = messages.iter().collect();

        let window = valid_recent_window(&refs, 2);

        assert_eq!(window.len(), 1);
        assert_eq!(window[0].role, "user");
    }

    #[test]
    fn rolling_plan_context_preserves_approved_plan_and_prunes_done_tasks() {
        let mut history = vec![
            Message::system("system"),
            Message::tool_result(
                "todo-2",
                "todo_write",
                "Updated todo [0] done: inspect state",
            ),
        ];
        let plan = "- [ ] Inspect state\n- [ ] Implement fix\n- [x] Update docs";

        ensure_rolling_plan_context(&mut history, plan, Some(3));

        let state = history
            .iter()
            .find(|msg| is_rolling_plan_context(msg))
            .and_then(|msg| msg.content.as_deref())
            .unwrap();
        assert!(state.contains(ROLLING_PLAN_MARKER));
        assert!(state.contains("todo index 3"));
        assert!(!state.contains("Inspect state"));
        assert!(state.contains("Implement fix"));
        assert!(!state.contains("Update docs"));
    }

    #[test]
    fn rolling_plan_context_is_replaced_not_duplicated() {
        let mut history = vec![Message::system("system")];

        ensure_rolling_plan_context(&mut history, "- [ ] First", None);
        ensure_rolling_plan_context(&mut history, "- [ ] Second", None);

        let states: Vec<_> = history
            .iter()
            .filter(|msg| is_rolling_plan_context(msg))
            .collect();
        assert_eq!(states.len(), 1);
        assert!(states[0].content.as_deref().unwrap().contains("Second"));
        assert!(!states[0].content.as_deref().unwrap().contains("First"));
    }
}
