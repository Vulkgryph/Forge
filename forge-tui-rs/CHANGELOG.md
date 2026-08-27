# Changelog

All notable changes to the Forge terminal client are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **The agent's last stderr lines could be lost.** `Exited` was sent by the thread
  reading stdout, with no coordination with the thread reading stderr, so the two
  raced — and `main.rs` treats `Exited` as the end of the session and stops
  listening. The lines this loses are the ones at the end, which is exactly when
  they matter: during OAuth login the agent prints the URL and instructions to
  stderr and then exits. The exit is now announced only after stderr has drained,
  or after half a second if the child is holding stderr open, so it cannot stall
  the exit either. Found by CI on Linux; macOS scheduling had been hiding it.

## [0.3.0] — 2026-08-26

First release of the Rust terminal client. It replaces `forge-tui`, the earlier TypeScript one, which is retired and removed from the repository — see the [README](README.md) for why it was rewritten rather than repaired.

Versioned with `forge-agent` and `forge-ide` rather than starting at 0.1.0, since the three ship together and a reader comparing them should not have to work out which numbers correspond.

### Added

- Renders itself: escape-sequence decoding, text measurement, wrapping and cursor movement are all its own, with no TUI framework underneath. Each of the four properties this buys — text cannot overlap, scrollback cannot be damaged, frames cannot tear, idle costs nothing — is a failure the previous client had, and each is prevented structurally rather than patched. The reasoning is in `src/lib.rs`.
- Slash commands, a menu, model and reasoning selection, permission modes including plan mode, rewind, session resume, and subagent display.
- `Up`/`Down` walk back through messages already sent, from the first or last line of the input; typing makes the line yours again so the next `Up` starts from the newest.
- `Escape` takes back a message the agent has not answered yet, so a typo noticed after `Enter` can be corrected rather than retyped. Only while nothing has come back but thought — a written reply, a tool call or a question all mean it has been acted on.
- `Ctrl-Y` and `/copy` put the agent's last message on the system clipboard, via the platform helper where there is one and OSC 52 otherwise, which is the only route that reaches the right machine over SSH.
- Tool results are summarised rather than printed whole: a `read_file` shows the line its own output already leads with, and other results show their first lines and say how many they are not showing. Todo lists render as a checklist and are never truncated.

### Fixed

- Reflow on terminal resize, including multi-line input and paragraphs that grow from one row to three.
- Paste detection, so a multi-line paste arrives as one message rather than sending on every newline. Apple's Terminal does not implement bracketed paste, so this is a heuristic on the read chunk.
- Continuous backspace, caret movement within a message, and a bounded rolling window over long input.
- The transcript's "Thinking…" line turns while a block is live, and the status line stops repeating it — two spinners were giving one state two different names.
