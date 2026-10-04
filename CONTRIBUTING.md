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
| [`forge-agent/`](forge-agent/) | The agent. The only component that executes tools. |
| [`forge-tui-rs/`](forge-tui-rs/) | The terminal client, installed as `forge`. |
| [`forge-ide/`](forge-ide/) | The editor, plus its terminal emulator, its `forge-server` pty host, and the model proxy that lends a credentialled endpoint to a remote agent. |
| [`forge-agent-proto/`](forge-agent-proto/) | The wire protocol shared by the agent and the terminal client. |
| [`forge-search/`](forge-search/) | The crawler and index behind `web_search` and `search_documents`, and the Europe PMC client behind `search_papers`. |
| [`forge-ide/forge-proto/`](forge-ide/forge-proto/) | The JSON-RPC protocol shared by `forge-ide` and the `forge-server` pty host. |

The three that people install — `forge-agent`, `forge-tui-rs`, `forge-ide` — are
versioned together and each keeps a `CHANGELOG.md`. The rest are internal and
keep none: a change to one is described in the changelog of whatever it changed
for. `forge-search` has no changelog of its own for that reason, and its
user-visible behaviour is recorded under `forge-agent`, where the tools that
expose it live.

Internal does not mean the version never moves. `forge-server` is at `0.1.7` and
is bumped when its wire behaviour changes, because `forge-ide` compiles the
number into `SERVER_VERSION` and a remote running an older daemon has to be
recognised as older. It is simply not bumped *with* a release.

`default-members` is `forge-agent` alone, so a bare `cargo build` does not pull in the editor's GPU stack. Build the others explicitly:

```bash
cargo test -p forge-agent -p forge-agent-proto -p forge-search -p forge-tui-rs
cargo test -p forge-ide -p forge-proto -p forge-server   # needs a GPU-capable toolchain
```

That is all seven workspace members. No CI job runs `cargo test --workspace`:
the macOS job runs `-p forge-ide -p forge-proto -p forge-server`, and the Linux
job runs the other four. Running it locally is still the way to cover
everything at once.

## What CI enforces

- Those tests, on pushes to `main` and on pull requests
- `RUSTFLAGS=-D warnings` — the crates are at zero warnings and should stay there
- That `forge-agent/install.sh`, `forge-agent/update.sh` and `bootstrap.sh` still parse, and that every repo path the bootstrap installers name actually exists — `bash -n` passes on a bootstrap whose filenames have gone stale, which is how the one-command install broke once
- That the macOS app bundle still builds and contains both binaries
- That no documented version literal is pinned to a number that drifts — a version printed in the docs or sent as a user agent has to be derived, not typed
- That `NOTICE` names every licence in the shipped dependency graph that is not MIT or Apache-2.0 — a new dependency with unusual terms fails the build rather than waiting for an audit
- That documentation links resolve to a file that exists (vendored trees excluded; the checker strips `#fragments`, so anchors are not machine-verified)
- On Windows: the protocol, search and the terminal client run their full suites, and the agent builds and runs its provider fault matrix — not its unit tests. That much gates. The editor **is** built there, but as a `continue-on-error` step that cannot fail the build, so a Windows regression in `forge-ide` will still not be caught by CI.
- That no file in the tree names anybody's home directory — `scripts/check_private.py --tracked`. `%USERPROFILE%`, `$HOME` or a relative path instead: an absolute path through someone's home publishes their username and only works on their machine. Placeholders (`/Users/someone`, `/home/sysadmin`, `C:\Users\<username>`) are recognised and allowed, which is why this can read source and not only prose.

## Keeping names and machines out of what we publish

Four things can carry an identity out of here, and the same checker covers all of them from one list of names:

| surface | what runs it |
| --- | --- |
| the tracked tree | CI, on every push and pull request |
| commit messages | the `pre-push` hook |
| release binaries | `package_macos.sh`, before anything is signed |

**Enable the hook once per clone** — hooks are not part of a checkout:

```sh
git config core.hooksPath scripts/githooks
```

It refuses a push that would publish a home directory or a name, and scans commit messages as well as files. A username leaked here twice; the second time was in the message of the very commit that removed the first, so files alone are not enough. `git push --no-verify` overrides it when you have looked and it is wrong.

None of those three is an audit. They read the tree as it stands and the range being pushed, which is what they are for — but a path deleted in a later commit stays in the history forever, and a file that was fixed is not a file that was never wrong. `scripts/check_private.py --history` reads every version of every file ever committed plus every commit message; run it after a rewrite, or before showing the repository to anyone.

**Names that must never appear** go in `.git/private-names`, one per line. That path is deliberate: `.git` is never part of a commit, so the list cannot itself become the leak — which a list of protected names checked into a public repository would be. CI has no such file and no developer identity to compare against, so it applies the generic rule only; the identity-specific half is the hook's job.

**A release binary is not covered by reading the source.** Every `panic!`, `unwrap` and slice index stores its source location in `.rodata` as a string literal for `core::panic::Location`, and `strip` does not remove them — the 0.6.0 disk image shipped about nine hundred copies of the builder's home path. `--remap-path-prefix` fixes those. It does **not** fix a path that arrives as data rather than as a source location: `env!("CARGO_MANIFEST_DIR")` and anything a build script bakes in (openssl-sys records its install directory) are ordinary strings. Those need the compile-time paths themselves to be anonymous, which is why release builds use a target directory outside `$HOME`. If you add a dependency that compiles native code, the packaging guard is what will tell you.

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
