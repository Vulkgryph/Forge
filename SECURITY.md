# Security Policy

Forge is maintained by Vulkgryph LLC. We take security issues seriously and appreciate responsible disclosure.

## Scope

This policy covers everything in this monorepo:

- `forge-agent` — the agent binary, which is the only component that talks to an LLM or executes tools
- `forge-tui-rs` — the terminal client (`forge`)
- `forge-ide` — the editor, its terminal emulator, and its pty-host daemon (`forge-server`)
- `forge-agent-proto` — the shared wire protocol

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
- **The pty-host daemon (`forge-server`) outlives the editor** so terminals survive a reload. It listens on a unix socket under the user's config directory, and is reachable by any process running as that user.
- **`forge-ide` executes what its terminal is told to execute**, including from a file dropped onto it, which inserts that path into the shell line.
