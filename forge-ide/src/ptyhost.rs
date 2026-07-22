//! Client for the local pty-host daemon (`forge-server --listen <socket>`).
//!
//! Talking to a separate, detached daemon process — rather than Forge IDE
//! owning PTYs directly — means local terminal shells (and whatever's
//! running inside them, an active Claude Code session included) survive
//! Forge IDE's own process restarting via "Reload Window". `exec()`
//! replaces Forge IDE's process image; the kernel handles that by closing
//! Forge IDE's own file descriptors marked close-on-exec — which, when
//! Forge IDE owned the PTY master fd directly, included the one whose
//! closure sends `SIGHUP` to the shell attached to it. A *different*
//! process's file descriptors are untouched by that, so as long as the
//! daemon isn't a child of Forge IDE's own process group, the shells it
//! owns are never affected by Forge IDE restarting.

use forge_proto::*;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

fn socket_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("."))
        .join("forge-ide")
        .join("ptyhost.sock")
}

/// Finds `forge-server` next to the running `forge-ide` binary — they land
/// in the same `target/{debug,release}/` directory when built together as
/// one workspace, and an installed layout is expected to keep them side by
/// side too. `current_exe()` is canonicalized first since `forge-ide` is
/// commonly run via a symlink (e.g. `~/.local/bin/forge-ide`).
fn server_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let candidate = exe.parent()?.join("forge-server");
    candidate.exists().then_some(candidate)
}

/// Cheap, dependency-free "unique enough" id for a single-user local
/// machine with at most a handful of concurrent Forge IDE windows — not
/// cryptographically unique, just enough that two windows opening
/// terminals around the same moment won't collide in practice. Existing
/// ids remain fully caller-chosen for backward compatibility with the
/// SSH-remote path (which hardcodes id 0 for its single PTY).
fn gen_id() -> u32 {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    pid.wrapping_mul(2_654_435_761).wrapping_add(nanos).max(1)
}

type PendMap = Arc<Mutex<HashMap<i64, mpsc::SyncSender<Result<serde_json::Value, String>>>>>;
type PushMap = Arc<Mutex<HashMap<u32, mpsc::SyncSender<Vec<u8>>>>>;

pub struct PtyHostClient {
    next_id: Arc<Mutex<i64>>,
    pending: PendMap,
    writer:  Arc<Mutex<Box<dyn Write + Send>>>,
    pushes:  PushMap,
}

impl PtyHostClient {
    /// Connects to the daemon, spawning it first if it isn't already
    /// running. `None` if `forge-server` can't be found or a connection
    /// still can't be established — callers should fall back to owning a
    /// PTY directly rather than failing outright.
    #[cfg(unix)]
    pub fn connect() -> Option<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).ok().or_else(|| {
            spawn_daemon(&path)?;
            // Give the freshly-spawned daemon a moment to bind the socket.
            for _ in 0..50 {
                if let Ok(s) = UnixStream::connect(&path) { return Some(s); }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            None
        })?;
        Some(Self::from_stream(stream))
    }

    #[cfg(not(unix))]
    pub fn connect() -> Option<Self> { None }

    #[cfg(unix)]
    fn from_stream(stream: UnixStream) -> Self {
        let read_half = stream.try_clone().expect("clone unix stream");
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(stream)));
        let pending: PendMap = Arc::new(Mutex::new(HashMap::new()));
        let pushes:  PushMap = Arc::new(Mutex::new(HashMap::new()));

        let pending2 = Arc::clone(&pending);
        let pushes2  = Arc::clone(&pushes);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(read_half);
            while let Some(msg) = read_rpc(&mut reader) {
                if msg.is_response() {
                    if let Some(tx) = pending2.lock().unwrap().remove(&msg.id.unwrap()) {
                        let result = match msg.error {
                            Some(e) => Err(e.message),
                            None    => Ok(msg.result.unwrap_or(serde_json::Value::Null)),
                        };
                        let _ = tx.send(result);
                    }
                } else if msg.method.as_deref() == Some("pty/data") {
                    if let Ok(p) = serde_json::from_value::<PtyDataPush>(msg.params.unwrap_or_default()) {
                        if let Some(tx) = pushes2.lock().unwrap().get(&p.id) {
                            let _ = tx.send(p.data);
                        }
                    }
                }
            }
        });

        Self { next_id: Arc::new(Mutex::new(1)), pending, writer, pushes }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = { let mut n = self.next_id.lock().unwrap(); let i = *n; *n += 1; i };
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, tx);
        let msg = Rpc::request(id, method, params);
        if let Ok(mut w) = self.writer.lock() {
            write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
        }
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| format!("timeout waiting for {method}"))
            .and_then(|r| r)
    }

    /// Opens a brand-new PTY and registers its push channel.
    pub fn pty_open(&self, cols: u16, rows: u16, cwd: &str)
        -> Result<(u32, mpsc::Receiver<Vec<u8>>), String>
    {
        let id = gen_id();
        self.call("pty/open", serde_json::json!({
            "id": id, "cols": cols, "rows": rows, "cwd": cwd,
        }))?;
        Ok((id, self.reattach(id)))
    }

    /// Registers a push channel for a session id already known to exist in
    /// the daemon (from `pty/list`, typically right after Forge IDE's own
    /// process restarted) — no `pty/open` call, since it's already running.
    pub fn reattach(&self, id: u32) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::sync_channel(256);
        self.pushes.lock().unwrap().insert(id, tx);
        rx
    }

    pub fn pty_write(&self, id: u32, data: &[u8]) -> Result<(), String> {
        self.call("pty/write", serde_json::json!({ "id": id, "data": data })).map(|_| ())
    }

    pub fn pty_resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        self.call("pty/resize", serde_json::json!({ "id": id, "cols": cols, "rows": rows })).map(|_| ())
    }

    pub fn pty_close(&self, id: u32) -> Result<(), String> {
        self.pushes.lock().unwrap().remove(&id);
        self.call("pty/close", serde_json::json!({ "id": id })).map(|_| ())
    }

    /// Sessions currently alive in the daemon — used right after (re)connecting
    /// to figure out which of our remembered terminal ids are still real.
    pub fn pty_list(&self) -> Result<Vec<PtyInfo>, String> {
        let v = self.call("pty/list", serde_json::json!({}))?;
        let sessions = v.get("sessions").cloned().unwrap_or_default();
        serde_json::from_value(sessions).map_err(|e| e.to_string())
    }
}

static SHARED: std::sync::OnceLock<Option<Arc<PtyHostClient>>> = std::sync::OnceLock::new();

/// The one client connection shared by every local `Terminal` in this
/// process — connects (spawning the daemon if needed) on first use, then
/// reuses the same connection for every terminal tab. `None` once cached
/// means "already tried and failed"; callers fall back to owning a PTY
/// directly rather than retrying every frame.
pub fn shared() -> Option<Arc<PtyHostClient>> {
    SHARED.get_or_init(|| PtyHostClient::connect().map(Arc::new)).clone()
}

/// Spawns the daemon in its own process group — detached from Forge IDE's,
/// so nothing sent to (or done to) Forge IDE's process reaches it, and no
/// `wait()` is ever called on it (deliberately not tracked as a child to
/// clean up; it's meant to outlive this process).
#[cfg(unix)]
fn spawn_daemon(socket_path: &Path) -> Option<()> {
    use std::os::unix::process::CommandExt;
    let bin = server_binary()?;
    std::process::Command::new(bin)
        .arg("--listen").arg(socket_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    Some(())
}
