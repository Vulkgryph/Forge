# Forge

Created by **Vulkgryph LLC**.

An autonomous AI coding agent that runs locally against OpenAI-compatible, Anthropic, or ChatGPT Codex endpoints. Forge reads, writes, and executes code in your project via a terminal UI and a headless JSON protocol.

![Forge writing a zero-dependency VM runtime in Rust](assets/forge-demo.gif)

> _Forge writing a zero-dependency VM runtime in Rust — ~2 min 19 sec, played at 2× speed._

## Philosophy

Forge is built for engineers who want a tool they can rely on, not a tool they chase. The aim is a small, readable codebase with a stable interface — so the command you learn today behaves the same way the next time you use it, and your config file doesn't need to be rewritten between releases.

**Versioning and compatibility:**

- Every release is built to be as backwards-compatible as possible. Commands, config keys, file formats, and the headless JSON protocol stay valid across minor versions by default.
- Major versions are reserved for changes that genuinely need a break. When one ships, it includes a clear explanation of *why* the change was required and *what* it affects, plus either a straightforward manual migration or automatic migration.
- Deprecations are flagged in advance. Nothing that worked in the last release gets removed in a surprise patch.

Forge is intentionally not chasing the newest agent architecture every month. If you want a tool that ships a new "workflow paradigm" every release, Forge is probably not for you. If you want a tool whose interfaces stay stable while the implementation gets smaller, faster, and more reliable underneath, that's what Forge is trying to be.

**Inspection is the point.** The source is here. The architecture is documented in [ARCHITECTURE.md](ARCHITECTURE.md). The public roadmap lives at [vulkgryph.com/roadmap](https://vulkgryph.com/roadmap/). Read it, verify it, disagree with it. If something looks wrong, file an issue or email `contact@vulkgryph.com`. If you want to take it a different direction, fork it.

**On limitations.** Forge is maintained by a small team, and catching every edge case after a patch is beyond what testing alone can cover. After an update, the maintainers know roughly what changed; the community is what surfaces the edge cases and unexpected behavior that a release notes line can miss. If a patch breaks something for you — a workflow that worked before, an integration that no longer behaves the same, a config that stopped being honored — file an issue. Even a one-line report helps — knowing something changed for someone is what testing can't replicate.

## Features

- **Fully offline-capable** — runs with no internet when paired with a local LLM (LM Studio, Ollama, llama.cpp, vLLM, etc.). See [Offline use](#offline-use) below.
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

**Launching with `--dangerously-allow-all` requires an interactive confirmation.** The flag is exactly what it says — every tool approval gate is bypassed for the whole session — so Forge stops before rendering the TUI and asks you to type `yes` to continue. Anything else exits. Set `FORGE_SKIP_DANGEROUS_CONFIRM=1` for scripted / CI use where the operator has already accepted the risk.

## Offline use

Forge runs with no internet when paired with a local LLM. Useful for airgapped environments, secure facilities, weak connections, or anyone who simply doesn't want their code shipped to a cloud provider.

**What requires network**:

| Component | When it talks to the network |
|---|---|
| LLM endpoint | Always — but if it's local (`127.0.0.1:1234`, etc.) that traffic stays on your machine |
| `web_search` / `web_fetch` tools | Only when the model invokes them. Disable both via `agent.disabled_tools = ["web_search", "web_fetch"]` if you want them off the table |
| Codex subscription auth | Only on login + periodic token refresh, only if you're using the ChatGPT Codex provider |
| Codex version self-check | Background, once a week, only if you're actively using Codex; if GitHub is unreachable forge falls back to a cached value |

**Minimum offline setup**:

1. Local LLM running (LM Studio / Ollama / llama.cpp / vLLM)
2. Wizard option 1 (Local LLM server) when running `install.sh`
3. Disable network tools in `~/.config/forge/config.toml`:
   ```toml
   [agent]
   disabled_tools = ["web_search", "web_fetch"]
   ```
4. Set `FORGE_NO_AUTO_VERSION_CHECK=1` to suppress the once-a-week GitHub poll Forge uses to keep its Codex `client_version` current (only relevant if you'd ever use the ChatGPT Codex provider anyway):
   ```bash
   export FORGE_NO_AUTO_VERSION_CHECK=1
   ```

After that, Forge has zero outgoing network traffic outside your local LLM.

### Environment variables

Forge respects a small set of environment variables for users who want to override defaults. None are required.

| Variable | Effect |
|---|---|
| `FORGE_NO_AUTO_VERSION_CHECK=1` | Skip the weekly GitHub poll that keeps the Codex `client_version` string current. Cached values are still used; a hardcoded baseline applies if the cache is empty. |
| `FORGE_SHOW_INTERNAL_MODELS=1` | Show ChatGPT Codex models marked as internal (e.g. `codex-auto-review`). These aren't general chat targets — selecting one will likely fail at the API. Hidden by default. |
| `FORGE_SKIP_DANGEROUS_CONFIRM=1` | Skip the confirmation prompt that fires when launching with `--dangerously-allow-all`. Intended for scripted / CI usage; never set in interactive shells. |
| `FORGE_AGENT_PATH` | Override the path the wrapper uses to find `forge-agent`. Useful for testing local builds. |
| `FORGE_RUSTUP_SHA256` / `FORGE_BUN_SHA256` | Pin the expected SHA-256 of the rustup / bun installers when `install.sh` fetches them. If unset, the script prints the hash so you can pin it on a future run. |
| `FORGE_REPO` / `FORGE_DEST` / `FORGE_BRANCH` | Override defaults in `bootstrap.sh` / `bootstrap.ps1`. |

## Requirements

- **macOS, Linux, or Windows**
- **Rust** (installed automatically by the installer if missing)
- **Bun** (installed automatically by the installer if missing)
- **An LLM endpoint** — OpenAI-compatible, Anthropic, or ChatGPT Codex

**Linux preflight** — on a minimal Ubuntu/Debian image you may need to install a C toolchain and `unzip` before running `install.sh`:

```bash
sudo apt-get update && sudo apt-get install -y git build-essential unzip
```

`install.sh` will detect these and tell you exactly what to install if any are missing.

**Windows preflight** — `install.ps1` uses `winget` to install missing prerequisites (Git, Rust via rustup, Bun). It assumes Visual Studio Build Tools 2022 (or higher) is already installed for the MSVC linker — install from [aka.ms/vs/17/release/vs_BuildTools.exe](https://aka.ms/vs/17/release/vs_BuildTools.exe) with the "Desktop development with C++" workload if missing.

## Installation

### macOS / Linux — one-command install

```bash
curl -fsSL https://raw.githubusercontent.com/Vulkgryph/Forge/main/bootstrap.sh | bash
```

### Windows — one-command install

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/Vulkgryph/Forge/main/bootstrap.ps1 | iex
```

Both bootstrap scripts handle the preflight, clone the repo to `~/forge` (or `$env:USERPROFILE\forge` on Windows), and run the appropriate installer.

Override defaults via environment variables:
- `FORGE_DEST` — clone destination (default: `~/forge` or `$env:USERPROFILE\forge`)
- `FORGE_BRANCH` — branch to check out (default: `main`)
- `FORGE_REPO` — alternative repo URL

### Manual install

**macOS / Linux:**

```bash
git clone https://github.com/Vulkgryph/Forge.git forge
cd forge
./install.sh
```

**Windows (PowerShell):**

```powershell
git clone https://github.com/Vulkgryph/Forge.git forge
cd forge
.\install.ps1
```

The installer's first question is how you want Forge to reach an LLM:

```
  1) Local LLM server   (LM Studio, Ollama, llama.cpp, vLLM, etc.)
  2) Claude   (Anthropic API key — subscription OAuth login is not supported)
  3) ChatGPT Codex subscription   (OAuth login)
  4) Direct API key   (Anthropic, OpenAI, OpenRouter, custom OpenAI-compatible)
  5) Skip — I'll edit the config file myself
```

- **Local (1):** you'll be asked for the base URL, model ID, and context window. No defaults — paste whatever your server uses.
- **Claude (2):** you'll paste an Anthropic API key and pick a model. Claude Pro/Max *subscription* login is not supported — Anthropic's terms restrict subscription credentials to its own apps (see CHANGELOG).
- **ChatGPT Codex subscription (3):** writes a minimal config and offers to run the OAuth login inline. On a local machine, just say yes and a browser opens. On a remote VM over SSH, the installer detects this and tells you to first re-connect with port forwarding so the OAuth callback can reach the listener on the remote host:
  - ChatGPT Codex OAuth uses port **1455**: `ssh -L 1455:localhost:1455 <user>@<host>`
- **Direct API key (4):** you'll pick a provider, paste your key, choose a model. The key is stored in `~/.config/forge/config.toml` (so file permissions matter — `chmod 600` it if you're paranoid).
- **Skip (5):** writes a placeholder config you can edit by hand at `~/.config/forge/config.toml`. The file is annotated with examples for every endpoint type. Re-run `./install.sh` later if you want the interactive wizard.

When the wizard finishes:

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

### Subscription login

Forge can use your existing **ChatGPT Codex** subscription via OAuth — no API key purchase required:

From the command line:

```bash
forge --login chatgpt           # OAuth for ChatGPT Codex
forge --login-chatgpt           # shortcut form

# Lower-level equivalent (skip the wrapper):
forge-agent --login-chatgpt
```

Or from inside the TUI:

```text
/login --chatgpt
```

After login, Forge stores OAuth credentials at `~/.config/forge/chatgpt_auth.json` and adds the corresponding endpoint to your config.

> **Remote / firewall users:** the OAuth flow listens on `localhost:1455` (ChatGPT Codex). If your browser can't reach it (SSH session without port forwarding, corporate firewall, etc.), forge prints both the URL to visit AND a prompt to paste the callback code. After approving in your browser, the redirect page will fail to load — just copy the URL from your browser's address bar after it fails to load, and paste it into forge.

> **Claude (Anthropic):** subscription (Pro/Max) login via Forge is **not supported** — there is no Claude OAuth code path in Forge, and `forge --login claude` exits with an error. Anthropic's terms restrict subscription OAuth credentials to its own applications and prohibit routing requests through Pro/Max credentials in third-party tools, so we don't. Use an **Anthropic API key** instead — add it to an `endpoint_type = "anthropic"` endpoint in `~/.config/forge/config.toml` (or pick **Claude** in the installer wizard). See the [CHANGELOG](CHANGELOG.md) for details.

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
| `Ctrl+N` &nbsp;*or*&nbsp; `\` then `Enter` | New line in input |
| `Ctrl+C` | Quit |
| `Escape` | Cancel current run when the agent is thinking |
| `Shift+Tab` | Cycle permission mode |

Forge uses `Ctrl+N` and the trailing-backslash idiom instead of `Shift+Enter` because `Shift+Enter` is not reliably distinguishable from plain `Enter` across terminal emulators.

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
| `/login --chatgpt` | Start ChatGPT Codex OAuth login (Claude subscription login is not supported — use an Anthropic API key) |
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

## Known Issues

**Occasional streamed-message truncation or duplication.** Under some streaming conditions the assistant's reply can appear cut off or partially duplicated in the UI. This has been practically mitigated through streaming-parser fixes but still surfaces rarely. The full, authoritative version of every turn is preserved in `.forge/sessions/{session_id}/conversation.jsonl` regardless of how it rendered in the UI — so the session log is the source of truth if you suspect a display issue. Work in progress; please file an issue if you reproduce a case that lets us nail the remaining edge.

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

`forge-agent` accepts `--headless` for programmatic use. It speaks a JSON newline protocol on stdin/stdout — see `src/headless.rs` for message types and `ui/src/agent-bridge.ts` for a reference client implementation. ChatGPT Codex OAuth login is also available through `forge-agent --login-chatgpt`.

## Contributions

Forge does not currently accept pull requests. The project is maintained by Vulkgryph LLC and contributions are closed to keep maintenance scope constrained.

**Issues are welcome.** If you have a fix or suggestion, include it in the issue itself — code snippet, patch, or written approach. If the suggested solution is used, you'll be credited in the commit and release notes.

For security issues, see [SECURITY.md](SECURITY.md) — please do not file public issues for vulnerabilities.

## License

Forge is licensed under the [Apache License, Version 2.0](LICENSE). See the [NOTICE](NOTICE) file for attribution.

Copyright © 2026 Vulkgryph LLC.

### Disclaimer

Forge is provided **"AS IS"**, without warranty of any kind, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement.

Forge is a tool that reads, writes, and executes code on the user's machine. It can modify or delete files, run arbitrary shell commands, and call out to external LLM providers and other network services. In no event shall Vulkgryph LLC or any contributor be liable for any claim, damages, or other liability — whether in contract, tort, or otherwise — arising from the use of Forge, including but not limited to:

- Lost, corrupted, or overwritten files
- System damage or unintended state changes
- Commands executed by the agent that the user did not anticipate
- Leaked credentials, secrets, or API keys via model output, tool output, or session logs
- Financial costs incurred through LLM API or subscription usage
- Indirect, incidental, special, consequential, or punitive damages of any kind

Use of Forge implies acceptance of these terms. The full legal language is in [LICENSE](LICENSE), which is the binding document; the plain-English summary above is provided for clarity, not as a replacement.
