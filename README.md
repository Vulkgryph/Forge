# Forge

Created by **Vulkgryph LLC**.

Forge is an autonomous AI coding agent, plus two independent clients that drive it: a terminal UI and a full Vulkan-based code editor. This repository hosts all three as one monorepo.

## The three projects

| Project | What it is | Docs |
|---|---|---|
| [`forge-agent/`](forge-agent/) | The headless Rust agent — the actual model loop, tool execution, and safety gating. Everything else talks to this. | [README](forge-agent/README.md) · [Architecture](forge-agent/ARCHITECTURE.md) |
| [`forge-tui/`](forge-tui/) | The reference terminal client (Bun/React/Ink). Spawns `forge-agent --headless` and drives it over its JSON protocol. | [README](forge-tui/README.md) |
| [`forge-ide/`](forge-ide/) | A Rust/Vulkan code editor with an integrated agent panel — spawns the same `forge-agent --headless` process independently, alongside its own editor, git, LSP, and SSH-remote features. | [README](forge-ide/README.md) |

## How they fit together

`forge-agent` is the only piece that talks to an LLM or touches tools directly. It exposes one thing: a JSON-newline protocol over stdin/stdout (`forge-agent --headless`), documented in [`forge-agent/ARCHITECTURE.md`](forge-agent/ARCHITECTURE.md). `forge-tui` and `forge-ide` are two separate, independent implementations of a client against that same protocol — neither depends on the other, and neither reimplements any agent logic. This means the agent's actual behavior (tool execution, model calls, safety gating) can never diverge between the two clients, since it's the literal same compiled binary in both cases.

What *can* diverge is each client's own hand-maintained mirror of the wire protocol's shape (`forge-tui`'s `protocol.ts` zod schemas vs. `forge-ide`'s Rust structs) — there's no shared schema generating both, so a protocol change has to be applied by hand in each client that needs it.

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
      │     forge-tui       │          │       forge-ide        │
      │  terminal client    │          │  Vulkan code editor    │
      │  (Bun/React/Ink)    │          │  (Rust/egui), with its │
      │                     │          │  own editor/git/LSP/   │
      │                     │          │  SSH-remote features   │
      └─────────────────────┘          └─────────────────────┘
```

## License

All three projects are licensed under the [Apache License, Version 2.0](LICENSE), copyright © 2026 Vulkgryph LLC. See each project's own `LICENSE`/`NOTICE` for its own copy, and `SECURITY.md` for how to report a vulnerability in that specific project.

## Contributing

None of the three projects currently accept pull requests — each is maintained by Vulkgryph LLC with contributions closed to keep maintenance scope constrained. Issues are welcome on each project; see the relevant README for details.
