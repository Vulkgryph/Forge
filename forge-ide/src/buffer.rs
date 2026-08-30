use std::path::PathBuf;

pub struct Buffer {
    pub path:     Option<PathBuf>,
    pub lines:    Vec<String>,
    pub cursor:   (usize, usize), // (line, col)
    pub modified: bool,
    /// When `Some`, this is a read-only diff view (HEAD ↔ working tree) rather
    /// than an editable text buffer.  `lines` is unused in that case.
    pub diff:     Option<Vec<crate::git::DiffRow>>,
    undo_stack:   Vec<Vec<String>>,
    redo_stack:   Vec<Vec<String>>,
    /// Whether the file on disk ended with a trailing newline when loaded.
    /// `str::lines()` silently discards this ("a\n".lines() == "a".lines()),
    /// so it has to be tracked separately and restored on save — otherwise
    /// every save strips the trailing newline, which fights any formatter
    /// (rustfmt included) that enforces one. Only load/reload touch this;
    /// in-session edits don't toggle it.
    trailing_newline: bool,
    /// Raw file bytes when this is an image preview tab (`lines` is unused
    /// in that case, same as `diff`). Set at load time; never edited.
    pub image_bytes: Option<Vec<u8>>,
    /// Lazily decoded from `image_bytes` the first time it's drawn — texture
    /// upload needs an `egui::Context`, which load time doesn't have. Holds one
    /// texture, reused: an animation composites into it frame by frame.
    pub image_view: Option<crate::app::ImageView>,
}

fn is_image_ext(ext: &str) -> bool {
    crate::img::is_supported_ext(ext)
}

/// Largest file the editor will load.
///
/// Loading is not just the read: the text is split into one heap-allocated
/// `String` per line, and every later edit clones the whole `Vec<String>` onto
/// the undo stack. A multi-hundred-MB log therefore costs far more than its
/// size in memory, and all of it happens on the event-loop thread before a
/// frame can render — so refuse with a message instead of hanging the window.
pub const MAX_OPEN_BYTES: u64 = 64 * 1024 * 1024;

fn human_bytes(n: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if n >= MB { format!("{:.1} MB", n as f64 / MB as f64) }
    else       { format!("{:.1} KB", n as f64 / 1024.0) }
}

/// `Err` with a ready-to-display message when `path` is too big to open.
fn check_size(path: &std::path::Path) -> Result<(), String> {
    let Ok(meta) = std::fs::metadata(path) else { return Ok(()) };
    if meta.len() > MAX_OPEN_BYTES {
        return Err(format!(
            "{} is {} — too large to open (limit {})",
            path.file_name().map_or_else(|| path.display().to_string(),
                                         |n| n.to_string_lossy().into_owned()),
            human_bytes(meta.len()),
            human_bytes(MAX_OPEN_BYTES),
        ));
    }
    Ok(())
}

impl Buffer {
    pub fn new() -> Self {
        Self { path: None, lines: vec![String::new()], cursor: (0, 0), modified: false, diff: None,
               undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true,
               image_bytes: None, image_view: None }
    }

    pub fn from_file(path: PathBuf) -> Result<Self, String> {
        check_size(&path)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if is_image_ext(ext) {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            return Ok(Self {
                path: Some(path), lines: vec![String::new()], cursor: (0, 0), modified: false,
                diff: None, undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true,
                image_bytes: Some(bytes), image_view: None,
            });
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let trailing_newline = text.ends_with('\n');
        let lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(String::from).collect()
        };
        Ok(Self { path: Some(path), lines, cursor: (0, 0), modified: false, diff: None,
                  undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline,
                  image_bytes: None, image_view: None })
    }

    /// A read-only diff tab for `path`, holding precomputed diff rows.
    pub fn diff_view(path: PathBuf, rows: Vec<crate::git::DiffRow>) -> Self {
        Self { path: Some(path), lines: vec![String::new()], cursor: (0, 0),
               modified: false, diff: Some(rows), undo_stack: Vec::new(),
               redo_stack: Vec::new(), trailing_newline: true,
               image_bytes: None, image_view: None }
    }

    /// Full buffer text, plus the trailing newline the file had on disk when
    /// loaded (if any) — use this, not `text()`, whenever writing to disk.
    pub fn text_for_disk(&self) -> String {
        let mut t = self.text();
        if self.trailing_newline { t.push('\n'); }
        t
    }

    pub fn save(&mut self) -> Result<(), String> {
        if self.diff.is_some() || self.image_bytes.is_some() { return Ok(()); } // read-only
        let path = self.path.as_ref().ok_or("no path")?;
        std::fs::write(path, self.text_for_disk()).map_err(|e| format!("write: {e}"))?;
        self.modified = false;
        Ok(())
    }

    /// Re-read this buffer's content from disk, discarding in-memory state.
    /// Callers are responsible for checking `modified` first — this always
    /// overwrites, it does not merge or warn about unsaved edits.
    pub fn reload(&mut self) -> Result<(), String> {
        if self.diff.is_some() { return Ok(()); } // read-only diff view
        let path = self.path.as_ref().ok_or("no path")?.clone();
        // Same cap as `from_file`: this runs from the file-watch handler on the
        // event-loop thread, so a file that grows huge externally (an appended
        // log) must not be pulled in wholesale.
        check_size(&path)?;
        if self.image_bytes.is_some() {
            self.image_bytes = Some(std::fs::read(&path)
                .map_err(|e| format!("reload {}: {e}", path.display()))?);
            // force re-decode/re-upload with the new bytes
            self.image_view = None;
            return Ok(());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reload {}: {e}", path.display()))?;
        self.trailing_newline = text.ends_with('\n');
        self.lines = if text.is_empty() { vec![String::new()] } else { text.lines().map(String::from).collect() };
        self.cursor.0 = self.cursor.0.min(self.lines.len().saturating_sub(1));
        self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
        self.modified = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        Ok(())
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                if self.diff.is_some()    { format!("{name} (Diff)") }
                else if self.modified     { format!("{name} ●") }
                else                      { name.to_string() }
            }
            None => if self.modified { "untitled ●".into() } else { "untitled".into() },
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        self.snapshot();
        let (row, col) = self.cursor;
        if ch == '\n' {
            let rest = self.lines[row].split_off(col);
            self.lines.insert(row + 1, rest);
            self.cursor = (row + 1, 0);
        } else {
            self.lines[row].insert(col, ch);
            self.cursor.1 += ch.len_utf8();
        }
        self.modified = true;
    }

    fn snapshot(&mut self) {
        if self.undo_stack.len() >= 100 { self.undo_stack.remove(0); }
        self.undo_stack.push(self.lines.clone());
        // A fresh edit invalidates any previously-undone future.
        self.redo_stack.clear();
    }

    pub fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(std::mem::replace(&mut self.lines, prev));
            let row = self.cursor.0.min(self.lines.len().saturating_sub(1));
            let col = self.cursor.1.min(self.lines[row].len());
            self.cursor = (row, col);
            self.modified = true;
        }
    }

    pub fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(std::mem::replace(&mut self.lines, next));
            let row = self.cursor.0.min(self.lines.len().saturating_sub(1));
            let col = self.cursor.1.min(self.lines[row].len());
            self.cursor = (row, col);
            self.modified = true;
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

#[cfg(test)]
mod size_cap_tests {
    use super::{Buffer, MAX_OPEN_BYTES};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-buf-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Sparse file: reports a huge length without writing that many bytes, so
    /// the cap is exercised without a multi-GB temp file.
    fn sparse(path: &std::path::Path, len: u64) {
        let f = std::fs::File::create(path).unwrap();
        f.set_len(len).unwrap();
    }

    #[test]
    fn refuses_a_file_over_the_cap() {
        let root = scratch("too-big");
        let big = root.join("huge.log");
        sparse(&big, MAX_OPEN_BYTES + 1);

        let err = match Buffer::from_file(big) {
            Err(e) => e,
            Ok(_)  => panic!("should refuse a file over the cap"),
        };
        assert!(err.contains("too large to open"), "got: {err}");
        assert!(err.contains("huge.log"), "message should name the file: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn opens_a_file_at_the_cap() {
        let root = scratch("at-cap");
        let ok = root.join("fine.txt");
        sparse(&ok, MAX_OPEN_BYTES);
        // Exactly at the limit is allowed; only strictly-greater is refused.
        assert!(Buffer::from_file(ok).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normal_files_are_unaffected() {
        let root = scratch("normal");
        let p = root.join("a.rs");
        std::fs::write(&p, "fn main() {}\n").unwrap();
        let buf = match Buffer::from_file(p) {
            Ok(b)  => b,
            Err(e) => panic!("should open: {e}"),
        };
        assert_eq!(buf.lines, vec!["fn main() {}"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The watch-driven reload path shares the cap: a file that grows huge
    /// externally must not get pulled in wholesale.
    #[test]
    fn reload_refuses_a_file_that_grew_past_the_cap() {
        let root = scratch("grew");
        let p = root.join("log.txt");
        std::fs::write(&p, "small\n").unwrap();
        let mut buf = match Buffer::from_file(p.clone()) {
            Ok(b)  => b,
            Err(e) => panic!("should open: {e}"),
        };

        sparse(&p, MAX_OPEN_BYTES + 1);
        let err = buf.reload().expect_err("reload should refuse");
        assert!(err.contains("too large to open"), "got: {err}");
        // Buffer keeps its previous contents rather than being clobbered.
        assert_eq!(buf.lines, vec!["small"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
