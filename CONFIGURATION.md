# Configuring Forge

Everything lives in one file:

```
~/.config/forge/config.toml
```

It is read at startup. Settings that can be changed while running — the model,
the permission mode — are changed from the interface and written back here, so
hand edits and the editor's own changes land in the same place.

**The file holds API keys.** It is yours alone; nothing in this repository
reads it, and it must never be pasted into an issue, a commit, or a release
note. See [RELEASING.md](RELEASING.md) for what else must not be published.

## Endpoints

A model Forge can talk to. There can be as many as you like; `default` names
the one used at startup.

```toml
[models]
default = "Claude Sonnet 4.6"

[[models.endpoints]]
name     = "Claude Sonnet 4.6"       # what you see in the model picker
base_url = "https://api.anthropic.com"
model_id = "claude-sonnet-4-6"
api_key  = "…"                       # omit to read it from the environment
endpoint_type = "anthropic"          # "openai" (default) or "anthropic"
max_context_tokens  = 200000
max_output_tokens   = 16384          # per response; default 16384
request_timeout_secs = 500           # default 500
```

`endpoint_type` is the **wire format**, not the vendor: anything that speaks
the OpenAI chat-completions shape — vLLM, llama.cpp, LM Studio, Ollama, mlx —
is `openai`, whoever made the model.

### `model_id = "auto"`

For a local server where the loaded model changes, set `model_id = "auto"` (or
omit it). Forge asks the endpoint's `/v1/models` at startup and uses the first
model it reports, so you do not have to edit this file every time you load a
different checkpoint.

### `web_tool_model`

```toml
[models]
web_tool_model = "Qwen3.8-27B (local M5)"
```

Names an endpoint to summarise fetched web pages, by `name`. Unset means the
main model does it. Pointing it at something small and local keeps page
summarisation off a metered API.

### `xai_priority_tier`

```toml
xai_priority_tier = true
```

Opts into xAI's priority tier: better scheduling under load, at **twice the
per-token price**. Only sent when the endpoint's `base_url` actually looks like
xAI's, so it is a harmless no-op elsewhere — but it is real money where it
does apply.

## Reasoning

Per endpoint, because providers disagree about everything here.

```toml
[models.endpoints.reasoning.open_ai_compatible]
thinking          = "provider_default"   # "on" | "off" | "provider_default"
preserve_thinking = "provider_default"

[models.endpoints.reasoning.anthropic]
thinking      = "on"
budget_tokens = 4096

[models.endpoints.reasoning.chatgpt_codex]
effort = "medium"                        # none | low | medium | high | xhigh
```

`provider_default` means *send nothing and let the server decide*, which is
not the same as `off`.

**`preserve_thinking` is the one that surprises people.** A model that reasons
in the open writes its thinking into the same channel as its answer, wrapped in
`<think>…</think>`. Left at the default, Forge strips those blocks. Set to
`"on"`, it leaves them in — and they then read as part of the reply, because
nothing has labelled them as anything else.

The better fix is on the server. A local server given a reasoning parser —
for vLLM, `--reasoning-parser <name>` — sends the thinking in its own field
instead, and Forge labels it as thinking wherever it appears. Start the server
that way and none of this matters. The same applies to tools: without
`--enable-auto-tool-choice` and `--tool-call-parser <name>`, a vLLM server
refuses every request that carries tools, and the agent cannot act at all.

## The agent

```toml
[agent]
permission_mode  = "default"   # default | accept_edits | bypass_permissions | dont_ask | plan
context_strategy = "compaction"  # compaction | rolling_window
compact_at_percent = 80
max_history_messages = 0       # 0 = no cap; let the strategy decide
disabled_tools = []
min_shell_timeout_secs      = 0
forced_shell_background_secs = 0
thinking_mode = false
```

### Permission modes

| Mode | What it does |
|---|---|
| `default` | Asks before anything that writes or runs |
| `accept_edits` | File edits go through; commands still ask |
| `dont_ask` | Stops asking, but still refuses what it is not allowed to do |
| `bypass_permissions` | No gate at all — it can change and run anything |
| `plan` | Read and explore only; it cannot change anything until a plan is approved |

`bypass_permissions` is the one spelled "dangerously skip all" in the
interface, and the name is the honest one. It is appropriate in a container or
a scratch checkout. On a machine with anything you care about, it means an
agent can run a command you would not have approved, and the first you hear of
it is afterwards.

### Context strategy

- **`compaction`** — at `compact_at_percent` of the window, the conversation is
  summarised and the summary continues in place of the history. Keeps long
  sessions coherent; costs a model call at the boundary.
- **`rolling_window`** — oldest messages are dropped as new ones arrive. Costs
  nothing and forgets permanently.

`compact_at_percent = 80` leaves a fifth of the window for the model to write
the summary in. Raising it leaves less room and risks a compaction that cannot
finish; lowering it compacts more often than it needs to.

### Shell timeouts

`min_shell_timeout_secs` raises the floor on how long a command is given before
it is treated as hung — useful where builds are slow.
`forced_shell_background_secs` sends anything still running after that long to
the background instead, so a long build does not hold the turn.

### `disabled_tools`

```toml
disabled_tools = ["web_search", "web_fetch"]
```

Names tools to withhold. They are removed from what the model is offered, not
merely refused when called, so it does not keep trying.

## Subagents and the scratchpad

```toml
[agent.subagents]
# how many may run at once, and how deep they may nest

[agent.scratchpad]
# where a subagent may write working files
```

Nesting depth and concurrency bound how much a single request can turn into.
Both are worth setting deliberately before running anything unattended.

## Web bot authentication

```toml
[agent]
send_etag = true
web_bot_auth_key       = "…"
web_bot_auth_directory = "…"
```

Signs outbound web requests so a site can verify the fetch came from this
install rather than an impersonator. The key is generated on first use with
owner-only permissions. Optional; sites that do not check it do not care.

## Where else settings live

- `~/.config/forge/` — this file, and credentials
- `~/Library/Application Support/forge-ide/` (macOS) — window layout,
  conversations, the editor's own `settings.toml`

The editor's `settings.toml` is appearance and layout: font size, theme, which
panels are open. It is written by the editor and does not need hand editing.

Uninstalling leaves both alone unless you pass `--purge`; see `uninstall.sh`.
