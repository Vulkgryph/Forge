// SPDX-License-Identifier: Apache-2.0
//! Finding saved sessions on disk.
//!
//! Read from the filesystem rather than asked for over the protocol, because the
//! agent does not offer them: `ListSessions` is accepted and ignored, and
//! `ResumeSession` is marked "runtime resume path not yet implemented". Resuming
//! happens by restarting the agent with `--resume-session <id>`, which is also
//! how the TypeScript client did it.
//!
//! Layout, one directory per session:
//!
//! ```text
//! <project>/.forge/sessions/<id>/meta.json
//! ```

use std::path::{Path, PathBuf};

use forge_agent_proto::json;

/// A saved session, as its `meta.json` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionMeta {
    pub id:            String,
    pub title:         String,
    /// ISO-8601, compared as a string — the format sorts correctly that way, and
    /// parsing dates to sort them would be work for no gain.
    pub updated_at:    String,
    pub message_count: usize,
    pub model:         String,
}

/// Where sessions live for a project.
pub fn sessions_dir(project_root: &Path) -> PathBuf {
    project_root.join(".forge").join("sessions")
}

/// Every saved session, newest first.
///
/// Unreadable or malformed entries are skipped rather than failing the whole
/// listing: one corrupt `meta.json` should not make every other session
/// unreachable.
pub fn list(project_root: &Path) -> Vec<SessionMeta> {
    let dir = sessions_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out: Vec<SessionMeta> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| read_meta(&entry.path()))
        .collect();

    // Newest first. ISO-8601 sorts lexicographically, so this needs no date
    // parsing; sessions missing the field sink to the bottom rather than
    // claiming to be the newest.
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

/// Read one session's metadata, if it looks like a session at all.
fn read_meta(dir: &Path) -> Option<SessionMeta> {
    let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    let value = json::parse(&text).ok()?;

    let id = value.str_or_empty("id");
    // An entry with no id cannot be resumed, so it is not worth listing.
    if id.is_empty() {
        return None;
    }

    Some(SessionMeta {
        id,
        title: value.str_or_empty("title"),
        updated_at: value.str_or_empty("updated_at"),
        message_count: value.usize_or_zero("message_count"),
        model: value.str_or_empty("model"),
    })
}

impl SessionMeta {
    /// A one-line label for a menu.
    pub fn label(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            // Untitled sessions still need to be distinguishable.
            self.id.clone()
        } else {
            title.replace(['\n', '\r'], " ")
        }
    }

    /// The detail shown beside the label.
    pub fn detail(&self) -> String {
        let when = self.updated_at.split('T').next().unwrap_or("");
        let plural = if self.message_count == 1 { "" } else { "s" };
        match (when.is_empty(), self.model.is_empty()) {
            (true, true) => format!("{} message{plural}", self.message_count),
            (true, false) => format!("{} message{plural} · {}", self.message_count, self.model),
            (false, true) => format!("{when} · {} message{plural}", self.message_count),
            (false, false) => {
                format!("{when} · {} message{plural} · {}", self.message_count, self.model)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a project tree with the given sessions.
    fn project(sessions: &[(&str, &str)]) -> tempdir::TempDir {
        let dir = tempdir::TempDir::new();
        for (id, meta) in sessions {
            let path = sessions_dir(dir.path()).join(id);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("meta.json"), meta).unwrap();
        }
        dir
    }

    fn meta_json(id: &str, title: &str, updated: &str, count: usize) -> String {
        format!(
            r#"{{"id":"{id}","title":"{title}","created_at":"{updated}",
                 "updated_at":"{updated}","message_count":{count},
                 "compaction_count":0,"model":"example-model"}}"#,
        )
    }

    #[test]
    fn a_project_with_no_sessions_lists_nothing() {
        let dir = tempdir::TempDir::new();
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        assert!(list(Path::new("/nonexistent/project/path")).is_empty());
    }

    /// The shape here is taken from a real `meta.json`, not invented.
    #[test]
    fn a_real_meta_file_parses() {
        let dir = project(&[(
            "20260725_214542_d67",
            r#"{"id":"20260725_214542_d67","title":"give me an overview o thid repoditory",
                "created_at":"2026-07-25T21:46:35.716134Z",
                "updated_at":"2026-07-25T21:46:35.716134Z",
                "message_count":1,"compaction_count":0,"model":"grok-4.5"}"#,
        )]);
        let found = list(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "20260725_214542_d67");
        assert_eq!(found[0].title, "give me an overview o thid repoditory");
        assert_eq!(found[0].message_count, 1);
        assert_eq!(found[0].model, "grok-4.5");
    }

    /// Newest first, because that is the one you almost always want.
    #[test]
    fn sessions_are_listed_newest_first() {
        let dir = project(&[
            ("old", &meta_json("old", "older work", "2026-01-01T00:00:00Z", 3)),
            ("new", &meta_json("new", "newer work", "2026-07-01T00:00:00Z", 9)),
            ("mid", &meta_json("mid", "middle work", "2026-04-01T00:00:00Z", 5)),
        ]);
        let ids: Vec<String> = list(dir.path()).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
    }

    /// One corrupt file must not make every other session unreachable.
    #[test]
    fn a_corrupt_meta_file_is_skipped_not_fatal() {
        let dir = project(&[
            ("good", &meta_json("good", "fine", "2026-01-01T00:00:00Z", 1)),
            ("broken", "{ this is not json"),
        ]);
        let found = list(dir.path());
        assert_eq!(found.len(), 1, "the good one still lists");
        assert_eq!(found[0].id, "good");
    }

    /// A session with no id cannot be resumed, so listing it would offer an
    /// action that cannot work.
    #[test]
    fn a_session_without_an_id_is_not_listed() {
        let dir = project(&[("no-id", r#"{"title":"orphan","message_count":2}"#)]);
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn a_directory_without_a_meta_file_is_ignored() {
        let dir = tempdir::TempDir::new();
        std::fs::create_dir_all(sessions_dir(dir.path()).join("empty")).unwrap();
        assert!(list(dir.path()).is_empty());
    }

    /// A session missing its timestamp must not sort as though it were the
    /// newest, which is what an empty-string-as-greatest comparison would do.
    #[test]
    fn sessions_without_a_timestamp_sink_to_the_bottom() {
        let dir = project(&[
            ("dated", &meta_json("dated", "dated", "2026-01-01T00:00:00Z", 1)),
            ("undated", r#"{"id":"undated","title":"undated","message_count":1}"#),
        ]);
        let ids: Vec<String> = list(dir.path()).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["dated", "undated"]);
    }

    // ── Labels ────────────────────────────────────────────────────────────

    #[test]
    fn an_untitled_session_falls_back_to_its_id() {
        let meta = SessionMeta {
            id: "20260101_000000_abc".into(),
            title: "   ".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            message_count: 0,
            model: String::new(),
        };
        assert_eq!(meta.label(), "20260101_000000_abc");
    }

    /// A title spanning lines would break the single-row menu layout.
    #[test]
    fn a_multiline_title_is_flattened() {
        let meta = SessionMeta {
            id: "x".into(),
            title: "first line\nsecond line".into(),
            updated_at: String::new(),
            message_count: 1,
            model: String::new(),
        };
        assert!(!meta.label().contains('\n'));
        assert_eq!(meta.label(), "first line second line");
    }

    #[test]
    fn the_detail_line_reads_naturally() {
        let meta = SessionMeta {
            id: "x".into(),
            title: "t".into(),
            updated_at: "2026-07-25T21:46:35Z".into(),
            message_count: 1,
            model: "grok-4.5".into(),
        };
        assert_eq!(meta.detail(), "2026-07-25 · 1 message · grok-4.5");

        let many = SessionMeta { message_count: 4, ..meta.clone() };
        assert!(many.detail().contains("4 messages"), "pluralised");

        let bare = SessionMeta {
            updated_at: String::new(),
            model: String::new(),
            ..meta
        };
        assert_eq!(bare.detail(), "1 message");
    }

    /// A scratch directory, removed on drop. Written here rather than taken as a
    /// dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                // A counter rather than randomness: `Math.random`-style sources
                // are unavailable in some contexts, and a process-unique
                // sequence is enough to keep concurrent tests apart.
                use std::sync::atomic::{AtomicU32, Ordering};
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("forge-sessions-test-{}-{n}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("scratch dir");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
