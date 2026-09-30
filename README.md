# Forge

Created by **Vulkgryph LLC**.

Forge is an autonomous AI coding agent, plus two independent clients that drive it: a terminal UI and a native code editor. This repository hosts them as one monorepo.

![Forge IDE editing forge-agent, with the agent panel showing tool calls, a diffstat and a rewind checkpoint](forge-ide/assets/forge-ide.png)

> _Forge IDE editing `forge-agent`, with the agent panel mid-task: tool calls folded into a checklist, an edit with its diffstat and an **Open diff** action, and a checkpoint you can rewind to._

![Forge's terminal client building a no_std Rust VM from scratch: plan mode, an approved plan, then the crate written and its tests run](forge-tui-rs/assets/forge-tui-demo.gif)

_Forge building a `no_std` Rust VM from an empty directory: it asks what kind of VM is wanted, plans in plan mode, and — once the plan is approved with auto-accept — writes the crate, runs `cargo fmt`, `cargo test` and `cargo run`, and reports what it verified._

## Why this rather than something else

Forge exists because the alternatives make a different trade, not because they
are bad. What is different here:

**The agent is a separate program, not a feature of an editor.** `forge-agent`
runs headless and speaks a documented JSON-lines protocol
([`forge-agent-proto`](forge-agent-proto/)). The terminal client and the editor
are two independent programs that drive it, and neither embeds the other — the
[diagram below](#how-they-fit-together) is the real architecture, not a
description of modules. So the same agent is available over SSH in a terminal
and in a window with a file tree, you can script it directly, and a third
client is a protocol away rather than a fork.

**The editor is written, not forked.** `forge-ide` is a native Rust application
using egui — not a VS Code fork and not Electron. That has costs, and they are
listed honestly in [Platforms](#platforms): one supported platform, and a
smaller feature surface than an editor with a decade of extensions behind it.
What it buys is no upstream to track, and an agent panel that is part of the
editor rather than an extension in a sandbox — checkpoints, diffs and rewind
reach the buffers directly.

**Dependencies are decisions here.** The terminal client declares two
third-party crates directly, both Unicode tables — serde and serde_json reach
it through the shared protocol crate, thirteen in the built graph. Every part of
its rendering, wrapping, markdown and diffing is in this repository. The editor
decodes PNG and GIF with decoders written for it, which replaced the `image`
crate and three of its transitive dependencies. No
third-party binary ships in this repository at all.

**Permission is the design, not a setting.** Every tool call is approvable;
plan mode is read-only until you approve a plan; auto-accept is a mode the
status bar shows and you can leave; the agent gets a scratchpad of its own so
throwaway work does not land in your project; and a rewind checkpoint lets you
put a file back after the agent has edited it. The failure mode being designed
against is an agent that has already done something you would not have allowed.

**Your keys, your machine.** Any OpenAI-compatible endpoint works, nothing is
routed through a Vulkgryph service, and offline mode turns off the tools that
reach the network. The first-party network calls are a weekly GitHub releases
check for updates — one in each client — and, only when ChatGPT Codex is the
configured endpoint, a model-catalog fetch and a GitHub lookup of the Codex
client version. `agent.offline_mode` turns all of them off.

**It tells you what does not work.** The changelog says when a fix of ours was
incomplete and review caught it, and the security section names the releases an
SSRF was shipped in rather than the ones it was fixed in. Platform support is a
table of what has and has not been run by a person, not a list of logos. That is
the standard the rest of the documentation is held to as well.

### Where it is the wrong choice

If you want an editor with a mature extension ecosystem, a team workflow with
pull requests, or Windows and Linux support for the GUI, this is not it —
contributions are closed, and only macOS is verified. The agent and the
terminal client are more portable than the editor; the table in
[Platforms](#platforms) says exactly how far each has been taken.

## The projects

| Project | What it is | Docs |
|---|---|---|
| [`forge-agent/`](forge-agent/) | The headless Rust agent — the actual model loop, tool execution, and safety gating. Everything else talks to this. | [README](forge-agent/README.md) · [Architecture](forge-agent/ARCHITECTURE.md) |
| [`forge-tui-rs/`](forge-tui-rs/) | The terminal client, installed as `forge`. Spawns `forge-agent --headless` and drives it over its JSON protocol. | [README](forge-tui-rs/README.md) |
| [`forge-ide/`](forge-ide/) | A native code editor with an integrated agent panel — spawns the same `forge-agent --headless` process independently, alongside its own editor, git, LSP, and SSH-remote features. | [README](forge-ide/README.md) |
| [`forge-agent-proto/`](forge-agent-proto/) | The wire protocol shared by the agent and the terminal client. | — |

## How they fit together

`forge-agent` is the only piece that runs the model loop or touches tools directly. One exception, in the other direction: in remote development `forge-ide` proxies the model endpoint so the credential never leaves your machine ([`forge-ide/src/model_proxy.rs`](forge-ide/src/model_proxy.rs)) — the outbound request to the provider is made by the editor, and the agent holds no provider credential at all. It exposes one thing: a JSON-newline protocol over stdin/stdout (`forge-agent --headless`), documented in [`forge-agent/ARCHITECTURE.md`](forge-agent/ARCHITECTURE.md). `forge-tui-rs` and `forge-ide` are two separate, independent implementations of a client against that same protocol — neither depends on the other, and neither reimplements any agent logic. This means the agent's actual behavior (tool execution, model calls, safety gating) can never diverge between the two clients, since it's the literal same compiled binary in both cases.

What *can* diverge is each client's own view of the wire protocol's shape. The terminal client shares [`forge-agent-proto`](forge-agent-proto/) with the agent, so those two cannot drift; `forge-ide` keeps its own hand-maintained Rust structs, so a protocol change has to be applied there by hand.

```text
                   ┌───────────────────────┐
                   │      forge-agent      │
                   │  (the model loop,     │
                   │  tools, safety gate)  │
                   └───────────┬───────────┘
                               │  JSON-newline protocol, stdin/stdout
               ┌───────────────┴────────────────┐
               │                                │
    ┌──────────┴──────────┐        ┌────────────┴────────────┐
    │    forge-tui-rs     │        │        forge-ide        │
    │  terminal client    │        │  native code editor     │
    │  (Rust), installed  │        │  (Rust/egui), with its  │
    │  as `forge`         │        │  own editor/git/LSP/    │
    │                     │        │  SSH-remote features    │
    └─────────────────────┘        └─────────────────────────┘
```

## Install / defaults

On **Ubuntu/Debian x86-64**, run `bash install.sh` from the source directory.
It installs missing build tools, both interfaces, and branded application-menu
entries. See [Linux installation](LINUX.md) for options and tested scope.

For the local native Windows development installer, see [Windows installation](WINDOWS.md).
From an existing checkout, double-click `install.cmd` to build and install both
interfaces and the shared agent. Windows support is under development; the
Windows guide records its current limitations.

This monorepo is the **canonical** Forge source. The standalone `forge` and `Forge-IDE` checkouts are retired.

```bash
# from a checkout of this repo:
./forge-agent/install.sh
# or later:
forge-update
```

The older terminal-only installer above installs:

| Command | Points at |
|---|---|
| `forge` | `forge-tui-rs`, built from this workspace and installed as `~/.local/share/forge/bin/forge` |
| `forge-agent` | built from this workspace and installed as `~/.local/share/forge/bin/forge-agent` |
| `forge-update` | `forge-agent/update.sh` in this repo |

With that terminal-only installer, `forge-ide` is built separately. The root Linux
and Windows installers include it by default.

On macOS there is also a notarized `.dmg` on the [releases page](https://github.com/Vulkgryph/Forge/releases) if you would rather not build the editor yourself. It is signed with Vulkgryph LLC's Developer ID and notarized by Apple, so it opens without the unidentified-developer warning. The terminal client is not distributed that way — `forge-agent/install.sh` builds it from this checkout.

## Platforms

**macOS is the supported platform.** It is where Forge is developed and used
every day, and the only one where any of this has been verified by a person
using it. Linux and Windows compatibility is unverified — see the table for what
that means per component.

Architecture matters here as much as the operating system, and the two Linux
columns have never been the same machine: CI runs on x86-64, and the Linux box
this is actually used against is ARM64.

| | macOS (Apple Silicon) | Linux x86-64 | Linux ARM64 | Windows x86-64 |
|---|---|---|---|---|
| `forge-agent` | supported | builds and passes tests in CI | runs headless on a remote | builds in CI; the provider fault matrix passes there, the unit suite is not run |
| `forge-search` | supported | builds and passes tests in CI | untested | builds and passes tests in CI |
| `forge-agent-proto` | supported | builds and passes tests in CI | untested | builds and passes tests in CI |
| `forge-tui-rs` (`forge`) | supported | builds and passes tests in CI | untested | builds and passes tests in CI |
| `forge-ide` | supported | untested | untested | attempted in CI as a non-gating step; nobody has watched it draw a frame |
| `forge-server` | n/a — runs on the remote | cross-compiled, never run | runs headless on a remote | attempted in CI as a non-gating step, never run |

"Builds and passes tests in CI" means exactly that and no more: a machine
compiled it and its tests passed. It does not mean a person has used it. Where
a person has, the word is "supported". CI runs on every push to `main` and on every pull request — see
[`ci.yml`](.github/workflows/ci.yml) for which crates each platform covers.

One feature is narrower than its component. When a bot check refuses the
crawler, the agent can offer the page to a person, who opens it in a real
browser and hands it back — and that browser is `WKWebView`, so it exists on
macOS and nowhere else. The agent is told at startup whether its client can
open a page at all, so on every other platform it says the site refused an
automated request and answers from what else it has, rather than offering a
handoff nothing can satisfy. A Linux or Windows port of the editor would need
its own web view before that feature came with it.

**supported** — developed and used on this platform every day, which is the
evidence behind the word. CI coverage is not what it rests on, and differs by
component: `forge-ide` is built and its tests run on a macOS runner on every
push; `forge-agent` is built on macOS by the packaging job but its test suite
runs on Linux; `forge-tui-rs` is built and tested on Linux and Windows.

**builds and passes tests in CI** — a machine compiled it and its tests passed.
Nobody has sat in front of it there. A build that works and a program that
behaves are different claims, and only the first one is being made.

**attempted in CI as a non-gating step** — the build runs but is
`continue-on-error`, so it reports rather than gates: a regression there will not
fail the workflow. Used for the editor on Windows, where nothing had ever built
it on a runner and the point was to find out.

**runs headless on a remote** — the agent and the file/pty server are uploaded to
a Linux machine and driven over SSH by remote development, exercised regularly
against an **aarch64** host (`Linux 6.11 aarch64`). That is real use, but it is
unattended: no terminal of its own, no window, no keyboard. It is also the only
ARM64 Linux evidence there is — nothing on that column has been through CI.

**cross-compiled, never run** — the x86-64 musl binary is built by the packaging
script and shipped in the app bundle, so an x86-64 remote would receive it, and
no such remote has ever been connected to.

On macOS, only Apple Silicon: the binary this repository builds is arm64, and
Intel Macs are untested. Rosetta is not a substitute for having tried it.

**untested** means literally that: nobody has run it. It is kept distinct from
"builds and passes tests in CI", which means a machine compiled it and its tests
passed and nothing more. The editor renders through wgpu — D3D12 on Windows,
Vulkan on Linux — and takes its window from winit, so there is no known reason
it cannot work; it is built on Windows in CI as a non-gating
step — which is how a `cfg` bug making `forge-server` uncompilable there was
found, and which also means a Windows regression in the editor will not fail the
build. Nobody has watched it draw a frame off macOS. A report that it does not run is worth filing.

The macOS app bundle, its signing, and the "add to Dock" option are macOS-only by
nature. Remote development is exercised from a macOS host to a Linux remote; the
reverse has never been run.

## Model providers

Forge talks to any OpenAI-compatible endpoint, to Anthropic, and to a local
model server, in each case with an API key you supply. It also supports signing
in to a **ChatGPT Codex** subscription, which is worth understanding before you
rely on it:

- The flow is OAuth against OpenAI's own endpoints (`forge-agent
  --login-chatgpt`, or the wizard in the editor). The token it returns is
  stored at `~/.config/forge/chatgpt_auth.json`, readable only by you, and
  refreshed automatically.
- OpenAI does not publish an OAuth integration for third-party clients, so this
  drives a consumer subscription through an interface documented for OpenAI's
  own tools. It works today. It is not a sanctioned integration, and it could
  stop working, or be objected to, at any point — in which case this project
  will comply and remove it.
- The risk of an objection lands on your OpenAI account rather than on this
  project: a consumer subscription driven through an unsanctioned client can be
  flagged or suspended without notice. That is exactly why Anthropic
  subscription login was removed outright rather than kept — see
  [forge-agent's CHANGELOG](forge-agent/CHANGELOG.md).
- If that matters to you, use an API key. Every other provider path is a
  documented, sanctioned one.

xAI has OAuth of the same shape for SuperGrok and X Premium+ subscriptions, and
Forge deliberately does **not** implement it: xAI's consumer terms prohibit
programmatic access and reverse engineering, and route developer use to their
Enterprise terms with an API key. Grok works here through an API key.

## Search is ours now

`web_search` used to scrape DuckDuckGo's HTML, which challenges automated
queries and refused most of them. A tool that usually returns nothing is worse
than one that is absent: the model spends a turn on it and then reasons about
the emptiness as if it meant something. So it was off by default, with a note
saying "off until there is a real search behind it".

There is one now, in `forge-search` — a crawler, an inverted index, BM25
ranking and snippets, with no dependencies. It answers from pages Forge has
actually read. The cost is that it only knows what it has crawled, so a query
the index cannot answer crawls first, which takes up to about two and a half
minutes on a first crawl at the default of 120 pages — the budget scales with
`max_pages` and is capped at 300 seconds — and milliseconds afterwards. Pass `sites` to aim the crawl somewhere specific
rather than rephrasing the query.

It is **on by default** now. Anyone who ran an earlier version has
`disabled_tools = ["web_search"]` written to their config already, and that is
indistinguishable from having chosen it, so it is left alone — the tools menu
in either client turns it back on.

Crawling has limits that are not bugs. `robots.txt` is obeyed, including
`Crawl-delay`, and requests to one host are spaced a second apart by default.
Sites that refuse crawlers are refused. PubMed Central disallows most of its
site but explicitly allows `/articles/` and `/api/` with a one-second
`Crawl-delay`, so its full text is crawlable — `search_papers` uses the Europe
PMC API anyway, because a published API beats crawling even where crawling is
permitted. The point stands where it actually applies:
its `robots.txt` is `User-agent: *` and `Disallow: /`.

### If Forge has been at your site

It identifies itself, and the version tells you which build:

```
forge-search/<version> (+https://vulkgryph.com/projects/forge/)
forge-agent/<version> (+https://vulkgryph.com/projects/forge/)
```

The first is the crawler, the second fetches a single page somebody asked for
by name. Neither pretends to be a browser and neither ignores `robots.txt`, so
`Disallow` is enough to stop them — from one user at a page a second, which is
the scale this runs at.

**What a site learns about the person running Forge: nothing that
distinguishes them from anyone else running it.** The complete set of headers
sent is `Host`, `Accept`, and the user agent above — measured against a local
listener, not read off the source, and asserted in the test suite so a fourth
header cannot appear without the claim failing. The crawler's `Accept` is
reqwest's default `*/*`; `web_fetch` asks for HTML, because it is fetching a
page somebody named. No cookies, no `Referer`, no `Accept-Language`, no
`Accept-Encoding`, no client hints, no machine or session identifier. The agent's own session id travels only to the model
endpoint the user configured, never to a crawled site.

Two things do vary. The **IP address**, which is true of any HTTP client and
which Forge will not hide — routing around it would be the kind of evasion this
project declines. And **which pages get requested**, which is inherent to
crawling.

One thing deliberately does *not* vary: on a re-crawl, Forge returns
`If-Modified-Since` but **not** `If-None-Match`. `Last-Modified` is a property
of the content — every visitor sees the same value, so echoing it identifies
nobody. An `ETag` is chosen by the server and can be minted per visitor, which
is a known tracking technique. Most of the bandwidth saving comes from the
harmless one, so that is the default; `agent.send_etag = true` opts in to the
other where a site issues no `Last-Modified`.

A user agent is only a claim, though, and anyone can write one. If the
operator has configured a signing key, requests also carry a **Web Bot Auth**
signature — `draft-meunier-webbotauth-httpsig-protocol`, built on RFC 9421
HTTP Message Signatures — so you can check the claim instead of weighing it:

```
Signature-Agent: sig1="https://example.com"
Signature-Input: sig1=("@authority" "signature-agent";key="sig1")
                 ;created=…;expires=…;keyid=…;alg="ed25519";nonce=…
                 ;tag="web-bot-auth"
Signature: sig1=:…:
```

The public key is published at
`/.well-known/http-message-signatures-directory` on the origin named by
`Signature-Agent`, and `keyid` is that key's JWK thumbprint. Cloudflare
validates these at its edge as part of its Verified Bots programme, so a site
behind it can allow or refuse this crawler by identity rather than by
guesswork. Unsigned is the default — a signature only means something once the
directory is published.

If something is wrong anyway, **contact@vulkgryph.com**. Worth saying what
counts as wrong: crawling you said not to, requests faster than `Crawl-delay`,
or anything that looks like it is pretending to be something else. Those would
be defects, and we would want to know.

## Literature, through the front door

Which is why there is a second tool. `search_papers` asks Europe PMC's REST
API — the channel published for programmatic access — rather than crawling a
website that has asked not to be crawled. It searches, fetches the full text of
articles whose licence permits keeping it, and indexes that; anything else
comes back as a citation and a link, and says so.

Only open-access articles have their text kept. That is more conservative than
it has to be, and deliberately so: it is one sentence to state and one
condition to audit. Every result reports the licence its text is held under,
because a passage quoted without its terms is a passage quoted blind — `cc by`
wants attribution, `cc by-nc` excludes commercial use, and the index carries
that with the document so it survives being saved and searched a month later.

This is what the crawler could not do. Asked for pyramidal neuron patch-clamp
recordings at a stated temperature, it answers out of a Methods section:

> We performed electrophysiological recordings at room temperature
> (20°C-25°C), but the recording chamber might be heated to
> near-physiological temperatures using a bath-controller

`web_fetch` is unaffected: give it a URL and it fetches that page.

## Using this safely

Forge is a sharp tool. It reads, writes, and executes code on your machine with
your user account's permissions. There is no sandbox — approval prompts are the
barrier, and auto-approval modes remove them on purpose. `forge-ide` also
uploads a helper binary to any SSH host you connect to and runs it there.

Read the [Safety Model](forge-agent/README.md#safety-model) before turning on
anything that skips approval, and the per-component disclaimers in
[forge-agent](forge-agent/README.md) and [forge-ide](forge-ide/README.md).

Forge is provided **"AS IS"**, without warranty of any kind, and you are
responsible for what you approve it to do. [LICENSE](LICENSE) is the binding
document; [SECURITY.md](SECURITY.md) is how to report a vulnerability.

## How this is built

Forge is built with the assistance of AI coding tools, and is directed,
reviewed and released by Vulkgryph LLC. See [NOTICE](NOTICE) for that and for
the third-party attributions.

## License

Everything here is licensed under the [Apache License, Version 2.0](LICENSE), copyright © 2026 Vulkgryph LLC. See each project's own `LICENSE`/`NOTICE` for its own copy, and `SECURITY.md` for how to report a vulnerability in that specific project.

## Contributing

None of the three projects currently accept pull requests — each is maintained by Vulkgryph LLC with contributions closed to keep maintenance scope constrained. Issues are welcome on each project; see the relevant README for details.
