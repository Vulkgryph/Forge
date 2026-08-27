# Changelog

All notable changes to Forge are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Forge adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.0] — 2026-08-27

### Added (forced background ceiling on every top-level shell — default 5 minutes)

- **Any still-running top-level `shell_exec` is now force-moved to the background after `agent.forced_shell_background_secs` (default `300` / 5 minutes), no matter what the model requested.** That includes `wait=true`, a huge `timeout_secs`, and the interactive-prompt heuristic pausing the normal timer. The command is **not** killed — it keeps running as `bg-N`, the turn unblocks with the usual `BACKGROUND:` tool result (poll via `background_id`, stop via `background_action=kill`, automatic `BgDone` when it finishes). Set the config value to `0` to disable (not recommended). Subagent/direct `shell_exec` still uses its own hard timeout (cannot own a parent `bg-N` slot); the hang-fix path above covers that case. Tool description updated so the model knows the ceiling exists and how to follow up on backgrounded work.

### Fixed (a stuck subagent `shell_exec` could freeze the parent chat forever)

- **Root cause of overnight "still running" chats:** subagents (and any other direct `ToolExecutor::execute("shell_exec")` caller) used a fallback `run_command` that blocked on `.output().await` with **no timeout at all**. A hung pipeline such as `rg … | head` inside an explore `delegate_task` therefore never returned a `tool_result`, so the parent turn stayed `Running` indefinitely — conversation log frozen after `tool_approved`, UI still alive, zero model progress. Compounding that: Unix shells are spawned with `setsid()` (PTY path, and now the piped path too), which makes the direct child a process-group leader, but every kill site only called `Child::kill()` on that one PID — pipeline grandchildren (`rg`, nested `sh`) survived. And when the interactive-prompt heuristic set `input_waiting = true`, the top-level wall-clock timeout was skipped entirely, so even streaming shells could lose their only escape hatch.
- **Fixes (agent/server side — `forge-agent`):**
  1. **Subagent/direct `shell_exec` now has a hard timeout** (default 300s, same as top-level `wait=true`; overridable via `timeout_secs`). On expiry the whole process group is killed and a `TIMEOUT:` result is returned instead of hanging.
  2. **`terminate_child` process-group kill** — all timeout/cancel/bg-kill paths now `kill(-pgid, SIGKILL)` before `Child::kill`, so `rg | head` grandchildren die with the outer `sh`. Piped and PTY spawns also set `kill_on_drop(true)` and (on Unix) `setsid()` so the group is well-defined.
  3. **`delegate_task` wall-clock ceiling** — new `agent.subagents.max_delegate_secs` (default **1800**). Phase 3's `JoinSet` wait aborts unfinished runners when the deadline hits and returns a timeout summary to the parent instead of blocking forever.
  4. **`input_waiting` no longer disables timeout forever** — a hard cap of `max(timeout_secs × 3, 600s)` still kills/escapes even while the prompt heuristic thinks the command is interactive.
- This is deliberately an agent-server patch: TUI/IDE clients only observe the existing tool-result / subagent-finished events; no wire-protocol change required.

### Added (`agent.min_shell_timeout_secs` — a floor under how aggressively the model can time out its own shell commands)

- **`shell_exec`'s `timeout_secs` (with `wait=true`) is entirely the model's own per-call choice, and it can simply guess wrong for a task that runs longer than expected — a build + long-running suite got killed at the 600s the model picked for it, well before it actually finished.** New `agent.min_shell_timeout_secs` config value (default `0`, i.e. no floor — today's behavior unchanged) raises the model's own requested timeout up to at least this many seconds when set higher, never lowers it, and only applies to `wait=true` (a detached/auto-backgrounded command was never killed by this value anyway). Verified directly: with the floor set above a deliberately-too-short model-requested `timeout_secs`, the command now runs to completion instead of being killed.

### Added (a project with no git repo now gets one automatically, before it needs it)

- **A brand-new project directory with no git repo never got one — nothing in forge or its clients ever initialized one — meaning rewind checkpoints for that project had no real git backing at all: nothing meaningful to actually restore file state to.** forge now auto-initializes a git repo at the project root at the start of any turn, if one doesn't already exist (a cheap no-op check once it does), with a plain notice in the conversation the first time it happens. Checked and initialized *before* the turn runs — not after, from inside checkpoint creation, where it was tried initially — so the notice arrives correctly ordered as the first thing that turn says, not something trailing in after that turn's own `Done` event already fired. Verified directly: a fresh directory with no `.git` gets one on the very first message, confirmed via `git rev-parse --is-inside-work-tree`, with the notice showing up in the right place in the event stream.

### Added (the reference TUI now has the xAI priority tier and provider-busy handling too)

- **The two features below were added to forge-agent's core protocol but only ever reached Forge IDE — the terminal UI (`ui/`) had no idea either existed.** Reconciled: `EndpointInfo.xai_priority_tier` and `update_xai_priority_tier` added to `ui/src/protocol.ts`; the `/thinking` menu for any xAI endpoint now shows a third "Priority tier" row (cycling it calls `update_xai_priority_tier`, same as the other reasoning toggles); a `Provider at capacity:`-tagged error for an xAI endpoint not already on priority now shows a dedicated dialog (`ProviderBusyDialog`) offering "Switch to priority tier (2x cost)" or "Dismiss", instead of just plain error text. Verified end-to-end in a live TUI session (a stub agent process standing in for forge-agent to reproduce the capacity error on demand): the menu row appears only for xAI endpoints, toggling it persists via the real `update_xai_priority_tier` message and reverts cleanly, and the busy dialog appears and resolves correctly both ways.

### Added (opt-in xAI priority processing tier)

- **`ModelEndpoint.xai_priority_tier` (default off) — when set, every request to that endpoint adds `service_tier: "priority"`, which xAI bills at 2x its standard per-token rate in exchange for higher scheduling priority during high demand.** Off by default and per-endpoint, since not every xAI key has priority access and it's a real, provider-billed cost increase, not a Forge one. New `update_xai_priority_tier` incoming message (mirrors `update_endpoint_reasoning`'s shape) to flip it at runtime; `EndpointInfo.xai_priority_tier` reports current state so a client can reflect it. Also fixed non-streaming OpenAI-compatible errors (`chat_openai`) to include the response body, not just the bare status code — the streaming path already did this, but a provider's actual rejection reason (e.g. "at capacity") was being silently dropped on this path.

### Added (rejections from an overloaded provider are now distinguishable from a generic API error)

- **A provider rejecting a request because it's at capacity (xAI's `resource-exhausted`/429 "at capacity", or an equivalent from another OpenAI-compatible provider) looked identical to any other API failure** — same generic "API error: ..." text, no way for a client to tell "this specific request was malformed" apart from "there was simply no room to serve it right now, try later or pay for priority." Now tagged with a distinct, stable `Provider at capacity: ...` prefix when detected, so a client can offer a relevant action (e.g. switching that endpoint to the priority tier above) instead of just showing a red error.

### Added (`ToolRequest` now says whether it actually needs approval)

- **`needs_approval: bool` added to `ToolRequest`, computed from the session's real trust settings** (`--dangerously-allow-all`, auto-mode, `auto_approve_writes`/`_reads`) at the exact same point the agent itself decides whether to block — not a guess. Previously a client had no way to know this and had to infer it from `kind` alone (typically "read = auto-approved, everything else = pending"), which had no visibility into those settings at all: a write/execute call under `--dangerously-allow-all` rendered as a permanently "awaiting approval" card that nothing was ever going to answer, even though the agent was never actually blocked on it — indistinguishable, from the user's side, from the agent genuinely being stuck. A client should now trust this field directly instead of re-deriving it.

### Fixed (`shell_exec` could falsely report "waiting for input" on ordinary command output)

- **The interactive-prompt heuristic (`looks_like_prompt`) fired immediately on any single PTY read chunk ending in a colon** — but a PTY read's chunk boundary can land anywhere, including right after a grep/ripgrep match or a compiler diagnostic's own "file:line:" (`src/main.rs:454:`), or mid-line after ordinary code content that happens to end in a colon (`ui.label(egui:`). Neither is an actual prompt, but the command just kept running normally regardless — which also meant `input_waiting` got stuck true (silently disabling the timeout/auto-background check) for the rest of that command's run, since the only other place it reset was a "user provided input" action no client sends yet. Fixed two ways: (1) `looks_like_prompt` now excludes the shape where the colon is preceded by a line number, and (2) more fundamentally, a prompt-shaped chunk is no longer confirmed immediately — it arms a candidate that's only actually reported via `ProcessInputNeeded` if 350ms pass with no further output, and cleared (self-healing `input_waiting` too) the moment more output arrives. A genuine interactive prompt is followed by silence; ordinary tool output isn't. Verified with a new permanent regression test (`prompt_heuristic_ignores_file_line_colons`) plus the existing suite (38 passing).

### Added (`SubagentStarted` now says which subagent nested it, if any)

- **`parent_id: Option<String>` added to `SubagentStarted`** — `Some(parent_slot_id)` when a subagent spawned this one via its own `delegate_task` call (nesting), `None` for a top-level one spawned by the main agent. The nested subagent's own id already encoded this implicitly (`"parent_slot_id:tool_call_id"`), but a client had no explicit, robust way to use that without parsing the id string. Omitted from the JSON when absent, so existing clients are unaffected.

### Fixed (a subagent's tool result was truncated to 200 characters before a client ever saw it)

- **`agent/subagent.rs` capped the `result` field of a subagent's `ToolResult` event at 200 characters** — a leftover from before that event was rendered as detailed content in a client; the top-level agent's equivalent event has never truncated it. A client showing the full result (a big read, a long search, a diff) would silently see it cut off no matter what it did on its own end, since the data was already gone by the time it arrived. Fixed by sending the full result, matching the top level. The short 200-char version is still used internally for the subagent's own final-summary bookkeeping, where a short version is actually correct.

### Fixed (a subagent's read-only tool calls never produced a `ToolRequest`/`ToolResult` at all)

- **A subagent doing pure read-only work (`read_file`, `search_code`, `list_directory`, `glob_files`) sent no `ToolRequest`/`ToolResult` events for any of it** — only Write/Execute/Unknown-kind calls did. A client following a subagent's activity through those events (rather than the coarser `SubagentStatus` line) would see it apparently do nothing at all, even while it was actively reading through the codebase. The top-level agent has never had this gap — `core.rs`'s own tool-call handling always sends both events regardless of kind. Fixed by sending them unconditionally from `agent/subagent.rs` too, same as the top level; approval is still only requested for Write/Execute/Unknown; reads are still auto-approved. Verified with the existing test suite (37 passing).

### Added (`ToolRequest`/`ToolResult` now say which subagent they belong to)

- **`subagent_id: Option<String>` added to the `ToolRequest`/`ToolResult` wire messages** — `Some(slot_id)` when the call came from a running subagent, `None` for the top-level agent's own calls. Previously a subagent's tool activity arrived over the same event stream as everything else with no way to tell it apart from the top-level agent's, or from a *different* concurrently-running subagent's — a client had no way to give a subagent its own dedicated view. Omitted from the JSON entirely when absent (`skip_serializing_if`), so existing clients that don't know about the field are unaffected.

### Fixed (a tool call's approval prompt could be answered by a click meant for a different one)

- **With two or more `delegate_task` subagents running concurrently (the normal way to parallelize multi-file work), approving or denying one subagent's pending write/execute could silently wave through a *different* subagent's next approval-gated call** — no real wait, no genuine decision for it, even though its own prompt had correctly shown up. Root cause: the parent has no cheap way to know which of several concurrently-running subagents a click was meant for, so it forwards every `approve`/`deny` to *all* of them (`agent/core.rs`'s Phase 3 select loop); each subagent's own wait then treated *any* approval message as approval for whatever it happened to be blocked on (`agent/subagent.rs`), regardless of which tool call actually requested it. `ApproveAction` already carried the real `tool_id` on the wire, but `DenyAction` didn't (only a `reason`), and nothing on either side actually checked the id against the pending call — a residue of the earlier "subagents could get stuck indefinitely" fix, which explicitly called this out as a known, not-yet-done gap. Fixed by adding `tool_id` to `DenyAction`'s wire shape and having every approval-wait site (top-level tool calls, `enter_plan_mode`, sequential and concurrent subagent dispatch) check the incoming id against the specific call it's blocked on, looping past anything addressed to someone else instead of treating it as an answer. Verified with the existing test suite (37 passing) plus manual review of all four wait sites; a client (Forge IDE included) must now send the `tool_id` it's actually responding to on both approve and deny.

### Added (xAI/Grok model auto-discovery)

- **Any `api.x.ai` endpoint configured in `config.toml` now gets its full model catalog auto-discovered on launch**, the same way ChatGPT Codex already does — instead of only ever showing the one model you'd manually typed in. Grouped by `(base_url, api_key)` (there's no OAuth/login concept for xAI, just whatever key a configured endpoint already has), queries the account's live `/v1/models`, adds any new models found (name auto-generated from the model id), updates context length on existing entries, and prunes ones no longer offered — only when the live query genuinely succeeded and returned at least one model, so a network hiccup can never wipe the list. Image/video-generation models (`grok-imagine-*`) are filtered out since they're not chat-completions models a text/tool-calling agent can use. Verified live against a real xAI account: discovered 6 chat models automatically, confirmed a synthetic stale model gets pruned, and confirmed `models.default` is untouched by any of this.
- **Generated display names now strip bare date-stamp segments** instead of showing them as a meaningless number — `grok-4.20-0309-reasoning` is now "Grok 4.20 Reasoning" rather than "Grok 4.20 0309 Reasoning". The rule (`looks_like_date_code`) is a catch-all, not a hardcoded xAI special case: a model-id segment that's a *bare* run of 4/6/8 digits (MMDD/YYMMDD/YYYYMMDD, however a provider chose to stamp it) is treated as a date and dropped, while dotted version numbers (`4.20`, `0.1`) are always left alone since a real version segment in these IDs is never dot-free. Existing config entries created by a prior run keep whatever name they already have (matching how Codex discovery already avoids clobbering a name you might have customized) — I manually regenerated the 3 already-misnamed entries on the live account this was found on, but any other affected setup would need the same one-time fix (delete the entry, let the next launch recreate it).

### Fixed (generic OpenAI-compatible endpoints never sent their API key)

- **Any endpoint with `endpoint_type = "open_ai"` and an `api_key` set — real OpenAI, OpenRouter, xAI/Grok, or any other authenticated cloud provider using the OpenAI wire format — silently never sent that key on any request.** `ApiClient::from_endpoint` built `Backend::OpenAi` from only `base_url`, dropping `api_key` entirely; every request (chat, streaming chat, `/models` auto-discovery, context-length probing) went out with no `Authorization` header at all. This was invisible for local/self-hosted servers (LM Studio, llama.cpp, Oxide) that don't require auth in the first place — which is presumably why it went unnoticed — but made it impossible to actually use any authenticated `open_ai`-type endpoint. Fixed by carrying `api_key` through `Backend::OpenAi` and adding it as a bearer token on every request that type makes.
- **Switching models mid-session (`switch_model`) also dropped the API key, independently of the bug above.** The client only ever receives `EndpointInfo` (deliberately excludes `api_key` — that field never leaves the server), but the `SwitchModel` handler was building a fresh `ModelEndpoint` straight from the incoming message's fields, hardcoding `api_key: None` instead of looking the real key up from the server's own config by name. Fixed to look it up from `app_config.models.endpoints` by name instead of trusting (and hardcoding around) the client-supplied message. Verified both fixes together against a local mock HTTP server requiring a bearer token: confirmed a 401 before either fix, still 401 after only the first fix (switching models re-lost the key), and a correctly-authenticated request after both.

### Fixed

- **Subagents could get stuck indefinitely, with no way to recover.** Three compounding bugs in `delegate_task`/subagent execution:
  - A subagent's own tool calls always required a fresh approval for write/execute tools, ignoring `--dangerously-allow-all`, auto-mode, and `auto_approve_writes` — the same trust settings the top-level agent already respects. Since every subagent (including read-only types like "explore") automatically gets `delegate_task` added to its own toolset, a subagent nesting another subagent would always block on an approval the session's trust settings should have skipped.
  - Cancelling a run while a subagent was active didn't actually stop it — it recorded a synthetic "cancelled" result but never aborted the real task, then unconditionally waited for that task to finish anyway. A genuinely stuck subagent (per the bug above) made Cancel hang too, with no recovery short of killing the process.
  - A nested subagent (a subagent calling `delegate_task` itself) reused its parent's id for its `subagent_started`/`subagent_finished` events. A client tracking subagents by id would see the *outer* subagent marked finished the moment the nested call completed, even though the outer subagent kept running afterward — surfacing as "subagent shows started, then nothing else happens."

  Subagents now respect the same trust settings as the top level, Cancel actually aborts stuck subagent tasks (`JoinSet::abort_all`), and nested subagents get their own unique id so they show up as independent, correctly-tracked entries. Verified live against all three scenarios (trusted-mode nesting, cancelling a genuinely stuck nested approval, and unique id assignment).

  **Known follow-up, not yet done:** `ApproveAction`/`DenyAction` still don't carry a real per-request identifier server-side (the wire `tool_id` is accepted but unused — any approval unblocks whatever's currently pending). This works correctly as long as only one thing is pending approval at a time, which is the common case, but two simultaneous pending approvals (e.g. two concurrent subagents both awaiting approval) aren't distinguished. Fixing this properly means a `ToolRequest`/`ApproveAction` wire-shape change affecting every client (Forge's own UI included), so it's deliberately out of scope here.

## [0.2.1] — 2026-07-01

### Fixed

- **Break the edit_file "old_string not found" death-spiral.** When an `edit_file` target string doesn't match, Forge now returns bounded recovery hints instead of a bare error: it flags whitespace-only differences, shows the single closest-matching region with line numbers (capped — never dumps the file), and on multiple matches lists the occurrence lines to disambiguate. This stops weaker local models from looping after a failed edit.
- **Token usage and auto-compaction restored for OpenAI-compatible streaming.** Forge now sends `stream_options: { include_usage: true }`, so spec-compliant servers (mlx_lm, vLLM, llama.cpp, LM Studio, OpenAI…) report token counts while streaming. Without it those servers sent no usage, leaving `/usage` and the context footer stuck at 0 and silently disabling auto-compaction — on a long local session context would grow unbounded until the model's real window overflowed.
- **Recover tool calls that misbehaving servers leak as raw text.** Some OpenAI-compatible servers (notably mlx_lm at high context) fail to parse a model's `<tool_call>` block into structured `tool_calls`, instead leaking the raw markup into the content/reasoning stream and ending the turn with no tool to run — which made the agent appear to stall, loop, or return an empty turn. Forge now recovers a complete leaked `<tool_call>` block as a real tool call (handling both the JSON/Hermes form and Qwen3-Coder's `<function=…><parameter=…>` XML dialect), gated so a genuine text answer or a properly-structured call is never affected.

### Changed

- Installer/launcher hardening: the `forge` wrapper now locates `bun` robustly (`~/.bun/bin/bun` or `PATH`, with a clear error if absent), and `install.sh` checks for `curl` and `ripgrep` up front (the web tools and `search_code` need them).

## [0.2.0] — 2026-06-25

### Removed

- **Claude subscription (Pro/Max) OAuth login — removed entirely (breaking).** The Claude OAuth flow (`forge --login` / `--login-claude` / the in-TUI `/login --anthropic`), the embedded Claude Code OAuth client id, the `claude-cli` user-agent and `claude-code` beta-header impersonation, the Claude token store (`~/.config/forge/auth.json`), and the weekly Claude `client_version` self-check are all gone. Forge no longer contains any code path that authenticates to Anthropic with subscription credentials.

  **Why:** Anthropic's Consumer Terms and the Claude Code legal terms restrict subscription OAuth tokens to Anthropic's own applications and prohibit routing requests through Free, Pro, or Max plan credentials in any other product, tool, or service. Forge had been authenticating to Anthropic with the Claude Code OAuth client and a `claude-cli` user-agent — i.e. using subscription credentials outside a native Anthropic app. We were not aware of this restriction until recently; this release removes the behavior outright to respect Anthropic's terms. The risk it avoided lands on the end user's Claude account (which can be flagged or suspended without notice), so removal is the right call. We will not reintroduce Anthropic subscription sign-in unless and until Anthropic permits it.

  **Anthropic is still fully supported via an API key** — set `endpoint_type = "anthropic"` with `api_key = "sk-ant-…"` in `~/.config/forge/config.toml` (or pick **Claude** in the installer wizard). **ChatGPT Codex** subscription login is unchanged and remains the only supported subscription sign-in.

### Added

- **Streaming reasoning display.** Reasoning ("thinking") models now show their chain-of-thought live as a compact `✻ Thinking… (elapsed · ~tokens)` line that settles into a persistent `✻ Thought for Xs` when the answer arrives — press **Ctrl+T** to expand or collapse it. This works with any OpenAI-compatible endpoint — local servers like LM Studio, Ollama, vLLM, or mlx_lm, as well as OpenAI-compatible APIs — that streams reasoning in a separate field (`reasoning_content`, `reasoning`, or `thinking`). Models or servers that don't send a separate reasoning field are unaffected; their output renders as normal.

### Changed

- The installer's **Claude** option now configures an Anthropic API-key endpoint instead of subscription OAuth, matching the auth change above.

### Fixed

- **Compatibility with strict OpenAI-compatible servers.** Forge injects some system-role messages mid-conversation (continuation nudges, plan-mode notes, etc.). Servers that require the system message to come first — notably `mlx_lm` — rejected those turns with `System message must be at the beginning`. Forge now keeps the leading system prompt and relocates later ones, so these servers work.
- Ctrl-key shortcuts (e.g. **Ctrl+F** copy mode, **Ctrl+T** expand reasoning) no longer leak their letter into the message input.

## [0.1.0] — 2026-06-18

Initial public release.

### Added

- Headless Rust agent (`forge-agent`) speaking a JSON-newline protocol on stdin/stdout
- Bun/Ink terminal UI (`forge`) that drives the agent
- Twelve built-in tools: read/write/edit files, apply unified diffs, list directory, search code, glob files, todo write, shell exec, web search, web fetch, delegate task
- Built-in agent definitions: bash, explore, general, plan
- Custom shell-backed tools loaded from `~/.config/forge/tools/` and `.agent/tools/`
- Custom Markdown agent definitions loaded from `~/.config/forge/agents/` and `.agent/agents/`
- Endpoint backends: OpenAI-compatible, Anthropic `/v1/messages`, ChatGPT Codex Responses API
- OAuth login for Claude (`forge --login`) and ChatGPT Codex (`forge --login-chatgpt`) subscriptions
- Direct API key support for Anthropic, OpenAI, OpenRouter, and custom OpenAI-compatible endpoints
- Paste-the-code OAuth fallback for environments where the localhost callback can't land (remote SSH without port forwarding, firewall restrictions, etc.)
- Live ChatGPT Codex model catalog discovery — no dependency on the official `codex` CLI
- Plan mode with explicit approval before edits
- Session persistence and `--resume-session`
- Git-backed per-turn snapshots and `/revert`
- LLM-backed context compaction and rolling-window context strategies
- Approval-based command gating with `--dangerously-allow-all` for trusted sessions
- Native installers for macOS, Linux, and Windows
- One-command bootstrap installers (`bootstrap.sh` / `bootstrap.ps1`)
- Five-way setup wizard: local LLM / Claude subscription / ChatGPT Codex subscription / direct API key / skip
- Cross-platform browser launching for OAuth flows (`open` on macOS, `xdg-open` on Linux/BSD, `cmd /c start` on Windows)

[Unreleased]: https://github.com/Vulkgryph/Forge/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/Vulkgryph/Forge/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Vulkgryph/Forge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Vulkgryph/Forge/releases/tag/v0.1.0
