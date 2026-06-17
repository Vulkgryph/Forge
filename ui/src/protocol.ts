// SPDX-License-Identifier: Apache-2.0
import { z } from "zod";

// ── Agent → TUI messages ──────────────────────────────────────────────

export const ToolInfoSchema = z.object({
  name: z.string(),
  enabled: z.boolean(),
});
export type ToolInfo = z.infer<typeof ToolInfoSchema>;

const UsageSnapshotSchema = z.object({
  last_prompt_tokens: z.number(),
  last_completion_tokens: z.number(),
  total_prompt_tokens: z.number(),
  total_completion_tokens: z.number(),
  total_requests: z.number(),
  max_context_tokens: z.number(),
  history_messages: z.number(),
});

const QuestionOptionSchema = z.object({
  label: z.string(),
  description: z.string(),
});

const QuestionItemSchema = z.object({
  question: z.string(),
  header: z.string(),
  options: z.array(QuestionOptionSchema),
  multi_select: z.boolean(),
});

const ProviderToggleSchema = z.enum(["provider_default", "on", "off"]);
const ChatGptReasoningEffortSchema = z.enum([
  "provider_default",
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
]);

const EndpointReasoningConfigSchema = z.object({
  open_ai_compatible: z.object({
    thinking: ProviderToggleSchema.default("provider_default"),
    preserve_thinking: ProviderToggleSchema.default("provider_default"),
  }).default({
    thinking: "provider_default",
    preserve_thinking: "provider_default",
  }),
  anthropic: z.object({
    thinking: ProviderToggleSchema.default("on"),
    budget_tokens: z.number().default(8192),
  }).default({
    thinking: "on",
    budget_tokens: 8192,
  }),
  chatgpt_codex: z.object({
    effort: ChatGptReasoningEffortSchema.default("medium"),
  }).default({
    effort: "medium",
  }),
});

const EndpointInfoSchema = z.object({
  name: z.string(),
  base_url: z.string(),
  model_id: z.string(),
  max_context_tokens: z.number(),
  max_output_tokens: z.number(),
  endpoint_type: z.string().default("open_ai"),
  reasoning: EndpointReasoningConfigSchema.default({
    open_ai_compatible: {
      thinking: "provider_default",
      preserve_thinking: "provider_default",
    },
    anthropic: {
      thinking: "on",
      budget_tokens: 8192,
    },
    chatgpt_codex: {
      effort: "medium",
    },
  }),
});

const AgentDefInfoSchema = z.object({
  name: z.string(),
  description: z.string(),
  model: z.string(),
  max_turns: z.number().nullable(),
  tools: z.array(z.string()),
  source: z.string(),
});

export const AgentMessageSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("init"),
    project_root: z.string(),
    model_name: z.string(),
    model_id: z.string(),
    max_context_tokens: z.number(),
    log_path: z.string(),
    dangerously_allow_all: z.boolean(),
    agent_definitions: z.array(AgentDefInfoSchema),
    endpoints: z.array(EndpointInfoSchema),
    session_id: z.string().optional(),
    available_tools: z.array(ToolInfoSchema).default([]),
    context_strategy: z.string().default("compaction"),
    anthropic_logged_in: z.boolean().default(false),
    chatgpt_logged_in: z.boolean().default(false),
  }),
  z.object({ type: z.literal("thinking") }),
  z.object({ type: z.literal("reasoning") }),
  z.object({ type: z.literal("assistant_message"), content: z.string() }),
  z.object({ type: z.literal("assistant_token"), content: z.string() }),
  z.object({ type: z.literal("assistant_done"), content: z.string().default("") }),
  z.object({
    type: z.literal("tool_request"),
    tool_name: z.string(),
    tool_args: z.string(),
    tool_id: z.string(),
    kind: z.enum(["read", "write", "execute"]),
  }),
  z.object({
    type: z.literal("tool_result"),
    tool_name: z.string(),
    result: z.string(),
    success: z.boolean(),
  }),
  z.object({
    type: z.literal("tool_output"),
    tool_name: z.string(),
    content: z.string(),
  }),
  z.object({ type: z.literal("process_input_needed"), prompt: z.string().default("") }),
  z.object({
    type: z.literal("background_prompt_needed"),
    bg_id: z.string(),
    command: z.string(),
    prompt: z.string(),
  }),
  z.object({ type: z.literal("error"), message: z.string() }),
  z.object({
    type: z.literal("api_retry"),
    attempt: z.number(),
    max_attempts: z.number(),
    delay_secs: z.number(),
    error: z.string(),
  }),
  z.object({ type: z.literal("turn_discarded") }),
  z.object({ type: z.literal("done") }),
  z.object({ type: z.literal("cancelled") }),
  z.object({ type: z.literal("usage"), snapshot: UsageSnapshotSchema }),
  z.object({ type: z.literal("usage_update"), snapshot: UsageSnapshotSchema }),
  z.object({
    type: z.literal("model_switched"),
    name: z.string(),
    model_id: z.string(),
    max_context_tokens: z.number(),
  }),
  z.object({
    type: z.literal("session_cleared"),
    session_id: z.string(),
    log_path: z.string(),
  }),
  z.object({
    type: z.literal("subagent_started"),
    id: z.string(),
    agent_type: z.string(),
    prompt: z.string(),
  }),
  z.object({
    type: z.literal("subagent_status"),
    id: z.string(),
    tool_name: z.string(),
    detail: z.string(),
  }),
  z.object({
    type: z.literal("subagent_finished"),
    id: z.string(),
    agent_type: z.string(),
    summary: z.string(),
  }),
  z.object({
    type: z.literal("question_request"),
    question: z.string(),
    tool_id: z.string(),
    items: z.array(QuestionItemSchema),
  }),
  z.object({ type: z.literal("login_status"), message: z.string() }),
  z.object({ type: z.literal("login_complete"), success: z.boolean(), message: z.string() }),
  z.object({ type: z.literal("endpoints_updated"), endpoints: z.array(EndpointInfoSchema) }),
  z.object({ type: z.literal("plan_mode_entered"), plan_path: z.string() }),
  z.object({ type: z.literal("plan_mode_exited"), reason: z.string().optional() }),
  z.object({
    type: z.literal("plan_ready"),
    plan_path: z.string(),
    content: z.string(),
  }),
  z.object({
    type: z.literal("rewind_checkpoint"),
    id: z.string(),
    preview: z.string(),
    message_count: z.number(),
    keep_on_restore: z.boolean().default(false),
  }),
  z.object({
    type: z.literal("rewind_preview"),
    checkpoint_id: z.string(),
    preview: z.string(),
    summary: z.string(),
  }),
  z.object({
    type: z.literal("session_loaded"),
    session_id: z.string(),
    title: z.string(),
    message_count: z.number(),
    compaction_count: z.number(),
    entries: z.array(z.object({
      kind: z.enum(["user", "assistant", "tool_call", "tool_result"]),
      content: z.string(),
      tool_name: z.string().optional(),
      success: z.boolean().optional(),
    })),
    rewind_checkpoints: z.array(z.object({
      id: z.string(),
      preview: z.string(),
      message_count: z.number(),
      display_index: z.number(),
      keep_on_restore: z.boolean().default(false),
    })).default([]),
  }),
]);

export type AgentMessage = z.infer<typeof AgentMessageSchema>;
export type UsageSnapshot = z.infer<typeof UsageSnapshotSchema>;
export type QuestionItem = z.infer<typeof QuestionItemSchema>;
export type QuestionOption = z.infer<typeof QuestionOptionSchema>;
export type AgentDefInfo = z.infer<typeof AgentDefInfoSchema>;
export type EndpointInfo = z.infer<typeof EndpointInfoSchema>;
export type EndpointReasoningConfig = z.infer<typeof EndpointReasoningConfigSchema>;
export type RewindCheckpointInfo = Extract<AgentMessage, { type: "rewind_checkpoint" }>;

// ── TUI → Agent messages ──────────────────────────────────────────────

export type UserMessage =
  | { type: "send_message"; content: string }
  | { type: "approve_action"; tool_id: string }
  | { type: "deny_action"; reason: string }
  | { type: "toggle_auto_mode" }
  | { type: "switch_model"; name: string; base_url: string; model_id: string; max_context_tokens: number; max_output_tokens: number; endpoint_type: string; reasoning: EndpointReasoningConfig }
  | { type: "update_subagent_config"; enabled?: boolean; max_concurrent?: number; max_depth?: number; default_model?: string; clear_default_model?: boolean }
  | { type: "update_web_model"; model: string }
  | { type: "update_tool_config"; tool: string; enabled: boolean }
  | { type: "update_endpoint_reasoning"; endpoint_name: string; reasoning: EndpointReasoningConfig }
  | { type: "compact" }
  | { type: "revert"; checkpoint_id?: string }
  | { type: "revert_preview"; checkpoint_id: string }
  | { type: "request_usage" }
  | { type: "enter_plan_mode" }
  | { type: "approve_plan" }
  | { type: "reject_plan"; feedback: string }
  | { type: "answer_question"; answer: string }
  | { type: "clear_and_approve_plan" }
  | { type: "clear_session" }
  | { type: "process_input"; content: string }
  | { type: "bg_process_input"; bg_id: string; content: string }
  | { type: "login_anthropic" }
  | { type: "login_chatgpt" }
  | { type: "cancel_run" }
  | { type: "update_context_strategy"; strategy: string }
  | { type: "quit" };
