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

/// How often the spinner advances while a turn is running.
///
/// Only applied while something is animating: an idle session goes back to
/// blocking indefinitely, which is what keeps it at no CPU.
const SPINNER_TICK: Duration = Duration::from_millis(120);

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

    // Before the agent is spawned, not after: a session the user is about to
    // cancel should never have had a process behind it.
    if dangerous_confirm_wanted(&opts.agent_args) && !confirm_dangerous_allow_all() {
        println!("Cancelled.");
        return Ok(());
    }

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
        // Blocks indefinitely when nothing is animating, so an idle session costs
        // nothing. While a turn runs, wake on the spinner's cadence instead.
        let first = if app.animating() {
            match events.recv_timeout(SPINNER_TICK) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match events.recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            }
        };

        // A timeout with no event is the spinner's turn.
        let Some(first) = first else {
            app.tick();
            app.render(&mut inline, &mut out)?;
            continue;
        };

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
///
/// A genuine `exec`, not a spawn-and-wait. This function used to call
/// `Command::status`, which starts a *child* and leaves this process running —
/// and a running terminal program still has a thread blocked reading the tty. Two
/// readers on one terminal means every keystroke goes to whichever wins the race,
/// so half of the user's typing vanished into the parent that no longer drew
/// anything. It looked exactly like needing to press each key twice, and only
/// after resuming a session, because that is the one path that restarts.
fn exec_restart(opts: &Options, resume: Option<String>) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = Vec::new();
    if let Some(dir) = &opts.cwd {
        args.push("--cwd".into());
        args.push(dir.display().to_string());
    }
    // Carry the original flags across, minus any previous --resume-session: its
    // value is a session id, and keeping it would resume the wrong conversation
    // — or the same one forever, whatever the user just chose.
    let mut original = opts.agent_args.iter();
    while let Some(arg) = original.next() {
        if arg == "--resume-session" {
            let _ = original.next(); // and its value
            continue;
        }
        if arg == "--headless" {
            continue; // added by the bridge
        }
        args.push(arg.clone());
    }
    if let Some(id) = resume {
        args.push("--resume-session".into());
        args.push(id);
    }

    // Only returns if the replacement failed, in which case this process is
    // still the one running and has to say so.
    Err(sys::exec(&exe, &args))
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
/// Whether a chunk of input was pasted rather than typed.
///
/// The tell is a line break with something after it. A person cannot type one
/// anywhere but at the end of what a single `read` returns — pressing Enter is
/// what ends the read — so text following a break in the same chunk was not typed.
/// One trailing break is ignored before looking, which is exactly what a typed
/// Enter is, and covers a terminal that sends CRLF for it.
///
/// Chunks containing an escape byte are left alone: those are sequences, possibly
/// the bracketed-paste markers themselves, and the decoder owns them.
///
/// The failure this exists for: pasting a structured message into Apple's
/// Terminal.app submitted it one line at a time. The TUI asks for bracketed paste
/// at startup, but that terminal does not implement it, so the request buys
/// nothing and the paste arrives indistinguishable from typing.
fn looks_pasted(chunk: &[u8]) -> bool {
    if chunk.contains(&0x1b) {
        return false;
    }
    let body = chunk
        .strip_suffix(b"\r\n")
        .or_else(|| chunk.strip_suffix(b"\r"))
        .or_else(|| chunk.strip_suffix(b"\n"))
        .unwrap_or(chunk);
    body.iter().any(|b| *b == b'\r' || *b == b'\n')
}

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

    // Polled alongside stdin so a SIGWINCH can wake this thread. Without it the
    // handler sets a flag nobody reads until the next keystroke, because
    // `signal` installs handlers with SA_RESTART and the blocked `poll` simply
    // resumes.
    let wake = term::wake_fd();
    let watched: Vec<std::os::raw::c_int> = match wake {
        Some(fd) => vec![sys::STDIN, fd],
        None => vec![sys::STDIN],
    };
    const STDIN_BIT: u32 = 0b1;
    const WAKE_BIT: u32 = 0b10;

    loop {
        // A resize can arrive between iterations; report it before blocking
        // again, or the new size would not be picked up until the next keypress.
        if term::take_resize() && tx.send(Event::Resized).is_err() {
            return;
        }

        let timeout = decoder.has_pending().then_some(ESCAPE_TIMEOUT);
        match sys::wait_readable_many(&watched, timeout) {
            Ready::Readable(mask) => {
                if mask & WAKE_BIT != 0 {
                    // A signal woke us. Empty the pipe and loop, so the resize is
                    // reported at the top rather than mistaken for input.
                    term::drain_wake();
                    continue;
                }
                if mask & STDIN_BIT == 0 {
                    continue;
                }
            }
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
                // A terminal that does not implement bracketed paste hands pasted
                // text over as if it had been typed, so the line breaks in it are
                // Enters and a multi-line message is submitted a line at a time.
                // Apple's Terminal.app is one such terminal, and asking for the
                // mode (`?2004h`, which this does at startup) buys nothing there.
                if !decoder.has_pending() && looks_pasted(&buf[..n]) {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if tx.send(Event::Key(Input::Paste(text))).is_err() {
                        return;
                    }
                    continue;
                }
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

/// `--dangerously-allow-all` bypasses every tool approval gate for the whole
/// session. The flag name is the warning, but a confirmation that defaults to
/// no catches the case the name cannot: another tool, or muscle memory, having
/// launched forge with it when nobody read what it does.
///
/// This is a port of the gate the TypeScript client carried (`forge-tui/src/
/// index.tsx`, added in af3bf96) — the same banner, the same accepted answers,
/// the same `Cancelled.` and exit status. It was deleted with that client in
/// f3b3c60 and the README went on describing it, which is the worst of the
/// three possible states: no gate, and a promise of one.
fn dangerous_confirm_wanted(agent_args: &[String]) -> bool {
    agent_args.iter().any(|a| a == "--dangerously-allow-all")
        && !skip_dangerous_confirm()
}

/// `FORGE_SKIP_DANGEROUS_CONFIRM=1` opts a pipeline out of the prompt. An empty
/// value does not count as set — the original tested the variable for JavaScript
/// truthiness, where `FOO=` is falsy, and a variable someone cleared rather than
/// unset should mean what it looks like it means.
fn skip_dangerous_confirm() -> bool {
    std::env::var_os("FORGE_SKIP_DANGEROUS_CONFIRM")
        .is_some_and(|v| !v.is_empty())
}

/// Only `yes` or `y` proceed. Anything else does — deliberately — nothing.
fn answer_approves(answer: &str) -> bool {
    let a = answer.trim().to_lowercase();
    a == "yes" || a == "y"
}

/// Prints the banner and reads one line. A closed or unreadable stdin is not a
/// yes: a pipeline that wants no prompt sets the environment variable.
fn confirm_dangerous_allow_all() -> bool {
    println!();
    println!("\x1b[31m\x1b[1mDANGER: --dangerously-allow-all is set.\x1b[0m");
    println!();
    println!("This bypasses EVERY tool approval prompt for this entire session.");
    println!("Forge can read, write, edit, and execute anything on this machine");
    println!("(within your user's permissions) without asking you first.");
    println!();
    println!("Only continue if you are in a sandbox, a VM, or a disposable workspace");
    println!("where you have already accepted that risk.");
    println!();
    println!("To skip this prompt in scripted environments:");
    println!("    FORGE_SKIP_DANGEROUS_CONFIRM=1 forge --dangerously-allow-all");
    println!();
    print!("Type 'yes' to continue, anything else to exit: ");
    let _ = io::Write::flush(&mut io::stdout());

    let mut answer = String::new();
    match io::BufRead::read_line(&mut io::stdin().lock(), &mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => answer_approves(&answer),
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
                // The commit too: between two releases every build says the
                // same number, and "which build is this" is the actual question
                // being asked. `/version` inside the TUI says more, including
                // which agent it is talking to.
                println!("forge {} · commit {}",
                    env!("CARGO_PKG_VERSION"), env!("FORGE_BUILD_COMMIT"));
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
  -V, --version              Show the version and build commit, then exit
      --cwd <dir>            Run the agent rooted at <dir>
      --resume-session <id>  Resume a previous session
      --dangerously-allow-all
                             Skip every tool approval prompt
      --login-chatgpt        Log in to ChatGPT Codex via OAuth

Keys:
  Enter          send                 Ctrl-T   expand/collapse reasoning
  Up/Down        scroll               Ctrl-O   menu (model, tools, settings)
  PgUp/PgDn      scroll a page        Ctrl-X   interrupt the current turn
  Esc            interrupt the turn   Ctrl-C   quit

In the menu: Up/Down or j/k to move, Enter to choose, Esc to go back.
At a prompt: y approves, n denies; always-allow must be selected.
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::looks_pasted;

    /// The reported bug: a structured message pasted into Apple's Terminal.app was
    /// submitted one line at a time. That terminal does not implement bracketed
    /// paste, so the text arrives as if typed and every break in it is an Enter.
    #[test]
    fn a_multi_line_chunk_is_recognised_as_a_paste() {
        for (name, chunk) in [
            ("two lines",      &b"first\rsecond"[..]),
            ("trailing break", &b"first\rsecond\r"[..]),
            ("CRLF",           &b"first\r\nsecond\r\n"[..]),
            ("LF",             &b"first\nsecond"[..]),
            ("blank line",     &b"first\r\rsecond"[..]),
        ] {
            assert!(looks_pasted(chunk), "{name} should read as a paste");
        }
    }

    /// Typing must not be mistaken for a paste, or Enter would stop submitting.
    #[test]
    fn typing_is_not_mistaken_for_a_paste() {
        for (name, chunk) in [
            ("a bare Enter",            &b"\r"[..]),
            ("Enter as CRLF",           &b"\r\n"[..]),
            ("fast typing, then Enter", &b"hello\r"[..]),
            ("ordinary characters",     &b"hello"[..]),
            ("nothing at all",          &b""[..]),
        ] {
            assert!(!looks_pasted(chunk), "{name} should read as typing");
        }
    }

    /// Anything with an escape byte belongs to the decoder — including a paste the
    /// terminal *did* bracket, which must not be handled twice.
    #[test]
    fn sequences_are_left_to_the_decoder() {
        assert!(!looks_pasted(b"\x1b[200~one\rtwo\x1b[201~"), "already bracketed");
        assert!(!looks_pasted(b"\x1b[A"), "an arrow key");
        assert!(!looks_pasted(b"a\x1b[Ab\r c"), "text mixed with a sequence");
    }
}

#[cfg(test)]
mod dangerous_confirm_tests {
    use super::{answer_approves, dangerous_confirm_wanted};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The gate exists at all. It is documented in forge-agent's README, it was
    /// deleted with the TypeScript client that implemented it, and the README
    /// then described a safety mechanism that no code performed — so what this
    /// test really pins is that the two cannot drift apart again silently.
    #[test]
    fn the_flag_asks_before_anything_runs() {
        assert!(dangerous_confirm_wanted(&args(&["--dangerously-allow-all"])));
    }

    /// And only that flag does. A normal session is not interrogated.
    #[test]
    fn nothing_else_asks() {
        assert!(!dangerous_confirm_wanted(&args(&[])));
        assert!(!dangerous_confirm_wanted(&args(&["--resume-session", "abc"])));
        assert!(!dangerous_confirm_wanted(&args(&["--login-chatgpt"])));
        // Near misses are not the flag. A prefix match here would gate on a
        // future `--dangerously-allow-all-writes` and, worse, not on this one.
        assert!(!dangerous_confirm_wanted(&args(&["--dangerously-allow"])));
    }

    /// Yes means yes, and it is the only thing that does. Empty input is the
    /// case that matters: a prompt answered with Enter must not proceed.
    #[test]
    fn only_yes_proceeds() {
        for ok in ["yes", "y", "YES", "Y", "  yes  ", "Yes\n"] {
            assert!(answer_approves(ok), "{ok:?} should have been accepted");
        }
        for no in ["", "\n", " ", "n", "no", "nope", "yesss", "ye", "yeah", "1", "sure"] {
            assert!(!answer_approves(no), "{no:?} should NOT have been accepted");
        }
    }
}
