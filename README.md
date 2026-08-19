# Forge

Created by **Vulkgryph LLC**.

Forge is an autonomous AI coding agent, plus two independent clients that drive it: a terminal UI and a native code editor. This repository hosts them as one monorepo.

## The projects

| Project | What it is | Docs |
|---|---|---|
| [`forge-agent/`](forge-agent/) | The headless Rust agent — the actual model loop, tool execution, and safety gating. Everything else talks to this. | [README](forge-agent/README.md) · [Architecture](forge-agent/ARCHITECTURE.md) |
| [`forge-tui-rs/`](forge-tui-rs/) | The terminal client, installed as `forge`. Spawns `forge-agent --headless` and drives it over its JSON protocol. | [README](forge-tui-rs/README.md) |
| [`forge-ide/`](forge-ide/) | A native code editor with an integrated agent panel — spawns the same `forge-agent --headless` process independently, alongside its own editor, git, LSP, and SSH-remote features. | [README](forge-ide/README.md) |
| [`forge-agent-proto/`](forge-agent-proto/) | The wire protocol shared by the agent and the terminal client. | — |

## How they fit together

`forge-agent` is the only piece that talks to an LLM or touches tools directly. It exposes one thing: a JSON-newline protocol over stdin/stdout (`forge-agent --headless`), documented in [`forge-agent/ARCHITECTURE.md`](forge-agent/ARCHITECTURE.md). `forge-tui-rs` and `forge-ide` are two separate, independent implementations of a client against that same protocol — neither depends on the other, and neither reimplements any agent logic. This means the agent's actual behavior (tool execution, model calls, safety gating) can never diverge between the two clients, since it's the literal same compiled binary in both cases.

What *can* diverge is each client's own view of the wire protocol's shape. The terminal client shares [`forge-agent-proto`](forge-agent-proto/) with the agent, so those two cannot drift; `forge-ide` keeps its own hand-maintained Rust structs, so a protocol change has to be applied there by hand.

```text
                    ┌───────────────────────┐
                    │      forge-agent       │
                    │  (the model loop,      │
                    │   tools, safety gate)  │
                    └───────────┬───────────┘
                                │  JSON-newline protocol,
                                │  stdin/stdout
                ┌───────────────┴───────────────┐
                │                                │
      ┌─────────┴─────────┐          ┌───────────┴───────────┐
      │    forge-tui-rs     │          │       forge-ide        │
      │  terminal client    │          │  native code editor    │
      │  (Rust), installed  │          │  (Rust/egui), with its │
      │  as `forge`         │          │  own editor/git/LSP/   │
      │                     │          │  SSH-remote features   │
      └─────────────────────┘          └─────────────────────┘
```

## Install / defaults

This monorepo is the **canonical** Forge source. The standalone `forge` and `Forge-IDE` checkouts are retired.

```bash
# from a checkout of this repo:
./forge-agent/install.sh
# or later:
forge-update
```

That installs:

| Command | Points at |
|---|---|
| `forge` | `forge-tui-rs`, built from this workspace and installed as `~/.local/share/forge/bin/forge` |
| `forge-agent` | `target/release/forge-agent` from this workspace |
| `forge-update` | `forge-agent/update.sh` in this repo |

`forge-ide` is optional and built separately (`cargo build -p forge-ide`) when you want the editor; it is not part of the default PATH install.

## License

All three projects are licensed under the [Apache License, Version 2.0](LICENSE), copyright © 2026 Vulkgryph LLC. See each project's own `LICENSE`/`NOTICE` for its own copy, and `SECURITY.md` for how to report a vulnerability in that specific project.

## Contributing

None of the three projects currently accept pull requests — each is maintained by Vulkgryph LLC with contributions closed to keep maintenance scope constrained. Issues are welcome on each project; see the relevant README for details.
