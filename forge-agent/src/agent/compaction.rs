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
    // The window is whatever came after the system prompt and the summary.
    let rolling_window = messages_after.saturating_sub(2);

    // Write compaction_commit marker
    log.log_compaction_commit(messages_after, rolling_window)?;

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

    let chunks = split_transcript(&transcript, CHUNK_CHARS, MAX_CHUNKS);
    if chunks.len() > 1 {
        return summarize_in_chunks(client, model_id, &chunks).await;
    }

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

/// Summarize a transcript too large for one call: summarize each piece, then
/// merge the pieces into one summary.
///
/// Every chunk is seen by the model. That is the whole point — the previous
/// implementation made exactly one call and simply deleted whatever did not
/// fit, so a decision taken in the middle of a session was not condensed, it
/// was gone, and the model was left to infer the gap. Observed inventing a
/// range of results it had never been shown.
async fn summarize_in_chunks(
    client: &ApiClient,
    model_id: &str,
    chunks: &[String],
) -> Result<CompactionSummary> {
    let total = chunks.len();
    let mut parts: Vec<CompactionSummary> = Vec::new();
    let mut failed = 0usize;

    for (i, chunk) in chunks.iter().enumerate() {
        let prompt = format!(
            r#"You are summarizing part {} of {} of a coding session transcript, in order.
This part will be merged with the others, so summarize only what is in it and do
not speculate about what came before or after.

Preserve verbatim, wherever they appear: decisions and the reasoning behind
them, constraints and rules the user stated, exact file paths, identifiers,
commands, and numeric results. These are what the agent will have left.

The summary MUST be valid JSON with exactly these fields:
{{
  "goal": "what the user is trying to accomplish, as far as this part shows",
  "repo_map": ["key files and what they do"],
  "work_completed": ["completed work items"],
  "current_state": "what is working, what is failing",
  "commands_run": ["commands and their outcomes"],
  "decisions": ["decisions made, and why"],
  "next_actions": ["what should happen next"],
  "pitfalls": ["things to avoid or known issues"]
}}

Respond with ONLY the JSON object, no markdown fences, no explanation.

TRANSCRIPT PART {} OF {}:
{}"#,
            i + 1,
            total,
            i + 1,
            total,
            chunk
        );
        match client.chat_simple(model_id, &prompt).await {
            Ok(response) => match parse_summary_response(&response) {
                Ok(summary) => parts.push(summary),
                Err(_) => failed += 1,
            },
            // One bad chunk must not lose the rest: a failure here costs the
            // detail of one part, where returning early would cost the whole
            // conversation and leave the caller running unsummarized.
            Err(_) => failed += 1,
        }
    }

    if parts.is_empty() {
        anyhow::bail!("every summarizer call failed ({total} parts)");
    }
    let mut merged = merge_summaries(parts);
    if failed > 0 {
        merged.pitfalls.push(format!(
            "{failed} of {total} parts of this conversation could not be summarized; \
             detail from those parts is missing"
        ));
    }
    Ok(merged)
}

/// Fold per-chunk summaries into one, in order.
///
/// Deliberately mechanical rather than another model call: the list fields are
/// the facts worth keeping, and a second pass over them is another chance to
/// drop one. Duplicates are removed because chunks overlap in subject matter,
/// not because either copy is wrong.
fn merge_summaries(parts: Vec<CompactionSummary>) -> CompactionSummary {
    fn dedup(items: Vec<String>) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        items
            .into_iter()
            .filter(|s| !s.trim().is_empty() && seen.insert(s.trim().to_lowercase()))
            .collect()
    }

    let goal = parts
        .iter()
        .map(|p| p.goal.trim())
        .find(|g| !g.is_empty())
        .unwrap_or_default()
        .to_string();
    // The last part that said anything: state is the one field where the newest
    // account supersedes the older ones rather than adding to them.
    let current_state = parts
        .iter()
        .rev()
        .map(|p| p.current_state.trim())
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string();
    // Likewise next actions: what to do next is whatever was outstanding at the
    // end, not everything that was ever outstanding.
    let next_actions = parts
        .iter()
        .rev()
        .find(|p| !p.next_actions.is_empty())
        .map(|p| p.next_actions.clone())
        .unwrap_or_default();

    CompactionSummary {
        goal,
        repo_map: dedup(parts.iter().flat_map(|p| p.repo_map.clone()).collect()),
        work_completed: dedup(parts.iter().flat_map(|p| p.work_completed.clone()).collect()),
        current_state,
        commands_run: dedup(parts.iter().flat_map(|p| p.commands_run.clone()).collect()),
        decisions: dedup(parts.iter().flat_map(|p| p.decisions.clone()).collect()),
        next_actions: dedup(next_actions),
        pitfalls: dedup(parts.iter().flat_map(|p| p.pitfalls.clone()).collect()),
    }
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

    // Returned whole. This used to keep only the first and last 6,000
    // characters and delete everything between, which for a long session meant
    // the summarizer saw well under 1% of the conversation — measured at
    // 12,000 characters out of 1,750,130 on a real 44 MB session. Anything
    // decided in the middle was not summarized, it was discarded, and the
    // summary that replaced the conversation was written by a model that had
    // never seen it. Splitting the work is `split_transcript`'s job now.
    transcript
}

/// Characters of transcript per summarizer call.
///
/// Each chunk plus the prompt around it has to fit comfortably in the
/// summarizer's own context — that call is made when the conversation is
/// already near the limit, so it cannot be generous. At roughly four
/// characters per token this is about 30k tokens of transcript per call.
const CHUNK_CHARS: usize = 120_000;

/// Most summarizer calls one compaction will make.
///
/// A bound is needed or a huge session would fan out into an unbounded number
/// of requests. When it is hit the chunks are made bigger rather than dropped,
/// so coverage stays complete and only resolution suffers — the opposite of
/// the previous behaviour, which kept full resolution over 0.7% of the
/// material and silently lost the rest.
const MAX_CHUNKS: usize = 12;

/// Split a transcript into summarizable pieces, on line boundaries so a
/// message is never cut in half between chunks.
fn split_transcript(transcript: &str, chunk_chars: usize, max_chunks: usize) -> Vec<String> {
    if transcript.is_empty() {
        return Vec::new();
    }
    // Grow the chunk rather than drop material once the call budget is reached.
    let needed = transcript.len().div_ceil(chunk_chars.max(1));
    let size = if needed > max_chunks {
        // Plus the longest line: chunks are packed on line boundaries, so each
        // one stops a little short of the target and an even division would
        // spill into one extra chunk.
        let longest = transcript.lines().map(|l| l.len() + 1).max().unwrap_or(0);
        transcript.len().div_ceil(max_chunks.max(1)) + longest
    } else {
        chunk_chars
    };

    let mut out = Vec::new();
    let mut current = String::new();
    for line in transcript.lines() {
        // A single line longer than the chunk still goes in on its own rather
        // than being split: the per-message caps above already bound it.
        if !current.is_empty() && current.len() + line.len() + 1 > size {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
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
/// Whether the conversation should be compacted before the next request.
///
/// `at_percent` is a fraction of the context window, not a ceiling. This used
/// to be `>= max_context_tokens`, i.e. 100%, which is an overflow rather than
/// a threshold: compaction only ran after a request had already been built
/// oversized, and the summarizer call that follows — which needs context of
/// its own — was made at the exact moment there was none left.
pub fn should_compact(last_prompt_tokens: u32, max_context_tokens: usize, at_percent: u8) -> bool {
    if max_context_tokens == 0 {
        return false;
    }
    // A nonsensical setting should not disable compaction entirely, so it is
    // clamped rather than trusted: 0 would compact on every turn, and anything
    // over 100 restores the overflow behaviour this replaced.
    let pct = at_percent.clamp(10, 95) as u64;
    let limit = (max_context_tokens as u64 * pct) / 100;
    last_prompt_tokens as u64 >= limit
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

    // A message with no text still costs something on the wire (a tool call is
    // its name and arguments), so an absent body is charged a nominal size
    // rather than nothing.
    let msg_chars = |m: &Message| m.content.as_ref().map_or(400, |c| c.len());
    let total_chars: usize = history.iter().map(&msg_chars).sum();

    // Use server-reported token count as ground truth; fall back to char estimate
    let mut current_tokens = if actual_prompt_tokens > 0 {
        actual_prompt_tokens as usize
    } else {
        total_chars / 4
    };

    // What one dropped message is worth, in the same units as `current_tokens`.
    //
    // This used to subtract `tokens_per_message` — the *marginal* cost measured
    // across the last two turns — from a total that is the server's real count.
    // Those are not the same measure. A session that has just exchanged a few
    // short messages reports a small marginal cost, while the messages at the
    // front of the history, which are the ones dropped, are the large ones:
    // tool results and file dumps. Shedding 20k tokens then looked like it
    // needed hundreds of drops, and the loop emptied the transcript instead of
    // trimming it — observed as context falling from about 90% to about 11% in
    // one step, with the agent left no memory of what it had been doing.
    //
    // So each message is charged its own size, converted at the ratio the real
    // total implies, with the marginal figure kept only as a floor.
    let per_msg_floor = tokens_per_message.max(50) as usize;
    let tokens_per_char = if total_chars > 0 && actual_prompt_tokens > 0 {
        actual_prompt_tokens as f64 / total_chars as f64
    } else {
        0.25 // the usual English rule of thumb, when there is nothing better
    };
    let cost_of = |m: &Message| {
        ((msg_chars(m) as f64 * tokens_per_char).ceil() as usize).max(per_msg_floor)
    };

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
            let cost: usize = history[idx..end].iter().map(&cost_of).sum();
            history.drain(idx..end);
            current_tokens = current_tokens.saturating_sub(cost);
            dropped += removed;
        } else {
            break;
        }
    }

    dropped
}

/// Shorten a single tool result to `max_tokens`, keeping both ends.
///
/// Applied as the result enters the conversation, so no one call can put the
/// history over the window on its own. Returns the text unchanged when it
/// already fits, which is the overwhelming majority of results.
pub fn clamp_tool_result(tool_name: &str, result: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if max_bytes == 0 || result.len() <= max_bytes {
        return result.to_string();
    }
    // The notice counts against the budget too, or the result comes back over
    // the cap it was supposed to be brought under.
    let notice = format!(
        "[{tool_name} returned {} bytes; shortened to fit the context window. \
         Read a specific range if you need the rest.]\n",
        result.len()
    );
    let room = max_bytes.saturating_sub(notice.len());
    format!("{notice}{}", elide_middle(result, room))
}

/// What `fit_history_to_window` had to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FitReport {
    /// Messages removed from the front.
    pub dropped: usize,
    /// Messages whose content was shortened because dropping was not enough.
    pub truncated: usize,
}

impl FitReport {
    /// Whether the history is any smaller than it was. A retry is only worth
    /// making if something actually came off.
    pub fn changed(&self) -> bool {
        self.dropped > 0 || self.truncated > 0
    }
}

/// Roughly what a message costs, from its own text.
///
/// Deliberately independent of anything the server reported. This is used when
/// the server's number cannot be trusted — either because the request it
/// described was rejected, or because history has grown since — so taking that
/// number as the starting point is the mistake being corrected.
fn estimated_tokens(msg: &Message) -> usize {
    let content = msg.content.as_ref().map_or(0, |c| c.len());
    let calls = msg.tool_calls.as_ref().map_or(0, |cs| {
        cs.iter()
            .map(|tc| tc.function.name.len() + tc.function.arguments.len() + 32)
            .sum::<usize>()
    });
    // Four characters to a token, and never free: an empty tool message still
    // costs its role and id on the wire.
    ((content + calls) / 4).max(8)
}

/// The whole history's estimated cost.
pub fn estimate_history_tokens(history: &[Message]) -> usize {
    history.iter().map(estimated_tokens).sum()
}

/// Bring `history` down to `budget_tokens`, however far over it is.
///
/// Oldest context units go first, so what survives is the most recent
/// conversation — the part still being worked on. When dropping is not enough
/// on its own, the largest message left is shortened rather than the history
/// emptied: a single tool result can be many times the whole window (a
/// `read_file` on a large file has no cap), and no amount of dropping fixes
/// that when the oversized message is also the newest one.
///
/// Sized from the history's own text rather than from a reported token count.
/// The emergency path used to pass the count from the last *successful*
/// request, which is by definition the size before the thing that overflowed:
/// at 14x the window it read as comfortably under budget and shed nothing at
/// all, so the turn failed and the oversized message stayed in history, and
/// every turn after it failed the same way.
pub fn fit_history_to_window(history: &mut Vec<Message>, budget_tokens: usize) -> FitReport {
    let mut report = FitReport::default();
    if budget_tokens == 0 {
        return report;
    }

    // Drop oldest whole units while there is something droppable left.
    while estimate_history_tokens(history) > budget_tokens {
        let Some(idx) = history.iter().position(|m| m.role != "system") else {
            break;
        };
        // Keep the last user turn: an agent that cannot see what it was asked
        // is worse than one with a shortened transcript.
        if history[idx].role != "tool" && conversational_anchor_count(history) <= 1 {
            break;
        }
        let end = context_unit_end(history, idx);
        report.dropped += end.saturating_sub(idx).max(1);
        history.drain(idx..end);
    }

    // Still over: shorten the biggest thing left, repeatedly, since a history
    // can hold more than one oversized message.
    while estimate_history_tokens(history) > budget_tokens {
        let Some((idx, _)) = history
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role != "system" && m.content.is_some())
            .max_by_key(|(_, m)| estimated_tokens(m))
        else {
            break;
        };

        let others: usize = history
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, m)| estimated_tokens(m))
            .sum();
        let allowance = budget_tokens.saturating_sub(others);
        let before = estimated_tokens(&history[idx]);
        let content = history[idx].content.as_ref().expect("filtered on is_some");
        let shortened = elide_middle(content, allowance.saturating_mul(4));
        if shortened.len() >= content.len() {
            // Nothing more to give: the fixed costs alone exceed the budget.
            break;
        }
        history[idx].content = Some(shortened);
        report.truncated += 1;
        if estimated_tokens(&history[idx]) >= before {
            break;
        }
    }

    report
}

/// Shorten `text` to about `max_bytes`, keeping both ends.
///
/// The head and the tail are both worth keeping and for different reasons: a
/// file or a diff identifies itself at the top, while a command's verdict —
/// the error, the test count — is at the bottom. Cutting from one end only
/// would reliably lose one of the two. What was removed is said plainly, so
/// the model treats what it has as a fragment rather than the whole.
fn elide_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker_room = 96;
    let keep = max_bytes.saturating_sub(marker_room);
    if keep < 200 {
        // No room to keep anything useful from both ends; say so and stop.
        return format!("[{} bytes of output omitted: no room left in context]", text.len());
    }
    let head_len = keep * 2 / 5;
    let tail_len = keep - head_len;
    let head_end = floor_char_boundary(text, head_len);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_len));
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}\n\n[... {} bytes omitted to fit the context window ...]\n\n{}",
        &text[..head_end],
        omitted,
        &text[tail_start..]
    )
}

/// `str::floor_char_boundary` is unstable, and slicing a multi-byte character
/// in half panics.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Retain the user-approved plan as non-droppable rolling-window context.
///
/// Unlike the old heuristic working-state anchor, this preserves only the plan
/// the model wrote and the user approved. Completed checklist/task lines are
/// pruned so the retained plan stays focused as the transcript rolls forward.
pub fn ensure_rolling_plan_context(history: &mut Vec<Message>, plan: &str) {
    let plan = prune_completed_plan_lines(plan, history);
    if plan.trim().is_empty() {
        remove_rolling_plan_context(history);
        return;
    }

    // Named, not numbered. A todo is identified by its own text now, so there
    // is one way to refer to it and it is the way a person would.
    let completion_instruction =
        "When every remaining plan task is complete, mark the todo named \"plan completed\" as done. \
         That marks this rolling-window session complete.";

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

    /// Build a history a given percentage over the window.
    fn oversized_history(window: usize, percent: usize) -> Vec<Message> {
        // Enough conversation to be worth keeping some of, plus one tool result
        // large enough to put the whole thing `percent` of the way over.
        let mut history = vec![Message::system("sys")];
        for i in 0..30 {
            history.push(Message::user(&format!("turn {i}")));
            history.push(Message::assistant(&format!("reply {i}")));
        }
        history.push(Message::user("read the big file and tell me what it does"));
        let target_tokens = window * percent / 100;
        let have: usize = estimate_history_tokens(&history);
        let need_chars = target_tokens.saturating_sub(have) * 4;
        history.push(Message::tool_result("t1", "read_file", &"x".repeat(need_chars)));
        history
    }

    /// An ordinary result is passed through untouched — the cap must not tax
    /// the common case.
    #[test]
    fn a_normal_tool_result_is_not_touched() {
        let body = "File: src/lib.rs (240 lines total)\n".to_string() + &"line\n".repeat(240);
        assert_eq!(clamp_tool_result("read_file", &body, 50_000), body);
    }

    /// A whole-file read of something enormous is bounded before it can put the
    /// conversation over the window.
    #[test]
    fn an_enormous_tool_result_is_capped_on_the_way_in() {
        let window = 200_000usize;
        let cap = window / 4;
        let body = format!("FIRST\n{}\nLAST", "junk\n".repeat(2_000_000));
        let out = clamp_tool_result("read_file", &body, cap);

        assert!(out.len() < body.len() / 10, "barely shortened: {} bytes", out.len());
        assert!(estimate_history_tokens(&[Message::tool_result("t", "read_file", &out)]) <= cap);
        // The model is told, so it does not mistake a fragment for the file.
        assert!(out.contains("shortened to fit the context window"));
        assert!(out.contains("Read a specific range"));
        assert!(out.contains("FIRST") && out.contains("LAST"), "both ends kept");
    }

    /// Why the size has to come from the history and not from the last bill.
    ///
    /// `apply_rolling_window` believes the token count it is given. Handed the
    /// figure from the last *successful* request — which describes the
    /// conversation before the thing that overflowed it — it decides a history
    /// 14x over the window is comfortably under budget and sheds nothing. That
    /// is what left a session unable to make any request at all: the turn
    /// failed, the oversized message stayed, and every turn after it failed the
    /// same way.
    #[test]
    fn a_stale_token_count_sheds_nothing() {
        let window = 200_000usize;

        let mut history = oversized_history(window, 1432);
        let stale = (window / 4) as u32; // last good request: a quarter full
        let dropped = apply_rolling_window(&mut history, window, stale, 500);
        assert_eq!(dropped, 0, "the stale figure is supposed to look harmless");
        assert!(
            estimate_history_tokens(&history) > window,
            "history should still be over the window"
        );

        // The same history, sized from its own text, comes back under.
        let mut history = oversized_history(window, 1432);
        let report = fit_history_to_window(&mut history, (window as f64 * 0.80) as usize);
        assert!(report.changed());
        assert!(estimate_history_tokens(&history) <= (window as f64 * 0.80) as usize);
    }

    /// The case this was written for: a conversation 1432% of the window.
    ///
    /// Dropping messages cannot fix it on its own — the oversized message is
    /// the newest one — so the largest remaining message is shortened until the
    /// history fits.
    #[test]
    fn a_history_far_over_the_window_is_brought_back_under_it() {
        let window = 200_000usize;
        let mut history = oversized_history(window, 1432);
        let before = estimate_history_tokens(&history);
        assert!(before > window * 14, "fixture is only {}% of the window", before * 100 / window);

        let budget = (window as f64 * 0.80) as usize;
        let report = fit_history_to_window(&mut history, budget);

        assert!(report.changed(), "nothing was shed from a history 14x over");
        let after = estimate_history_tokens(&history);
        assert!(after <= budget, "still {after} tokens against a budget of {budget}");
        // The conversation survives: this is a trim, not a reset.
        assert!(history.len() > 2, "history was emptied: {} left", history.len());
        assert_eq!(history[0].role, "system", "the system prompt must be kept");
        assert!(
            history.iter().any(|m| m.role == "user"),
            "no user turn left — the agent cannot see what it was asked"
        );
    }

    /// What survives is the *recent* conversation, since that is what is still
    /// being worked on.
    #[test]
    fn the_newest_turns_are_the_ones_kept() {
        let window = 100_000usize;
        let mut history = oversized_history(window, 400);
        fit_history_to_window(&mut history, (window as f64 * 0.80) as usize);

        let text: String = history
            .iter()
            .filter_map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("read the big file"), "the latest request was dropped");
        assert!(!text.contains("turn 0"), "the oldest turn was kept: {}", &text[..80.min(text.len())]);
    }

    /// A single message bigger than the whole window is the shape that used to
    /// wedge a session permanently, because there was nothing left to drop.
    #[test]
    fn one_message_larger_than_the_window_is_shortened() {
        let window = 50_000usize;
        let mut history = vec![
            Message::system("sys"),
            Message::user("read it"),
            Message::tool_result("t1", "read_file", &"y".repeat(window * 4 * 8)),
        ];
        let budget = (window as f64 * 0.80) as usize;
        let report = fit_history_to_window(&mut history, budget);

        assert_eq!(report.truncated, 1, "the oversized message was not shortened");
        assert!(estimate_history_tokens(&history) <= budget);
        let kept = history.last().unwrap().content.as_ref().unwrap();
        assert!(kept.contains("omitted to fit the context window"), "the cut is not declared");
    }

    /// Both ends are kept: a file says what it is at the top, a command run
    /// says whether it passed at the bottom.
    #[test]
    fn shortening_keeps_the_head_and_the_tail() {
        let body = format!("FIRST LINE\n{}\nLAST LINE", "middle\n".repeat(50_000));
        let out = elide_middle(&body, 4_000);
        assert!(out.starts_with("FIRST LINE"), "head lost");
        assert!(out.ends_with("LAST LINE"), "tail lost");
        assert!(out.len() <= 4_000, "still {} bytes", out.len());
    }

    /// A history that already fits is left exactly as it is.
    #[test]
    fn a_history_within_budget_is_untouched() {
        let mut history = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let before = history.clone();
        let report = fit_history_to_window(&mut history, 100_000);
        assert!(!report.changed());
        assert_eq!(history.len(), before.len());
        assert_eq!(history[1].content, before[1].content);
    }

    /// Multi-byte text must not be sliced through a character.
    #[test]
    fn shortening_does_not_split_a_character() {
        let body = "日本語のテキスト".repeat(5_000);
        let out = elide_middle(&body, 1_000);
        assert!(out.len() <= 1_000);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
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

    /// A real session lost almost its whole transcript at once: context usage
    /// fell from about 90% to about 11% in a single step and the agent had no
    /// memory of what it was doing.
    ///
    /// The cause is a unit mismatch. `current_tokens` starts as the server's
    /// real prompt total, but each dropped message subtracts
    /// `tokens_per_message` — a *marginal* figure measured from the last two
    /// turns. A conversation that has just exchanged a few short messages
    /// reports a small marginal cost, while the messages at the front of the
    /// history are the large ones (tool results, file dumps). Shedding 20k
    /// tokens then looks like it needs hundreds of drops, so the loop runs
    /// until the history is empty rather than until the budget is met.
    #[test]
    fn rolling_window_stops_when_the_budget_is_met_not_when_history_runs_out() {
        // 200k window, sitting at 90% — needs to shed roughly 20k to reach 80%.
        let max_context = 200_000;
        let actual = 180_000u32;

        // Twenty turns, each with a large tool result: dropping one or two
        // units is enough to get under the target.
        let big = "x".repeat(40_000); // ~10k tokens
        let mut history = vec![Message::system("system")];
        for i in 0..20 {
            history.push(Message::assistant_with_tools(None, vec![tool_call(&format!("t{i}"))]));
            history.push(Message::tool_result(&format!("t{i}"), "read_file", &big));
        }
        history.push(Message::user("carry on with the plan"));
        let before = history.len();

        // The recent marginal cost: two short messages were just exchanged.
        let tokens_per_message = 50;
        let dropped = apply_rolling_window(&mut history, max_context, actual, tokens_per_message);

        // Each turn is worth roughly 10k tokens, so shedding 20k should cost
        // two or three of them — not the whole conversation.
        assert!(
            dropped <= 8,
            "dropped {dropped} of {before} messages, leaving {} — the transcript was \
             emptied rather than trimmed",
            history.len()
        );
        // And it has to actually trim: leaving the prompt over budget would
        // just move the failure to the provider.
        assert!(dropped >= 2, "dropped only {dropped}, which cannot have freed 20k tokens");
        assert!(
            history.iter().any(|m| m.role == "user"),
            "the pending request itself was dropped"
        );
        assert_eq!(history[0].role, "system", "the system prompt must survive");
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

        ensure_rolling_plan_context(&mut history, plan);

        let state = history
            .iter()
            .find(|msg| is_rolling_plan_context(msg))
            .and_then(|msg| msg.content.as_deref())
            .unwrap();
        assert!(state.contains(ROLLING_PLAN_MARKER));
        // Named, not numbered: the todo is identified by its own text now.
        assert!(state.contains("\"plan completed\""), "got {state}");
        assert!(!state.contains("Inspect state"));
        assert!(state.contains("Implement fix"));
        assert!(!state.contains("Update docs"));
    }

    #[test]
    fn rolling_plan_context_is_replaced_not_duplicated() {
        let mut history = vec![Message::system("system")];

        ensure_rolling_plan_context(&mut history, "- [ ] First");
        ensure_rolling_plan_context(&mut history, "- [ ] Second");

        let states: Vec<_> = history
            .iter()
            .filter(|msg| is_rolling_plan_context(msg))
            .collect();
        assert_eq!(states.len(), 1);
        assert!(states[0].content.as_deref().unwrap().contains("Second"));
        assert!(!states[0].content.as_deref().unwrap().contains("First"));
    }

    // ── When compaction triggers ──────────────────────────────────────────

    #[test]
    fn compaction_happens_with_headroom_left() {
        // The point of a threshold: fire while a summarizer call still fits.
        assert!(should_compact(80_000, 100_000, 80), "at the threshold");
        assert!(should_compact(95_000, 100_000, 80), "past it");
        assert!(!should_compact(79_999, 100_000, 80), "below it");
    }

    #[test]
    fn the_old_behaviour_was_an_overflow_not_a_threshold() {
        // 79% of a 500k window is nearly 400k tokens of conversation that the
        // previous `>= max` test would have let through untouched.
        assert!(should_compact(400_000, 500_000, 80));
        assert!(!should_compact(400_000, 500_000, 100), "what it used to do");
    }

    #[test]
    fn a_nonsensical_percentage_cannot_disable_or_thrash_compaction() {
        // 0 would compact every turn; over 100 restores the overflow.
        assert!(!should_compact(0, 100_000, 0), "clamped up, not compacting at zero");
        assert!(should_compact(95_000, 100_000, 250), "clamped down to 95%");
    }

    #[test]
    fn an_unknown_context_window_never_triggers_compaction() {
        // Some endpoints report nothing; guessing would compact constantly.
        assert!(!should_compact(1_000_000, 0, 80));
    }

    // ── Splitting a long transcript instead of deleting its middle ────────

    fn summary_with(decisions: &[&str], state: &str) -> CompactionSummary {
        CompactionSummary {
            goal: "port the study".into(),
            repo_map: vec![],
            work_completed: vec![],
            current_state: state.into(),
            commands_run: vec![],
            decisions: decisions.iter().map(|s| s.to_string()).collect(),
            next_actions: vec![],
            pitfalls: vec![],
        }
    }

    #[test]
    fn every_line_of_a_long_transcript_ends_up_in_some_chunk() {
        // The property the old code broke: it kept the head and tail and
        // deleted the middle, so most lines reached the summarizer nowhere.
        let transcript: String = (0..500).map(|i| format!("[USER]: line {i}\n")).collect();
        let chunks = split_transcript(&transcript, 1_000, 12);
        assert!(chunks.len() > 1, "this should have been split");
        let rejoined: String = chunks.concat();
        for i in 0..500 {
            assert!(rejoined.contains(&format!("line {i}\n")), "line {i} was lost");
        }
    }

    #[test]
    fn a_message_is_never_cut_in_half_between_chunks() {
        let transcript: String = (0..200).map(|i| format!("[USER]: message {i}\n")).collect();
        for chunk in split_transcript(&transcript, 300, 12) {
            for line in chunk.lines() {
                assert!(line.starts_with("[USER]: message"), "split line: {line:?}");
            }
        }
    }

    #[test]
    fn a_huge_transcript_grows_its_chunks_rather_than_dropping_material() {
        // Coverage is what matters; resolution is what gives. The bound is on
        // how many calls are made, never on how much is seen.
        let transcript: String = (0..20_000).map(|i| format!("[USER]: line {i}\n")).collect();
        let chunks = split_transcript(&transcript, 1_000, 12);
        assert!(chunks.len() <= 12, "made {} calls", chunks.len());
        assert_eq!(chunks.concat().len(), transcript.len(), "material was dropped");
    }

    #[test]
    fn a_short_transcript_is_left_as_one_piece() {
        // One call, exactly as before — the chunked path is for long sessions.
        let chunks = split_transcript("[USER]: hello\n", 1_000, 12);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn merging_keeps_a_decision_from_every_part() {
        // The failure this whole change is about: a decision taken in the
        // middle of a session must survive into the merged summary.
        let merged = merge_summaries(vec![
            summary_with(&["no third-party crates"], "starting"),
            summary_with(&["chose a hash index over a B-tree"], "midway"),
            summary_with(&["cap the frame at 6 MB"], "finished"),
        ]);
        assert!(merged.decisions.iter().any(|d| d.contains("hash index")), "{:?}", merged.decisions);
        assert_eq!(merged.decisions.len(), 3);
    }

    #[test]
    fn merging_takes_the_latest_account_of_the_current_state() {
        // State supersedes; a list of every state the session passed through
        // would describe the past as if it were the present.
        let merged = merge_summaries(vec![
            summary_with(&[], "tests failing"),
            summary_with(&[], "tests passing"),
        ]);
        assert_eq!(merged.current_state, "tests passing");
    }

    #[test]
    fn merging_drops_repeats_between_parts() {
        // Chunks overlap in subject matter; the same rule restated is noise.
        let merged = merge_summaries(vec![
            summary_with(&["No third-party crates"], "a"),
            summary_with(&["no third-party crates"], "b"),
        ]);
        assert_eq!(merged.decisions.len(), 1);
    }
}
