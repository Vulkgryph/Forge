# Changelog

All notable changes to Forge are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Forge adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/Vulkgryph/Forge/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Vulkgryph/Forge/releases/tag/v0.1.0
