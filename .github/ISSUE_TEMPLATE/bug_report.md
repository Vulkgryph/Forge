---
name: Bug report
about: A behavior in Forge that's wrong, unexpected, or broken
labels: bug
---

## Surface

Which part of Forge is affected? (Check all that apply.)

- [ ] `forge-agent` — the agent binary itself (model loop, tools, safety gating)
- [ ] `forge-tui-rs` — the terminal client (`forge` command)
- [ ] `forge-ide` — the graphical IDE
- [ ] `forge-server` — the SSH-remote daemon uploaded to remote hosts
- [ ] Not sure

## What happened

<!-- A clear, specific description of the problem. -->

## Steps to reproduce

1.
2.
3.

## What you expected

<!-- What you thought Forge would do instead. -->

## Environment

- Version:
  - `forge --version` (CLI):
  - Forge IDE build info (Help → About), if IDE-related:
- Operating system + version:
- LLM endpoint type: <!-- Anthropic / OpenAI / OpenRouter / ChatGPT Codex / local (LM Studio / Ollama / etc.) -->
- Install method: <!-- bootstrap.sh / bootstrap.ps1 / install.sh / install.ps1 / built from source / .dmg installer -->

## Logs

<!--
For the CLI: paste any relevant output from forge (stderr) or session logs from
~/.forge/sessions/<session-id>/conversation.jsonl.

For the IDE: paste any error dialog text, and if available include the
contents of a recent log file from the IDE's log directory.

Redact API keys and OAuth tokens before posting.
-->

```text
(paste logs here)
```

## Tried any workaround?

<!-- Optional — if you found something that works around the bug, sharing it helps us judge impact and may help others hitting the same issue. -->

## Suggested fix

<!--
Optional. Forge does not currently accept pull requests, but if you have a
clear idea of what the fix should look like (code snippet, patch, written
approach), include it here. If the suggested solution lands as the actual
fix, you'll be credited in the commit and release notes.
-->
