// SPDX-License-Identifier: Apache-2.0
//! Resuming a conversation that is already larger than the context window.
//!
//! Reported from use: a resumed session showed 114% and the question was what
//! the first message would cost. Reading the code twice gave two different
//! answers — that the summarisation request would itself be over the window
//! and fail, and then that chunking handles it — so this settles it by running
//! the real binary against a provider that records what it was asked.
//!
//! The property that matters, and the one the user asked for: at over 100%,
//! the summary is still produced, the last messages survive verbatim beside
//! it, and the turn completes. The alternative — compaction failing, then the
//! turn failing, then a blunt drop-and-truncate backstop — loses the summary
//! exactly when it is the only thing carrying what the session was doing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(90);

/// Every request the agent made, so the test can tell a summariser call from
/// the turn's own call and inspect both.
struct Provider {
    requests: Mutex<Vec<Value>>,
}

impl Provider {
    /// Requests whose prompt asks for a structured summary — the compaction
    /// calls, however many chunks it took.
    fn summariser_calls(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                r.to_string().contains("conversation summarizer")
                    || r.to_string().contains("summarizing part")
            })
            .cloned()
            .collect()
    }

    fn all(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// The largest prompt the agent ever sent, in characters. A request far
    /// over the window would show up here.
    fn largest_request_chars(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.to_string().len())
            .max()
            .unwrap_or(0)
    }
}

fn start_provider() -> (Arc<Provider>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Provider { requests: Mutex::new(Vec::new()) });
    let served = Arc::clone(&state);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let state = Arc::clone(&served);
            thread::spawn(move || {
                let _ = handle(stream, &state);
            });
        }
    });
    (state, port)
}

fn handle(mut stream: TcpStream, state: &Provider) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    if request_line.starts_with("GET") {
        let payload = json!({ "data": [{ "id": "compaction-fixture" }] }).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )?;
        return stream.flush();
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let text = request.to_string();
    state.requests.lock().unwrap().push(request);

    // A summariser call wants JSON in the shape `CompactionSummary` parses.
    // Everything that identifies this fixture goes in a field the merge keeps,
    // so the test can find it in the rebuilt history.
    let reply = if text.contains("conversation summarizer") || text.contains("summarizing part") {
        json!({
            "goal": "FIXTURE-GOAL finish the loader work",
            "repo_map": ["src/loader.rs — the thing under test"],
            "work_completed": ["FIXTURE-COMPLETED an earlier step"],
            "current_state": "FIXTURE-STATE mid-change",
            "commands_run": ["cargo test"],
            "decisions": ["FIXTURE-DECISION little-endian only"],
            "next_actions": ["FIXTURE-NEXT keep going"],
            "pitfalls": ["FIXTURE-PITFALL do not reformat"]
        })
        .to_string()
    } else {
        "acknowledged".to_string()
    };

    // Streaming or not, as asked. The summariser does not stream — it wants a
    // completion object — and answering it with SSE was the first failure
    // here: "JSON error: error decoding response body", which reads like the
    // agent's fault and is the harness's.
    let streaming = text.contains("\"stream\":true");
    if streaming {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
        )?;
        delta(&mut stream, &json!({ "content": reply }), None)?;
        delta(&mut stream, &json!({}), Some("stop"))?;
        stream.write_all(b"data: [DONE]\n\n")?;
        return stream.flush();
    }

    let payload = json!({
        "id": "fixture",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": reply },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 }
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    )?;
    stream.flush()
}

fn delta(w: &mut TcpStream, body: &Value, finish: Option<&str>) -> std::io::Result<()> {
    let payload = json!({ "choices": [{ "index": 0, "delta": body, "finish_reason": finish }] });
    write!(w, "data: {payload}\n\n")?;
    w.flush()
}

struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Agent {
    fn start(home: &Path, resume: Option<&str>) -> Agent {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge-agent"));
        command.arg("--headless");
        if let Some(session) = resume {
            command.arg("--resume-session").arg(session);
        }
        command
            .current_dir(home.join("workspace"))
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local").join("share"))
            .env("FORGE_CONFIG_FILE", home.join(".config/forge/config.toml"))
            .env("FORGE_NO_AUTO_VERSION_CHECK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(home.join("stderr.log"))
                    .expect("open stderr log"),
            ));
        let mut child = command.spawn().expect("spawn forge-agent");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| json!({ "type": "invalid_json", "line": line }));
                recorded.lock().unwrap().push(message.clone());
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        Agent { child, stdin, rx, seen }
    }

    fn send(&mut self, message: Value) {
        let frame = format!("{message}\n");
        let _ = self.stdin.write_all(frame.as_bytes());
        let _ = self.stdin.flush();
    }

    fn wait_for_any(&self, kinds: &[&str]) -> Option<Value> {
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(message) => {
                    if let Some(t) = message.get("type").and_then(|t| t.as_str()) {
                        if kinds.contains(&t) {
                            return Some(message);
                        }
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// Everything the agent said, as text — for finding the notices it emits.
    fn transcript(&self) -> String {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture(name: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("compaction-over").join(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/forge")).unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    git(&workspace, &["init", "--quiet"]);
    git(&workspace, &["config", "user.email", "fixture@forge.invalid"]);
    git(&workspace, &["config", "user.name", "Forge fixture"]);
    home
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A small window, so an over-window history can be built without writing
/// hundreds of thousands of characters.
const WINDOW: usize = 8_000;

fn write_config(home: &Path, port: u16) {
    let config = format!(
        r#"
[models]
default = "fixture"
[[models.endpoints]]
name = "fixture"
base_url = "http://127.0.0.1:{port}/v1"
model_id = "compaction-fixture"
max_context_tokens = {WINDOW}
max_output_tokens = 512
request_timeout_secs = 10
endpoint_type = "open_ai"
api_key = "loopback-fixture-not-a-real-key"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 500
context_strategy = "compaction"
compact_at_percent = 80
"#
    );
    std::fs::write(home.join(".config/forge/config.toml"), config).expect("write config");
}

/// Grow the conversation past the window by talking, which is how it happens.
///
/// Planting a session log was the first attempt and the resume path rejected
/// it — a session is more than a directory with a `conversation.jsonl` in it.
/// Sending messages exercises the same state through the door a person uses,
/// and it also means the history is built by the agent's own bookkeeping
/// rather than by this test's idea of the format.
///
/// Returns once the agent reports it has compacted, or after `limit` turns.
fn talk_until_compacted(agent: &mut Agent, limit: usize) -> bool {
    // Sized against WINDOW: roughly four characters to a token in the
    // estimator, so each of these is about a fifth of the window and a handful
    // of them crosses it.
    let bulk = "the loader reads a header and then each segment in turn. ".repeat(120);
    for i in 0..limit {
        let marker = if i == 0 { "FIRST-MESSAGE-MARKER" } else { "LATER-MESSAGE" };
        agent.send(json!({
            "type": "send_message",
            "content": format!("{marker} turn {i}: {bulk}")
        }));
        let end = agent.wait_for_any(&["done", "error", "cancelled"]);
        match end.as_ref().and_then(|m| m.get("type")).and_then(|t| t.as_str()) {
            Some("done") => {}
            other => panic!("turn {i} ended as {other:?}: {end:?}\n{}", agent.transcript()),
        }
        if agent.transcript().contains("Context compacted") {
            return true;
        }
    }
    false
}

/// At over 100%, the summary is produced, the recent messages survive beside
/// it, and the turn completes.
///
/// This is the case the user asked about and the one two readings of the code
/// disagreed on. The assertions are made against what the provider received,
/// because that is the only place the answer is unambiguous.
#[test]
fn over_the_window_the_summary_is_made_and_the_turn_completes() {
    let (provider, port) = start_provider();
    let home = fixture("over-window");
    write_config(&home, port);

    let mut agent = Agent::start(&home, None);
    assert!(agent.wait_for_any(&["init"]).is_some(), "the agent never started");

    let compacted = talk_until_compacted(&mut agent, 8);
    let transcript = agent.transcript();
    agent.stop();

    assert!(
        compacted,
        "the conversation never compacted within eight turns against a \
         {WINDOW}-token window: {transcript}"
    );

    // The summary was actually generated. This is what the failure mode would
    // have skipped, leaving a blunt drop in its place.
    assert!(
        !provider.summariser_calls().is_empty(),
        "no summariser call was made, so the conversation was trimmed without a \
         summary. {} requests in total",
        provider.all().len()
    );
    assert!(
        !transcript.contains("Compaction failed"),
        "compaction failed on an over-window history: {transcript}"
    );
    // No request went out anywhere near the window's worth of characters — the
    // chunking is what makes summarising an over-window history possible.
    let largest = provider.largest_request_chars();
    assert!(
        largest < WINDOW * 40,
        "a request of {largest} characters went out against a {WINDOW}-token \
         window; the summariser input is not bounded"
    );
}

/// What survives: the summary, and the most recent messages verbatim.
#[test]
fn the_summary_and_the_recent_messages_both_survive() {
    let (provider, port) = start_provider();
    let home = fixture("what-survives");
    write_config(&home, port);

    let mut agent = Agent::start(&home, None);
    assert!(agent.wait_for_any(&["init"]).is_some(), "the agent never started");
    assert!(talk_until_compacted(&mut agent, 8), "never compacted");

    // One more turn, so there is a request built from the compacted history.
    agent.send(json!({ "type": "send_message", "content": "AFTER-COMPACTION carry on" }));
    assert_eq!(
        agent
            .wait_for_any(&["done", "error", "cancelled"])
            .as_ref()
            .and_then(|m| m.get("type"))
            .and_then(|t| t.as_str()),
        Some("done"),
        "the turn after compaction did not complete"
    );
    agent.stop();

    let all = provider.all();
    let last = all.last().expect("at least one request").to_string();

    // What the post-compaction request actually contains, by role, so a
    // failure says which part went missing rather than only that one did.
    let roles: Vec<String> = all
        .last()
        .and_then(|r| r.get("messages"))
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .map(|m| {
                    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                    let body = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                    let head: String = body.chars().take(60).collect();
                    format!("{role}: {head}")
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        last.contains("FIXTURE-DECISION"),
        "the summary's content is not in the request that followed compaction, \
         so the summary was discarded. The request carried:\n  {}",
        roles.join("\n  ")
    );
    assert!(
        last.contains("AFTER-COMPACTION"),
        "the newest message did not reach the model"
    );
    assert!(
        !last.contains("FIRST-MESSAGE-MARKER"),
        "the oldest message is still there verbatim, so nothing was compacted"
    );
}

/// One compaction has to get under the threshold, or every message pays for
/// another.
#[test]
fn after_compacting_the_conversation_is_under_the_window() {
    let (provider, port) = start_provider();
    let home = fixture("fits-after");
    write_config(&home, port);

    let mut agent = Agent::start(&home, None);
    assert!(agent.wait_for_any(&["init"]).is_some(), "the agent never started");
    assert!(talk_until_compacted(&mut agent, 8), "never compacted");

    let after_compaction = provider.all().len();

    // A short message, so this turn is not itself large enough to re-trigger.
    agent.send(json!({ "type": "send_message", "content": "ok" }));
    assert_eq!(
        agent
            .wait_for_any(&["done", "error", "cancelled"])
            .as_ref()
            .and_then(|m| m.get("type"))
            .and_then(|t| t.as_str()),
        Some("done"),
        "the turn after compaction did not complete"
    );
    agent.stop();

    let second: Vec<Value> = provider.all().into_iter().skip(after_compaction).collect();
    assert!(!second.is_empty(), "the second turn made no request");
    let resummarised = second
        .iter()
        .filter(|r| {
            let t = r.to_string();
            t.contains("conversation summarizer") || t.contains("summarizing part")
        })
        .count();
    assert_eq!(
        resummarised, 0,
        "a short message straight after compacting compacted again, so one \
         compaction does not bring the conversation under the threshold"
    );
}
