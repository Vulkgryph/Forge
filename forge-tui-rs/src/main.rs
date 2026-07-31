// SPDX-License-Identifier: Apache-2.0
//! The Forge terminal UI.
//!
//! Spawns `forge-agent --headless` and drives it over the shared protocol. See
//! `lib.rs` for why the rendering is built the way it is.
//!
//! The event loop folds two sources — the terminal and the agent — into a single
//! channel, one thread each. That is what keeps an idle session at no CPU: the
//! loop blocks on one `recv`, instead of polling the terminal on a timer and
//! waking dozens of times a second to find nothing happened.
//!
//! Everything already queued is drained before drawing, so a burst of streamed
//! tokens becomes one frame. The TypeScript client needed a 75 ms debounce for
//! that, because ink re-rendered per event; draining gets the same coalescing
//! with no latency floor.
//!
//! Run with `cargo run -p forge-tui-rs`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use forge_tui_rs::app::{App, Input, Outcome};
use forge_tui_rs::bridge::{AgentBridge, BridgeEvent};
use forge_tui_rs::screen::Screen;
use forge_tui_rs::session::Effect;
use forge_tui_rs::{input, term};

/// How long the agent gets to exit on its own before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

/// Cap on how many events are folded into one frame. A flood should still yield
/// to drawing rather than starving the screen.
const MAX_BATCH: usize = 512;

/// One thing that happened, from either source.
enum Event {
    Terminal(crossterm::event::Event),
    Agent(BridgeEvent),
    /// The terminal reader stopped — stdin is gone, so the session is over.
    InputClosed,
}

struct Options {
    cwd: Option<PathBuf>,
    /// Passed through to the agent untouched.
    agent_args: Vec<String>,
}

fn main() -> io::Result<()> {
    let opts = match parse_args() {
        Ok(Some(opts)) => opts,
        Ok(None) => return Ok(()), // --help / --version already printed
        Err(msg) => {
            eprintln!("forge: {msg}");
            std::process::exit(2);
        }
    };

    // Start the agent before taking the terminal, so a spawn failure prints
    // normally rather than onto an alternate screen we then leave.
    let mut bridge = match AgentBridge::spawn(&opts.agent_args, opts.cwd.as_deref()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "forge: could not start the agent ({e}).\n\
                 Looked next to this binary, in the workspace target directories, \
                 and on PATH. Set FORGE_AGENT_PATH to point at it directly.",
            );
            std::process::exit(1);
        }
    };

    let _guard = term::Guard::new()?;

    let (cols, rows) = term::size();
    let mut screen = Screen::new(cols, rows);
    let mut app = App::new();
    let mut out = io::BufWriter::new(io::stdout());

    let events = merge_sources(&mut bridge);

    app.view(&mut screen);
    screen.flush(&mut out)?;

    'session: loop {
        // Blocks. An idle session costs nothing here.
        let Ok(first) = events.recv() else { break };

        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match events.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }

        for event in batch {
            match event {
                Event::InputClosed => break 'session,

                Event::Terminal(crossterm::event::Event::Resize(c, r)) => {
                    screen.resize(c as usize, r as usize);
                    app.update(Input::Resize(c as usize, r as usize), &screen);
                }

                Event::Terminal(ev) => {
                    if let Some(decoded) = input::decode(ev) {
                        let (outcome, effects) = app.update(decoded, &screen);
                        if outcome == Outcome::Quit || !dispatch(&mut bridge, effects) {
                            break 'session;
                        }
                    }
                }

                Event::Agent(BridgeEvent::Message(msg)) => {
                    let effects = app.session_mut().apply(*msg);
                    app.follow_tail();
                    if !dispatch(&mut bridge, effects) {
                        break 'session;
                    }
                }

                Event::Agent(BridgeEvent::Stderr(line)) => {
                    // The agent's own logging, shown deliberately instead of let
                    // loose on the screen. OAuth prints its URL here.
                    app.session_mut().push_system(line);
                    app.follow_tail();
                }

                Event::Agent(BridgeEvent::Unknown(tag)) => {
                    // A newer agent than this build. Worth saying, in the
                    // transcript, rather than failing the session.
                    app.session_mut()
                        .push_system(format!("(ignored an unrecognised event: {tag})"));
                }

                Event::Agent(BridgeEvent::ProtocolError(msg)) => {
                    app.session_mut().push_error(msg);
                    app.follow_tail();
                }

                Event::Agent(BridgeEvent::Exited(code)) => {
                    app.session_mut().push_error(match code {
                        Some(c) => format!("the agent exited ({c})"),
                        None => "the agent exited".to_string(),
                    });
                    app.view(&mut screen);
                    screen.flush(&mut out)?;
                    break 'session;
                }
            }
        }

        app.view(&mut screen);
        screen.flush(&mut out)?;
    }

    out.flush()?;
    bridge.shutdown(SHUTDOWN_GRACE);
    Ok(())
}

/// Carry out what the state machine asked for. False means the agent is gone.
fn dispatch(bridge: &mut AgentBridge, effects: Vec<Effect>) -> bool {
    for effect in effects {
        match effect {
            Effect::Send(msg) => {
                if bridge.send(&msg).is_err() {
                    return false;
                }
            }
            Effect::TurnComplete => {
                // Deliberately silent. A bell belongs behind a setting, and an
                // unconfigurable one is worse than none.
            }
        }
    }
    true
}

/// Fold the terminal and the agent into one channel.
fn merge_sources(bridge: &mut AgentBridge) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();

    let term_tx = tx.clone();
    std::thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || loop {
            match crossterm::event::read() {
                Ok(ev) => {
                    if term_tx.send(Event::Terminal(ev)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = term_tx.send(Event::InputClosed);
                    break;
                }
            }
        })
        .expect("spawn terminal reader");

    let agent_rx = bridge.take_events();
    std::thread::Builder::new()
        .name("agent-forwarder".into())
        .spawn(move || {
            while let Ok(ev) = agent_rx.recv() {
                if tx.send(Event::Agent(ev)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn agent forwarder");

    rx
}

fn parse_args() -> Result<Option<Options>, String> {
    let mut cwd = None;
    let mut agent_args = Vec::new();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("forge {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--cwd" => {
                let dir = args.next().ok_or("--cwd needs a directory")?;
                let path = PathBuf::from(&dir);
                if !path.is_dir() {
                    return Err(format!("not a directory: {dir}"));
                }
                cwd = Some(path);
            }
            // Passed straight through; the agent owns their meaning.
            "--dangerously-allow-all" | "--login-chatgpt" => agent_args.push(arg),
            "--resume-session" => {
                agent_args.push(arg);
                agent_args.push(args.next().ok_or("--resume-session needs a session id")?);
            }
            other => return Err(format!("unrecognised option: {other}\n\n{}", usage())),
        }
    }

    Ok(Some(Options { cwd, agent_args }))
}

fn usage() -> String {
    "\
Forge — terminal UI for the Forge agent

Usage:
  forge [options]

Options:
  -h, --help                 Show this help and exit
  -V, --version              Show the version and exit
      --cwd <dir>            Run the agent rooted at <dir>
      --resume-session <id>  Resume a previous session
      --dangerously-allow-all
                             Skip every tool approval prompt
      --login-chatgpt        Log in to ChatGPT Codex via OAuth

Keys:
  Enter          send                 Ctrl-C   quit
  Up/Down        scroll               Esc      follow the newest output
  PgUp/PgDn      scroll a page        Ctrl-X   interrupt the current turn

With a prompt open, y/n/a answer it when nothing has been typed yet.
"
    .to_string()
}
