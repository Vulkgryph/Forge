# Security Policy

Forge TUI is maintained by Vulkgryph LLC. We take security issues seriously and appreciate responsible disclosure.

## Scope

This policy covers the terminal client in this directory (`forge-tui`) — its handling of `forge-agent`'s protocol, its local UI state, and its own dependencies. It does **not** cover:

- `forge-agent` itself — see [`../forge-agent/SECURITY.md`](../forge-agent/SECURITY.md)
- Third-party LLM endpoints, models, or providers reached through the agent it drives
- Misuse of Forge by an authenticated user against their own machine (see `forge-agent`'s Safety Model — Forge is a sharp tool by design)
- Vulnerabilities in dependencies, unless the TUI's use of the dependency creates a new attack surface

## Reporting a vulnerability

**Do not file a public GitHub issue for security vulnerabilities.**

Use one of the following private channels:

1. **GitHub Private Vulnerability Reporting** — preferred. Open a report at
   https://github.com/Vulkgryph/Forge/security/advisories/new
2. **Email** — `security@vulkgryph.com`

Please include:

- A clear description of the issue and its impact
- Steps to reproduce (proof-of-concept or minimal repro)
- Forge TUI's version/commit and platform
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

- The TUI faithfully surfacing whatever `forge-agent` reports (tool calls, shell output, file edits) — that's the agent's behavior, not the client's.
- Auto-approval modes doing exactly what they advertise.
- Resource exhaustion caused by user-approved commands or unbounded model output.

Borderline cases — please report them anyway and let us decide.
