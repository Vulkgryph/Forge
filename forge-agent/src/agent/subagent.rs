// SPDX-License-Identifier: Apache-2.0
use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use tokio::sync::mpsc;

use super::agent_def::{AgentDefinition, AgentModel};
use super::core::{AgentEvent, ToolKindEvent, UserAction};
use crate::api::{ApiClient, Message};
use crate::config::AppConfig;
use crate::tools::{delegate_task_definition, ToolExecutor, ToolKind};

const WRAP_UP_REMAINING_RATIO: f64 = 0.30;
const FINAL_SUMMARY_REMAINING_RATIO: f64 = 0.20;

#[derive(Debug, Clone)]
#[allow(dead_code)] // status payloads are emitted but parent loop currently only matches Tool* variants
pub enum SubagentEvent {
    ToolRunning {
        tool_name: String,
        args_summary: String,
    },
    ToolDone {
        tool_name: String,
        success: bool,
        result_summary: String,
    },
    Message(String),
    Finished {
        summary: String,
    },
    Error(String),
}

fn normalized_model_override(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "default" | "inherit" | "inherited" | "current" | "auto"
        )
    {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub struct SubagentRunner {
    agent_def: AgentDefinition,
    client: ApiClient,
    model_id: String,
    max_context_tokens: usize,
    project_root: PathBuf,
    prompt: String,
    depth: usize,
    max_depth: usize,
    status_tx: mpsc::UnboundedSender<SubagentEvent>,
    agent_definitions: Vec<AgentDefinition>,
    app_config: AppConfig,
    parent_event_tx: mpsc::UnboundedSender<AgentEvent>,
    approval_rx: Option<mpsc::UnboundedReceiver<UserAction>>,
    /// Slot ID for parallel subagent tracking. Nested subagents inherit the parent's slot_id.
    slot_id: String,

    // Anti-thrash: detect identical errors repeated without progress
    last_error_sig: Option<u64>,
    same_error_count: u32,
    /// When true, Write tools are blocked until the agent produces a diagnosis.
    analyze_mode: bool,
}

impl SubagentRunner {
    pub fn new(
        agent_def: AgentDefinition,
        client: ApiClient,
        model_id: String,
        max_context_tokens: usize,
        project_root: PathBuf,
        prompt: String,
        depth: usize,
        max_depth: usize,
        status_tx: mpsc::UnboundedSender<SubagentEvent>,
        agent_definitions: Vec<AgentDefinition>,
        app_config: AppConfig,
        parent_event_tx: mpsc::UnboundedSender<AgentEvent>,
        approval_rx: Option<mpsc::UnboundedReceiver<UserAction>>,
        slot_id: String,
    ) -> Self {
        Self {
            agent_def,
            client,
            model_id,
            max_context_tokens,
            project_root,
            prompt,
            depth,
            max_depth,
            status_tx,
            agent_definitions,
            app_config,
            parent_event_tx,
            approval_rx,
            slot_id,
            last_error_sig: None,
            same_error_count: 0,
            analyze_mode: false,
        }
    }

    /// Public entry point — owns the receiver, called by tokio::spawn from Agent
    pub async fn run(mut self) -> Result<String> {
        let mut approval_rx = self
            .approval_rx
            .take()
            .expect("top-level SubagentRunner must have approval_rx");
        let (summary, _history, _turns) = self.run_inner(&mut approval_rx, None).await?;
        Ok(summary)
    }

    /// Shared tool loop — borrows the receiver, used by both top-level and nested.
    /// Returns (summary_text, full_message_history, turn_count).
    ///
    /// When `existing_history` is `Some`, the loop resumes from that history
    /// instead of starting fresh (used for retries with persistent context).
    pub(crate) async fn run_inner(
        &mut self,
        approval_rx: &mut mpsc::UnboundedReceiver<UserAction>,
        existing_history: Option<Vec<Message>>,
    ) -> Result<(String, Vec<Message>, usize)> {
        let executor = ToolExecutor::new(self.project_root.clone());

        // Build filtered tool list
        let mut tools = executor.tool_definitions_filtered(&self.agent_def.tools);

        // Include delegate_task if nesting depth allows
        if self.depth < self.max_depth {
            tools.push(delegate_task_definition(&self.agent_definitions));
        }

        // Build system prompt with workspace context
        let delegate_hint = if self.depth < self.max_depth {
            "\n             - You can use delegate_task to spawn nested subagents for subtasks that benefit from \
               a separate context window. Each nested subagent gets its own tools and context."
        } else {
            ""
        };
        let system_prompt = format!(
            "{}\n\nWorkspace: {}\n\n\
             IMPORTANT INSTRUCTIONS:\n\
             - You are running as a subagent with a dedicated context window.\n\
             - Complete your task thoroughly using the tools available to you.\n\
             - Use read_file to view file contents. Do NOT use shell_exec with cat, sed, awk, grep, less, or similar commands to inspect files when read_file/search_code can do it; shell output is not retained in file memory.\n\
             - NEVER use shell_exec (shell commands) to create, write, or modify files. Do NOT use cat, echo, sed, awk, tee, printf, or heredocs to write file content. Always use the dedicated file tools: apply_patch for changes (preferred), edit_file for small edits, write_file for new files. shell_exec is ONLY for non-file tasks like building, testing, linting, running scripts, and system commands.{}\n\
             - When you have gathered enough information or completed your work, you MUST stop calling tools \
               and write a comprehensive final summary of everything you found or did.\n\
             - Your final message (with no tool calls) is what gets returned to the parent agent, so make it \
               complete and detailed. Do not just say what you plan to do — report what you actually found.\n\
             - Never end with just a plan or intention. Always end with results.",
            self.agent_def.system_prompt,
            self.project_root.display(),
            delegate_hint,
        );

        // Init history — resume from existing if provided, else start fresh
        let mut history = match existing_history {
            Some(h) => h,
            None => vec![Message::system(&system_prompt), Message::user(&self.prompt)],
        };

        let mut turns: usize = 0;
        let mut warned_context = false;
        let mut forced_final_summary = false;
        let mut final_text = String::new(); // only the last text message (the summary)
        let mut last_prompt_tokens: u32 = 0;
        let mut toolless_intent_retries: usize = 0;
        let mut tool_observations: Vec<String> = Vec::new();
        let max_turns = self.agent_def.max_turns.filter(|max| *max > 0);

        loop {
            let estimated_prompt_tokens =
                last_prompt_tokens.max(estimate_prompt_tokens_from_history(&history));
            let remaining_ratio =
                context_remaining_ratio(estimated_prompt_tokens, self.max_context_tokens);

            if !forced_final_summary && max_turns.is_some_and(|max| turns >= max.saturating_sub(1))
            {
                let _ = self.status_tx.send(SubagentEvent::Message(format!(
                    "[Reached tool turn budget of {}; reserving final turn for summary]",
                    max_turns.unwrap()
                )));
                history.push(Message::system(
                    "[You have reached your tool turn budget. Stop calling tools and write the final result summary now.]",
                ));
                forced_final_summary = true;
            }

            if !warned_context
                && remaining_ratio <= WRAP_UP_REMAINING_RATIO
                && estimated_prompt_tokens > 0
            {
                warned_context = true;
                history.push(Message::system(&format!(
                    "[Context budget: approximately {} tokens used, {} remaining ({:.0}% remaining). Start wrapping up: only call tools that are absolutely necessary, and prepare your final findings.]",
                    estimated_prompt_tokens,
                    context_remaining_tokens(estimated_prompt_tokens, self.max_context_tokens),
                    remaining_ratio * 100.0,
                )));
            }

            if !forced_final_summary
                && remaining_ratio <= FINAL_SUMMARY_REMAINING_RATIO
                && estimated_prompt_tokens > 0
            {
                history.push(Message::system(&format!(
                    "[Context budget critical: approximately {} tokens used, {} remaining ({:.0}% remaining). Do not call more tools. Write your complete final findings and summary now.]",
                    estimated_prompt_tokens,
                    context_remaining_tokens(estimated_prompt_tokens, self.max_context_tokens),
                    remaining_ratio * 100.0,
                )));
                forced_final_summary = true;
            }

            // Call LLM
            let empty_tools: Vec<crate::api::types::ToolDefinition> = Vec::new();
            let active_tools = if forced_final_summary {
                &empty_tools
            } else {
                &tools
            };
            let response = match self
                .client
                .chat(&self.model_id, &history, active_tools)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = format!("Subagent API error: {}", e);
                    let _ = self.status_tx.send(SubagentEvent::Error(err_msg.clone()));

                    // Try context overflow recovery
                    let err_lower = e.to_lowercase();
                    let is_overflow = err_lower.contains("context")
                        || err_lower.contains("too long")
                        || err_lower.contains("maximum")
                        || err_lower.contains("exceed")
                        || err_lower.contains("413")
                        || err_lower.contains("400");

                    if is_overflow {
                        // Apply rolling window
                        let dropped = crate::agent::compaction::apply_rolling_window(
                            &mut history,
                            self.max_context_tokens,
                            last_prompt_tokens,
                            0, // subagent has no snapshot history; falls back to estimate
                        );
                        if dropped > 0 {
                            let _ = self.status_tx.send(SubagentEvent::Message(format!(
                                "[Context overflow - dropped {} messages, retrying]",
                                dropped
                            )));
                            continue;
                        }
                    }

                    break;
                }
            };

            // Track token usage
            if let Some(ref usage) = response.usage {
                last_prompt_tokens = usage.prompt_tokens;
            }

            let choice = match response.choices.first() {
                Some(c) => c,
                None => break,
            };

            // Collect assistant text — always overwrite with latest message.
            // Intermediate turns (with tool calls) are just narration ("Let me check X...").
            // The final turn (no tool calls) is the actual summary we want to return.
            let current_text = choice.message.content.clone().unwrap_or_default();
            if !current_text.is_empty() {
                let _ = self
                    .status_tx
                    .send(SubagentEvent::Message(current_text.clone()));
            }

            // Anti-thrash: if the agent produced substantial reasoning text while in
            // analyze_mode, it has provided its diagnosis — unblock write tools.
            if self.analyze_mode && current_text.len() > 100 {
                self.analyze_mode = false;
                self.same_error_count = 0;
            }

            // Check for tool calls
            if let Some(ref tool_calls) = choice.message.tool_calls {
                if tool_calls.is_empty() {
                    // Empty tool_calls = done. This text is the final summary.
                    final_text = current_text;
                    history.push(choice.message.clone());
                    break;
                }

                // Has tool calls — this text is just narration, don't keep it
                history.push(choice.message.clone());

                for tc in tool_calls {
                    let tool_name = &tc.function.name;
                    let args_summary = summarize_args(&tc.function.arguments);

                    let _ = self.status_tx.send(SubagentEvent::ToolRunning {
                        tool_name: tool_name.clone(),
                        args_summary: args_summary.clone(),
                    });

                    // Check allowlist
                    let allowed = self.agent_def.tools.contains(tool_name)
                        || (tool_name == "delegate_task" && self.depth < self.max_depth);

                    let result = if !allowed {
                        format!(
                            "DENIED: Tool '{}' is not in this agent's allowlist",
                            tool_name
                        )
                    } else if self.analyze_mode
                        && matches!(executor.classify_tool_name(tool_name), ToolKind::Write)
                    {
                        // Anti-thrash: Write tools are blocked until a diagnosis is produced.
                        "ANALYZE-ONLY MODE: Writing is blocked because you produced the same error \
                         twice without progress. You must first output a structured diagnosis:\n\
                         • Error signature: (summarize the core error)\n\
                         • Likely root cause:\n\
                         • Alternative causes (2):\n\
                         • Next inspection step: (file/line + why)\n\
                         • Proposed patch:\n\
                         • Expected change in output:\n\n\
                         Once you write this diagnosis in your response text, the block will lift \
                         and you may patch again."
                            .to_string()
                    } else {
                        // Classify tool and request approval for write/exec tools
                        let kind = executor.classify_tool_name(tool_name);
                        let mut denied = false;

                        if matches!(
                            kind,
                            ToolKind::Write | ToolKind::Execute | ToolKind::Unknown
                        ) {
                            let kind_event = match kind {
                                ToolKind::Write => ToolKindEvent::Write,
                                ToolKind::Execute => ToolKindEvent::Execute,
                                _ => ToolKindEvent::Read,
                            };
                            let _ = self.parent_event_tx.send(AgentEvent::ToolRequest {
                                tool_name: tool_name.clone(),
                                tool_args: tc.function.arguments.clone(),
                                tool_id: tc.id.clone(),
                                kind: kind_event,
                            });

                            // Block until user approves/denies
                            match approval_rx.recv().await {
                                Some(UserAction::ApproveAction(_)) => { /* proceed */ }
                                _ => {
                                    denied = true;
                                }
                            }
                        }

                        if denied {
                            format!("DENIED: User denied {}", tool_name)
                        } else if tool_name == "delegate_task" && self.depth < self.max_depth {
                            // Handle nested subagent — pass borrowed approval_rx through
                            match self.run_nested_subagent(tc, approval_rx).await {
                                Ok(r) => r,
                                Err(e) => format!("Subagent error: {}", e),
                            }
                        } else {
                            let args: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Null);
                            let summarizer = self.resolve_web_summarizer();
                            let exec_result = match executor
                                .execute(
                                    tool_name,
                                    &args,
                                    summarizer.as_ref().map(|(c, m)| (c, m.as_str())),
                                )
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => format!("Tool error: {}", e),
                            };

                            // Emit ToolResult for write/exec tools so TUI shows scrollback
                            if matches!(kind, ToolKind::Write | ToolKind::Execute) {
                                let success = !exec_result.starts_with("Tool error:");
                                let result_summary: String =
                                    exec_result.chars().take(200).collect();
                                let _ = self.parent_event_tx.send(AgentEvent::ToolResult {
                                    tool_name: tool_name.clone(),
                                    result: result_summary,
                                    success,
                                });
                            }

                            exec_result
                        }
                    };

                    // Anti-thrash: track error signatures for shell_exec results.
                    // If the same error appears twice, enter analyze_mode to force diagnosis.
                    if let Some(sig) = compute_error_sig(tool_name, &result) {
                        if self.last_error_sig == Some(sig) {
                            self.same_error_count += 1;
                        } else {
                            self.last_error_sig = Some(sig);
                            self.same_error_count = 1;
                        }

                        if self.same_error_count >= 2 && !self.analyze_mode {
                            self.analyze_mode = true;
                            history.push(Message::system(
                                "[ANALYZE-ONLY MODE ACTIVATED: You have produced the same error twice \
                                 without meaningful progress. Write/edit tools are now BLOCKED.\n\n\
                                 You MUST produce a structured diagnosis in your next response before \
                                 making any more changes:\n\
                                 • Error signature: (the core error in one line)\n\
                                 • Likely root cause:\n\
                                 • Alternative causes (2):\n\
                                 • Next inspection step: (file/line + why)\n\
                                 • Proposed patch: (what you will change)\n\
                                 • Expected change in output: (how the error should differ)\n\n\
                                 Writing this diagnosis in your response text will lift the block.]"
                            ));
                        }
                    }

                    let success =
                        !result.starts_with("Tool error:") && !result.starts_with("DENIED:");
                    let result_summary: String = result.chars().take(200).collect();
                    tool_observations.push(format!(
                        "{} [{}]: {}",
                        tool_name,
                        if success { "ok" } else { "err" },
                        result_summary
                    ));

                    let _ = self.status_tx.send(SubagentEvent::ToolDone {
                        tool_name: tool_name.clone(),
                        success,
                        result_summary,
                    });

                    let tool_msg = Message::tool_result(&tc.id, tool_name, &result);
                    history.push(tool_msg);
                    let estimated_after_tool =
                        last_prompt_tokens.max(estimate_prompt_tokens_from_history(&history));
                    let remaining_after_tool =
                        context_remaining_ratio(estimated_after_tool, self.max_context_tokens);
                    history.push(Message::system(&format!(
                        "[Context budget after {}: approximately {} tokens used, {} remaining ({:.0}% remaining).]",
                        tool_name,
                        estimated_after_tool,
                        context_remaining_tokens(estimated_after_tool, self.max_context_tokens),
                        remaining_after_tool * 100.0,
                    )));
                }

                toolless_intent_retries = 0;
                turns += 1;
            } else {
                if !forced_final_summary
                    && toolless_intent_retries < 1
                    && looks_like_tool_intent_without_action(Some(&current_text))
                {
                    toolless_intent_retries += 1;
                    history.push(choice.message.clone());
                    history.push(Message::system(
                        "Your last response described a next action but returned no tool calls. \
                         If you still need to inspect or execute something, call the appropriate \
                         tool now. Only end with plain text when you are actually finished.",
                    ));
                    turns += 1;
                    continue;
                }

                // No tool calls — agent is done. This text is the final summary.
                final_text = current_text;
                history.push(choice.message.clone());
                break;
            }
        }

        // If the loop ended without a final text-only message, force one more
        // LLM call with no tools to extract a summary from what it learned.
        if final_text.trim().is_empty() && turns > 0 {
            history.push(Message::system(
                "[Your tool access has ended. Based on everything you have done so far, \
                 provide your complete findings and summary now. Do NOT call any tools.]",
            ));

            // Call with empty tools to force text-only response
            let empty_tools: Vec<crate::api::types::ToolDefinition> = Vec::new();
            match self
                .client
                .chat(&self.model_id, &history, &empty_tools)
                .await
            {
                Ok(response) => {
                    if let Some(choice) = response.choices.first() {
                        if let Some(ref content) = choice.message.content {
                            if !content.trim().is_empty() {
                                final_text = content.clone();
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = self.status_tx.send(SubagentEvent::Error(format!(
                        "Failed to get final summary: {}",
                        e
                    )));
                }
            }
        }

        let summary = if final_text.trim().is_empty() {
            if tool_observations.is_empty() {
                format!(
                    "[Subagent completed after {} turns but could not produce a summary]",
                    turns
                )
            } else {
                format!(
                    "[Subagent reached its turn limit after {} turns without a final summary. Tool observations:\n- {}]",
                    turns,
                    tool_observations.join("\n- ")
                )
            }
        } else {
            final_text.trim().to_string()
        };

        let _ = self.status_tx.send(SubagentEvent::Finished {
            summary: summary.clone(),
        });

        Ok((summary, history, turns))
    }

    /// Resolve the ApiClient + model_id for web_fetch summarization.
    fn resolve_web_summarizer(&self) -> Option<(ApiClient, String)> {
        match &self.app_config.models.web_tool_model {
            Some(name) => self.app_config.get_endpoint(name).map(|ep| {
                let mut client = ApiClient::from_endpoint(ep, None);
                client.apply_agent_reasoning_defaults(&self.app_config.agent);
                (client, ep.model_id.clone())
            }),
            None => {
                // Fall back to this subagent's model without sharing its KV cache.
                Some((
                    self.client.clone().without_forge_session(),
                    self.model_id.clone(),
                ))
            }
        }
    }

    /// Handle a nested delegate_task call from within this subagent.
    async fn run_nested_subagent(
        &mut self,
        tc: &crate::api::ToolCall,
        approval_rx: &mut mpsc::UnboundedReceiver<UserAction>,
    ) -> Result<String> {
        let args: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).unwrap_or(serde_json::Value::Null);

        let agent_type = args["agent_type"].as_str().unwrap_or("general").to_string();
        let prompt = args["prompt"].as_str().unwrap_or("").to_string();

        // Resolve base definition
        let mut def = if agent_type == "custom" {
            AgentDefinition {
                name: "custom".to_string(),
                description: "Custom inline agent".to_string(),
                tools: Vec::new(),
                model: AgentModel::Inherit,
                max_turns: None,
                system_prompt: "You are a helpful coding agent.".to_string(),
                source: super::agent_def::AgentDefSource::BuiltIn,
            }
        } else {
            self.agent_definitions
                .iter()
                .find(|d| d.name == agent_type)
                .cloned()
                .unwrap_or_else(|| AgentDefinition {
                    name: agent_type.clone(),
                    description: "Unknown agent type".to_string(),
                    tools: vec![
                        "read_file".to_string(),
                        "list_directory".to_string(),
                        "search_code".to_string(),
                        "glob_files".to_string(),
                    ],
                    model: AgentModel::Inherit,
                    max_turns: None,
                    system_prompt: "You are a helpful coding agent.".to_string(),
                    source: super::agent_def::AgentDefSource::BuiltIn,
                })
        };

        // Apply overrides
        if let Some(tools) = args["tools_override"].as_array() {
            def.tools = tools
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if let Some(sp) = args["system_prompt_override"].as_str() {
            def.system_prompt = sp.to_string();
        }
        if let Some(model_name) = args["model_override"].as_str() {
            if let Some(model_name) = normalized_model_override(model_name) {
                def.model = AgentModel::Named(model_name);
            }
        }
        if let Some(mt) = args["max_turns_override"].as_u64() {
            def.max_turns = Some(mt as usize);
        }

        // Resolve model
        let (client, model_id, max_ctx) = match &def.model {
            AgentModel::Inherit => (
                self.client
                    .clone()
                    .with_forge_session_suffix(&format!("nested:{}", tc.id)),
                self.model_id.clone(),
                self.max_context_tokens,
            ),
            AgentModel::Named(endpoint_name) => {
                match self.app_config.get_endpoint(endpoint_name).or_else(|| {
                    self.app_config
                        .models
                        .endpoints
                        .iter()
                        .find(|ep| ep.model_id == *endpoint_name)
                }) {
                    Some(ep) => {
                        let mut new_client = ApiClient::from_endpoint(ep, None);
                        new_client.apply_agent_reasoning_defaults(&self.app_config.agent);
                        let max_ctx = match new_client.fetch_context_length(&ep.model_id).await {
                            Some(detected) => detected,
                            None => ep.max_context_tokens,
                        };
                        (new_client, ep.model_id.clone(), max_ctx)
                    }
                    None => (
                        self.client.clone(),
                        self.model_id.clone(),
                        self.max_context_tokens,
                    ),
                }
            }
        };

        // Emit SubagentStarted so the UI shows the nested subagent (same slot)
        let _ = self.parent_event_tx.send(AgentEvent::SubagentStarted {
            id: self.slot_id.clone(),
            agent_type: agent_type.clone(),
            prompt: prompt.clone(),
        });

        // Pass the parent's status_tx so nested ToolRunning/ToolDone events
        // flow through the existing forwarding task in core.rs → SubagentStatus
        let mut nested_runner = SubagentRunner::new(
            def,
            client,
            model_id,
            max_ctx,
            self.project_root.clone(),
            prompt,
            self.depth + 1,
            self.max_depth,
            self.status_tx.clone(),
            self.agent_definitions.clone(),
            self.app_config.clone(),
            self.parent_event_tx.clone(), // same TUI channel
            None,                         // no owned rx — will borrow
            self.slot_id.clone(),         // inherit parent's slot_id
        );

        // Run nested with borrowed approval_rx — no new channel, no forwarding
        let (result, _history, _turns) =
            Box::pin(nested_runner.run_inner(approval_rx, None)).await?;

        // Emit SubagentFinished so the TUI closes the nested subagent panel
        let summary: String = result.chars().take(200).collect();
        let _ = self.parent_event_tx.send(AgentEvent::SubagentFinished {
            id: self.slot_id.clone(),
            agent_type: agent_type,
            summary,
        });

        Ok(result)
    }
}

fn looks_like_tool_intent_without_action(content: Option<&str>) -> bool {
    let text = match content {
        Some(t) => t.trim(),
        None => return false,
    };
    if text.is_empty() {
        return false;
    }

    let lower = text.to_lowercase();
    if lower.contains("let me know") || lower.contains("how can i help") {
        return false;
    }

    [
        "let me ",
        "i'll ",
        "i will ",
        "i am going to ",
        "i'm going to ",
        "first, i'll",
        "next, i'll",
        "let me check",
        "let me look",
        "let me inspect",
        "let me explore",
        "let me search",
        "let me read",
        "let me synthesize",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn estimate_prompt_tokens_from_history(history: &[Message]) -> u32 {
    let chars: usize = history
        .iter()
        .map(|msg| {
            msg.content
                .as_ref()
                .map(|content| content.len())
                .unwrap_or(0)
                + msg
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tc| tc.function.name.len() + tc.function.arguments.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
                + msg.tool_call_id.as_ref().map(|id| id.len()).unwrap_or(0)
                + msg.name.as_ref().map(|name| name.len()).unwrap_or(0)
                + msg.role.len()
        })
        .sum();
    (chars / 4).max(history.len() * 12) as u32
}

fn context_remaining_tokens(prompt_tokens: u32, max_context_tokens: usize) -> usize {
    max_context_tokens.saturating_sub(prompt_tokens as usize)
}

fn context_remaining_ratio(prompt_tokens: u32, max_context_tokens: usize) -> f64 {
    if max_context_tokens == 0 {
        return 1.0;
    }
    context_remaining_tokens(prompt_tokens, max_context_tokens) as f64 / max_context_tokens as f64
}

/// Compute a hash signature from a shell_exec result to detect "same error twice" loops.
/// Returns None if the result doesn't look like a meaningful failure.
fn compute_error_sig(tool_name: &str, result: &str) -> Option<u64> {
    if tool_name != "shell_exec" {
        return None;
    }

    let lower = result.to_lowercase();
    let is_failure = (lower.contains("exit code:") && !lower.contains("exit code: 0"))
        || lower.contains("error[e")       // Rust compile errors
        || lower.contains("test failed")
        || lower.contains("assertion failed")
        || lower.contains("panicked at")
        || lower.contains("build failed")
        || lower.contains("failed to compile");

    if !is_failure {
        return None;
    }

    // Hash the first 600 chars — enough to capture error type + location without noise from
    // line numbers that shift with each edit.
    let sig_text: String = result.chars().take(600).collect();
    let mut hasher = DefaultHasher::new();
    sig_text.hash(&mut hasher);
    Some(hasher.finish())
}

fn summarize_args(args_json: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(obj) = val.as_object() {
            for key in &["command", "path", "query", "prompt"] {
                if let Some(v) = obj.get(*key) {
                    if let Some(s) = v.as_str() {
                        let display: String = s.chars().take(80).collect();
                        if s.len() > 80 {
                            return format!("{}...", display);
                        }
                        return display;
                    }
                }
            }
        }
    }
    let display: String = args_json.chars().take(60).collect();
    if args_json.len() > 60 {
        format!("{}...", display)
    } else {
        display
    }
}
