// External file-change watcher — picks up changes made outside the editor
// (git checkout, a pull/rebase, another process, an agent writing outside its
// own live-reload path) so open buffers don't silently go stale. Reload
// policy mirrors the agent's own live-reload feature: reload if the buffer
// has no unsaved edits, warn instead of clobbering if it does.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

pub enum WatchEvent {
    Changed(PathBuf),
    Removed(PathBuf),
}

/// Directory components we never want events from — VCS internals and build
/// output churn constantly and aren't anything an open buffer would track.
const IGNORED_COMPONENTS: &[&str] = &[".git", "target", "node_modules", ".forge"];

fn is_ignored(path: &Path) -> bool {
    path.components()
        .any(|c| IGNORED_COMPONENTS.iter().any(|ig| c.as_os_str() == *ig))
}

pub struct FileWatcher {
    // Held only to keep the watcher alive — dropping it stops watching.
    _watcher: RecommendedWatcher,
    pub rx: mpsc::Receiver<WatchEvent>,
}

impl FileWatcher {
    /// Watches `root` recursively. Returns `None` if the watcher couldn't be
    /// set up (e.g. platform limits) — callers should treat that as "no file
    /// watching this session" rather than a hard error.
    pub fn new(root: &Path) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            match event.kind {
                notify::EventKind::Remove(_) => {
                    for path in event.paths {
                        if !is_ignored(&path) {
                            let _ = tx.send(WatchEvent::Removed(path));
                            crate::wake::wake();
                        }
                    }
                }
                notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                    for path in event.paths {
                        if !is_ignored(&path) {
                            let _ = tx.send(WatchEvent::Changed(path));
                            crate::wake::wake();
                        }
                    }
                }
                _ => {}
            }
        })
        .ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        Some(Self { _watcher: watcher, rx })
    }
}
