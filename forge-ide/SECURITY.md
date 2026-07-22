# Security Policy

Forge IDE is maintained by Vulkgryph LLC. We take security issues seriously and appreciate responsible disclosure.

## Scope

This policy covers the Forge IDE binary (`forge-ide`), the remote daemon (`forge-server`), and the shared protocol crate (`forge-proto`) — all in this directory. It does **not** cover:

- `forge-agent`, which Forge IDE's agent panel spawns independently — see [`../forge-agent/SECURITY.md`](../forge-agent/SECURITY.md)
- Language servers, debug adapters, or other external tools Forge IDE connects to (e.g. `rust-analyzer`, `lldb-dap`, `debugpy`)
- Misuse of Forge IDE by an authenticated user against their own machine or their own SSH hosts
- Vulnerabilities in dependencies, unless Forge IDE's use of the dependency creates a new attack surface
- Third-party plugins (`.so`/`.dylib`/`.dll`) loaded via the plugin ABI — a plugin runs with the same privileges as the editor itself, by design

## Reporting a vulnerability

**Do not file a public GitHub issue for security vulnerabilities.**

Use one of the following private channels:

1. **GitHub Private Vulnerability Reporting** — preferred. Open a report at
   https://github.com/Vulkgryph/Forge/security/advisories/new
2. **Email** — `security@vulkgryph.com`

Please include:

- A clear description of the issue and its impact
- Steps to reproduce (proof-of-concept, minimal repro project, or commit/version)
- The Forge IDE version and platform, and whether SSH Remote was involved
- Any suggested mitigation, if you have one

## Response timeline

We aim to:

- Acknowledge your report within **5 business days**
- Provide an initial assessment within **14 days**
- Ship a fix or coordinated disclosure plan within **90 days** for confirmed high-severity issues

These are targets, not guarantees. Forge IDE is maintained by a small team and timelines may vary.

## Credit

If your report leads to a fix, you will be credited in the release notes and the commit that addresses it, unless you ask to remain anonymous.

## Out of scope

The following are explicitly **not** considered vulnerabilities:

- Forge IDE running shell commands, opening files, or executing `forge-server` on a remote host the user has explicitly authenticated to via SSH. This is the intended behavior of the Terminal and SSH Remote features.
- Plugins loaded via the plugin ABI behaving exactly as their native code dictates — plugins are not sandboxed, and loading one is an explicit user action.
- Resource exhaustion caused by user-initiated builds, tasks, or terminal commands.
- `forge-server` accepting commands over an SSH channel the user themselves established and authenticated.

Borderline cases — please report them anyway and let us decide.
