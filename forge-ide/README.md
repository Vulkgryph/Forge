# Forge IDE

Created by **Vulkgryph LLC**.

A fast, native code editor built in Rust on Vulkan and [egui](https://github.com/emilk/egui). Forge IDE aims to cover the same daily workflow as VS Code — editing, git, language servers, a terminal, and remote development over SSH — in a smaller, more inspectable codebase. Its integrated agent panel spawns [`forge-agent`](../forge-agent/) directly — the same binary [`forge-tui-rs`](../forge-tui-rs/) drives independently. See the [top-level README](../README.md) for how the three projects in this monorepo fit together.

## Philosophy

Forge IDE is built for engineers who want to see and understand the tool they use all day. The rendering pipeline is Vulkan because almost everything runs Vulkan — there's no browser, no Electron, no JavaScript runtime underneath. What you see in the source is what runs on your GPU.

**Dependencies are kept deliberately low.** Every crate in `Cargo.toml` is there because it does real, hard-to-replace work — a graphics binding, a windowing layer, an SSH implementation. Where a dependency was only doing something trivial (tilde expansion, an unused trait), it has been replaced with a few lines of our own code instead.

**winit is the one exception, and it is vendored.** It is the only third-party source this repository redistributes rather than fetching at build time, because it sits at [`vendor/winit-0.30.13/`](vendor/) with two macOS patches — drag-and-drop accepting the pasteboard type Apple has recommended since 10.13, and a way to supply the Dock icon's right-click menu, which upstream cannot express because winit owns the application delegate. Both patches exist only because that delegate is not ours. For scale, if anyone is considering doing without it: winit's macOS backend is ~6,500 lines and the `egui-winit` input translation another ~2,300. See [NOTICE](../NOTICE) for the attribution this arrangement requires.

**Inspection is the point.** The source is here. If something looks wrong, file an issue. If you want to take it in a different direction, fork it.

**On limitations.** This is young software. We have not run it at the scale VS Code has, and it will have rough edges that only real usage surfaces. If something breaks — a keybinding that doesn't fire, a remote connection that hangs, a panel that renders wrong — file an issue with what you were doing. That's the fastest way it gets fixed.

## Features

- **Editor** — multi-tab, syntax highlighting, line numbers, undo, find/replace, multi-file search, split editors, multi-cursor editing, bracket matching + pair colorization, minimap, indent guides, auto-close brackets, word wrap, go-to-line
- **LSP** — full language server support (diagnostics, completions, hover, go-to-definition, find references, rename, code actions, signature help, formatting, symbol outline). Ships wired up for `rust-analyzer`; any LSP-compliant server works.
- **Git** — status-colored file tree, staged/unstaged source control panel, inline diff gutter, unified diff view, commit/push/pull/fetch, inline blame, `gh`-backed "Publish to GitHub," and a `~/.ssh/config`-integrated remote picker
- **SSH Remote** — a small Rust daemon (`forge-server`) uploads itself to the remote machine over SFTP, runs there, and speaks JSON-RPC back over the SSH channel. Remote file tree, remote terminal, and remote file editing all route through it — no VS Code Server–style background install step, and it works fully offline on the remote end (nothing is downloaded there).
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

- **macOS on Apple Silicon.** It is the only platform this has been run on, and
  the binary built here is arm64 — Intel Macs are untested too. Linux (either
  architecture) and Windows are unverified: the renderer goes through wgpu and
  the window through winit, so there is no known reason it cannot work, but
  nobody has tried it and a report that it does not build is expected rather than
  surprising. See the [platform table](../README.md#platforms).
- **Rust** (stable, 2024 edition)
- **Vulkan** — only for the optional `vulkan-renderer` build; the default renderer is wgpu and needs none of this. On macOS Vulkan means [MoltenVK](https://github.com/KhronosGroup/MoltenVK), which is **not bundled** — this repository ships no third-party binaries. Install it with `brew install molten-vk`, or drop your own `libMoltenVK.dylib` at `runtime/macos/`. Linux and Windows use their native Vulkan drivers.
- **A GLSL compiler toolchain** for `shaderc` (used at build time to compile the egui shaders) — this typically means `cmake` and a C++ compiler are available.

### Build the IDE

```bash
cargo build -p forge-ide --release
cargo run -p forge-ide --release
```

### The app bundle is not notarized

`scripts/package_macos.sh` signs the bundle with a Developer ID if one is in
your keychain and ad-hoc signs it otherwise. Neither is notarized by Apple.

A bundle you did not build yourself will therefore be refused on first open —
*"cannot be opened because the developer cannot be verified"*. Right-click →
**Open**, or **System Settings → Privacy & Security → Open Anyway**, allows it.
If that is not a trade you want to make, build from source: it is the two
`cargo build` commands above.

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

Everything — the remote file tree, remote file open/save, and the remote terminal — goes through that one channel.

One thing does open a port over there, and it is worth knowing about: when the agent runs on the remote and this machine lends it a model endpoint, SSH forwards a port on the remote's **loopback** back here, and Forge adds your API credential on this side so the key never reaches that machine. Loopback is not privacy — other processes and other local users on that host can reach it — so each session issues a random token, hands it to the agent as the endpoint's key, and the tunnel refuses any request that does not present it. The token grants use of the tunnel only, is never written to the remote's disk, and dies with the session.

### What lands on the remote

Two static binaries and a version marker beside each, under `~/.forge/`:

| Path | What | Size |
|---|---|---|
| `~/.forge/forge-server` | the file/pty/LSP server this editor talks to | ~1.2 MB |
| `~/.forge/forge-agent` | the agent, so its tool calls are local to the machine you are working on rather than a round trip back | ~6 MB |

Uploaded on demand, not at connect: a session that never opens the agent panel never pays for the agent. Each is replaced when this client's version moves on, which the marker beside it is for. Removing the directory is safe and costs one re-upload.

Two names, one directory, worth knowing about: a *workspace*'s own `.forge/` holds that project's session logs, so if you open a remote home directory as a workspace, both live in `~/.forge/` — the binaries alongside a `sessions/` folder. Nothing collides, but clearing session history there triggers a re-upload.

Where VS Code would use `~/.forge-server/`, this uses `~/.forge/`. Deliberate: renaming it now would re-upload on every remote's first connect and leave the old directory behind, for no behaviour anyone would notice.

No `sudo`, no compiler, and nothing written outside `~/.forge/` except a workspace's own `.forge/` when the agent runs there. Forge itself never invokes a package manager.

One exception, which is the agent rather than Forge: remote revert runs on git, so before the agent modifies files on a remote host it checks that git is present and that the directory is inside a worktree. If git is missing it must ask you before installing it — including when `--dangerously-allow-all` is active, because that flag waives your approval prompts and not the policy of a machine that may not be yours. Decline and the agent keeps working, but it has to tell you revert is unavailable for that path first, and it is told to prefer the file tools over shell commands: Forge snapshots every file its own tools write, with or without git, and cannot recover a file a shell command changed.

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
