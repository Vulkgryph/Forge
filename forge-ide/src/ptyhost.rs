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

/// How long the daemon gets to answer its first request before it is written off.
///
/// This is a liveness test, not a performance budget: a daemon on a local unix
/// socket answers in well under a millisecond. It exists because a *wedged*
/// daemon still `accept`s connections — connecting proves only that some process
/// is holding the socket, so nothing short of a reply proves it is working.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

/// Long enough for a busy daemon under load, short enough not to read as a hang.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct PtyHostClient {
    next_id: Arc<Mutex<i64>>,
    pending: PendMap,
    writer:  Arc<Mutex<Box<dyn Write + Send>>>,
    pushes:  PushMap,
    /// Cleared the first time a request goes unanswered, after which every call
    /// fails immediately instead of waiting out its own timeout. One wedged
    /// daemon would otherwise cost `CALL_TIMEOUT` per terminal, per operation,
    /// for the life of the process.
    responsive: Arc<std::sync::atomic::AtomicBool>,
}

impl PtyHostClient {
    /// Connects to the daemon, spawning it first if it isn't already
    /// running. `None` if `forge-server` can't be found or a connection
    /// still can't be established — callers should fall back to owning a
    /// PTY directly rather than failing outright.
    #[cfg(unix)]
    pub fn connect() -> Option<Self> {
        Self::connect_at(&socket_path())
    }

    /// The path is a parameter so this can be tested against a socket of the
    /// test's own making rather than the real one in the user's config directory.
    #[cfg(unix)]
    fn connect_at(path: &Path) -> Option<Self> {
        if let Ok(stream) = UnixStream::connect(path) {
            let client = Self::from_stream(stream);
            if client.is_responsive() {
                return Some(client);
            }
            // It accepted the connection and then never answered. That is a
            // daemon from an earlier run that has wedged, and it is worse than no
            // daemon at all: it holds the socket, so every process that starts
            // afterwards connects to it successfully and then waits out a full
            // timeout on the very first call. Observed in practice — a daemon
            // stuck for six days made every launch of Forge IDE hang for ten
            // seconds before its first window drew anything.
            //
            // Take the socket away from it and start a fresh daemon. The wedged
            // process keeps its now-unlinked socket and simply stops receiving
            // new connections; its PTYs (and whatever is running in them) are
            // left alone rather than killed.
            eprintln!("ptyhost: {} is not responding; starting a new daemon",
                      path.display());
            let _ = std::fs::remove_file(path);
        }
        Self::spawn_and_connect(path)
    }

    #[cfg(unix)]
    fn spawn_and_connect(path: &Path) -> Option<Self> {
        spawn_daemon(path)?;
        // Give the freshly-spawned daemon a moment to bind the socket.
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(path) {
                let client = Self::from_stream(s);
                return client.is_responsive().then_some(client);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    /// Does anyone actually answer? Uses `pty/list` because it is side-effect
    /// free and every version of the daemon has it.
    #[cfg(unix)]
    fn is_responsive(&self) -> bool {
        self.call_within("pty/list", serde_json::json!({}), HANDSHAKE_TIMEOUT).is_ok()
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
                        crate::wake::wake();
                    }
                } else if msg.method.as_deref() == Some("pty/data") {
                    if let Ok(p) = serde_json::from_value::<PtyDataPush>(msg.params.unwrap_or_default()) {
                        if let Some(tx) = pushes2.lock().unwrap().get(&p.id) {
                            let _ = tx.send(p.data);
                            crate::wake::wake();
                        }
                    }
                }
            }
        });

        Self {
            next_id: Arc::new(Mutex::new(1)), pending, writer, pushes,
            responsive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.call_within(method, params, CALL_TIMEOUT)
    }

    fn call_within(&self, method: &str, params: serde_json::Value, timeout: std::time::Duration)
        -> Result<serde_json::Value, String>
    {
        use std::sync::atomic::Ordering;
        if !self.responsive.load(Ordering::Relaxed) {
            return Err("pty host stopped responding".into());
        }
        let id = { let mut n = self.next_id.lock().unwrap(); let i = *n; *n += 1; i };
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, tx);
        let msg = Rpc::request(id, method, params);
        if let Ok(mut w) = self.writer.lock() {
            write_rpc(&mut *w, &msg).map_err(|e| e.to_string())?;
        }
        match rx.recv_timeout(timeout) {
            Ok(r)  => r,
            Err(_) => {
                // Nothing is coming. Every later call would wait just as long,
                // and callers have a working fallback (owning the PTY directly),
                // so stop paying for this one.
                self.responsive.store(false, Ordering::Relaxed);
                self.pending.lock().unwrap().remove(&id);
                Err(format!("timeout waiting for {method}"))
            }
        }
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

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A listener that accepts connections and then says nothing — exactly how a
    /// wedged daemon behaves, and the reason connecting successfully cannot be
    /// taken as proof that the daemon works.
    fn unresponsive_socket(path: &Path) -> UnixListener {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).expect("bind test socket");
        let cloned = listener.try_clone().expect("clone listener");
        std::thread::spawn(move || {
            // Accept, hold the connection open, never reply.
            let mut held = Vec::new();
            while let Ok((s, _)) = cloned.accept() {
                held.push(s);
            }
        });
        listener
    }

    /// The reported symptom: a daemon wedged since the previous week made every
    /// launch of Forge IDE hang for ten seconds — the first call's whole timeout —
    /// before a window drew anything. Connecting must not be able to cost that.
    #[test]
    fn a_wedged_daemon_does_not_hang_startup() {
        let path = std::env::temp_dir().join(format!("forge-wedged-{}.sock", std::process::id()));
        let _listener = unresponsive_socket(&path);

        let start = std::time::Instant::now();
        let client = PtyHostClient::connect_at(&path);
        let took = start.elapsed();

        // No daemon binary sits next to the test executable, so the respawn
        // cannot succeed here; what matters is that it gave up quickly.
        assert!(client.is_none(), "there is no real daemon to connect to");
        assert!(
            took < std::time::Duration::from_secs(5),
            "connecting to a wedged daemon took {took:?} — it must not wait out CALL_TIMEOUT",
        );
        // And it got the socket out of the way so a fresh daemon could bind.
        assert!(!path.exists(), "the unresponsive socket was left in place");

        let _ = std::fs::remove_file(&path);
    }

    /// A daemon that answers is used as-is, with no respawn and no unlinking.
    #[test]
    fn a_responding_daemon_is_kept() {
        let path = std::env::temp_dir().join(format!("forge-live-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut w = stream;
                // Answer every request with an empty session list.
                while let Some(msg) = read_rpc(&mut reader) {
                    if let Some(id) = msg.id {
                        let reply = Rpc {
                            jsonrpc: "2.0".into(), id: Some(id),
                            method: None, params: None,
                            result: Some(serde_json::json!({ "sessions": [] })),
                            error: None,
                        };
                        if write_rpc(&mut w, &reply).is_err() { break; }
                    }
                }
            }
        });

        let client = PtyHostClient::connect_at(&path).expect("a responding daemon is usable");
        assert!(client.pty_list().is_ok(), "and stays usable afterwards");
        assert!(path.exists(), "a working daemon's socket must not be unlinked");
        let _ = std::fs::remove_file(&path);
    }
}
