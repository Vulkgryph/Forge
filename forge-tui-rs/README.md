# forge-tui-rs

The terminal client, installed as `forge`. It spawns `forge-agent --headless` and drives it over the JSON-newline protocol in [`forge-agent-proto`](../forge-agent-proto/) — the same protocol [`forge-ide`](../forge-ide/)'s agent panel speaks, independently. No agent logic lives here.

It replaced an earlier TypeScript client, [`forge-tui`](../forge-tui/), which is retired and no longer built.

## Running it

```bash
./forge-agent/install.sh   # builds and installs `forge`
forge                      # in whichever directory you want to work in
```

Or from a checkout, without installing:

```bash
cargo run -p forge-tui-rs
```

## What it is, and what it isn't

Rendered from scratch. There is no TUI framework underneath — it decodes its own escape sequences, measures its own text, wraps its own lines, and writes to the terminal directly. The reasons are in the module documentation of [`src/lib.rs`](src/lib.rs), which is the place to start reading, and each one is a failure the previous client had:

- **Text cannot overlap.** One module decides how wide text is, and both the wrapper and the cursor-advancer use it. Two implementations of that rule drift; one cannot.
- **Scrollback cannot be damaged.** It draws on the alternate screen, so the user's history is never Forge's to erase.
- **Frames cannot tear.** Each frame is diffed against what is on screen and written inside one synchronized update.
- **Idle costs nothing.** The loop blocks on input rather than repainting on a timer, so an untouched session uses no CPU. Several things here — the spinner, the reasoning line — animate only while a turn is running, deliberately.

## Keys

| Key | Does |
|---|---|
| `Enter` | Send |
| `\` + `Enter`, or `Ctrl-N` | Newline |
| `Up` / `Down` | Previous messages you sent, when the caret is on the first or last line |
| `Escape` | Take back an unanswered message, or interrupt a running turn |
| `Shift-Tab` | Step through permission modes |
| `Ctrl-T` | Expand or collapse reasoning |
| `Ctrl-Y` | Copy the agent's last message |
| `Ctrl-O` | Menu |
| `Ctrl-X` | Interrupt |
| `Ctrl-C`, `Ctrl-D` | Leave |

`/help` lists the commands.

## Tests

```bash
cargo test -p forge-tui-rs
```

Everything above the terminal is a pure state machine over an `Input` enum, so scrolling, wrapping, editing and layout are all testable without a terminal. That is most of the suite. What it cannot cover is how the result looks on a real screen — several bugs here were only visible when run for real, and one was invisible under tmux but wrong in Apple's Terminal. If a change affects rendering or input, say what you saw, at what width, in which terminal.

## Licence

[Apache 2.0](../LICENSE), copyright © 2026 Vulkgryph LLC.
