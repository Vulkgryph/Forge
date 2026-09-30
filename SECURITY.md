# Security Policy

Forge is maintained by Vulkgryph LLC. We take security issues seriously and appreciate responsible disclosure.

## Scope

This policy covers everything in this monorepo. Two components also keep a
more detailed policy of their own, linked below; this file is the map, and
reporting here is always correct.

- `forge-agent` — the agent binary, the only component that executes tools. Detail: [`forge-agent/SECURITY.md`](forge-agent/SECURITY.md)
- `forge-ide` — the editor, its terminal emulator, its pty-host daemon (`forge-server`), and its model proxy, which holds provider credentials and calls provider APIs on a remote agent's behalf. Detail: [`forge-ide/SECURITY.md`](forge-ide/SECURITY.md)
- `forge-tui-rs` — the terminal client (`forge`)
- `forge-search` — the crawler and index behind `web_search` and `search_documents`, and the Europe PMC client behind `search_papers`. This is the component that fetches and parses content from servers nobody controls, so it is the one most likely to be sent something hostile
- `forge-agent-proto` — the wire protocol shared by the agent and the terminal client
- `forge-proto` — the JSON-RPC protocol shared by `forge-ide` and `forge-server`, under `forge-ide/forge-proto/`

It does **not** cover:

- Third-party LLM endpoints, models, or providers used through Forge
- Misuse of Forge by an authenticated user against their own machine (see [Safety Model](forge-agent/README.md#safety-model) and [Using this safely](README.md#using-this-safely) — Forge is a sharp tool by design)
- Vulnerabilities in dependencies, unless Forge's use of the dependency creates a new attack surface

## Reporting a vulnerability

**Do not file a public GitHub issue for security vulnerabilities.**

Use one of the following private channels:

1. **GitHub Private Vulnerability Reporting** — preferred. Open a report at
   https://github.com/Vulkgryph/Forge/security/advisories/new
2. **Email** — `security@vulkgryph.com`

Please include:

- A clear description of the issue and its impact
- Steps to reproduce (proof-of-concept, minimal repro script, or commit/version)
- The Forge version (`forge --version` or commit SHA), which component, and your platform
- Any suggested mitigation, if you have one

## Response timeline

We aim to:

- Acknowledge your report within **5 business days**
- Provide an initial assessment within **14 days**
- Ship a fix or coordinated disclosure plan within **90 days** for confirmed high-severity issues

These are targets, not guarantees. Forge is maintained by a small team and timelines may vary.

## Credit

If your report leads to a fix, you will be credited in the release notes and the commit that addresses it, unless you ask to remain anonymous.

## Out of scope

The following are explicitly **not** considered vulnerabilities:

- Forge running shell commands or modifying files that the operating system permits the launching user to access. This is the intended behavior and is documented in [Safety Model](forge-agent/README.md#safety-model).
- Auto-approval modes (`--dangerously-allow-all`, the auto-accept and approve-everything permission modes) doing exactly what they advertise.
- Prompt-injection results that depend on the user pasting untrusted content into the LLM context. We are interested in **novel injection paths** (e.g. tool output that escalates beyond the approval boundary), not generic prompt injection.
- Resource exhaustion caused by user-approved commands or unbounded model output.

Borderline cases — please report them anyway and let us decide.

## Known behaviours worth knowing about

These are documented rather than hidden. They are consequences of the design, but a reader deserves to see them stated:

- **A command's stdin prompt is answered in the clear.** When a running command asks for input (`[sudo] password for …`), Forge shows a dialog and sends what you type to that process. What you type is echoed in the transcript and written to the session log under `.forge/sessions/`, which is not encrypted. Treat a session log as containing anything you typed at such a prompt.
- **Session logs hold the whole conversation**, including file contents and command output the agent read. They live in the project directory.
- **The editor's model proxy terminates an HTTP connection and replays it to your provider with your key attached.** When you lend a model endpoint to an agent on another machine, `forge-ide` listens on a loopback port, and SSH forwards the remote agent's requests back to it. The remote agent never holds a provider credential; this machine adds it. The listener is guarded by a random per-session token issued to that agent as an ephemeral `api_key`, because a loopback port on the remote host is reachable by every process and every other local user there. See `forge-ide/src/model_proxy.rs`.
- **The pty-host daemon (`forge-server`) outlives the editor** so terminals survive a reload. It listens on a unix socket under the user's config directory, and is reachable by any process running as that user.
- **`forge-ide` executes what its terminal is told to execute**, including from a file dropped onto it, which inserts that path into the shell line.
