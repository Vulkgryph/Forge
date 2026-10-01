// SPDX-License-Identifier: Apache-2.0
//! The memory tools, driven through the real agent binary.
//!
//! The unit tests in `agent::memory` prove the store. They cannot prove the
//! things that actually decide whether this is usable in development: that the
//! tool is dispatched, that what it writes lands in the workspace, that a note
//! survives a process restart and reaches the model, that an expired one does
//! not, and that a refusal comes back as something the model can read rather
//! than as a failed turn.
//!
//! So this spawns `forge-agent --headless` against a loopback provider that
//! records every request and answers from a script. The assertion that matters
//! is made against what the provider *received*: if the note is in the system
//! prompt of a later request, it reached the model, and nothing short of
//! driving the real binary establishes that.
//!
//! Writing this found two defects the unit tests could not see — `remember`
//! was unclassified and so demanded approval on every call, and the notes were
//! read once at construction rather than per turn. Both are the kind of thing
//! only an integration test reaches.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(60);

/// A provider that answers from a script and keeps what it was asked.
struct Provider {
    /// Responses, in order. Each is either a tool call or a final message.
    script: Mutex<Vec<Reply>>,
    requests: Mutex<Vec<Value>>,
}

#[derive(Clone)]
enum Reply {
    /// Call `name` with `arguments`, then stop for the result.
    Tool { name: String, arguments: String },
    /// Say `text` and end the turn.
    Text(String),
}

impl Provider {
    fn next(&self) -> Reply {
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            // Anything past the script ends the turn, so an unexpected extra
            // request cannot hang the test waiting for a reply.
            return Reply::Text("done".into());
        }
        script.remove(0)
    }

    /// The system prompt of the nth request the provider received.
    fn system_prompt(&self, n: usize) -> String {
        let requests = self.requests.lock().unwrap();
        let Some(request) = requests.get(n) else {
            return String::new();
        };
        request
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|messages| {
                messages
                    .iter()
                    .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
                    .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn start_provider(script: Vec<Reply>) -> (Arc<Provider>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Provider {
        script: Mutex::new(script),
        requests: Mutex::new(Vec::new()),
    });
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

    // The request line first, because it is what distinguishes the model-list
    // probe from a completion. Deciding that from the body instead — "does it
    // have `messages`?" — reads the same for a GET and for a POST whose body
    // failed to load, and answering a completion with a model list surfaces as
    // "stream ended before completion" with nothing pointing here.
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    // Lowercased: reqwest sends `content-length`, and HTTP header names are
    // case-insensitive. Matching `Content-Length:` exactly left the length at
    // zero and the body empty.
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
        let payload = json!({ "data": [{ "id": "memory-fixture" }] }).to_string();
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

    let reply = {
        state.requests.lock().unwrap().push(request);
        state.next()
    };

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    )?;
    match reply {
        Reply::Tool { name, arguments } => {
            delta(
                &mut stream,
                &json!({
                    "tool_calls": [{
                        "index": 0, "id": "memory-fixture-call", "type": "function",
                        "function": { "name": name, "arguments": arguments }
                    }]
                }),
                None,
            )?;
            delta(&mut stream, &json!({}), Some("tool_calls"))?;
        }
        Reply::Text(text) => {
            delta(&mut stream, &json!({ "content": text }), None)?;
            delta(&mut stream, &json!({}), Some("stop"))?;
        }
    }
    stream.write_all(b"data: [DONE]\n\n")?;
    stream.flush()
}

fn delta(w: &mut TcpStream, body: &Value, finish: Option<&str>) -> std::io::Result<()> {
    let payload = json!({
        "choices": [{ "index": 0, "delta": body, "finish_reason": finish }]
    });
    write!(w, "data: {payload}\n\n")?;
    w.flush()
}

struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Agent {
    fn start(home: &Path) -> Agent {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge-agent"));
        command.arg("--headless");
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
                if let Some(t) = message.get("type").and_then(|t| t.as_str()) {
                    recorded.lock().unwrap().push(t.to_string());
                }
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

    /// Wait for a frame of `kind`, returning it.
    fn wait_for(&self, kind: &str) -> Option<Value> {
        self.wait_for_any(&[kind])
    }

    /// Wait for the first frame whose type is any of `kinds`.
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

    fn stop(mut self) {
        // The process this test started, by handle — never by name.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture(name: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("memory-live").join(name);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/forge")).unwrap();
    let workspace = home.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // Its own repository, so the agent's rewind snapshots do not walk up and
    // find the checkout this test is running from.
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

fn write_config(home: &Path, port: u16) {
    let config = format!(
        r#"
[models]
default = "fixture"
[[models.endpoints]]
name = "fixture"
base_url = "http://127.0.0.1:{port}/v1"
model_id = "memory-fixture"
max_context_tokens = 131072
max_output_tokens = 256
request_timeout_secs = 5
endpoint_type = "open_ai"
api_key = "loopback-fixture-not-a-real-key"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 200
"#
    );
    std::fs::write(home.join(".config/forge/config.toml"), config).expect("write config");
}

fn memory_dir(home: &Path) -> PathBuf {
    home.join("workspace").join(".forge").join("memory")
}

fn notes_on_disk(home: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(memory_dir(home)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("memory"))
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Run one turn: send `text`, wait for the turn to finish.
fn one_turn(agent: &mut Agent, text: &str) {
    agent.send(json!({ "type": "send_message", "content": text }));
    // `done` is the end of the model loop, including any tool calls it made
    // along the way.
    let end = agent.wait_for_any(&["done", "error", "cancelled"]);
    match end.as_ref().and_then(|m| m.get("type")).and_then(|t| t.as_str()) {
        Some("done") => {}
        Some(other) => panic!("the turn ended as {other}: {:?}", end),
        None => panic!(
            "the turn never ended. Frames seen: {:?}",
            agent.seen.lock().unwrap()
        ),
    }
}

/// The agent records a note, and it is on disk in the workspace.
#[test]
fn a_note_the_agent_records_lands_in_the_workspace() {
    let (provider, port) = start_provider(vec![
        Reply::Tool {
            name: "remember".into(),
            arguments: json!({
                "key": "loader-endianness",
                "note": "The Bastion loader is little-endian only.",
                "hours": 2
            })
            .to_string(),
        },
        Reply::Text("Noted.".into()),
    ]);
    let home = fixture("records");
    write_config(&home, port);

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "remember how the loader handles endianness");
    agent.stop();

    assert_eq!(
        notes_on_disk(&home),
        vec!["loader-endianness"],
        "the note is not in the workspace's memory directory"
    );
    let body = std::fs::read_to_string(memory_dir(&home).join("loader-endianness.memory"))
        .expect("the note is readable");
    assert!(body.contains("little-endian only"), "{body}");
    assert!(body.contains("expires_at:"), "no expiry was recorded: {body}");
    assert!(provider.request_count() >= 2, "the tool result was never sent back");
}

/// The assertion that matters: a note reaches the model on a later turn.
///
/// Nothing short of reading what the provider received establishes this. The
/// note is written in the first turn and has to appear in the system prompt of
/// a request made after it — which is also what proves the prompt is rebuilt
/// per turn rather than once at construction.
#[test]
fn a_recorded_note_reaches_the_model_on_a_later_turn() {
    let (provider, port) = start_provider(vec![
        Reply::Tool {
            name: "remember".into(),
            arguments: json!({
                "key": "other-agent-owns-botauth",
                "note": "The user's other agent owns botauth.rs. Do not edit it.",
                "hours": 1
            })
            .to_string(),
        },
        Reply::Text("Noted.".into()),
        // A second turn, whose request is the one that must carry the note.
        Reply::Text("Understood.".into()),
    ]);
    let home = fixture("reaches-model");
    write_config(&home, port);

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "note who owns botauth");
    one_turn(&mut agent, "what are you working on?");
    agent.stop();

    let last = provider.request_count() - 1;
    let prompt = provider.system_prompt(last);
    assert!(
        prompt.contains("other-agent-owns-botauth"),
        "the note never reached the model. System prompt of request {last}:\n{prompt}"
    );
    assert!(
        prompt.contains("Do not edit it"),
        "the note's text is missing from the prompt:\n{prompt}"
    );
    assert!(
        prompt.contains("not instructions"),
        "the notes are not framed as checkable claims:\n{prompt}"
    );
}

/// An expired note is not offered to the model, and is gone from disk.
#[test]
fn an_expired_note_is_not_offered_and_is_deleted() {
    let (provider, port) = start_provider(vec![Reply::Text("Nothing to report.".into())]);
    let home = fixture("expired");
    write_config(&home, port);

    // Planted directly, with an expiry in the past — the only way to test a
    // 90-day ceiling without waiting for it.
    std::fs::create_dir_all(memory_dir(&home)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        memory_dir(&home).join("stale.memory"),
        format!("written_at: {}\nexpires_at: {}\n\nThis stopped being true.\n", now - 9999, now - 1),
    )
    .unwrap();
    std::fs::write(
        memory_dir(&home).join("current.memory"),
        format!("written_at: {}\nexpires_at: {}\n\nThis is still true.\n", now - 10, now + 3600),
    )
    .unwrap();

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "anything to report?");
    agent.stop();

    let prompt = provider.system_prompt(0);
    assert!(
        !prompt.contains("stopped being true"),
        "an expired note was given to the model:\n{prompt}"
    );
    assert!(prompt.contains("This is still true."), "the live note is missing:\n{prompt}");
    assert_eq!(
        notes_on_disk(&home),
        vec!["current"],
        "the expired note is still on disk and a later run could load it"
    );
}

/// A refused write comes back as something the model can read, and nothing
/// escapes the memory directory.
///
/// The key becomes a filename, so this is the boundary that has to hold at the
/// tool layer too, not only in the module's own tests.
#[test]
fn a_hostile_key_is_refused_without_escaping_the_directory() {
    let (provider, port) = start_provider(vec![
        Reply::Tool {
            name: "remember".into(),
            arguments: json!({
                "key": "../../../../../../tmp/forge-memory-escape",
                "note": "should not be written outside the memory directory",
                "hours": 1
            })
            .to_string(),
        },
        Reply::Text("That was refused.".into()),
    ]);
    let home = fixture("hostile-key");
    write_config(&home, port);

    let escape = std::env::temp_dir().join("forge-memory-escape.memory");
    let _ = std::fs::remove_file(&escape);

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "try a hostile key");
    agent.stop();

    assert!(
        !escape.exists(),
        "a memory key escaped the directory and wrote to {}",
        escape.display()
    );
    // Whatever it did, it stayed inside.
    for name in notes_on_disk(&home) {
        assert!(
            !name.contains("..") && !name.contains('/'),
            "a note was created with the unsafe name {name:?}"
        );
    }
    // And the turn completed rather than failing, so the model got to read the
    // refusal — a tool error here would look like a fault in the agent.
    assert!(provider.request_count() >= 2, "the refusal never came back to the model");
}

/// An oversized note is refused, with the reason, and the turn survives it.
#[test]
fn an_oversized_note_is_refused_and_the_turn_continues() {
    let big = "x".repeat(4000);
    let (provider, port) = start_provider(vec![
        Reply::Tool {
            name: "remember".into(),
            arguments: json!({ "key": "too-big", "note": big, "hours": 1 }).to_string(),
        },
        Reply::Text("Too long, understood.".into()),
    ]);
    let home = fixture("oversized");
    write_config(&home, port);

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "try an enormous note");
    agent.stop();

    assert!(notes_on_disk(&home).is_empty(), "the oversized note was stored");

    // The refusal has to reach the model as a tool result it can act on.
    let requests = provider.requests.lock().unwrap();
    let text = requests
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Not remembered"),
        "the model was never told the note was refused"
    );
}

/// `forget` removes a note, and the model stops being shown it.
#[test]
fn forgetting_a_note_removes_it_from_the_prompt() {
    let (provider, port) = start_provider(vec![
        Reply::Tool {
            name: "forget".into(),
            arguments: json!({ "key": "wrong-note" }).to_string(),
        },
        Reply::Text("Dropped it.".into()),
        Reply::Text("Nothing in my notes.".into()),
    ]);
    let home = fixture("forget");
    write_config(&home, port);

    std::fs::create_dir_all(memory_dir(&home)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        memory_dir(&home).join("wrong-note.memory"),
        format!("written_at: {now}\nexpires_at: {}\n\nSomething I got wrong.\n", now + 86400),
    )
    .unwrap();

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    // First turn sees it and forgets it.
    one_turn(&mut agent, "that note is wrong, drop it");
    // Second turn must not be shown it.
    one_turn(&mut agent, "what do you know?");
    agent.stop();

    assert!(notes_on_disk(&home).is_empty(), "forget left the file behind");
    let last = provider.request_count() - 1;
    let prompt = provider.system_prompt(last);
    assert!(
        !prompt.contains("Something I got wrong"),
        "a forgotten note was still in the prompt:\n{prompt}"
    );
}

/// With no notes, the prompt gains nothing — an agent that has recorded
/// nothing should not be told about an empty section, and should not pay for
/// one.
#[test]
fn an_empty_store_adds_nothing_to_the_prompt() {
    let (provider, port) = start_provider(vec![Reply::Text("Hello.".into())]);
    let home = fixture("empty");
    write_config(&home, port);

    let mut agent = Agent::start(&home);
    assert!(agent.wait_for("init").is_some(), "the agent never started");
    one_turn(&mut agent, "hello");
    agent.stop();

    let prompt = provider.system_prompt(0);
    assert!(
        !prompt.contains("Your notes"),
        "an empty store still added a section to the prompt:\n{prompt}"
    );
}
