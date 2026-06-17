# Forge Architecture

## Overview

Forge is an autonomous AI coding assistant built around a Rust headless agent and a Bun/Ink terminal UI. The UI speaks a JSON-newline protocol to `forge-agent --headless`; the agent calls OpenAI-compatible chat APIs, Anthropic Messages APIs, or ChatGPT Codex Responses APIs, then executes local tools for code reading, editing, testing, and search.

The public build includes the core coding workflow: approval-gated tools, planning mode, context compaction or rolling-window trimming, parallel subagents, session persistence, revert checkpoints, OAuth-backed provider login, and local custom agent/tool definitions.

## Core Purpose

Forge lets developers work with a codebase through natural language. The agent can:

- Read and analyze files
- Search codebases with regex and glob patterns
- Apply unified diffs and direct file edits
- Run build, test, lint, and shell commands
- Search and fetch web content
- Ask structured clarification questions
- Delegate bounded work to specialized subagents
- Maintain plans and task lists
- Resume sessions and revert conversation/workspace state to prior turn boundaries

## Safety Boundary

Forge's public safety model is approval-based command gating, not host isolation.

The agent, its tools, and approved shell commands run as the user account that launched Forge. Forge does not currently provide a practical sandbox, container boundary, filesystem jail, process jail, network jail, or credential isolation layer. The project root is a default working directory only; absolute paths and shell commands can reach outside it whenever normal operating system permissions allow.

This means Forge should be treated as a sharp tool. It can be very effective, but it can also damage files, run destructive commands, leak secrets through model/tool output, or modify system state if the user approves unsafe actions or enables auto-approval modes. The approval prompts, disabled tool list, planning mode, and `--dangerously-allow-all` warning are UX guardrails. They are not a security boundary.

## High-Level Architecture

```text
┌─────────────────────────────────────────────────────────────────────┐
│                         Bun/Ink TUI Layer (ui/)                     │
│  - React Ink terminal application                                   │
│  - Spawns forge-agent --headless through AgentBridge               │
│  - Validates protocol messages with zod                             │
│  - Owns menus, approvals, plans, sessions, and local UI state       │
└─────────────────────────────────────────────────────────────────────┘
                              ↓ ↑
┌─────────────────────────────────────────────────────────────────────┐
│                       Headless Protocol Layer                       │
│  - JSON newline protocol on stdin/stdout                            │
│  - Streams assistant text, tool events, usage, plans, and prompts   │
│  - Accepts user messages, approvals, model/config updates, control  │
└─────────────────────────────────────────────────────────────────────┘
                              ↓ ↑
┌─────────────────────────────────────────────────────────────────────┐
│                          Agent Layer (agent/)                       │
│  - Main LLM/tool loop                                               │
│  - Message history, compaction, and rolling-window trimming         │
│  - Tool orchestration and approval workflow                         │
│  - Session logging, resume, and revert checkpoints                  │
│  - Parallel subagent orchestration                                  │
└─────────────────────────────────────────────────────────────────────┘
                              ↓ ↑
┌─────────────────────────────────────────────────────────────────────┐
│                          Tools Layer (tools/)                       │
│  - File reads/writes, search, glob, patch, shell execution          │
│  - Web search/fetch                                                 │
│  - Planning, question, todo, and delegation tools                   │
│  - User-defined shell-backed tools                                  │
└─────────────────────────────────────────────────────────────────────┘
                              ↓ ↑
┌─────────────────────────────────────────────────────────────────────┐
│                          API Layer (api/)                           │
│  - OpenAI-compatible /chat/completions                              │
│  - Anthropic /v1/messages adapter                                   │
│  - ChatGPT Codex Responses adapter                                  │
│  - Streaming, tool calls, model discovery, reasoning controls       │
└─────────────────────────────────────────────────────────────────────┘
                              ↓ ↑
┌─────────────────────────────────────────────────────────────────────┐
│                          Config Layer (config.rs)                   │
│  - Model endpoints and provider types                               │
│  - Agent approval behavior and disabled tools                       │
│  - Context strategy and limits                                      │
│  - Subagent concurrency/depth/model defaults                        │
└─────────────────────────────────────────────────────────────────────┘
```

## Component Details

### UI Layer (`ui/src/`)

**`agent-bridge.ts`** - `AgentBridge`
- Spawns `forge-agent --headless` with optional cwd/session flags
- Reads newline-delimited JSON from stdout
- Validates every agent message with `AgentMessageSchema`
- Batches high-volume `assistant_token` events before handing them to React
- Sends typed `UserMessage` commands to the agent stdin

**`protocol.ts`**
- Zod schemas for the complete agent-to-UI message surface
- TypeScript discriminated unions for UI-to-agent commands
- Mirrors backend events for init, streaming assistant output, tool calls/results, process input, prompts, plans, questions, logins, sessions, revert, usage, and model/config changes

**`hooks/useAgent.ts` and `components/App.tsx`**
- Own terminal UI state: scrollback, approvals, questions, plans, active subagents, revert checkpoints, endpoints, tool toggles, reasoning controls, and permission mode
- Implements slash commands such as `/model`, `/settings`, `/subagent`, `/plan`, `/sessions`, `/compact`, `/revert`, `/context`, `/thinking`, `/usage`, `/log`, `/clear`, `/login`, and `/help`
- Keeps approval UX partly local through normal/auto-accept/plan modes, per-session "approve always" memory, and safe-command heuristics

### Agent Layer (`src/agent/`)

**`core.rs`** - `Agent`
- Main orchestration loop
- Processes `UserAction` events from the headless protocol
- Manages conversation history with compaction or rolling-window trimming
- Handles tool calls, approval requests, cancellation, retries, and streamed assistant output
- Runs `shell_exec` through the PTY/streaming command path, including foreground and background stdin prompts
- Allows non-interactive SSH commands and injects a hidden remote-Git verification task before remote file changes
- Spawns subagents for `delegate_task`
- Emits revert checkpoints and can restore conversation/Git state to previous turn boundaries
- Supports provider reasoning controls and periodic review/status nudges

Key turn flow:

```text
SendMessage
  → append user message
  → create revert checkpoint
  → call model with current history and tools
  → stream assistant text and collect tool calls
  → approve/execute tool calls
  → append tool results
  → repeat until no more tool calls
  → persist final turn state
```

**`agent_def.rs`** - custom agent definitions
- Built-in agents: `explore`, `bash`, `plan`, and `general`
- Loads Markdown agent files from `~/.config/forge/agents/` and project-local `.agent/agents/`
- Override order: built-ins, then global agents, then project-local agents
- Agent files use frontmatter (`name`, `description`, `tools`, `model`, `max_turns`) followed by a system prompt body
- Subagent tool access is enforced by filtering tool definitions against the selected agent's allowlist

**`subagent.rs`** - `SubagentRunner`
- Executes subagents concurrently with a configurable limit
- Supports nested delegation up to the configured depth
- Gives each subagent its own context window and tool allowlist
- Handles approval flow for subagent tool calls
- Recovers from context overflow with rolling-window fallback
- Reports lifecycle and progress via `SubagentStatus` events

**`compaction.rs`**
- `perform_compaction()` produces structured model-written summaries
- `apply_rolling_window()` drops oldest messages directly when `agent.context_strategy = "rolling_window"`
- Summary fields include goal, repo map, work completed, current state, commands run, decisions, next actions, and pitfalls
- Replaces history with the system prompt, structured summary, and recent conversation tail

**`conversation_log.rs`**
- Append-only JSONL writer under `.forge/sessions/{session_id}/conversation.jsonl`
- Logs messages, tool proposals, approvals, denials, results, and run-state transitions
- Supports session resume without loading the entire log when possible
- Exposes replay entries and revert checkpoints for UI hydration

**`rewind.rs`** - revert snapshots
- Captures checkpoints before user turns and Git snapshots at turn boundaries
- Uses hidden Git refs/stashes to snapshot tracked and untracked changes
- Computes diff summaries before restore
- Restores workspace state and pairs with log truncation/reload so conversation history matches the restored turn

### Tools Layer (`src/tools/`)

**`executor.rs`** - `ToolExecutor`
- Classifies tools by risk: Read, Write, Execute, or Unknown
- Executes built-in tools and shell-backed custom tools
- Streams shell output and handles cancellation/input for long-running commands

Current built-in tools:

| Tool | Kind | Description |
|------|------|-------------|
| `read_file` | Read | Read file with optional line range |
| `list_directory` | Read | Walk directory tree with sizes |
| `search_code` | Read | Wrapper around ripgrep (`rg`) |
| `glob_files` | Read | Find files by glob pattern |
| `todo_write` | Read | Manage task lists |
| `apply_patch` | Write | Apply unified diffs |
| `write_file` | Write | Create or overwrite a file |
| `edit_file` | Write | Replace an exact string |
| `shell_exec` | Execute | Run shell commands through a PTY/streaming path |
| `web_search` | Execute | DuckDuckGo web search via curl |
| `web_fetch` | Execute | Web page extraction with LLM summarization |
| `ask_question` | Read | Ask structured user questions |
| `delegate_task` | Execute | Spawn parallel subagents |
| `enter_plan_mode` | Execute | Enter planning mode |
| `write_plan` | Read | Write an implementation plan |
| `exit_plan_mode` | Read | Submit plan for approval |

Security note: file tools can use absolute paths. The project root is the default base, not a filesystem jail. Approval prompts are the only built-in safety barrier before write and execute actions.

Remote note: interactive SSH sessions are blocked inside `shell_exec`. Non-interactive SSH commands are allowed. When SSH is used for remote workspace work, the agent is instructed to verify `git --version` and `git rev-parse --is-inside-work-tree` in the remote directory before modifying files. If Git is missing, installation requires user approval unless `--dangerously-allow-all` is active.

**`definitions.rs`**
- JSON Schema for each built-in tool's parameters
- Dynamic `delegate_task` definition that lists available agents
- `get_plan_mode_tools()` filters tools available during planning
- `get_toggleable_tool_names()` defines which normal tools can be enabled or disabled from the UI

**`patch.rs`**
- `validate_patch()` rejects patches targeting forbidden dirs such as `.git/`, `target/`, and `node_modules/`
- `apply_patch()` applies unified diffs through `git apply` when possible
- `get_git_diff()` runs `git diff --no-color`

**Custom tools**
- Loaded from `~/.config/forge/tools/` and project-local `.agent/tools/`
- Defined by a JSON schema plus executable shell script
- Receive arguments as JSON on stdin and in `FORGE_TOOL_ARGS`
- Also receive `FORGE_PROJECT_ROOT`, `FORGE_WORKING_DIR`, and `FORGE_TOOL_NAME`

### API Layer (`src/api/`)

**`client.rs`** - `ApiClient`
- `chat()`: non-streaming chat with tools
- `chat_stream()`: streaming chat used by the main agent loop
- `chat_simple()`: simple chat without tools
- `fetch_context_length()`: best-effort context-window detection
- Backend adapters:
  - `OpenAi`: OpenAI-compatible `/chat/completions` plus `/models` discovery
  - `Anthropic`: Anthropic Messages API with OAuth support
  - `ChatGptCodex`: ChatGPT Codex backend using Responses-style input/output conversion
- Converts Forge's internal messages and tool definitions into provider-specific wire formats
- Converts provider tool calls back into Forge `ToolCall` objects
- Applies endpoint reasoning controls for OpenAI-compatible, Anthropic, and ChatGPT Codex backends

**`types.rs`**
- OpenAI-compatible request/response structures
- Internal `Message`, `ToolDefinition`, and `ToolCall` types

**`auth.rs`**
- Claude OAuth login/refresh stored at `~/.config/forge/auth.json`
- ChatGPT Codex OAuth login/refresh stored at `~/.config/forge/chatgpt_auth.json`
- Runtime endpoint discovery for authenticated Anthropic and ChatGPT Codex accounts

### Config Layer (`src/config.rs`)

Config is saved to `~/.config/forge/config.toml`.

Important keys:

- `models.endpoints`: LLM endpoint definitions
- `models.default`: main agent endpoint name
- `models.web_tool_model`: optional endpoint for `web_fetch` summarization
- `ModelEndpoint.endpoint_type`: `open_ai`, `anthropic`, or `chatgpt_codex`
- `ModelEndpoint.request_timeout_secs`: per-endpoint HTTP timeout
- `ModelEndpoint.reasoning`: provider-specific reasoning/thinking controls
- `agent.disabled_tools`: normal tools removed from the tool list
- `agent.context_strategy`: `compaction` or `rolling_window`
- `agent.auto_approve_reads`: skip approval for read-only tools
- `agent.auto_approve_writes`: skip approval for write tools
- `agent.max_history_messages`: total history cap
- `agent.compaction_threshold`: message count that triggers compaction
- `agent.subagents.enabled`: enable/disable subagents
- `agent.subagents.max_concurrent`: parallel subagent limit
- `agent.subagents.max_depth`: nesting depth limit
- `agent.subagents.default_model`: default model for subagents

### Headless Protocol (`src/headless.rs`)

Agent to UI messages include:

- `init`
- `thinking`
- `assistant_token`, `assistant_done`, `assistant_message`
- `tool_request`, `tool_result`, `tool_output`
- `error`, `done`, `cancelled`
- `usage`, `usage_update`
- `model_switched`, `endpoints_updated`
- `subagent_started`, `subagent_status`, `subagent_finished`
- `question_request`
- `plan_mode_entered`, `plan_mode_exited`, `plan_mode_ready`
- `login_status`, `login_complete`
- `session_loaded`
- `rewind_checkpoint`, `rewind_preview`, `turn_discarded` (legacy protocol event names for revert UI)
- `process_input_needed`, `background_prompt_needed`

UI to agent commands include:

- `send_message`
- `approve_action`, `deny_action`
- `toggle_auto_mode`, `switch_model`
- `update_subagent_config`
- `update_web_model`
- `update_tool_config`, `update_context_strategy`, `update_endpoint_reasoning`
- `compact`, `request_usage`
- `revert`, `revert_preview`
- `enter_plan_mode`, `approve_plan`, `reject_plan`, `clear_and_approve_plan`
- `answer_question`
- `process_input`, `bg_process_input`
- `login_anthropic`, `login_chatgpt`
- `cancel_run`, `quit`
- `resume_session`

## Data Flow

### User Message Flow

```text
TUI sends {"type":"send_message"} over AgentBridge stdin
    ↓
headless.rs parses JSON into UserAction::SendMessage
    ↓
Agent.run() receives action
    ↓
Agent.process_turn()
    ↓
ApiClient.chat_stream(model_id, history, tools) → provider API
    ↓
Parse streamed assistant text and optional tool calls
    ↓
Emit AgentEvent::AssistantMessage / ToolRequest
    ↓
If tool calls: handle approval, execute tool, append tool result
    ↓
If delegate_task: spawn SubagentRunner
    ↓
ConversationLog appends message/tool/run-state records
    ↓
Loop continues or turn completes
```

### Tool Approval Flow

```text
Agent detects tool call
    ↓
Classify tool as Read, Write, Execute, or Unknown
    ↓
Check auto-approve flags, UI permission mode, and per-tool memory
    ↓
If approval is required:
  - Emit ToolRequest
  - Wait for ApproveAction or DenyAction
    ↓
Execute tool via ToolExecutor
    ↓
Emit ToolResult
    ↓
Add tool result to history
```

### Session Resume / Revert Flow

```text
New process starts with session_id
    ↓
ConversationLog writes .forge/sessions/{session_id}/conversation.jsonl
    ↓
Before each user turn: Agent creates a revert checkpoint
    ↓
At turn boundaries: hidden Git snapshot records workspace state
    ↓
UI requests revert preview or restore
    ↓
Agent computes diff summary, restores Git state, truncates/reloads log context
    ↓
UI receives revert events and hydrates visible scrollback
```

## Design Decisions

1. **JSON-newline protocol** keeps the UI replaceable and the agent automatable.
2. **Human approval by default** keeps write and execute actions visible unless the user opts into broader trust. This is a guardrail, not a sandbox.
3. **Streaming first** keeps long model responses and command output responsive.
4. **Git-backed revert** makes code rollback tied to conversation rollback instead of only trimming chat history.
5. **Provider adapters stay behind `ApiClient`** so model/provider differences do not leak through the agent loop.
6. **Subagents are bounded** by depth, concurrency, tool allowlists, and independent context windows.
7. **Custom tools are shell-backed** so users can extend Forge without changing Rust code.
