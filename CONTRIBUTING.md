# Contributing

**Pull requests are closed.** Forge is maintained by Vulkgryph LLC with
contributions closed, to keep the maintenance scope constrained — see
[Contributing](README.md#contributing) for why. **Issues are welcome**, and so
is a fork.

That is said first because the rest of this file reads like a contribution
guide, and it would be unfair to let you work through it before mentioning it.
It is still worth having: it is what to follow if you fork, what a bug report
needs to be useful, and what the rules will be if pull requests open.

This file is short on purpose — it covers the things that are specific to this
repository, not general advice about writing Rust.

## Layout

| Crate | What it is |
|---|---|
| [`forge-agent/`](forge-agent/) | The agent. The only component that talks to an LLM or executes tools. |
| [`forge-tui-rs/`](forge-tui-rs/) | The terminal client, installed as `forge`. |
| [`forge-ide/`](forge-ide/) | The editor, plus its terminal emulator and `forge-server` pty host. |
| [`forge-agent-proto/`](forge-agent-proto/) | The wire protocol shared by the agent and the terminal client. |

`default-members` is `forge-agent` alone, so a bare `cargo build` does not pull in the editor's GPU stack. Build the others explicitly:

```bash
cargo test -p forge-agent -p forge-agent-proto -p forge-tui-rs
cargo test -p forge-ide            # needs a GPU-capable toolchain
```

## What CI enforces

- Those tests, on every push
- `RUSTFLAGS=-D warnings` — the crates are at zero warnings and should stay there
- That `install.sh` and `update.sh` still parse, and that the macOS app bundle still builds and contains both binaries

## House rules

**No third-party dependencies where we can reasonably write it ourselves.** The terminal client decodes its own escape sequences, does its own wrapping and grapheme measurement, and speaks to the terminal directly. This is deliberate. A pull request that adds a crate to do something we already do by hand will be asked to justify itself.

**Comments explain why, not what.** The code is readable; the reasoning is not. When you fix something, leave behind the sentence that stops the next person reinstating it — including the measurement, if there was one.

**Tests should fail for the right reason.** A test that passes whether or not the code under it ran proves nothing. When fixing a bug, check that the new test fails against the old behaviour before you keep it.

**Verify against something real.** Terminal work in particular has a way of passing every unit test and being wrong on screen: several bugs here were only visible when run in an actual terminal, and one of them was invisible under tmux but broken in Apple's Terminal. If a change affects rendering or input, say what you saw, at what width, in which terminal.

## Commit messages

One change per commit, with a message that leads with the problem it solves and the evidence that it does. The existing history is the reference — `git log` shows the shape. The [CHANGELOG](forge-agent/CHANGELOG.md) follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows [Semantic Versioning](https://semver.org/).

## Reporting bugs

Include the component, your platform and terminal, and what you saw versus what you expected. For anything involving rendering, a screenshot is worth more than a description.

Security issues do **not** go in the issue tracker — see [SECURITY.md](SECURITY.md).

## Licence

By contributing — should pull requests open, or by any other route — you agree
that your contributions are licensed under the
[Apache License 2.0](LICENSE), the same as the rest of the project.
