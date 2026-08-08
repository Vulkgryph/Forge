# Changelog

All notable changes to Forge TUI are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Forge TUI adheres to [Semantic Versioning](https://semver.org/).

Forge TUI lived as `ui/` inside the combined `forge` repository before this monorepo split. Its history from before the split is mixed into [`../forge-agent/CHANGELOG.md`](../forge-agent/CHANGELOG.md) alongside the agent's own entries — this file starts fresh from the split forward.

## [Unreleased]

### Fixed (finished replies could leave a massive blank gap before the footer)

- **After some turns completed, the transcript showed the assistant text, then dozens of empty lines, then `Thought for` / `Worked for` / the input prompt.** Root cause was markdown **tables**: each row was an Ink horizontal `Box` of many `<Text>` cells with space-padding. When the row was wider than the terminal, Ink wrapped mid-row into a tall block of near-empty lines; the live-region height estimator only counted the short source `| col |` lines, so a huge table stayed in the live (non-`<Static>`) region and the next erase-to-end repaint left a permanent blank gap under the message.
- **Fix:** tables render as single monospaced lines, clamped to the terminal width (cells truncate with `…`). Very wide / many-column tables fall back to a stacked `label: value` list instead of overflowing. `estimateEntryLines` also counts pipe-table blocks closer to their rendered height so tall replies archive out of the live region sooner.
