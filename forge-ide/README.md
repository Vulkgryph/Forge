# Forge IDE

Created by **Vulkgryph LLC**.

A fast, native code editor built in Rust on Vulkan and [egui](https://github.com/emilk/egui). Forge IDE aims to cover the same daily workflow as VS Code — editing, git, language servers, a terminal, and remote development over SSH — in a smaller, more inspectable codebase. Its integrated agent panel spawns [`forge-agent`](../forge-agent/) directly — the same binary [`forge-tui`](../forge-tui/) drives independently. See the [top-level README](../README.md) for how the three projects in this monorepo fit together.

## Philosophy

Forge IDE is built for engineers who want to see and understand the tool they use all day. The rendering pipeline is Vulkan because almost everything runs Vulkan — there's no browser, no Electron, no JavaScript runtime underneath. What you see in the source is what runs on your GPU.

**Dependencies are kept deliberately low.** Every crate in `Cargo.toml` is there because it does real, hard-to-replace work — a Vulkan binding, a windowing layer, an SSH implementation. Where a dependency was only doing something trivial (tilde expansion, an unused trait), it's been replaced with a few lines of our own code instead. This will continue over time: expect dependencies to keep shrinking, not grow, as the project matures.

**Inspection is the point.** The source is here. If something looks wrong, file an issue. If you want to take it in a different direction, fork it.

**On limitations.** This is young software built by a small team. It has not been used at the scale VS Code has, and it will have rough edges that only real usage surfaces. If something breaks — a keybinding that doesn't fire, a remote connection that hangs, a panel that renders wrong — file an issue with what you were doing. That's the fastest way it gets fixed.

## Features

- **Editor** — multi-tab, syntax highlighting, line numbers, undo, find/replace, multi-file search, split editors, multi-cursor editing, bracket matching + pair colorization, minimap, indent guides, auto-close brackets, word wrap, go-to-line
- **LSP** — full language server support (diagnostics, completions, hover, go-to-definition, find references, rename, code actions, signature help, formatting, symbol outline). Ships wired up for `rust-analyzer`; any LSP-compliant server works.
- **Git** — status-colored file tree, staged/unstaged source control panel, inline diff gutter, unified diff view, commit/push/pull/fetch, inline blame, `gh`-backed "Publish to GitHub," and a `~/.ssh/config`-integrated remote picker
- **SSH Remote** — a small Rust daemon (`forge-server`) auto-uploads itself to the remote machine over SFTP and speaks JSON-RPC back over the SSH channel. Remote file tree, remote terminal, and remote file editing all route through it — no VS Code Server–style background install step, and it works fully offline on the remote end (nothing is downloaded there).
- **Debugging** — DAP client (breakpoints, step over/in/out, call stack, variables) against `lldb-dap`, `debugpy`, or any DAP-compliant adapter
- **Terminal** — a real PTY with VT100/ANSI support, plus an Output panel for build/task/connection logs
- **Task runner** — named tasks in `.forge/tasks.toml`, run from a picker, output streamed live
- **Forge Agent** — an optional AI coding assistant panel with multi-tab conversations and persistent history
- **Multi-window** — open more than one workspace or remote host at once
- **Themes & settings** — font size, tab width, word wrap, and a theme picker (Dark+, Light+, Monokai, One Dark), all in `~/.config/forge-ide/settings.toml`
- **Plugins** — a minimal C ABI for dynamically loaded `.so`/`.dylib`/`.dll` plugins, surfaced in the command palette

## Building

Forge IDE is a Cargo workspace with three crates:

| Crate | What it is |
|---|---|
| `forge-ide` (root) | The IDE itself — the binary you run |
| `forge-server` | The remote daemon uploaded to SSH hosts |
| `forge-proto` | Shared JSON-RPC types between the two |

### Requirements

- **Rust** (stable, 2024 edition)
- **Vulkan** — on macOS this means [MoltenVK](https://github.com/KhronosGroup/MoltenVK); a copy is bundled under `runtime/macos/` and picked up automatically. Linux and Windows use their native Vulkan drivers.
- **A GLSL compiler toolchain** for `shaderc` (used at build time to compile the egui shaders) — this typically means `cmake` and a C++ compiler are available.

### Build the IDE

```bash
cargo build -p forge-ide --release
cargo run -p forge-ide --release
```

### Build the remote server (optional — only needed for SSH Remote)

`forge-server` runs on the machine you SSH into, not on your own machine. It needs to be cross-compiled to Linux even if you're developing on macOS:

```bash
# One-time setup (macOS): install musl cross-compilers
brew install FiloSottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Build both architectures
cargo build -p forge-server --release --target x86_64-unknown-linux-musl
cargo build -p forge-server --release --target aarch64-unknown-linux-musl
```

The linker paths for these targets are already configured in `.cargo/config.toml`. Forge IDE looks for these binaries under `target/<triple>/release/forge-server` and uploads whichever one matches the remote machine's architecture the first time you connect. If forge-server's version changes, bump both `forge-server/Cargo.toml`'s `version` and the matching `SERVER_VERSION` constant in `src/ssh.rs` — that's what tells the client to re-upload instead of reusing what's already on the remote.

## SSH Remote

Click the `><` indicator in the bottom-left of the status bar (or a saved host from your `~/.ssh/config`) to connect. On first connect to a given machine, Forge IDE:

1. Authenticates using your SSH key, SSH agent, or a password prompt (in that order)
2. Checks whether `forge-server` is already present and up to date on the remote
3. If not, uploads the correct static binary for that machine's architecture via SFTP — no compiler, package manager, or internet access required *on the remote machine*
4. Starts `forge-server --stdio` over an SSH exec channel and speaks JSON-RPC to it for the rest of the session

Everything — the remote file tree, remote file open/save, and the remote terminal — goes through that one channel. Nothing else touches the remote machine's network.

## Status

Forge IDE is pre-1.0 and under active development. Expect breaking changes to settings file formats and keybindings while things stabilize.

## License

Forge IDE is licensed under the [Apache License, Version 2.0](LICENSE). See the [NOTICE](NOTICE) file for attribution.

Copyright © 2026 Vulkgryph LLC.

For security issues, see [SECURITY.md](SECURITY.md) — please do not file public issues for vulnerabilities.

### Disclaimer

Forge IDE is provided **"AS IS"**, without warranty of any kind, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement.

Forge IDE reads, writes, and executes operations on the user's machine and, when SSH Remote is used, on remote machines the user connects to. It can modify or delete files, run arbitrary shell commands in its integrated terminal, execute git operations including pushes, upload and run the `forge-server` binary on remote hosts, and load native plugin code with full process privileges. In no event shall Vulkgryph LLC or any contributor be liable for any claim, damages, or other liability — whether in contract, tort, or otherwise — arising from the use of Forge IDE, including but not limited to:

- Lost, corrupted, or overwritten files
- Unintended git operations (commits, pushes, or history changes)
- System damage or unintended state changes on local or remote machines
- Commands executed in the integrated terminal or by a loaded plugin that the user did not anticipate
- Leaked credentials, secrets, or SSH keys via terminal output, plugin behavior, or session logs
- Indirect, incidental, special, consequential, or punitive damages of any kind

Use of Forge IDE implies acceptance of these terms. The full legal language is in [LICENSE](LICENSE), which is the binding document; the plain-English summary above is provided for clarity, not as a replacement.
