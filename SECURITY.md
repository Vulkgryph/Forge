# Security Policy

Forge is maintained by Vulkgryph LLC. We take security issues seriously and appreciate responsible disclosure.

## Scope

This policy covers the Forge agent (`forge-agent` binary) and the terminal UI (`ui/`). It does **not** cover:

- Third-party LLM endpoints, models, or providers used through Forge
- Misuse of Forge by an authenticated user against their own machine (see the Safety Model section in the README — Forge is a sharp tool by design)
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
- The Forge version (`forge --version` or commit SHA) and platform
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

- Forge running shell commands or modifying files that the operating system permits the launching user to access. This is the intended behavior and is documented in the README's Safety Model section.
- Auto-approval modes (`--dangerously-allow-all`, etc.) doing exactly what they advertise.
- Prompt-injection results that depend on the user pasting untrusted content into the LLM context. We are interested in **novel injection paths** (e.g. tool output that escalates beyond the approval boundary), not generic prompt injection.
- Resource exhaustion caused by user-approved commands or unbounded model output.

Borderline cases — please report them anyway and let us decide.
