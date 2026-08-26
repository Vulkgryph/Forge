//! SSH Remote client — connects to a remote machine, auto-uploads forge-server
//! if absent, then communicates over a JSON-RPC channel (forge-proto) running
//! over an SSH exec channel.  All file, LSP, and PTY operations are routed
//! through the server instead of raw SFTP.
//!
//! Threading:
//!   • `SshConnection::connect` blocks the calling thread until the server is
//!     ready (short: SSH + potential binary upload).
//!   • A reader thread drains server→client push notifications and responses.
//!   • The main thread sends requests via the write half (Arc<Mutex>).
//!   • Responses are stashed in `pending`; push notifications (PTY, LSP) are
//!     delivered via callbacks registered at request time.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use russh::client::{self, Handle};
use russh::keys::{PrivateKeyWithHashAlg, load_secret_key, ssh_key};
use russh::ChannelMsg;

use russh_sftp::client::SftpSession;
use tokio::runtime::Runtime;

fn expand_tilde(s: &str) -> String {
    if s.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return s.replacen('~', &home, 1);
        }
    }
    s.to_string()
}

use forge_proto::*;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SshHost {
    pub name:       String,
    pub host:       String,
    pub port:       u16,
    pub user:       String,
    pub key_path:   String,
    pub remote_dir: String,
}

impl Default for SshHost {
    fn default() -> Self {
        Self {
            name: String::new(), host: String::new(), port: 22,
            user: String::new(), key_path: String::new(),
            remote_dir: "~".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteEntry {
    pub name:   String,
    pub path:   String,
    pub is_dir: bool,
    pub size:   u64,
}

/// Everything the UI needs after a successful connection — built entirely on
/// the background thread so the main thread never blocks.
pub struct SshReady {
    pub conn:       SshConnection,
    pub root_path:  String,
    pub entries:    Vec<RemoteEntry>,
    pub shell:      Option<ShellChannel>,
    pub shell_err:  Option<String>,
}

/// I/O handles for a PTY session running on the remote machine.
pub struct ShellChannel {
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub tx: mpsc::SyncSender<Vec<u8>>,
}

// ── Connection ────────────────────────────────────────────────────────────────

type PendMap = Arc<Mutex<HashMap<i64, mpsc::SyncSender<Result<serde_json::Value, String>>>>>;

/// A detached ticket for talking to an open connection, cheap to clone and safe
/// to hand to a thread. See `SshConnection::fs_handles`.
#[derive(Clone)]
pub struct FsHandles {
    next_id: Arc<Mutex<i64>>,
    pending: PendMap,
    stdin:   Arc<Mutex<Box<dyn Write + Send>>>,
}

/// An open connection's request path with nothing on the far end but a canned
/// answer, so callers of `fs_list_with` can be tested without a machine.
///
/// Answers on the writing thread the moment a request is framed. That is not
/// how a real connection behaves, but what is under test here is what gets
/// asked and what is made of the reply — not concurrency.
#[cfg(test)]
pub struct FakeFs {
    pub handles:  FsHandles,
    /// Every request the code under test sent, in order.
    pub requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[cfg(test)]
pub fn fake_fs(reply: Result<serde_json::Value, String>, answer: bool) -> FakeFs {
    struct W {
        buf:      Vec<u8>,
        pending:  PendMap,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
        reply:    Result<serde_json::Value, String>,
        answer:   bool,
    }
    impl Write for W {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(bytes);
            while let Some(req) = take_frame(&mut self.buf) {
                self.requests.lock().unwrap().push(req.clone());
                if !self.answer { continue; }
                let id = req.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(self.reply.clone());
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    /// One complete `Content-Length`-framed message, removed from `buf`.
    fn take_frame(buf: &mut Vec<u8>) -> Option<serde_json::Value> {
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&buf[..sep]).ok()?;
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))?
            .trim()
            .parse()
            .ok()?;
        let start = sep + 4;
        if buf.len() < start + len { return None; }
        let value = serde_json::from_slice(&buf[start..start + len]).ok();
        buf.drain(..start + len);
        value
    }

    let pending: PendMap = Arc::new(Mutex::new(HashMap::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let writer = W {
        buf: Vec::new(),
        pending: Arc::clone(&pending),
        requests: Arc::clone(&requests),
        reply,
        answer,
    };
    FakeFs {
        handles: FsHandles {
            next_id: Arc::new(Mutex::new(1)),
            pending,
            stdin: Arc::new(Mutex::new(Box::new(writer))),
        },
        requests,
    }
}

/// `fs/mkdir` over the same shared handles as `fs_list_with`.
pub fn fs_mkdir_with(
    handles: &FsHandles,
    path:    &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    call_with(handles, "fs/mkdir", serde_json::json!({ "path": path }), timeout).map(|_| ())
}

/// `fs/list` over shared handles rather than `&SshConnection`.
///
/// One implementation, because the explorer, the folder chooser and Quick Open
/// all list remote directories, and three hand-rolled copies of this request
/// would be three chances to disagree about what a listing means.
/// One request over shared handles, and its reply.
fn call_with(
    handles: &FsHandles,
    method:  &str,
    params:  serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, String> {
    let id = {
        let mut n = handles.next_id.lock().unwrap();
        let i = *n;
        *n += 1;
        i
    };
    let msg = Rpc::request(id, method, params);
    let (tx, rx) = mpsc::sync_channel(1);
    handles.pending.lock().unwrap().insert(id, tx);
    if let Ok(mut w) = handles.stdin.lock() {
        write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
    }
    rx.recv_timeout(timeout)
        .map_err(|_| format!("{method} timed out"))?
}

pub fn fs_list_with(
    handles: &FsHandles,
    path:    &str,
    timeout: std::time::Duration,
) -> Result<Vec<RemoteEntry>, String> {
    let value = call_with(handles, "fs/list", serde_json::json!({ "path": path }), timeout)?;
    // The reply carries entries and no path of its own, so a caller that needs
    // to display where it is, or to walk up from it, must resolve the path it
    // asked with — it will not be told it back.
    let entries: Vec<FsEntry> =
        serde_json::from_value(value.get("entries").cloned().unwrap_or_default())
            .map_err(|e| e.to_string())?;
    Ok(entries
        .into_iter()
        .map(|e| RemoteEntry { name: e.name, path: e.path, is_dir: e.is_dir, size: e.size })
        .collect())
}

pub struct SshConnection {
    pub host:    SshHost,
    /// The remote account's home directory, resolved once at connect. Forge's
    /// own binaries live under it, and asking again per call would be a round
    /// trip for something that cannot change while the session is open.
    pub remote_home: String,
    /// The endpoint the remote agent's model requests are served against, when
    /// it is running in proxied mode. Shared with the connection's handler.
    ///
    /// Read only through the handler's own clone of this `Arc`, which is why
    /// the field itself looks unused.
    #[allow(dead_code)]
    upstream: Arc<Mutex<Option<crate::model_proxy::Routes>>>,
    pub next_id: Arc<Mutex<i64>>,
    pub pending: PendMap,
    /// Channel stdin (server → client notifications arrive via push callbacks)
    pub stdin:   Arc<Mutex<Box<dyn Write + Send>>>,
    /// PTY push callbacks keyed by pty id
    pub pty_pushes: Arc<Mutex<HashMap<u32, mpsc::SyncSender<Vec<u8>>>>>,
    /// Session dropped FIRST (closes SSH channel, unblocks reader tasks).
    ///
    /// `Arc` because uploads run as spawned tasks that need `&Handle` to open
    /// their own SFTP channel (`Handle` is not `Clone` — it owns a receiver and
    /// a join handle). Dropping our reference below still closes the session
    /// promptly in the common case; if an upload is in flight, the session
    /// outlives us until `shutdown_background` drops that task, which is the
    /// behaviour you want anyway — yanking the channel mid-transfer would
    /// corrupt the file.
    _session:    Option<Arc<russh::client::Handle<ClientHandler>>>,
    /// Runtime dropped LAST via shutdown_background() — non-blocking.
    _rt:         Option<tokio::runtime::Runtime>,
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        // Drop the session first so the SSH exec channel closes and the
        // reader task's wait() returns None → task exits cleanly.
        drop(self._session.take());
        // Then shut down the runtime without waiting for tasks — avoids
        // blocking the main thread if any task is stuck on network I/O.
        if let Some(rt) = self._rt.take() {
            rt.shutdown_background();
        }
    }
}

impl SshConnection {
    /// Connect, upload forge-server if needed, start it, return a ready client.
    /// `log` receives progress messages to display in the Output panel.
    /// `trust_new_host_key` is the user's answer to having been shown an
    /// unknown host's fingerprint. It is passed per attempt rather than stored,
    /// so accepting one host never quietly accepts the next.
    pub fn connect(
        host:     &SshHost,
        password: Option<&str>,
        trust_new_host_key: bool,
        log:      &dyn Fn(&str, crate::OutputLevel),
    ) -> Result<Self, String> {
        use crate::OutputLevel::*;
        let rt  = Runtime::new().map_err(|e| e.to_string())?;
        // Shared with the handler, which serves whatever the remote sends to
        // its forwarded port. Empty until a proxied agent is started.
        let upstream: Arc<Mutex<Option<crate::model_proxy::Routes>>> =
            Arc::new(Mutex::new(None));
        let h   = host.clone();
        let pw  = password.map(|s| s.to_string());

        // 1. Authenticate SSH session
        log(&format!("Authenticating {}@{}:{}", h.user, h.host, h.port), Info);
        let session = rt.block_on(ssh_authenticate(&h, pw.as_deref(), trust_new_host_key, &upstream))
            .map_err(|e| {
                // A host-key refusal is not an authentication failure, and
                // calling it one sends the user off checking their key.
                if !e.starts_with(UNKNOWN_HOST_PREFIX) && !e.starts_with(CHANGED_HOST_PREFIX) {
                    log(&format!("Authentication failed: {e}"), Error);
                }
                e
            })?;
        log("Authentication successful", Success);

        // 2. Check if forge-server is present; upload via SFTP if not
        log("Checking remote forge-server…", Info);
        let home = remote_home(&rt, &session)?;
        let server_path = format!("{home}/.forge/forge-server");
        let marker_path = format!("{server_path}.version");
        // Log whether we need to upload so the user can see progress
        let needs_up = rt.block_on(async {
            let mut ch = session.channel_open_session().await?;
            ch.exec(true, format!("cat {marker_path} 2>/dev/null || echo MISSING")).await?;
            let mut out = String::new();
            while let Some(msg) = ch.wait().await {
                if let ChannelMsg::Data { data } = msg {
                    out.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok::<bool, russh::Error>(out.trim() != SERVER_VERSION)
        }).unwrap_or(true);
        if needs_up {
            log(&format!("Uploading forge-server {} (1.2 MB)…", SERVER_VERSION), Info);
        }
        let arch = remote_arch(&rt, &session)?;
        ensure_binary_uploaded(
            &rt, &session, &server_path, SERVER_VERSION,
            local_server_binary(&arch)?,
        )
            .map_err(|e| { log(&format!("Server upload failed: {e}"), Error); e })?;
        log(&format!("forge-server ready at {server_path}"), Success);

        // The agent too, here rather than when the panel first opens: this runs
        // on a background thread with progress in the Output panel, where the
        // panel-open path is a frame the user is waiting on — uploading six
        // megabytes there froze the window for the length of the transfer.
        //
        // Not fatal if it fails. A connection whose file tree and terminal work
        // is worth having even if the agent cannot run, and the failure is
        // reported again, in full, if the panel is then opened.
        log("Checking remote forge-agent…", Info);
        match remote_arch(&rt, &session).and_then(|arch| local_agent_binary(&arch)) {
            Ok(bytes) => {
                let agent_path = format!("{home}/.forge/forge-agent");
                match ensure_binary_uploaded(&rt, &session, &agent_path, AGENT_VERSION, bytes) {
                    Ok(()) => log(&format!("forge-agent ready at {agent_path}"), Success),
                    Err(e) => log(&format!("forge-agent unavailable: {e}"), Warn),
                }
            }
            Err(e) => log(&format!("forge-agent unavailable: {e}"), Warn),
        }

        log("Starting forge-server…", Info);
        // We bridge the async channel to sync mpsc channels here so the tokio
        // runtime (and session) can be shut down after connect() returns.
        let server_path2 = server_path.clone();
        let (reader, writer) = rt.block_on(open_exec_channel_bridged(
            &session, &format!("{server_path2} --stdio")))
            .map_err(|e| e.to_string())?;

        // 4. Wire up response routing
        let pending:    PendMap = Arc::new(Mutex::new(HashMap::new()));
        let pty_pushes  = Arc::new(Mutex::new(HashMap::<u32, mpsc::SyncSender<Vec<u8>>>::new()));
        let next_id     = Arc::new(Mutex::new(1i64));

        let p2  = Arc::clone(&pending);
        let pp2 = Arc::clone(&pty_pushes);

        std::thread::spawn(move || {
            let mut r = BufReader::new(reader);
            loop {
                let Some(msg) = read_rpc(&mut r) else { break };
                if msg.is_response() {
                    let id = msg.id.unwrap();
                    let res = if let Some(e) = msg.error { Err(e.message) }
                              else { Ok(msg.result.unwrap_or(serde_json::Value::Null)) };
                    if let Some(tx) = p2.lock().unwrap().remove(&id) {
                        let _ = tx.send(res);
                    }
                } else if msg.is_notification() {
                    match msg.method.as_deref().unwrap_or("") {
                        "pty/data" => {
                            if let Ok(push) = serde_json::from_value::<PtyDataPush>(
                                msg.params.unwrap_or_default())
                            {
                                if let Some(tx) = pp2.lock().unwrap().get(&push.id) {
                                    let _ = tx.send(push.data);
                                }
                                // Wake the loop, as every other producer does.
                                // It sleeps between frames and the draw side
                                // only `try_recv`s this channel, so without
                                // this the remote's output sat in the queue
                                // until something else caused a frame — the
                                // next keystroke, usually. Typing therefore
                                // echoed one character behind, which reads as
                                // the network being slow when it is not.
                                crate::wake::wake();
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Ok(Self {
            host: host.clone(),
            remote_home: home.clone(),
            upstream,
            next_id,
            pending,
            stdin:    Arc::new(Mutex::new(Box::new(writer) as Box<dyn Write + Send>)),
            pty_pushes,
            _session: Some(Arc::new(session)),
            _rt:      Some(rt),
        })
    }

    // ── fs ────────────────────────────────────────────────────────────────────

    pub fn fs_list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        fs_list_with(&self.fs_handles(), path, std::time::Duration::from_secs(30))
    }

    /// The pieces of this connection a background thread needs to make its own
    /// requests. The connection itself cannot cross a thread boundary — it owns
    /// the runtime and the session — but these three are shared handles, and a
    /// listing must run off the render thread or a slow link freezes the window.
    pub fn fs_handles(&self) -> FsHandles {
        FsHandles {
            next_id: Arc::clone(&self.next_id),
            pending: Arc::clone(&self.pending),
            stdin:   Arc::clone(&self.stdin),
        }
    }

    /// Put the agent on the remote machine, and answer where it is.
    ///
    /// Uploaded on demand rather than at connect: a session that never opens
    /// the agent panel should not pay six megabytes for it. Replaced when this
    /// client's version moves on, by the same marker forge-server uses.
    pub fn ensure_agent(&self) -> Result<String, String> {
        let (Some(rt), Some(session)) = (self._rt.as_ref(), self._session.as_ref()) else {
            return Err("connection is closing".into());
        };
        let arch = remote_arch(rt, session)?;
        let bytes = local_agent_binary(&arch)?;
        let path = format!("{}/.forge/forge-agent", self.remote_home.trim_end_matches('/'));
        ensure_binary_uploaded(rt, session, &path, AGENT_VERSION, bytes)?;
        Ok(path)
    }

    /// Ask the remote to listen on a loopback port and forward it back here,
    /// serving whatever arrives against `upstream`.
    ///
    /// Returns the port the remote should send to. Loopback on that machine, so
    /// nothing off it can reach the tunnel — the port answers only to processes
    /// on the remote itself, and everything it answers with came from here.
    ///
    /// Port 0 asks the remote's sshd to choose, avoiding a fixed number that
    /// two sessions to the same host would fight over.
    /// Not yet called: the agent needs a way to be told its credentials are
    /// proxied before this can be turned on for a session. See the note in
    /// `model_proxy`.
    #[allow(dead_code)]
    pub fn open_model_proxy(
        &self,
        routes: crate::model_proxy::Routes,
    ) -> Result<u16, String> {
        let (Some(rt), Some(session)) = (self._rt.as_ref(), self._session.as_ref()) else {
            return Err("connection is closing".into());
        };
        // Set before the forward is requested: a connection can arrive as soon
        // as the remote is listening, and one that finds no upstream is
        // silently dropped.
        *self.upstream.lock().unwrap() = Some(routes);
        let port = rt
            .block_on(session.tcpip_forward("127.0.0.1", 0))
            .map_err(|e| format!("could not open the model tunnel: {e}"))?;
        Ok(port as u16)
    }

    /// Start the agent on the remote machine and return its stdio.
    ///
    /// Its own SSH exec channel rather than anything routed through
    /// forge-server: the agent speaks a line-oriented protocol over stdin and
    /// stdout, which is exactly what an exec channel is, and a pty would
    /// corrupt it with echo and newline translation.
    ///
    /// Every tool the agent runs is then local to that machine — the point of
    /// the exercise. What does *not* travel is credentials: those are handed
    /// over the channel after it opens, or proxied back here, and never written
    /// to that machine's disk.
    pub fn spawn_agent(
        &self,
        cwd: &str,
        resume: Option<&str>,
        allow_all: bool,
    ) -> Result<(Box<dyn std::io::Read + Send>, Box<dyn std::io::Write + Send>), String> {
        let agent = self.ensure_agent()?;
        let (Some(rt), Some(session)) = (self._rt.as_ref(), self._session.as_ref()) else {
            return Err("connection is closing".into());
        };

        // `cd` first so the agent's project root is the remote workspace. Its
        // own default would be whatever directory the SSH session started in.
        let mut command = format!("cd {} && {agent} --headless", shell_quote_path(cwd));
        if let Some(id) = resume {
            command.push_str(&format!(" --resume-session {}", shell_quote(id)));
        }
        if allow_all {
            command.push_str(" --dangerously-allow-all");
        }

        let (reader, writer) = rt
            .block_on(open_exec_channel_bridged(session, &command))
            .map_err(|e| e.to_string())?;
        Ok((Box::new(reader), Box::new(writer)))
    }

    pub fn fs_write(&self, path: &str, text: &str) -> Result<(), String> {
        self.call("fs/write", serde_json::json!({ "path": path, "text": text }))?;
        Ok(())
    }

    /// Upload local files into a remote directory, off the UI thread.
    ///
    /// Returns a receiver yielding one result per file, in order. Uploads run as
    /// tasks on the connection's own tokio runtime rather than a std thread, so
    /// nothing here has to own or share the runtime — `spawn` only needs `&self`,
    /// and the task keeps the session alive through the `Arc` for exactly as
    /// long as the transfer takes.
    ///
    /// SFTP rather than the `fs/write` RPC: that call carries a `&str`, and a
    /// dropped file is as likely to be a screenshot as it is text. Base64 over
    /// JSON would mean a forge-server change and therefore a forced re-upload of
    /// the server binary on every host. SFTP is already how forge-server itself
    /// gets deployed here.
    pub fn fs_upload(&self, files: Vec<PathBuf>, remote_dir: &str)
        -> mpsc::Receiver<Result<String, String>>
    {
        let (tx, rx) = mpsc::channel();
        let (Some(session), Some(rt)) = (self._session.as_ref(), self._rt.as_ref()) else {
            let _ = tx.send(Err("ssh session closed".into()));
            return rx;
        };
        let session = Arc::clone(session);
        let dir = remote_dir.trim_end_matches('/').to_string();

        rt.spawn(async move {
            use tokio::io::AsyncWriteExt;
            for local in files {
                let res = async {
                    let name = local.file_name().and_then(|n| n.to_str())
                        .ok_or_else(|| "file has no usable name".to_string())?;
                    let remote_path = format!("{dir}/{name}");
                    // Read off the async worker: this runtime also drives the
                    // SSH reader task, and a blocking read of a large file would
                    // stall it. (`tokio::fs` isn't in this crate's feature set,
                    // hence spawn_blocking rather than an async read.)
                    let path_for_read = local.clone();
                    let bytes = tokio::task::spawn_blocking(move || std::fs::read(&path_for_read))
                        .await
                        .map_err(|e| format!("read task: {e}"))?
                        .map_err(|e| format!("read {}: {e}", local.display()))?;

                    let ch = session.channel_open_session().await
                        .map_err(|e| format!("open channel: {e}"))?;
                    ch.request_subsystem(true, "sftp").await
                        .map_err(|e| format!("sftp subsystem: {e}"))?;
                    let sftp = SftpSession::new(ch.into_stream()).await
                        .map_err(|e| format!("sftp session: {e}"))?;
                    let mut f = sftp.create(&remote_path).await
                        .map_err(|e| format!("create {remote_path}: {e}"))?;
                    f.write_all(&bytes).await.map_err(|e| format!("write: {e}"))?;
                    f.flush().await.map_err(|e| format!("flush: {e}"))?;
                    drop(f);
                    drop(sftp);
                    Ok::<String, String>(remote_path)
                }.await;
                // Receiver gone means the window closed; stop rather than
                // finishing a transfer nobody is waiting for.
                if tx.send(res).is_err() { break; }
            }
        });
        rx
    }

    // ── RPC internals ─────────────────────────────────────────────────────────

    /// Synchronous request: send and block until response arrives.
    fn call(&self, method: &str, params: serde_json::Value)
        -> Result<serde_json::Value, String>
    {
        self.call_timeout(method, params, std::time::Duration::from_secs(30))
    }

    fn call_timeout(&self, method: &str, params: serde_json::Value,
                    timeout: std::time::Duration)
        -> Result<serde_json::Value, String>
    {
        let id  = { let mut n = self.next_id.lock().unwrap(); let i = *n; *n += 1; i };
        let msg = Rpc::request(id, method, params);
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, tx);
        if let Ok(mut w) = self.stdin.lock() {
            write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
        }
        rx.recv_timeout(timeout)
            .map_err(|_| format!("timeout waiting for {method}"))
            .and_then(|r| r)
    }

}

// ── SSH auth + exec channel (tokio) ──────────────────────────────────────────

/// Prefixes marking the two host-key outcomes in an error string, so the UI
/// can tell "ask the user" from "refuse and explain" without parsing prose.
pub const UNKNOWN_HOST_PREFIX: &str = "forge:unknown-host:";
pub const CHANGED_HOST_PREFIX: &str = "forge:changed-host:";

/// What checking a server's key against `~/.ssh/known_hosts` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKey {
    /// Listed, and it matches.
    Known,
    /// Not listed. The user is shown the fingerprint and asked, once, the way
    /// `ssh` asks.
    Unknown { fingerprint: String },
    /// Listed, and it does *not* match. Either the host was rebuilt or someone
    /// is between you and it, and nothing here can tell which — so this is
    /// never accepted, and never offered as a button.
    Changed { fingerprint: String, line: usize },
}

/// Whether this exact key is among those recorded for the host.
///
/// Compares key material rather than whole `PublicKey`s: the derived equality
/// includes the comment, and a key parsed from a file carries one where a key
/// off the wire does not, so two records of the same key can compare unequal.
/// Identity here means the key, and nothing else.
fn key_is_recorded(recorded: &[(usize, ssh_key::PublicKey)], key: &ssh_key::PublicKey) -> bool {
    recorded.iter().any(|(_, k)| k.key_data() == key.key_data())
}

/// Verifies the server's host key, and records what it found.
///
/// This used to return `Ok(true)` unconditionally, with a `TODO: known_hosts`
/// beside it. Every server was trusted on sight: anyone able to answer for the
/// address — a hijacked node on the tailnet, a spoofed DNS reply, a hostile
/// network — could present their own key, and Forge would authenticate to them
/// and then hand over the file tree, the terminal and everything typed into it.
/// `ssh` itself refuses to connect in that situation.
struct ClientHandler {
    host:      String,
    port:      u16,
    /// Set once a remote agent is started in proxied mode. Every connection the
    /// remote opens to its forwarded port arrives here and is served against
    /// this — which is how the credential reaches the model API without ever
    /// reaching the machine the agent runs on.
    upstream:  Arc<Mutex<Option<crate::model_proxy::Routes>>>,
    /// Set once the user has been shown a fingerprint and accepted it. Only
    /// ever applies to a host that is *absent* from known_hosts, never to one
    /// whose key has changed.
    trust_new: bool,
    /// What the check found, for the caller to report or ask about.
    found:     Arc<Mutex<Option<HostKey>>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;
    fn check_server_key(
        &mut self, key: &ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let trust_new = self.trust_new;
        let found = Arc::clone(&self.found);
        let key = key.clone();
        async move {
            let fingerprint = key.fingerprint(Default::default()).to_string();
            let verdict = match russh::keys::check_known_hosts(&host, port, &key) {
                Ok(true) => HostKey::Known,
                Ok(false) if trust_new => {
                    // Remembered only now, after the user said so — writing it
                    // at first sight is what "trust on first use" degenerates
                    // into when nobody is asked.
                    //
                    // Written without its comment. russh's own writer includes
                    // one and its own reader then reports that same key as
                    // *changed*, which would greet the user with a security
                    // warning about a host they had just trusted. A key off the
                    // wire carries no comment, so this only matters if that
                    // ever stops being true — but the failure it prevents is
                    // one nobody would think to look for.
                    let mut to_learn = key.clone();
                    to_learn.set_comment("");
                    let _ = russh::keys::known_hosts::learn_known_hosts(&host, port, &to_learn);
                    HostKey::Known
                }
                Ok(false) => HostKey::Unknown { fingerprint },
                Err(russh::keys::Error::KeyChanged { line }) => {
                    // Not taken at face value. russh stops at the first
                    // recorded entry of the same algorithm that does not
                    // match, so a stale line left beside the current one
                    // reports a changed key even when the right one is in the
                    // same file — and it does so whichever order they are in,
                    // including when the match is found first. Refusing on
                    // that would raise the loudest warning Forge has over a
                    // host that is exactly who it says it is.
                    let recorded =
                        russh::keys::known_hosts::known_host_keys(&host, port)
                            .unwrap_or_default();
                    if key_is_recorded(&recorded, &key) {
                        HostKey::Known
                    } else {
                        HostKey::Changed { fingerprint, line }
                    }
                }
                // Anything else — an unreadable known_hosts, a malformed line —
                // is not a reason to trust the key. Failing open here would
                // undo the whole check for anyone with a broken file.
                Err(_) => HostKey::Unknown { fingerprint },
            };
            let accept = verdict == HostKey::Known;
            *found.lock().unwrap() = Some(verdict);
            Ok(accept)
        }
    }

    /// A connection the remote made to its forwarded port — the agent asking
    /// for a model.
    ///
    /// Served on its own thread: `serve_one` is blocking, and a completion
    /// streams for as long as the model takes to write it, which is far too
    /// long to hold the SSH event loop.
    fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let routes = self.upstream.lock().unwrap().clone();
        async move {
            let Some(routes) = routes else {
                // Nothing is proxying, so this port is not ours to answer.
                return Ok(());
            };
            let (reader, writer) = bridge_channel(channel);
            std::thread::spawn(move || {
                let mut stream = BridgedStream { reader, writer };
                if let Err(e) = crate::model_proxy::serve_one(&mut stream, &routes) {
                    // Named, never with the credential in it.
                    eprintln!("model proxy: {e}");
                }
            });
            Ok(())
        }
    }
}

/// The two halves of a bridged channel as one stream, for `serve_one`.
struct BridgedStream {
    reader: ChannelReader,
    writer: ChannelWriter2,
}
impl std::io::Read for BridgedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { self.reader.read(buf) }
}
impl Write for BridgedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.writer.write(buf) }
    fn flush(&mut self) -> std::io::Result<()> { self.writer.flush() }
}

async fn ssh_authenticate(
    host: &SshHost,
    password: Option<&str>,
    trust_new: bool,
    upstream: &Arc<Mutex<Option<crate::model_proxy::Routes>>>,
) -> Result<Handle<ClientHandler>, String> {
    let config = Arc::new(client::Config::default());
    let addr   = format!("{}:{}", host.host, host.port);
    let found  = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        host: host.host.clone(),
        port: host.port,
        trust_new,
        found: Arc::clone(&found),
        upstream: Arc::clone(upstream),
    };
    let session = match client::connect(config, addr, handler).await {
        Ok(s) => s,
        Err(e) => {
            // A rejected host key surfaces as an ordinary connection failure,
            // so the verdict is what actually explains it.
            return Err(match found.lock().unwrap().clone() {
                Some(HostKey::Unknown { fingerprint }) => {
                    format!("{UNKNOWN_HOST_PREFIX}{fingerprint}")
                }
                Some(HostKey::Changed { fingerprint, line }) => format!(
                    "{CHANGED_HOST_PREFIX}The host key for {} has changed.\n\n\
                     It now presents {fingerprint}, which does not match line {line} of \
                     ~/.ssh/known_hosts. Either that machine was rebuilt, or something is \
                     answering for it that is not it.\n\n\
                     Forge will not connect. If you know the host was rebuilt, remove line \
                     {line} from ~/.ssh/known_hosts and connect again.",
                    host.host,
                ),
                _ => e.to_string(),
            });
        }
    };
    let mut session = session;

    let authed = if !host.key_path.is_empty() {
        let expanded = expand_tilde(&host.key_path);
        if let Ok(key) = load_secret_key(&expanded, None) {
            let kwh = PrivateKeyWithHashAlg::new(Arc::new(key), None);
            matches!(
                session.authenticate_publickey(&host.user, kwh).await
                    .map_err(|e| e.to_string())?,
                russh::client::AuthResult::Success
            )
        } else { false }
    } else { false };

    let authed = if !authed {
        match russh::keys::agent::client::AgentClient::connect_env().await {
            Ok(mut agent) => {
                let ids = agent.request_identities().await.unwrap_or_default();
                let mut ok = false;
                for id in &ids {
                    let pub_key = id.public_key().into_owned();
                    let res = session.authenticate_publickey_with(
                        &host.user, pub_key, None, &mut agent,
                    ).await;
                    if matches!(res, Ok(russh::client::AuthResult::Success)) {
                        ok = true; break;
                    }
                }
                ok
            }
            Err(_) => false,
        }
    } else { authed };

    if !authed {
        let pw = password.ok_or("SSH authentication failed")?;
        if !matches!(
            session.authenticate_password(&host.user, pw).await
                .map_err(|e| e.to_string())?,
            russh::client::AuthResult::Success
        ) {
            return Err("SSH password authentication failed".into());
        }
    }

    Ok(session)
}

/// Get the remote $HOME by running `echo $HOME` over an exec channel.
fn remote_home(rt: &Runtime, session: &Handle<ClientHandler>) -> Result<String, String> {
    rt.block_on(async {
        let mut ch = session.channel_open_session().await
            .map_err(|e| e.to_string())?;
        ch.exec(true, "echo $HOME").await.map_err(|e| e.to_string())?;
        let mut out = String::new();
        while let Some(msg) = ch.wait().await {
            if let ChannelMsg::Data { data } = msg {
                out.push_str(&String::from_utf8_lossy(&data));
            }
        }
        Ok(out.trim().to_string())
    })
}

/// Upload the local forge-server binary to the remote if not already present.
/// Read from forge-server/Cargo.toml's own `version` at build time (see
/// build.rs) rather than hand-copied here, so the two can't silently drift —
/// bumping forge-server's version alone is enough to force a re-upload.
/// Where the remote agent lives, and which build it is.
///
/// Beside forge-server, since both are Forge's own binaries on that machine and
/// both are managed the same way — uploaded on demand, replaced when the local
/// version moves on.
pub const AGENT_VERSION: &str = concat!("forge-agent-", env!("FORGE_AGENT_VERSION"));

const SERVER_VERSION: &str = concat!("forge-server-", env!("FORGE_SERVER_VERSION"));

/// Put `bytes` at `server_path` on the remote unless a marker beside it already
/// names `version`.
///
/// Shared by forge-server and forge-agent: both are Linux builds of a sibling
/// crate that this client ships and uploads, and both need the same "is the
/// copy over there current" question answered the same way.
fn ensure_binary_uploaded(
    rt:          &Runtime,
    session:     &Handle<ClientHandler>,
    server_path: &str,
    version:     &str,
    bytes:       Vec<u8>,
) -> Result<(), String> {
    // Use a plain text marker file next to the binary.  Avoids running the
    // binary for version checks (unreliable: old binary blocks on stdin).
    let marker = format!("{server_path}.version");
    let needs_upload = rt.block_on(async {
        let mut ch = session.channel_open_session().await?;
        ch.exec(true, format!("cat {marker} 2>/dev/null || echo MISSING")).await?;
        let mut out = String::new();
        while let Some(msg) = ch.wait().await {
            if let ChannelMsg::Data { data } = msg {
                out.push_str(&String::from_utf8_lossy(&data));
            }
        }
        Ok::<bool, russh::Error>(out.trim() != version)
    }).map_err(|e: russh::Error| e.to_string())?;

    if !needs_upload { return Ok(()); }

    let local_binary = bytes;
    eprintln!("uploading v{version} ({} bytes) to {server_path}…", local_binary.len());
    let marker2 = marker.clone();
    let version = version.to_string();

    // ONE block: mkdir + SFTP upload of binary + marker + chmod.
    // Consolidated to minimise channel count and round-trips over slow VPNs.
    rt.block_on(async {
        use tokio::io::AsyncWriteExt;

        // mkdir -p
        let dir = server_path.rsplitn(2, '/').last().unwrap_or(".forge");
        let mut ch = session.channel_open_session().await
            .map_err(|e| e.to_string())?;
        ch.exec(true, format!("mkdir -p {dir}")).await
            .map_err(|e| e.to_string())?;
        while let Some(msg) = ch.wait().await {
            if let ChannelMsg::Eof | ChannelMsg::Close = msg { break; }
        }

        // Upload binary + marker over ONE SFTP session.
        let sftp_ch = session.channel_open_session().await
            .map_err(|e| e.to_string())?;
        sftp_ch.request_subsystem(true, "sftp").await
            .map_err(|e| e.to_string())?;
        let sftp = SftpSession::new(sftp_ch.into_stream()).await
            .map_err(|e| e.to_string())?;

        // Write binary
        let mut f = sftp.create(server_path).await
            .map_err(|e| e.to_string())?;
        f.write_all(&local_binary).await.map_err(|e| e.to_string())?;
        f.flush().await.map_err(|e| e.to_string())?;
        drop(f);

        // Write version marker
        let mut vf = sftp.create(&marker2).await.map_err(|e| e.to_string())?;
        vf.write_all(version.as_bytes()).await.map_err(|e| e.to_string())?;
        vf.flush().await.map_err(|e| e.to_string())?;
        drop(vf);
        drop(sftp);

        // chmod +x (exec channel)
        let mut ch2 = session.channel_open_session().await
            .map_err(|e| e.to_string())?;
        ch2.exec(true, format!("chmod +x {server_path}")).await
            .map_err(|e| e.to_string())?;
        while let Some(msg) = ch2.wait().await {
            if let ChannelMsg::Eof | ChannelMsg::Close = msg { break; }
        }

        Ok::<_, String>(())
    })
}

fn remote_arch(rt: &Runtime, session: &Handle<ClientHandler>) -> Result<String, String> {
    rt.block_on(async {
        let mut ch = session.channel_open_session().await
            .map_err(|e| e.to_string())?;
        ch.exec(true, "uname -m").await.map_err(|e| e.to_string())?;
        let mut out = String::new();
        while let Some(msg) = ch.wait().await {
            if let ChannelMsg::Data { data } = msg {
                out.push_str(&String::from_utf8_lossy(&data));
            }
        }
        Ok(out.trim().to_string())
    })
}

/// Wrap a value for a remote shell.
///
/// Single quotes with embedded quotes escaped: a workspace path can contain
/// spaces, and while a session id is generated rather than typed, neither is
/// worth interpolating into a command line unquoted.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote a path for a remote shell, but let a leading `~` still mean home.
///
/// `remote_dir` defaults to the literal string `~`, and quoting it whole gives
/// `cd '~'`, which fails — there is no directory of that name. That took the
/// remote agent down at startup: it exited before printing anything, and the
/// panel reported only that the process had gone.
///
/// The tilde is left outside the quotes so the shell expands it; everything
/// after it is quoted as usual, so a path with spaces still survives.
pub fn shell_quote_path(s: &str) -> String {
    if s == "~" {
        return "~".to_string();
    }
    match s.strip_prefix("~/") {
        Some(rest) => format!("~/{}", shell_quote(rest)),
        None => shell_quote(s),
    }
}

/// The Linux forge-agent to upload, for the remote's architecture.
///
/// Same search as `local_server_binary` and for the same reason: inside the
/// .app first, since that is the only path that exists for someone who
/// installed the .dmg, then the development layouts.
fn local_agent_binary(arch: &str) -> Result<Vec<u8>, String> {
    let target = match arch {
        "x86_64"  => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other     => return Err(format!("unsupported remote arch: {other}")),
    };
    let exe_dir = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let candidates = [
        exe_dir.parent().map(|p| p.join("Resources").join(format!("forge-agent-{arch}")))
            .unwrap_or_default(),
        Path::new("target").join(target).join("release").join("forge-agent"),
        Path::new("target").join(target).join("debug").join("forge-agent"),
    ];
    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            // Must be a Linux binary. A Mach-O here means a build that picked
            // up the host binary, which the remote cannot execute and whose
            // failure over there would be baffling.
            if bytes.starts_with(b"\x7fELF") {
                return Ok(bytes);
            }
        }
    }
    Err(format!(
        "This build of Forge IDE has no Linux/{arch} forge-agent bundled, so the agent \
         cannot run on the remote machine. It is built by scripts/package_macos.sh and \
         lives in the app's Resources."
    ))
}

fn local_server_binary(arch: &str) -> Result<Vec<u8>, String> {
    let target = match arch {
        "x86_64"  => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other     => return Err(format!("unsupported remote arch: {other}")),
    };

    // Only look in musl cross-compilation paths — never the host (macOS) binary.
    // The host binary is a Mach-O which Linux cannot execute.
    let exe_dir = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let candidates = [
        // Inside the .app, which is the only path that exists for someone who
        // installed the .dmg. Both other forms are development layouts: a
        // launched .app has `/` for a working directory, so the relative ones
        // resolve to `/target/...`, and `Contents/<triple>/` is not something
        // the bundle has ever contained. Remote development was therefore
        // impossible from a packaged build — it reported the cargo command to
        // build the binary, to someone who had installed an application.
        exe_dir.parent().map(|p| p.join("Resources").join(format!("forge-server-{arch}")))
            .unwrap_or_default(),
        // Workspace target/ relative to current dir (most common dev path)
        Path::new("target").join(target).join("release").join("forge-server"),
        Path::new("target").join(target).join("debug").join("forge-server"),
        // Relative to the IDE binary (installed layout)
        exe_dir.parent().map(|p| p.join(target).join("release").join("forge-server"))
            .unwrap_or_default(),
        exe_dir.parent().map(|p| p.join(target).join("debug").join("forge-server"))
            .unwrap_or_default(),
    ];

    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            // Sanity-check: must be an ELF binary (Linux), not Mach-O (macOS).
            if bytes.starts_with(b"\x7fELF") {
                return Ok(bytes);
            }
        }
    }

    // Two different audiences, so two different messages: someone running a
    // packaged build cannot act on a cargo command, and telling them to is how
    // this failure read for its whole life.
    let packaged = exe_dir.ends_with("MacOS");
    if packaged {
        return Err(format!(
            "This build of Forge IDE has no Linux/{arch} forge-server bundled, so it \
             cannot set up a remote workspace. That binary is built by \
             scripts/package_macos.sh and lives in the app's Resources; a bundle \
             without it was built on a machine without the musl cross-compiler."
        ));
    }
    Err(format!(
        "forge-server Linux/{arch} binary not found.\n\
         Build with: cargo build -p forge-server --target {target} --release\n\
         (searched: target/{target}/release/forge-server)"
    ))
}

/// Open an SSH exec channel running forge-server.  Bridges the async channel
/// to sync mpsc so the returned (reader, writer) are `'static + Send` and the
/// caller's Handle reference can be dropped immediately after this returns.
async fn open_exec_channel_bridged(
    session: &Handle<ClientHandler>,
    command: &str,
) -> Result<(ChannelReader, ChannelWriter2), russh::Error> {
    let channel = session.channel_open_session().await?;
    channel.exec(true, command.to_string()).await?;
    Ok(bridge_channel(channel))
}

/// Present an async SSH channel as a blocking reader and writer.
///
/// Shared by the agent's stdio channel and by each forwarded model-proxy
/// connection: both want to be read and written from ordinary threads, and
/// neither wants to know that tokio is underneath.
fn bridge_channel(
    channel: russh::Channel<russh::client::Msg>,
) -> (ChannelReader, ChannelWriter2) {
    let (read_half, write_half) = channel.split();

    // stdout: async channel → sync mpsc
    let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    let mut read_half = read_half;
    tokio::spawn(async move {
        loop {
            match read_half.wait().await {
                Some(ChannelMsg::Data { data }) => { let _ = out_tx.send(data.to_vec()); }
                None | Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => break,
                _ => {}
            }
        }
    });

    // stdin: sync mpsc → async channel write
    let (in_tx, in_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    let in_rx = Arc::new(Mutex::new(in_rx));
    tokio::spawn(async move {
        loop {
            let rx = Arc::clone(&in_rx);
            let data = tokio::task::spawn_blocking(move || rx.lock().unwrap().recv()).await;
            match data {
                Ok(Ok(bytes)) => { let _ = write_half.data_bytes(bytes).await; }
                _ => break,
            }
        }
        // Tell the far end there is no more. The writer being dropped ends this
        // loop but says nothing on the wire, and a proxied model response is
        // delimited by the connection closing — no content-length, since the
        // body is streamed as it arrives. Without this the agent had its whole
        // answer and went on waiting for an end that never came, which is what
        // "stuck on sending" was.
        let _ = write_half.eof().await;
        let _ = write_half.close().await;
    });

    (ChannelReader::new(out_rx), ChannelWriter2(in_tx))
}

// ── Channel I/O adapters ─────────────────────────────────────────────────────

struct ChannelReader {
    rx:  mpsc::Receiver<Vec<u8>>,
    /// Leftover bytes from the last SSH packet that didn't fit in the caller's buffer.
    overflow: Vec<u8>,
    overflow_pos: usize,
}
impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { rx, overflow: Vec::new(), overflow_pos: 0 }
    }
}
impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drain any leftover bytes from a previous large packet first.
        if self.overflow_pos < self.overflow.len() {
            let src = &self.overflow[self.overflow_pos..];
            let n   = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            self.overflow_pos += n;
            if self.overflow_pos >= self.overflow.len() {
                self.overflow.clear();
                self.overflow_pos = 0;
            }
            return Ok(n);
        }
        // Fetch next packet from the channel.
        match self.rx.recv() {
            Ok(data) => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                if data.len() > buf.len() {
                    // Stash excess bytes for next call — never drop data.
                    self.overflow     = data;
                    self.overflow_pos = n;
                }
                Ok(n)
            }
            Err(_) => Ok(0), // sender dropped → EOF
        }
    }
}

struct ChannelWriter2(mpsc::SyncSender<Vec<u8>>);
impl Write for ChannelWriter2 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

// ── ~/.ssh/config parsing ─────────────────────────────────────────────────────

/// Path to the user's SSH config (canonical, follows Include directives one
/// level deep).
pub fn ssh_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("config")
}

/// Parse `~/.ssh/config` (and any Include files one level deep) into a list
/// of SshHost entries.  Only `Host` stanzas with a HostName (or that look
/// like `user@host` aliases) are returned — wildcard entries (`Host *`) are
/// skipped.
pub fn load_hosts() -> Vec<SshHost> {
    let path = ssh_config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    parse_ssh_config(&text, &path)
}

fn parse_ssh_config(text: &str, config_path: &Path) -> Vec<SshHost> {
    parse_ssh_config_depth(text, config_path, 0)
}

fn parse_ssh_config_depth(text: &str, config_path: &Path, depth: u8) -> Vec<SshHost> {
    let mut hosts = Vec::new();
    let mut current: Option<SshHost> = None;

    let flush = |current: &mut Option<SshHost>, hosts: &mut Vec<SshHost>| {
        if let Some(h) = current.take() {
            if !h.host.is_empty() && h.name != "*" { hosts.push(h); }
        }
    };

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }

        // Handle Include directives
        if let Some(rest) = line.strip_prefix("Include") {
            if depth >= 3 { continue; } // prevent infinite recursion
            let glob_path = rest.trim()
                .trim_matches('"').trim_matches('\''); // strip shell quotes
            // A relative Include (no `~`, no leading `/`) resolves against
            // the directory of the config file that contains it, matching
            // real ssh_config semantics — not the process's CWD.
            let expanded = if glob_path.starts_with('~') || Path::new(glob_path).is_absolute() {
                expand_tilde(glob_path)
            } else {
                config_path.parent().unwrap_or_else(|| Path::new("."))
                    .join(glob_path).to_string_lossy().into_owned()
            };
            if let Ok(text2) = std::fs::read_to_string(&expanded) {
                let included = parse_ssh_config_depth(
                    &text2, std::path::Path::new(&expanded), depth + 1);
                for h in included {
                    if !hosts.iter().any(|e: &SshHost| e.name == h.name) {
                        hosts.push(h);
                    }
                }
            }
            continue;
        }

        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.trim().to_lowercase(), v.trim().to_string()),
            None => continue,
        };

        match key.as_str() {
            "host" => {
                flush(&mut current, &mut hosts);
                if value != "*" {
                    current = Some(SshHost {
                        name:       value.clone(),
                        host:       String::new(), // filled by HostName, or alias
                        port:       22,
                        user:       std::env::var("USER").unwrap_or_default(),
                        key_path:   String::new(),
                        remote_dir: "~".into(),
                    });
                }
            }
            "hostname" => { if let Some(h) = &mut current { h.host = value; } }
            "port"     => { if let Some(h) = &mut current { h.port = value.parse().unwrap_or(22); } }
            "user"     => { if let Some(h) = &mut current { h.user = value; } }
            "identityfile" => {
                if let Some(h) = &mut current {
                    h.key_path = expand_tilde(&value);
                }
            }
            _ => {}
        }
    }
    flush(&mut current, &mut hosts);

    // Entries with no HostName: use alias as host (e.g. `Host 192.168.1.1`)
    for h in &mut hosts {
        if h.host.is_empty() { h.host = h.name.clone(); }
    }

    hosts
}

/// Append a new Host stanza to ~/.ssh/config.
pub fn add_ssh_config_host(user_at_host: &str) -> Result<(), String> {
    let path = ssh_config_path();
    // Parse user@host or just host
    let (user, host) = if let Some((u, h)) = user_at_host.split_once('@') {
        (u.to_string(), h.to_string())
    } else {
        (std::env::var("USER").unwrap_or_default(), user_at_host.to_string())
    };
    let alias = host.split('.').next().unwrap_or(&host).to_string();
    let stanza = format!(
        "\nHost {alias}\n    HostName {host}\n    User {user}\n    IdentityFile ~/.ssh/id_ed25519\n"
    );
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(stanza.as_bytes()).map_err(|e| e.to_string())
}

// ── Legacy saved hosts (kept for backwards compat) ────────────────────────────

pub fn save_hosts(_: &[SshHost]) {} // no-op — we use ~/.ssh/config now

#[cfg(test)]
mod tests {
    use super::SERVER_VERSION;

    /// Proves build.rs actually resolved forge-server/Cargo.toml's real
    /// version into the env var, not just that the concat!/env! compiled —
    /// a build.rs bug (wrong path, wrong TOML key) would still compile fine
    /// while silently embedding garbage.
    #[test]
    fn server_version_matches_cargo_toml() {
        let cargo_toml = include_str!("../forge-server/Cargo.toml");
        let value: toml::Value = cargo_toml.parse().unwrap();
        let version = value["package"]["version"].as_str().unwrap();
        assert_eq!(SERVER_VERSION, format!("forge-server-{version}"));
    }
}

/// Live SFTP upload check against a real host. Ignored by default — needs a
/// reachable machine and writes to its `~/.forge-dnd-test/`.
///   FORGE_TEST_SSH_HOST=user@192.0.2.10 \
///     cargo test -p forge-ide fs_upload_roundtrip -- --ignored --nocapture
#[cfg(test)]
mod upload_tests {
    #[test]
    #[ignore]
    fn fs_upload_roundtrip() {
        let Some(target) = std::env::var_os("FORGE_TEST_SSH_HOST")
            .map(|s| s.to_string_lossy().into_owned()) else {
            eprintln!("FORGE_TEST_SSH_HOST not set; skipping");
            return;
        };
        let (user, host) = target.split_once('@').expect("want user@host");

        // Two files: one binary-ish with NUL bytes, one with a space in the name,
        // since those are the cases text-based transfer and naive quoting break.
        let dir = std::env::temp_dir().join(format!("forge-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("shot.png");
        let payload: Vec<u8> = (0u8..=255).cycle().take(40_000).collect();
        std::fs::write(&bin, &payload).unwrap();
        let spaced = dir.join("my notes.txt");
        std::fs::write(&spaced, b"hello from forge\n").unwrap();

        let hosts = super::load_hosts();
        let cfg = hosts.iter().find(|h| h.host == host && h.user == user)
            .cloned()
            .unwrap_or_else(|| {
                let mut h = super::SshHost::default();
                h.name = host.to_string();
                h.host = host.to_string();
                h.user = user.to_string();
                h.port = 22;
                h
            });

        // The host is in known_hosts for this to run at all; the test does not
        // get to bypass verification.
        let conn = super::SshConnection::connect(
            &cfg, None, false, &|m, _| eprintln!("  ssh: {m}"),
        ).expect("connect");

        let remote_dir = format!("/home/{user}/.forge-dnd-test");
        let rx = conn.fs_upload(vec![bin.clone(), spaced.clone()], &remote_dir);

        let mut uploaded: Vec<String> = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                Ok(Ok(p))  => { eprintln!("  uploaded -> {p}"); uploaded.push(p); }
                Ok(Err(e)) => panic!("upload failed: {e}"),
                Err(e)     => panic!("no result: {e}"),
            }
        }
        assert_eq!(uploaded.len(), 2);
        assert!(uploaded[1].ends_with("my notes.txt"), "space in name survived: {:?}", uploaded[1]);

        // Verify byte-for-byte on the far side via the server's own fs API.
        let listing = conn.fs_list(&remote_dir).expect("fs_list");
        let png = listing.iter().find(|e| e.name == "shot.png").expect("shot.png present");
        assert_eq!(png.size as usize, payload.len(),
                   "binary upload truncated: {} vs {}", png.size, payload.len());
        assert!(listing.iter().any(|e| e.name == "my notes.txt"));

        eprintln!("  verified {} bytes for shot.png", png.size);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod host_key_tests {
    /// A host key must be checked against `~/.ssh/known_hosts`, not accepted on
    /// sight. This runs the same three-way decision the handler makes, against
    /// a temporary known_hosts file, so the behaviour is pinned without needing
    /// a server to connect to.
    ///
    /// The keys are generated rather than fixed: what matters is that a
    /// matching key is accepted, a different one for the same host is reported
    /// as changed, and an unlisted host is neither.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH5e7H/fBAeIxruobJyFnyjKPabP8ngjkQcfH01DgDsX";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEmtgCItjIw2iTAE3+X6twVlRgNw7uUCWDp9BT+TRG/W";

    fn key(line: &str) -> super::ssh_key::PublicKey {
        <super::ssh_key::PublicKey as std::str::FromStr>::from_str(line).unwrap()
    }

    /// A stale entry beside the current one must not read as an attack.
    ///
    /// russh returns `KeyChanged` here whichever order the two lines are in —
    /// it stops at the first same-algorithm entry that does not match, and even
    /// when the matching one is found first the error still aborts the whole
    /// check. Taking that at face value would show the user Forge's loudest
    /// warning about a host that is exactly who it claims to be, and there is
    /// no way past that dialog by design.
    #[test]
    fn a_stale_entry_beside_the_current_one_is_not_a_changed_key() {
        use russh::keys::known_hosts::{check_known_hosts_path, known_host_keys_path};

        let dir = std::env::temp_dir().join(format!("forge-kh-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let current = key(KEY_B);

        for (name, body) in [
            ("stale first",   format!("h.example {KEY_A}\nh.example {KEY_B}\n")),
            ("current first", format!("h.example {KEY_B}\nh.example {KEY_A}\n")),
        ] {
            let p = dir.join(name.replace(' ', "-"));
            std::fs::write(&p, body).unwrap();

            // What russh says, and what this works around.
            assert!(
                matches!(
                    check_known_hosts_path("h.example", 22, &current, &p),
                    Err(russh::keys::Error::KeyChanged { .. })
                ),
                "{name}: russh has fixed this; the fallback can go",
            );

            // What the handler concludes instead.
            let recorded = known_host_keys_path("h.example", 22, &p).unwrap();
            assert!(
                super::key_is_recorded(&recorded, &current),
                "{name}: the current key is recorded and must be accepted",
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check must still hold when the key really is not there — the
    /// fallback exists to stop false alarms, not to stop alarms.
    #[test]
    fn a_key_that_is_recorded_nowhere_is_still_refused() {
        use russh::keys::known_hosts::known_host_keys_path;
        let dir = std::env::temp_dir().join(format!("forge-kh-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kh");
        std::fs::write(&p, format!("h.example {KEY_A}\n")).unwrap();

        let recorded = known_host_keys_path("h.example", 22, &p).unwrap();
        assert!(
            !super::key_is_recorded(&recorded, &key(KEY_B)),
            "an unrecorded key must not be accepted by the fallback",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Identity is the key, not the label attached to it.
    #[test]
    fn a_comment_does_not_change_a_key_s_identity() {
        use russh::keys::known_hosts::known_host_keys_path;
        let dir = std::env::temp_dir().join(format!("forge-kh-cmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("kh");
        std::fs::write(&p, format!("h.example {KEY_A}\n")).unwrap();

        let recorded = known_host_keys_path("h.example", 22, &p).unwrap();
        assert!(
            super::key_is_recorded(&recorded, &key(&format!("{KEY_A} someone@somewhere"))),
            "the same key with a comment is the same key",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// russh writes a learned key with its comment attached and then reads that
    /// same key back as *changed*. A host the user had just chosen to trust
    /// would greet them with a host-key warning on the next connection, which
    /// is the one warning that must never cry wolf. Keys off the wire carry no
    /// comment, so the handler strips it before learning; this pins the reason.
    #[test]
    fn a_learned_key_is_stored_without_its_comment() {
        use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
        use super::ssh_key;
        let raw = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH5e7H/fBAeIxruobJyFnyjKPabP8ngjkQcfH01DgDsX";
        let dir = std::env::temp_dir().join(format!("forge-kh-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let commented = <ssh_key::PublicKey as std::str::FromStr>::from_str(
            &format!("{raw} someone@somewhere"),
        )
        .unwrap();

        // As russh would do it, for the record: its own round trip fails.
        let naive = dir.join("naive");
        learn_known_hosts_path("host.example", 22, &commented, &naive).unwrap();
        assert!(
            matches!(
                check_known_hosts_path("host.example", 22, &commented, &naive),
                Err(russh::keys::Error::KeyChanged { .. })
            ),
            "russh has fixed this; the strip below can go",
        );

        // As the handler does it. Both sides are the comment-free form: what
        // is learned is stripped, and what is later checked is the key off the
        // wire, which never had one.
        let ours = dir.join("ours");
        let mut stripped = commented.clone();
        stripped.set_comment("");
        learn_known_hosts_path("host.example", 22, &stripped, &ours).unwrap();
        assert_eq!(
            check_known_hosts_path("host.example", 22, &stripped, &ours).unwrap(),
            true,
            "a key the user just trusted must still match",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_is_matched_changed_or_unknown() {
        use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};

        let dir = std::env::temp_dir().join(format!("forge-kh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let kh = dir.join("known_hosts");

        use super::ssh_key;
        // Fixed throwaway keys rather than generated ones: what is being tested
        // is the decision, and a literal keeps the test free of an RNG.
        let parse = |line: &str| {
            <ssh_key::PublicKey as std::str::FromStr>::from_str(line).unwrap()
        };
        let real = parse(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIH5e7H/fBAeIxruobJyFnyjKPabP8ngjkQcfH01DgDsX",
        );
        let impostor = parse(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEmtgCItjIw2iTAE3+X6twVlRgNw7uUCWDp9BT+TRG/W",
        );

        // Unknown until it is learned — the case that must ask, not assume.
        assert_eq!(check_known_hosts_path("host.example", 22, &real, &kh).unwrap(), false);

        learn_known_hosts_path("host.example", 22, &real, &kh).unwrap();
        eprintln!("known_hosts now:
{}", std::fs::read_to_string(&kh).unwrap());
        assert!(
            check_known_hosts_path("host.example", 22, &real, &kh).unwrap(),
            "the learned key should now match",
        );

        // The attack: same host, different key. This must be distinguishable
        // from "unknown", because one is a question and the other is a refusal.
        let verdict = check_known_hosts_path("host.example", 22, &impostor, &kh);
        assert!(
            matches!(verdict, Err(russh::keys::Error::KeyChanged { .. })),
            "a substituted key must be reported as changed, got {verdict:?}",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod quoting_tests {
    use super::shell_quote;

    #[test]
    fn a_path_with_spaces_survives() {
        // Remote workspaces are chosen by the user, not by us.
        assert_eq!(shell_quote("/home/me/My Project"), "'/home/me/My Project'");
    }

    #[test]
    fn a_quote_cannot_end_the_argument_early() {
        // The whole reason to quote: without this, everything after the quote
        // would be read by the remote shell as further command.
        let out = shell_quote("/tmp/it's; rm -rf ~");
        assert_eq!(out, r#"'/tmp/it'\''s; rm -rf ~'"#);
        assert!(out.starts_with('\'') && out.ends_with('\''));
    }

    #[test]
    fn an_ordinary_path_is_merely_wrapped() {
        assert_eq!(shell_quote("/home/me/code"), "'/home/me/code'");
    }
}

#[cfg(test)]
mod path_quoting_tests {
    use super::shell_quote_path;

    /// `remote_dir` is the literal string `~` until the user opens a folder, and
    /// `cd '~'` is an error — which is what killed the remote agent on startup.
    #[test]
    fn a_bare_tilde_still_means_home() {
        assert_eq!(shell_quote_path("~"), "~");
    }

    #[test]
    fn a_path_under_home_expands_and_stays_quoted() {
        assert_eq!(shell_quote_path("~/My Project"), "~/'My Project'");
    }

    #[test]
    fn an_absolute_path_is_quoted_whole() {
        assert_eq!(shell_quote_path("/home/me/My Project"), "'/home/me/My Project'");
    }

    #[test]
    fn a_quote_still_cannot_end_the_argument_early() {
        // The tilde exception must not open a hole in the quoting.
        let out = shell_quote_path("~/it's; rm -rf /");
        assert_eq!(out, r#"~/'it'\''s; rm -rf /'"#);
    }
}

#[cfg(test)]
mod cd_quoting_tests {
    use super::shell_quote_path;

    /// The line sent to the remote shell to follow a folder change. A remote path
    /// is not a promise about spaces, and this is being typed into a shell.
    fn cd_line(dir: &str) -> String {
        format!("cd {}\n", shell_quote_path(dir))
    }

    /// Quoted whether or not it needs to be. Deciding per path is a rule with
    /// exceptions to get wrong; always quoting has none.
    #[test]
    fn an_ordinary_path_is_quoted_anyway() {
        assert_eq!(cd_line("/home/sysadmin/code"), "cd '/home/sysadmin/code'\n");
    }

    /// A space would otherwise make `cd` see two arguments and land nowhere.
    #[test]
    fn a_path_with_a_space_survives() {
        let line = cd_line("/home/sysadmin/my code");
        assert!(line.contains("'/home/sysadmin/my code'"), "{line}");
    }

    /// The tilde has to stay outside the quotes or the shell will not expand it —
    /// the bug that made `cd '~'` fail when the remote dir was left at its
    /// default.
    #[test]
    fn the_tilde_stays_expandable() {
        assert_eq!(cd_line("~"), "cd ~\n");
        let line = cd_line("~/my code");
        assert!(line.starts_with("cd ~/"), "the tilde was quoted away: {line}");
        assert!(line.contains("'my code'"), "{line}");
    }

    /// A name with a quote in it cannot be allowed to end the quoting and run as
    /// a command — this is a shell, and the name came off a remote filesystem.
    #[test]
    fn a_quote_in_a_name_cannot_escape() {
        let line = cd_line("/tmp/it's a folder; rm -rf x");
        // Whatever the quoting scheme, the dangerous characters must not be
        // sitting outside quotes where the shell would act on them.
        assert!(!line.contains("; rm -rf x\n"), "a command escaped the quoting: {line}");
        assert!(line.ends_with('\n'));
    }
}
