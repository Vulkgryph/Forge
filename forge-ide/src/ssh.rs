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

pub struct SshConnection {
    pub host:    SshHost,
    pub next_id: Arc<Mutex<i64>>,
    pub pending: PendMap,
    /// Channel stdin (server → client notifications arrive via push callbacks)
    pub stdin:   Arc<Mutex<Box<dyn Write + Send>>>,
    /// PTY push callbacks keyed by pty id
    pub pty_pushes: Arc<Mutex<HashMap<u32, mpsc::SyncSender<Vec<u8>>>>>,
    /// Session dropped FIRST (closes SSH channel, unblocks reader tasks).
    _session:    Option<russh::client::Handle<ClientHandler>>,
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
    pub fn connect(
        host:     &SshHost,
        password: Option<&str>,
        log:      &dyn Fn(&str, crate::OutputLevel),
    ) -> Result<Self, String> {
        use crate::OutputLevel::*;
        let rt  = Runtime::new().map_err(|e| e.to_string())?;
        let h   = host.clone();
        let pw  = password.map(|s| s.to_string());

        // 1. Authenticate SSH session
        log(&format!("Authenticating {}@{}:{}", h.user, h.host, h.port), Info);
        let session = rt.block_on(ssh_authenticate(&h, pw.as_deref()))
            .map_err(|e| { log(&format!("Authentication failed: {e}"), Error); e })?;
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
        ensure_server_uploaded(&rt, &session, &server_path)
            .map_err(|e| { log(&format!("Server upload failed: {e}"), Error); e })?;
        log(&format!("forge-server ready at {server_path}"), Success);

        log("Starting forge-server…", Info);
        // We bridge the async channel to sync mpsc channels here so the tokio
        // runtime (and session) can be shut down after connect() returns.
        let server_path2 = server_path.clone();
        let (reader, writer) = rt.block_on(open_exec_channel_bridged(&session, &server_path2))
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
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Ok(Self {
            host: host.clone(),
            next_id,
            pending,
            stdin:    Arc::new(Mutex::new(Box::new(writer) as Box<dyn Write + Send>)),
            pty_pushes,
            _session: Some(session),
            _rt:      Some(rt),
        })
    }

    // ── fs ────────────────────────────────────────────────────────────────────

    pub fn fs_list(&self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let r = self.call("fs/list", serde_json::json!({ "path": path }))?;
        let entries: Vec<FsEntry> = serde_json::from_value(
            r.get("entries").cloned().unwrap_or_default()
        ).map_err(|e| e.to_string())?;
        Ok(entries.into_iter().map(|e| RemoteEntry {
            name: e.name, path: e.path, is_dir: e.is_dir, size: e.size
        }).collect())
    }

    pub fn fs_write(&self, path: &str, text: &str) -> Result<(), String> {
        self.call("fs/write", serde_json::json!({ "path": path, "text": text }))?;
        Ok(())
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

struct ClientHandler;
impl client::Handler for ClientHandler {
    type Error = russh::Error;
    fn check_server_key(
        &mut self, _: &ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        async { Ok(true) } // TODO: known_hosts
    }
}

async fn ssh_authenticate(
    host: &SshHost, password: Option<&str>,
) -> Result<Handle<ClientHandler>, String> {
    let config = Arc::new(client::Config::default());
    let addr   = format!("{}:{}", host.host, host.port);
    let mut session = client::connect(config, addr, ClientHandler).await
        .map_err(|e| e.to_string())?;

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
const SERVER_VERSION: &str = concat!("forge-server-", env!("FORGE_SERVER_VERSION"));

fn ensure_server_uploaded(
    rt:          &Runtime,
    session:     &Handle<ClientHandler>,
    server_path: &str,
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
        Ok::<bool, russh::Error>(out.trim() != SERVER_VERSION)
    }).map_err(|e: russh::Error| e.to_string())?;

    if !needs_upload { return Ok(()); }

    // Choose the right binary based on remote arch.
    let arch = remote_arch(rt, session)?;
    let local_binary = local_server_binary(&arch)?;

    eprintln!("forge-server: uploading v{} ({} bytes) to {server_path}…",
        SERVER_VERSION, local_binary.len());
    let marker2 = marker.clone();

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
        vf.write_all(SERVER_VERSION.as_bytes()).await.map_err(|e| e.to_string())?;
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
    session:     &Handle<ClientHandler>,
    server_path: &str,
) -> Result<(ChannelReader, ChannelWriter2), russh::Error> {
    let channel = session.channel_open_session().await?;
    channel.exec(true, format!("{server_path} --stdio")).await?;

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
    });

    Ok((ChannelReader::new(out_rx), ChannelWriter2(in_tx)))
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
