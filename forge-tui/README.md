# Forge TUI

Created by **Vulkgryph LLC**.

The reference terminal client for [`forge-agent`](../forge-agent/) — a React/Ink terminal application that spawns `forge-agent --headless` and drives it over its JSON-newline protocol. See the top-level [monorepo README](../README.md) for how this fits alongside `forge-ide`, the other client.

This client owns no agent logic of its own — every tool call, model response, and safety decision comes from `forge-agent`. What lives here is entirely the terminal UI: menus, approval dialogs, scrollback rendering, and the local state (permission mode, "approve always" memory, active subagent cards) needed to present that protocol usefully in a terminal.

## Requirements

- **[Bun](https://bun.sh)**
- A built `forge-agent` binary — see [`../forge-agent/README.md`](../forge-agent/README.md#installation). This client looks for it (in order): the `FORGE_AGENT_PATH` environment variable, the same directory as the running script, `../target/release/forge-agent` / `../../target/release/forge-agent` relative to it (debug as a fallback), then `PATH`.

## Development

```bash
bun install
bun run start -- --cwd /path/to/project
```

`bun run build` bundles to `dist/forge.js` (`bun build src/index.tsx --outfile dist/forge.js --target bun`).

Most users won't run this directly — the top-level `forge` wrapper script in `forge-agent/` launches it (see that project's README for the end-user install path).

## Keyboard shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Ctrl+N` &nbsp;*or*&nbsp; `\` then `Enter` | New line in input |
| `Ctrl+C` | Quit |
| `Escape` | Cancel current run when the agent is thinking |
| `Shift+Tab` | Cycle permission mode |

`Ctrl+N` and the trailing-backslash idiom are used instead of `Shift+Enter` because `Shift+Enter` isn't reliably distinguishable from plain `Enter` across terminal emulators.

## Slash commands

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
| `/login --chatgpt` | Start ChatGPT Codex OAuth login |
| `/help` | Show command help |

## Project layout

```text
forge-tui/
├── package.json, tsconfig.json, bun.lock
└── src/
    ├── index.tsx              Entry point
    ├── protocol.ts            Zod schemas for the complete forge-agent message surface
    ├── agent-bridge.ts         Spawns forge-agent --headless, validates and batches its output
    ├── model-display.ts       Endpoint/provider display-label helpers (e.g. isXaiEndpoint)
    ├── hooks/
    │   └── useAgent.ts         Main state/reducer hook — owns scrollback, approvals, questions,
    │                           plans, subagents, revert checkpoints, endpoints, permission mode
    └── components/
        ├── App.tsx             Main UI, slash commands, menu system
        ├── ApprovalDialog.tsx  Tool approval prompt
        ├── PlanApproval.tsx    Plan-mode approve/reject
        ├── ProviderBusyDialog.tsx  "Provider at capacity" — switch to priority tier or dismiss
        ├── SubagentStatus.tsx  Live subagent progress card
        └── MarkdownRenderer.tsx  Renders assistant output (via `marked`, not a custom parser)
```

## Contributions

Forge TUI does not currently accept pull requests — it's maintained by Vulkgryph LLC with contributions closed to keep maintenance scope constrained.

**Issues are welcome.** If you have a fix or suggestion, include it in the issue itself. If the suggested solution is used, you'll be credited in the commit and release notes.

For security issues, see [SECURITY.md](SECURITY.md) — please do not file public issues for vulnerabilities.

## License

Forge TUI is licensed under the [Apache License, Version 2.0](LICENSE). See the [NOTICE](NOTICE) file for attribution.

Copyright © 2026 Vulkgryph LLC.
