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
use forge_tui_rs::inline::Inline;
use forge_tui_rs::keys::{Decoder, ESCAPE_TIMEOUT};
use forge_tui_rs::session::Effect;
use forge_tui_rs::sys::{self, Ready};
use forge_tui_rs::{input, term};

/// How long the agent gets to exit on its own before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

/// Cap on how many events are folded into one frame. A flood should still yield
/// to drawing rather than starving the screen.
const MAX_BATCH: usize = 512;

/// One thing that happened, from either source.
enum Event {
    Key(Input),
    /// The window changed size; the new size is read when this is handled, since
    /// several resizes can arrive faster than they are processed.
    Resized,
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
    let mut inline = Inline::new(cols, rows);
    let mut app = App::new();
    let mut out = io::BufWriter::new(io::stdout());

    let events = merge_sources(&mut bridge);

    // Set when the session ends because the agent should be restarted, carrying
    // the session id to resume. Resuming is a restart because the agent has no
    // runtime path for it — only the `--resume-session` flag at startup.
    let mut restart: Option<Option<String>> = None;

    app.render(&mut inline, &mut out)?;

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

                Event::Resized => {
                    let (cols, rows) = term::size();
                    inline.resized(cols, rows);
                    app.update(Input::Resize(cols, rows), inline.rows());
                }

                Event::Key(decoded) => {
                    let (outcome, effects) = app.update(decoded, inline.rows());
                    if outcome == Outcome::Quit {
                        break 'session;
                    }
                    match dispatch(&mut bridge, effects) {
                        Next::Continue => {}
                        Next::Stop => break 'session,
                        Next::Restart(resume) => {
                            restart = Some(resume);
                            break 'session;
                        }
                    }
                }

                Event::Agent(BridgeEvent::Message(msg)) => {
                    let effects = app.session_mut().apply(*msg);
                    app.follow_tail();
                    match dispatch(&mut bridge, effects) {
                        Next::Continue => {}
                        Next::Stop => break 'session,
                        Next::Restart(resume) => {
                            restart = Some(resume);
                            break 'session;
                        }
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
                    app.render(&mut inline, &mut out)?;
                    break 'session;
                }
            }
        }

        app.render(&mut inline, &mut out)?;
    }

    // Commit whatever the last turn left live, so the finished conversation is
    // all in the scrollback rather than half of it being erased on exit.
    app.commit_all(&mut inline, &mut out)?;
    inline.finish(&mut out)?;
    out.flush()?;
    bridge.shutdown(SHUTDOWN_GRACE);

    // A restart replaces this process, so the new agent starts with a clean
    // terminal and the guard above has already put it back. `exec` rather than a
    // nested loop: the alternative is threading a second bridge and its reader
    // threads through the loop, and the process has nothing left worth keeping.
    if let Some(resume) = restart {
        drop(_guard);
        return exec_restart(&opts, resume);
    }
    Ok(())
}

/// Replace this process with a fresh one, optionally resuming a session.
fn exec_restart(opts: &Options, resume: Option<String>) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(dir) = &opts.cwd {
        cmd.arg("--cwd").arg(dir);
    }
    // Carry the original flags across, minus any previous --resume-session: its
    // value is a session id, and keeping it would resume the wrong conversation
    // — or the same one forever, whatever the user just chose.
    let mut args = opts.agent_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--resume-session" {
            let _ = args.next(); // and its value
            continue;
        }
        if arg == "--headless" {
            continue; // added by the bridge
        }
        cmd.arg(arg);
    }
    if let Some(id) = resume {
        cmd.arg("--resume-session").arg(id);
    }

    let status = cmd.status()?;
    std::process::exit(status.code().unwrap_or(0));
}

/// What the loop should do after carrying out a batch of effects.
enum Next {
    Continue,
    /// The agent is gone, or was asked to leave.
    Stop,
    /// Restart it, optionally resuming a saved session.
    Restart(Option<String>),
}

/// Carry out what the state machine asked for.
fn dispatch(bridge: &mut AgentBridge, effects: Vec<Effect>) -> Next {
    for effect in effects {
        match effect {
            Effect::Send(msg) => {
                if bridge.send(&msg).is_err() {
                    return Next::Stop;
                }
            }
            Effect::Restart { resume } => return Next::Restart(resume),
            Effect::Quit => return Next::Stop,
            Effect::TurnComplete => {
                // Deliberately silent. A bell belongs behind a setting, and an
                // unconfigurable one is worse than none.
            }
        }
    }
    Next::Continue
}

/// Fold the terminal and the agent into one channel.
fn merge_sources(bridge: &mut AgentBridge) -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();

    let term_tx = tx.clone();
    std::thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || read_terminal(term_tx))
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

/// Read stdin, decode it, and post keys onto the loop's channel.
///
/// Waits with a timeout only while the decoder is holding a partial sequence.
/// Otherwise it blocks indefinitely, which is what keeps an idle session at no
/// CPU — polling on a timer would wake many times a second to find nothing.
///
/// The timeout exists for one specific ambiguity: a lone `ESC` is both the
/// Escape key and the first byte of every other sequence. Waiting briefly and
/// then resolving it as Escape is how terminals have always settled this.
fn read_terminal(tx: mpsc::Sender<Event>) {
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 1024];

    loop {
        // A resize can arrive between iterations; report it before blocking
        // again, or the new size would not be picked up until the next keypress.
        if term::take_resize() && tx.send(Event::Resized).is_err() {
            return;
        }

        let timeout = decoder.has_pending().then_some(ESCAPE_TIMEOUT);
        match sys::wait_readable(sys::STDIN, timeout) {
            Ready::Readable => {}
            Ready::TimedOut => {
                // Nothing more is coming, so a held ESC really was Escape.
                if let Some(key) = decoder.flush_pending_escape() {
                    if let Some(action) = input::bind(key) {
                        if tx.send(Event::Key(action)).is_err() {
                            return;
                        }
                    }
                }
                continue;
            }
            // A signal — a resize, most likely. Loop round and report it.
            Ready::Interrupted => continue,
            Ready::Failed => {
                let _ = tx.send(Event::InputClosed);
                return;
            }
        }

        match sys::read_bytes(sys::STDIN, &mut buf) {
            // Zero bytes after `poll` said readable means end of input, except
            // when a signal interrupted the read, which `read_bytes` also
            // reports as zero. Re-polling distinguishes them without guessing.
            Some(0) => continue,
            Some(n) => {
                for key in decoder.feed(&buf[..n]) {
                    if let Some(action) = input::bind(key) {
                        if tx.send(Event::Key(action)).is_err() {
                            return;
                        }
                    }
                }
            }
            None => {
                let _ = tx.send(Event::InputClosed);
                return;
            }
        }
    }
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
  Enter          send                 Ctrl-T   expand/collapse reasoning
  Up/Down        scroll               Ctrl-O   menu (model, tools, settings)
  PgUp/PgDn      scroll a page        Ctrl-X   interrupt the current turn
  Esc            follow newest output Ctrl-C   quit

In the menu: Up/Down or j/k to move, Enter to choose, Esc to go back.
At a prompt: y approves, n denies; always-allow must be selected.
"
    .to_string()
}
