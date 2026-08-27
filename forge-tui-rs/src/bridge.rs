// SPDX-License-Identifier: Apache-2.0
//! Talking to `forge-agent --headless`.
//!
//! The agent is a child process speaking newline-delimited JSON on stdin and
//! stdout. This module owns the process, decodes its output into
//! [`BridgeEvent`]s on a channel, and writes [`ClientMessage`]s back.
//!
//! Three decisions carried over from the TypeScript bridge, each for a reason:
//!
//!  * **stderr is piped, never inherited.** The agent's `eprintln!` output would
//!    otherwise paint straight onto the screen we are drawing, and during OAuth
//!    login it prints the URL and instructions there — interleaved mid-frame. It
//!    arrives as [`BridgeEvent::Stderr`] so the app can show it deliberately.
//!  * **Lines are read with a hard size cap.** A frame is capped at 10 MB, the
//!    same figure the agent uses. Past that the partial frame is discarded and
//!    decoding resynchronises at the next newline, rather than growing a buffer
//!    without bound because something upstream misbehaved.
//!  * **An unrecognised message is not fatal.** It becomes
//!    [`BridgeEvent::Unknown`] with its tag, so an agent newer than this client
//!    degrades to "some events ignored" instead of a dead session.
//!
//! What is deliberately *not* carried over is the 75 ms token-coalescing timer.
//! That existed because ink re-rendered per event, so a streaming reply meant a
//! render per token. Here the event loop drains everything pending and draws
//! once, which coalesces by construction — with no latency floor and no timer to
//! get wrong.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use forge_agent_proto::{AgentLine, AgentMessage, ClientMessage};

/// Largest single protocol frame we will buffer, matching the agent's own cap.
pub const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

/// Something that happened on the agent connection.
#[derive(Clone, Debug)]
pub enum BridgeEvent {
    /// A decoded protocol message.
    Message(Box<AgentMessage>),
    /// A well-formed message this build does not recognise, by tag. Carried so
    /// it can be logged rather than silently dropped.
    Unknown(String),
    /// One line of the agent's stderr.
    Stderr(String),
    /// Output that was not valid JSON, or a frame over the size cap.
    ProtocolError(String),
    /// The agent exited. No further events will arrive.
    Exited(Option<i32>),
}

/// An owned connection to a running agent.
pub struct AgentBridge {
    child:  Child,
    stdin:  Option<ChildStdin>,
    events: Receiver<BridgeEvent>,
}

impl AgentBridge {
    /// Spawn `forge-agent --headless` with extra arguments, rooted at `cwd`.
    pub fn spawn(args: &[String], cwd: Option<&Path>) -> io::Result<Self> {
        let binary = find_agent_binary();
        let mut full = vec!["--headless".to_string()];
        full.extend(args.iter().cloned());
        Self::spawn_command(&binary, &full, cwd)
    }

    /// Spawn an arbitrary command as the agent.
    ///
    /// Exists so tests can drive the decoder with a stand-in process instead of
    /// requiring a built agent and a configured machine.
    pub fn spawn_command(
        program: &Path,
        args:    &[String],
        cwd:     Option<&Path>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let stdin = child.stdin.take().expect("stdin was piped");

        let (tx, events) = mpsc::channel();

        // One reader per stream. Threads rather than async: this is two blocking
        // line readers, and a runtime would be the largest dependency in the
        // crate for no gain.
        // `Exited` has to be the last event, and it is the stdout reader that
        // notices the stream ending — so it waits for stderr to drain before
        // announcing it. Without that, the two threads race: a consumer that
        // treats `Exited` as "nothing more is coming" loses whatever stderr had
        // not been delivered yet, and `main.rs` does exactly that. It cost
        // nothing on macOS, where the scheduling happened to favour stderr, and
        // dropped the line on Linux. The lines that matter most are the ones at
        // the end: during OAuth login the agent prints the URL and instructions
        // to stderr and then exits.
        let stderr_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let out_tx = tx.clone();
        let out_done = std::sync::Arc::clone(&stderr_done);
        std::thread::Builder::new()
            .name("agent-stdout".into())
            .spawn(move || {
                read_protocol(BufReader::new(stdout), out_tx.clone());
                // Bounded, so a child that closes stdout while holding stderr
                // open cannot stall the exit event forever.
                let deadline = std::time::Instant::now() + STDERR_DRAIN_GRACE;
                while !out_done.load(std::sync::atomic::Ordering::Acquire)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                let _ = out_tx.send(BridgeEvent::Exited(None));
            })?;

        std::thread::Builder::new()
            .name("agent-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let line = line.trim_end().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(BridgeEvent::Stderr(line)).is_err() {
                        break;
                    }
                }
                stderr_done.store(true, std::sync::atomic::Ordering::Release);
            })?;

        Ok(Self { child, stdin: Some(stdin), events })
    }

    /// Events from the agent. Never blocks the caller.
    pub fn events(&self) -> &Receiver<BridgeEvent> {
        &self.events
    }

    /// Take ownership of the event receiver, leaving a disconnected one behind.
    ///
    /// For callers that fold this stream into a larger one — the event loop
    /// forwards these onto a channel it also feeds terminal input into, so it
    /// needs the receiver itself rather than a borrow.
    pub fn take_events(&mut self) -> Receiver<BridgeEvent> {
        let (_dead_tx, dead_rx) = mpsc::channel();
        std::mem::replace(&mut self.events, dead_rx)
    }

    /// Send a message to the agent.
    ///
    /// Errors once the agent has gone; callers generally treat that as "the
    /// session is over" rather than something to retry.
    pub fn send(&mut self, msg: &ClientMessage) -> io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "agent stdin closed"));
        };
        stdin.write_all(msg.to_line().as_bytes())?;
        stdin.flush()
    }

    /// Ask the agent to exit, then make sure it does.
    ///
    /// Closing stdin after `quit` matters: the agent's read loop is waiting on a
    /// line, and an open pipe with nothing coming would leave it parked. If it
    /// has not exited by the deadline it is killed, because a TUI that hangs on
    /// exit is worse than an agent that loses its last log flush.
    pub fn shutdown(&mut self, grace: std::time::Duration) -> Option<i32> {
        let _ = self.send(&ClientMessage::Quit);
        self.stdin = None; // drop closes the pipe, unblocking the agent

        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        self.child.wait().ok().and_then(|s| s.code())
    }
}

impl Drop for AgentBridge {
    fn drop(&mut self) {
        // A leaked agent would keep running with no one reading it, holding the
        // project's session log open.
        if matches!(self.child.try_wait(), Ok(None)) {
            self.stdin = None;
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// How long the stdout reader waits for stderr to finish before sending
/// `Exited`.
///
/// Long enough for lines already written to arrive, short enough that a child
/// holding stderr open does not hide the fact that it has gone.
const STDERR_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Decode the agent's stdout into events until the stream ends.
fn read_protocol(mut reader: impl BufRead, tx: Sender<BridgeEvent>) {
    let mut line = Vec::new();

    loop {
        line.clear();
        match read_capped_line(&mut reader, &mut line, MAX_LINE_BYTES) {
            Ok(ReadOutcome::Eof) => break,
            Ok(ReadOutcome::Line) => {
                let text = String::from_utf8_lossy(&line);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let event = match AgentLine::parse(trimmed) {
                    Ok(AgentLine::Known(msg)) => BridgeEvent::Message(Box::new(msg)),
                    Ok(AgentLine::Unknown(v)) => BridgeEvent::Unknown(
                        v.get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("<untagged>")
                            .to_string(),
                    ),
                    Err(e) => BridgeEvent::ProtocolError(format!(
                        "undecodable message ({e}): {}",
                        truncate(trimmed, 200),
                    )),
                };
                if tx.send(event).is_err() {
                    return; // receiver gone; nobody left to tell
                }
            }
            Ok(ReadOutcome::Oversized(n)) => {
                let msg = format!(
                    "dropped an oversized frame ({n} bytes over the {MAX_LINE_BYTES} cap); \
                     resynchronising at the next newline",
                );
                if tx.send(BridgeEvent::ProtocolError(msg)).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }

}

enum ReadOutcome {
    Line,
    /// The frame exceeded the cap; it was discarded up to the next newline.
    Oversized(usize),
    Eof,
}

/// Read one newline-terminated frame, refusing to buffer more than `cap` bytes.
///
/// `BufRead::read_line` and `read_until` both grow their buffer to whatever
/// arrives, which makes the peer's output an allocation budget. This walks the
/// buffered chunks so the cap is enforced *before* the bytes are retained.
fn read_capped_line(
    reader: &mut impl BufRead,
    out:    &mut Vec<u8>,
    cap:    usize,
) -> io::Result<ReadOutcome> {
    let mut discarded = 0usize;
    let mut overflowing = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(b) => b,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(if out.is_empty() && !overflowing {
                ReadOutcome::Eof
            } else if overflowing {
                ReadOutcome::Oversized(discarded)
            } else {
                ReadOutcome::Line
            });
        }

        if let Some(i) = available.iter().position(|&b| b == b'\n') {
            if !overflowing {
                out.extend_from_slice(&available[..i]);
            }
            reader.consume(i + 1);
            return Ok(if overflowing {
                ReadOutcome::Oversized(discarded + i)
            } else {
                ReadOutcome::Line
            });
        }

        let n = available.len();
        if overflowing {
            discarded += n;
        } else if out.len() + n > cap {
            // Past the cap: stop retaining, and keep scanning for the newline
            // that begins the next usable frame.
            discarded = out.len() + n;
            out.clear();
            overflowing = true;
        } else {
            out.extend_from_slice(available);
        }
        reader.consume(n);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Locate the `forge-agent` binary.
///
/// Order: `FORGE_AGENT_PATH`, then alongside this executable, then the
/// workspace's build directories, then bare `forge-agent` for `PATH` lookup.
/// Release is checked before debug deliberately — a debug binary left over from
/// an earlier build is the more likely of the two to be stale.
pub fn find_agent_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("FORGE_AGENT_PATH") {
        return PathBuf::from(p);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("forge-agent"));
            for rel in ["../release/forge-agent", "../debug/forge-agent"] {
                candidates.push(dir.join(rel));
            }
        }
    }

    // Running from a source checkout via `cargo run`.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in ["../target/release/forge-agent", "../target/debug/forge-agent"] {
        candidates.push(manifest.join(rel));
    }

    for candidate in candidates {
        if candidate.is_file() {
            return candidate;
        }
    }

    PathBuf::from("forge-agent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Drive the decoder directly, without a process.
    fn decode(input: &str) -> Vec<BridgeEvent> {
        let (tx, rx) = mpsc::channel();
        read_protocol(BufReader::new(io::Cursor::new(input.as_bytes().to_vec())), tx);
        rx.into_iter().collect()
    }
    // These decode stdout and nothing else: `read_protocol` no longer ends with
    // an `Exited`, because the process having gone is not something the decoder
    // knows — it is announced by the thread that also waits for stderr to drain,
    // so that the exit is genuinely the last event.

    fn tags(events: &[BridgeEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                BridgeEvent::Message(m) => m.tag().to_string(),
                BridgeEvent::Unknown(t) => format!("unknown:{t}"),
                BridgeEvent::Stderr(_) => "stderr".into(),
                BridgeEvent::ProtocolError(_) => "protocol_error".into(),
                BridgeEvent::Exited(_) => "exited".into(),
            })
            .collect()
    }

    #[test]
    fn decodes_a_sequence_of_frames() {
        let events = decode(
            "{\"type\":\"thinking\"}\n\
             {\"type\":\"assistant_token\",\"content\":\"hi\"}\n\
             {\"type\":\"done\"}\n",
        );
        assert_eq!(
            tags(&events),
            vec!["thinking", "assistant_token", "done"],
        );
    }

    /// The stream ends without a trailing newline on the last frame.
    #[test]
    fn a_final_frame_without_a_newline_is_still_decoded() {
        let events = decode("{\"type\":\"done\"}");
        assert_eq!(tags(&events), vec!["done"]);
    }

    #[test]
    fn blank_lines_are_skipped() {
        let events = decode("\n\n{\"type\":\"done\"}\n\n");
        assert_eq!(tags(&events), vec!["done"]);
    }

    /// A frame split across reads must be reassembled — chunk boundaries are
    /// arbitrary on a pipe.
    #[test]
    fn a_frame_split_across_chunks_is_reassembled() {
        // BufReader with a tiny capacity forces the split.
        let input = b"{\"type\":\"assistant_token\",\"content\":\"hello world\"}\n".to_vec();
        let (tx, rx) = mpsc::channel();
        read_protocol(BufReader::with_capacity(8, io::Cursor::new(input)), tx);
        let events: Vec<_> = rx.into_iter().collect();
        match &events[0] {
            BridgeEvent::Message(m) => match m.as_ref() {
                AgentMessage::AssistantToken { content } => assert_eq!(content, "hello world"),
                other => panic!("wrong variant: {other:?}"),
            },
            other => panic!("wrong event: {other:?}"),
        }
    }

    /// An event from a newer agent must not end the session.
    #[test]
    fn an_unknown_message_is_reported_and_decoding_continues() {
        let events = decode(
            "{\"type\":\"from_the_future\",\"x\":1}\n\
             {\"type\":\"done\"}\n",
        );
        assert_eq!(
            tags(&events),
            vec!["unknown:from_the_future", "done"],
            "decoding continues past the unknown",
        );
    }

    #[test]
    fn undecodable_output_is_reported_and_decoding_continues() {
        let events = decode("this is not json\n{\"type\":\"done\"}\n");
        assert_eq!(tags(&events), vec!["protocol_error", "done"]);
    }

    /// The cap must hold, and decoding must recover on the next frame rather
    /// than being poisoned by the oversized one.
    #[test]
    fn an_oversized_frame_is_dropped_and_the_stream_resynchronises() {
        let huge = "x".repeat(300);
        let input = format!("{{\"type\":\"error\",\"message\":\"{huge}\"}}\n{{\"type\":\"done\"}}\n");

        let mut reader = BufReader::with_capacity(64, io::Cursor::new(input.into_bytes()));
        let mut line = Vec::new();

        // Cap well below the first frame's size.
        let first = read_capped_line(&mut reader, &mut line, 100).unwrap();
        assert!(matches!(first, ReadOutcome::Oversized(_)), "first frame rejected");

        line.clear();
        let second = read_capped_line(&mut reader, &mut line, 100).unwrap();
        assert!(matches!(second, ReadOutcome::Line), "next frame recovered");
        assert_eq!(String::from_utf8_lossy(&line), r#"{"type":"done"}"#);
    }

    #[test]
    fn the_cap_bounds_retained_bytes() {
        // 5 KB of a frame with a 1 KB cap: nothing near 5 KB is kept.
        let input = format!("{}\n", "y".repeat(5000));
        let mut reader = BufReader::with_capacity(128, io::Cursor::new(input.into_bytes()));
        let mut line = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut line, 1024).unwrap();
        assert!(matches!(outcome, ReadOutcome::Oversized(_)));
        assert!(line.is_empty(), "oversized data is discarded, not retained");
    }

    #[test]
    fn eof_on_an_empty_stream_is_eof() {
        let mut reader = BufReader::new(io::Cursor::new(Vec::new()));
        let mut line = Vec::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut line, 1024).unwrap(),
            ReadOutcome::Eof,
        ));
    }

    /// Invalid UTF-8 must not abort the stream; the agent could emit a byte
    /// sequence from a tool's output.
    #[test]
    fn invalid_utf8_does_not_kill_the_stream() {
        let mut input = b"{\"type\":\"error\",\"message\":\"".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        input.extend_from_slice(b"\"}\n{\"type\":\"done\"}\n");
        let (tx, rx) = mpsc::channel();
        read_protocol(BufReader::new(io::Cursor::new(input)), tx);
        let events: Vec<_> = rx.into_iter().collect();
        assert!(
            tags(&events).contains(&"done".to_string()),
            "recovered to decode the following frame: {:?}",
            tags(&events),
        );
    }

    // ── Process-level behaviour, using a stand-in agent ────────────────────

    /// Spawning a real process and reading its framed output end to end.
    #[test]
    fn spawns_a_process_and_receives_its_frames() {
        let script = r#"printf '{"type":"thinking"}\n{"type":"done"}\n'"#;
        let bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            None,
        )
        .expect("spawn");

        let mut seen = Vec::new();
        while let Ok(ev) = bridge.events().recv_timeout(Duration::from_secs(5)) {
            let done = matches!(ev, BridgeEvent::Exited(_));
            seen.push(ev);
            if done {
                break;
            }
        }
        assert_eq!(tags(&seen), vec!["thinking", "done", "exited"]);
    }

    /// stderr must arrive as its own event, never mixed into the protocol
    /// stream — this is what stops the agent's logging painting over the UI.
    #[test]
    fn stderr_is_delivered_separately_from_protocol_output() {
        let script = r#"printf '{"type":"done"}\n'; printf 'a warning\n' >&2"#;
        let bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            None,
        )
        .expect("spawn");

        let mut protocol = Vec::new();
        let mut stderr = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match bridge.events().recv_timeout(Duration::from_millis(200)) {
                Ok(BridgeEvent::Stderr(l)) => stderr.push(l),
                Ok(BridgeEvent::Message(m)) => protocol.push(m.tag().to_string()),
                Ok(BridgeEvent::Exited(_)) => break,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        assert_eq!(protocol, vec!["done"], "protocol stream is clean");
        assert_eq!(stderr, vec!["a warning"], "stderr captured, not printed");
    }

    /// `Exited` is the last event, so a consumer may treat it as the end.
    ///
    /// `main.rs` does exactly that — it breaks out of the session loop — so any
    /// stderr still in flight when the exit is announced is lost. It was, on
    /// Linux: the two reader threads raced and macOS's scheduling happened to
    /// hide it. The lines this loses are the ones at the end, and during OAuth
    /// login the agent prints the URL and instructions to stderr and then exits.
    ///
    /// The script closes stdout, waits, and only then writes to stderr. Without
    /// the pause this passed on macOS whether or not the fix was present — the
    /// scheduling there favours stderr — and a test that only fails on somebody
    /// else's machine is not a test of anything.
    #[test]
    fn nothing_arrives_after_the_exit_event() {
        // stdout closes first, then stderr is written: the ordering that used to
        // drop the line.
        let script =
            r#"printf '{"type":"done"}\n'; exec 1>&-; sleep 0.15; printf 'a warning\n' >&2"#;
        let bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            None,
        )
        .expect("spawn");

        let mut order = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match bridge.events().recv_timeout(Duration::from_millis(200)) {
                Ok(ev) => {
                    let last = matches!(ev, BridgeEvent::Exited(_));
                    order.push(match ev {
                        BridgeEvent::Stderr(_) => "stderr",
                        BridgeEvent::Exited(_) => "exited",
                        _ => "message",
                    });
                    if last {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }

        assert_eq!(order.last(), Some(&"exited"), "exit was not last: {order:?}");
        assert!(
            order.contains(&"stderr"),
            "stderr was lost behind the exit event: {order:?}",
        );
    }

    /// What the app writes must reach the child's stdin, framed.
    #[test]
    fn sent_messages_reach_the_child_as_framed_lines() {
        // Echo stdin back as a stderr line, so the test can observe it.
        let script = r#"while IFS= read -r l; do printf '%s\n' "$l" >&2; done"#;
        let mut bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), script.to_string()],
            None,
        )
        .expect("spawn");

        bridge
            .send(&ClientMessage::SendMessage { content: "ping".into() })
            .expect("send");

        let mut echoed = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match bridge.events().recv_timeout(Duration::from_millis(200)) {
                Ok(BridgeEvent::Stderr(l)) => {
                    echoed = Some(l);
                    break;
                }
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        let echoed = echoed.expect("child received the line");
        let parsed = forge_agent_proto::json::parse(&echoed).expect("valid JSON");
        assert_eq!(parsed.str_or_empty("type"), "send_message");
        assert_eq!(parsed.str_or_empty("content"), "ping");
    }

    /// Shutdown must terminate a process that ignores `quit`, rather than
    /// hanging the exit path.
    #[test]
    fn shutdown_kills_an_unresponsive_agent() {
        let mut bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 300".to_string()],
            None,
        )
        .expect("spawn");

        let started = std::time::Instant::now();
        bridge.shutdown(Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "shutdown must not wait on a process that ignores quit",
        );
    }

    /// Sending after the agent is gone is an error, not a panic.
    #[test]
    fn sending_to_a_dead_agent_errors() {
        let mut bridge = AgentBridge::spawn_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), "exit 0".to_string()],
            None,
        )
        .expect("spawn");
        bridge.shutdown(Duration::from_millis(200));

        let err = bridge.send(&ClientMessage::RequestUsage);
        assert!(err.is_err(), "must report the broken connection");
    }

    /// The environment is process-wide while tests run in parallel threads, so
    /// these two raced: one cleared `FORGE_AGENT_PATH` while the other was
    /// asserting on it, and the suite failed roughly one run in ten. Held for the
    /// whole of each test, not just the mutation, since the assertion depends on
    /// the variable just as much as the setup does.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A poisoned lock here means a *different* test failed; that failure is the
    /// one worth reporting, so carry on rather than masking it with this one.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn binary_discovery_prefers_the_environment_override() {
        let _guard = env_guard();
        // Safe here: the test asserts on the value it just set, and the
        // discovery function reads it directly.
        let sentinel = "/nonexistent/sentinel-forge-agent";
        unsafe { std::env::set_var("FORGE_AGENT_PATH", sentinel) };
        assert_eq!(find_agent_binary(), PathBuf::from(sentinel));
        unsafe { std::env::remove_var("FORGE_AGENT_PATH") };
    }

    #[test]
    fn binary_discovery_falls_back_to_a_path_lookup() {
        let _guard = env_guard();
        unsafe { std::env::remove_var("FORGE_AGENT_PATH") };
        let found = find_agent_binary();
        // Either a real build was located, or the bare name for PATH lookup.
        assert!(
            found.is_file() || found == PathBuf::from("forge-agent"),
            "unexpected discovery result: {found:?}",
        );
    }
}
