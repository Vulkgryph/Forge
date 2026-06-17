# Forge

Created by **Vulkgryph LLC**.

An autonomous AI coding agent that runs locally against OpenAI-compatible, Anthropic, or ChatGPT Codex endpoints. Forge reads, writes, and executes code in your project via a terminal UI and a headless JSON protocol.

## Features

- **Full coding toolkit** — read/write files, apply unified diffs, search code, run shell commands, web search/fetch
- **Parallel subagents** — delegate subtasks to specialized agents running concurrently
- **Planning mode** — agent drafts a plan for your approval before making changes
- **Session persistence** — resume prior sessions with full context
- **Context compaction** — LLM-backed summarization keeps long sessions healthy
- **Rolling window context** — optionally drop oldest messages instead of compacting
- **Configurable tool access** — disable selected tools from the UI/settings

## Safety Model

Forge is a sharp tool: powerful, useful, and dangerous if mishandled.

Forge does **not** provide practical isolation from the host machine. Its safety mechanism is approval-based command gating: it asks before write and execute tools unless you enable auto-approval modes or `--dangerously-allow-all`. Once a tool is approved, Forge runs with the same filesystem, shell, network, credential, and process access as the user account that launched it.

The project root is the default working directory, not a sandbox. File tools and shell commands can access paths outside the project when the underlying operating system permissions allow it. Use Forge only in workspaces and user accounts where that level of access is acceptable, review commands before approving them, and treat auto-approval modes as trusted-session features.

## Requirements

- **macOS or Linux**
- **Rust** (installed automatically by `install.sh` if missing)
- **Bun** (installed automatically by `install.sh` if missing)
- **An LLM endpoint** — OpenAI-compatible, Anthropic, or ChatGPT Codex

**Linux preflight** — on a minimal Ubuntu/Debian image you may need to install a C toolchain and `unzip` before running `install.sh`:

```bash
sudo apt-get update && sudo apt-get install -y git build-essential unzip
```

`install.sh` will detect these and tell you exactly what to install if any are missing.

## Installation

```bash
git clone https://github.com/Vulkgryph/Forge.git forge
cd forge
./install.sh
```

Follow the prompts to configure your LLM endpoint, then run:

```bash
forge
```

**Prefer to configure manually?** See [Configuration](#configuration) below.

## Updating

From an installed checkout:

```bash
forge-update
```

Or from the repo:

```bash
./update.sh
```

The updater uses `git pull --ff-only`, rebuilds `forge-agent` and the UI, reinstalls the local wrappers, and preserves your config in `~/.config/forge`. If you have local source changes, it skips pulling and rebuilds the current checkout. Use `./update.sh --no-pull` to rebuild/reinstall without touching git.

## Configuration

Config file: `~/.config/forge/config.toml`

### Minimal setup (single local endpoint)

```toml
[models]
default = "local"

[[models.endpoints]]
name = "local"
base_url = "http://127.0.0.1:1234/v1"
model_id = "your-model-id"
max_context_tokens = 32768
max_output_tokens = 8192

[agent]
auto_approve_reads = true
auto_approve_writes = false
permission_mode = "default"
disabled_tools = []
context_strategy = "compaction"
max_history_messages = 200
compaction_threshold = 150

[agent.subagents]
enabled = true
max_depth = 4
max_concurrent = 4
default_model = "local"
```

### Multiple endpoints

```toml
[models]
default = "main"
web_tool_model = "fast"     # optional: smaller model for web_fetch summarization

[[models.endpoints]]
name = "main"
base_url = "http://127.0.0.1:8081/v1"
model_id = "Qwen3-Coder-80B"
max_context_tokens = 131072
max_output_tokens = 16384

[[models.endpoints]]
name = "fast"
base_url = "http://127.0.0.1:1234/v1"
model_id = "Qwen3-Coder-30B"
max_context_tokens = 65536
max_output_tokens = 8192

[agent]
auto_approve_reads = true
auto_approve_writes = false
disabled_tools = []
context_strategy = "compaction" # or "rolling_window"

[agent.subagents]
enabled = true
max_depth = 4
max_concurrent = 4
default_model = "fast"
```

### ChatGPT Codex subscription login

Forge can also use a ChatGPT account with Codex access. Log in from the UI:

```text
/login --chatgpt
```

Or from the agent binary:

```bash
forge-agent --login-chatgpt
```

After login, Forge adds a `chatgpt-codex` endpoint that uses ChatGPT OAuth credentials stored at `~/.config/forge/chatgpt_auth.json`.

Claude subscription login is also available:

```text
/login --anthropic
```

Or:

```bash
forge-agent --login
```

### Config reference

| Key | Default | Description |
|-----|---------|-------------|
| `models.default` | — | Endpoint name used for the main agent |
| `models.web_tool_model` | same as default | Endpoint for `web_fetch` summarization |
| `agent.auto_approve_reads` | `true` | Skip approval prompts for read-only tools |
| `agent.auto_approve_writes` | `false` | Skip approval prompts for file writes |
| `agent.permission_mode` | `"default"` | Stored permission preference. Related approval behavior is surfaced through multiple mechanisms: the TUI mode selector (`normal` / `auto_accept` / `plan`), per-session “approve always” tool memory, and the startup flag `--dangerously-allow-all`. The serialized enum supports `default`, `accept_edits`, `bypass_permissions`, `dont_ask`, `plan` |
| `agent.disabled_tools` | `[]` | Tool names to exclude from normal turns |
| `agent.context_strategy` | `"compaction"` | `"compaction"` or `"rolling_window"` |
| `agent.max_history_messages` | `200` | Hard cap on conversation history length |
| `agent.compaction_threshold` | `150` | Message count that triggers context compaction |
| `agent.subagents.enabled` | `true` | Enable/disable parallel subagents |
| `agent.subagents.max_concurrent` | `4` | Max subagents running at once (1, 2, or 4) |
| `agent.subagents.max_depth` | `4` | Max subagent nesting depth |
| `agent.subagents.default_model` | same as default | Model endpoint subagents use |

## Usage

```bash
forge [--cwd <path>]
forge-agent --headless [--resume-session <id>] [--dangerously-allow-all]
```

`forge` launches the terminal UI wrapper. `forge-agent` is the Rust agent binary; outside headless mode it exits with a usage message and expects to be driven by the UI.

If `--cwd` is not specified, Forge uses the current directory as the project root.

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` | New line in input |
| `Ctrl+C` | Quit |
| `Escape` | Cancel current run when the agent is thinking |
| `Shift+Tab` | Cycle permission mode |

### Slash commands

| Command | Description |
|---------|-------------|
| `/model` | Open model configuration |
| `/settings` | Open settings/tool/context menu |
| `/subagent` or `/agents` | Open subagent/agent definition menu |
| `/plan` | Enter planning mode |
| `/sessions` or `/resume` | Browse and resume prior sessions |
| `/compact` | Manually trigger context compaction |
| `/revert` | Restore a previous user turn and code snapshot |
| `/usage` | Show token usage for the current session |
| `/log` | Show the current session log path |
| `/login --anthropic` | Start Claude OAuth login |
| `/login --chatgpt` | Start ChatGPT Codex OAuth login |
| `/help` | Show command help |

### Tool approval

By default, Forge asks before writing files or running commands.

Approval behavior currently comes from several places:

- the TUI permission mode selector (`normal`, `auto_accept`, `plan`)
- per-session “approve always” memory for a tool after you choose that option
- the startup-wide bypass flag `forge-agent --dangerously-allow-all` (typically used via the `forge` wrapper)

Set `auto_approve_reads = true` in config to silently allow all file reads.

Approval is not sandboxing. It is the only built-in safety barrier. If you approve a shell command or enable auto-approval, that command runs as your user on the host machine.

### Context strategies

Forge supports two context management modes:

- `compaction` — summarize older history with the model and keep a structured summary
- `rolling_window` — drop the oldest messages directly without an extra LLM call

### Revert and Git

`/revert` is Git-backed. For local worktrees, Forge snapshots Git state at turn boundaries and can restore files plus conversation state to a selected user turn.

For remote work over SSH, use non-interactive commands such as `ssh host 'cd /path/to/repo && command'`. Before modifying files in a remote directory, Forge instructs the agent to verify that Git is installed and that the directory is inside a Git worktree. If Git is missing, the agent must ask before installing it unless the session was started with `--dangerously-allow-all`.

### Custom agents

Forge loads built-in agent definitions (`explore`, `bash`, `plan`, `general`), then overrides them with any markdown files found in `~/.config/forge/agents/` and `.agent/agents/` inside the project.

### Custom tools

Forge loads custom shell-backed tools from `~/.config/forge/tools/` and `.agent/tools/`. Project tools override global tools with the same name.

Each tool needs a JSON definition plus an executable script (`chmod +x run_linter.sh`):

```text
~/.config/forge/tools/
├── run_linter.json
└── run_linter.sh
```

```json
{
  "name": "run_linter",
  "description": "Run the project linter and return concise output.",
  "kind": "execute",
  "script": "run_linter.sh",
  "timeout_secs": 300,
  "parameters": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "Optional path to lint. Defaults to the project root."
      }
    }
  }
}
```

Forge runs the script from the project root. Tool arguments are passed as JSON on stdin and are also available in `FORGE_TOOL_ARGS`. Forge also sets `FORGE_PROJECT_ROOT`, `FORGE_WORKING_DIR`, and `FORGE_TOOL_NAME`.

`kind` controls approval behavior: `read`, `write`, or `execute`. Omit it and Forge treats the tool as `execute`.

## Troubleshooting

**"No endpoint 'X' found in config"** — `models.default` in your config doesn't match any endpoint name. Open `~/.config/forge/config.toml` and make sure `models.default` matches the `name` field of one of your `[[models.endpoints]]` entries.

**Forge hangs on startup** — your LLM server isn't running or the endpoint URL is wrong. Check that your server is up at the URL in your config, or re-run `./install.sh` to reconfigure.

**Config reset** — delete `~/.config/forge/config.toml` and re-run `./install.sh` to go through the setup wizard again.

## Project layout

```text
forge/
├── src/
│   ├── main.rs                  Entry point, model/auth/bootstrap logic
│   ├── agent/
│   │   ├── core.rs              Main agent loop, tool dispatch
│   │   ├── subagent.rs          Parallel subagent runner
│   │   ├── compaction.rs        Context compaction and rolling window helpers
│   ├── tools/
│   │   ├── executor.rs          Tool execution + classification
│   │   ├── definitions.rs       Tool JSON schemas
│   │   ├── web.rs               Web tools
│   │   └── patch.rs             Unified diff application
│   ├── api/
│   │   ├── client.rs            OpenAI/Anthropic/ChatGPT client
│   │   └── types.rs             Request/response types
│   ├── config.rs                Config loading
│   └── headless.rs              JSON protocol types
├── ui/
│   └── src/
│       ├── index.tsx            UI entry point
│       ├── protocol.ts          Zod schemas for agent protocol
│       ├── agent-bridge.ts      Spawns + communicates with forge-agent
│       ├── hooks/useAgent.ts    React hook for agent state
│       └── components/
│           ├── App.tsx          Main UI, menus, model hub
│           └── SubagentStatus.tsx  Subagent progress display
├── ARCHITECTURE.md              Detailed architecture reference
├── ADDING_TOOLS.md              Guide for adding new tools
└── install.sh                   Build + install script
```

## Adding tools

See [ADDING_TOOLS.md](ADDING_TOOLS.md) for the complete checklist. Every tool requires changes in `definitions.rs`, `executor.rs`, and `core.rs` at minimum.

## Headless mode

`forge-agent` accepts `--headless` for programmatic use. It speaks a JSON newline protocol on stdin/stdout — see `src/headless.rs` for message types and `ui/src/agent-bridge.ts` for a reference client implementation. OAuth login is also available through `forge-agent --login` and `forge-agent --login-chatgpt`.

## Contributions

Forge does not currently accept pull requests. The project is maintained by Vulkgryph LLC and contributions are closed to keep maintenance scope constrained.

**Issues are welcome.** If you have a fix or suggestion, include it in the issue itself — code snippet, patch, or written approach. If the suggested solution is used, you'll be credited in the commit and release notes.

For security issues, see [SECURITY.md](SECURITY.md) — please do not file public issues for vulnerabilities.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See the [NOTICE](NOTICE) file for attribution.

Copyright © 2026 Vulkgryph LLC.
