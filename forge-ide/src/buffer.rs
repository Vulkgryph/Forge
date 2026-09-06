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
    /// When the last edit landed, so a burst of typing becomes one undo entry
    /// rather than one per character.
    last_edit_at: Option<std::time::Instant>,
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
               undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true, last_edit_at: None,
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
                diff: None, undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true, last_edit_at: None,
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
                  undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline, last_edit_at: None,
                  image_bytes: None, image_view: None })
    }

    /// A read-only diff tab for `path`, holding precomputed diff rows.
    pub fn diff_view(path: PathBuf, rows: Vec<crate::git::DiffRow>) -> Self {
        Self { path: Some(path), lines: vec![String::new()], cursor: (0, 0),
               modified: false, diff: Some(rows), undo_stack: Vec::new(),
               redo_stack: Vec::new(), trailing_newline: true, last_edit_at: None,
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

    /// Give an untitled buffer somewhere to live: a file in the system's
    /// temporary directory, named after the tab.
    ///
    /// Chosen over refusing to save at all, which is what happened before — the
    /// status bar said "no path" and the keystrokes went nowhere. Somewhere
    /// temporary is not somewhere good, but it is recoverable, and the caller
    /// says so and offers to move it.
    pub fn adopt_temp_path(&mut self, name: &str) -> Result<PathBuf, String> {
        let dir = std::env::temp_dir().join("forge-untitled");
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

        // Never overwrite an existing file: two untitled tabs saved in one
        // session would otherwise land on each other.
        let stem = name.trim().replace('/', "-");
        let stem = if stem.is_empty() { "untitled" } else { &stem };
        let mut candidate = dir.join(format!("{stem}.txt"));
        let mut n = 2;
        while candidate.exists() {
            candidate = dir.join(format!("{stem}-{n}.txt"));
            n += 1;
        }
        self.path = Some(candidate.clone());
        Ok(candidate)
    }

    /// Whether this buffer lives in the temporary directory rather than
    /// somewhere the user chose.
    pub fn is_temporary(&self) -> bool {
        self.path.as_ref().is_some_and(|p| p.starts_with(std::env::temp_dir().join("forge-untitled")))
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

    /// Take the editor widget's text back into the buffer's lines.
    ///
    /// `split('\n')`, not `lines()`. The two differ on exactly one input, and
    /// it is the one that matters: text ending in a newline. `lines()` drops
    /// the trailing empty element, so pressing Enter at the end of a line —
    /// which is how you add a blank line, and how you add any line at the end
    /// of a file — produced text whose new line was then thrown away. Enter
    /// looked like it did nothing.
    ///
    /// The trailing newline a file had on disk is not represented in `lines` at
    /// all; it is remembered separately and re-added by `text_for_disk`, so
    /// splitting here cannot double it.
    pub fn set_text_from_editor(&mut self, text: &str) {
        let mut next: Vec<String> = text.split('\n').map(String::from).collect();
        if next.is_empty() {
            next.push(String::new());
        }
        if next == self.lines {
            return;
        }

        // Undo used to capture nothing typed: `snapshot` was reached only from
        // `insert_char`, which only the Tab key called, so Ctrl+Z did nothing
        // after ordinary editing.
        //
        // Snapshotting every keystroke would be correct and useless — a
        // hundred-entry stack would hold a hundred characters. So bursts of
        // typing coalesce into one entry, and anything structural (a line
        // added or removed, a paste, a deletion) always starts a new one, which
        // is where a person expects undo to stop.
        let structural = next.len() != self.lines.len();
        let stale = self.last_edit_at
            .is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(600));
        if structural || stale {
            self.snapshot();
        }
        self.last_edit_at = Some(std::time::Instant::now());

        self.lines = next;
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

#[cfg(test)]
mod editor_text_tests {
    use super::Buffer;

    fn buf(lines: &[&str]) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        b
    }

    /// The reported bug: Enter at the end of a line did nothing. The editor
    /// hands back text ending in a newline, and `lines()` discards its final
    /// empty element — so the line the user had just made was dropped before
    /// it was ever stored.
    #[test]
    fn enter_at_the_end_of_a_line_adds_a_line() {
        let mut b = buf(&["a", "b"]);
        // What the widget contains after the caret is at the end and Enter is pressed.
        b.set_text_from_editor("a\nb\n");
        assert_eq!(b.lines, vec!["a", "b", ""], "the new empty line was dropped");
    }

    /// And repeatedly, since a run of blank lines is the case that made it
    /// obvious.
    #[test]
    fn several_blank_lines_in_a_row_all_survive() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\n\n\n");
        assert_eq!(b.lines, vec!["a", "", "", ""]);
    }

    /// Enter in the middle still splits, which always worked and must keep
    /// working.
    #[test]
    fn enter_in_the_middle_splits_the_line() {
        let mut b = buf(&["hello world"]);
        b.set_text_from_editor("hello \nworld");
        assert_eq!(b.lines, vec!["hello ", "world"]);
    }

    /// Emptied entirely, a buffer still has one line to put a caret on.
    #[test]
    fn an_empty_buffer_keeps_one_line() {
        let mut b = buf(&["a", "b"]);
        b.set_text_from_editor("");
        assert_eq!(b.lines, vec![""]);
    }

    /// Splitting must not double the newline a file ended with on disk: that
    /// one is remembered separately, not stored as a line.
    #[test]
    fn the_file_s_own_trailing_newline_is_not_doubled() {
        let mut b = buf(&["a", "b"]);
        assert!(b.trailing_newline, "a fresh buffer assumes one");
        b.set_text_from_editor("a\nb");
        assert_eq!(b.text_for_disk(), "a\nb\n");
        // Now the user adds a blank line at the end.
        b.set_text_from_editor("a\nb\n");
        assert_eq!(b.text_for_disk(), "a\nb\n\n", "the added line reaches disk");
    }
}

#[cfg(test)]
mod undo_tests {
    use super::Buffer;

    fn buf(lines: &[&str]) -> Buffer {
        let mut b = Buffer::new();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        b
    }

    /// Undo captured nothing that was typed: the only path that snapshotted was
    /// `insert_char`, which only the Tab key used. Ctrl+Z after ordinary
    /// editing did nothing at all.
    #[test]
    fn undo_takes_back_an_edit() {
        let mut b = buf(&["hello"]);
        b.set_text_from_editor("hello world");
        b.undo();
        assert_eq!(b.lines, vec!["hello"], "the edit was never recorded");
    }

    /// Adding a line is structural, so it always starts its own entry — undo
    /// after Enter puts the line back, whatever the timing.
    #[test]
    fn adding_a_line_is_its_own_undo_step() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\n");
        assert_eq!(b.lines, vec!["a", ""]);
        b.undo();
        assert_eq!(b.lines, vec!["a"]);
    }

    /// A burst of typing coalesces. Snapshotting per keystroke would fill a
    /// hundred-entry stack with a hundred characters and undo would reach back
    /// about one word.
    #[test]
    fn a_burst_of_typing_is_one_step() {
        let mut b = buf(&[""]);
        for text in ["h", "he", "hel", "hell", "hello"] {
            b.set_text_from_editor(text);
        }
        assert_eq!(b.lines, vec!["hello"]);
        b.undo();
        assert_eq!(b.lines, vec![""], "each keystroke became its own entry");
    }

    /// Redo still works across the new path.
    #[test]
    fn redo_puts_it_back() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\nb");
        b.undo();
        assert_eq!(b.lines, vec!["a"]);
        b.redo();
        assert_eq!(b.lines, vec!["a", "b"]);
    }

    /// A frame that reports a change but changes nothing must not consume an
    /// undo step — the editor's layouter runs constantly.
    #[test]
    fn an_identical_frame_records_nothing() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a");
        b.set_text_from_editor("a");
        assert!(!b.modified, "nothing changed, so nothing was modified");
    }
}

#[cfg(test)]
mod temp_save_tests {
    use super::Buffer;

    /// An untitled buffer used to refuse to save at all: `save()` returned
    /// "no path" and the keystrokes went nowhere. It now gets a real file.
    #[test]
    fn an_untitled_buffer_gets_a_file() {
        let mut b = Buffer::new();
        assert!(b.save().is_err(), "with no path there is nowhere to write");
        let path = b.adopt_temp_path("untitled-1").expect("a temporary path");
        b.lines = vec!["hello".into()];
        b.save().expect("saves now");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(b.is_temporary(), "and it knows the file is not permanent");
        let _ = std::fs::remove_file(path);
    }

    /// Two untitled tabs saved in one session must not land on each other.
    #[test]
    fn a_second_buffer_does_not_overwrite_the_first() {
        let mut a = Buffer::new();
        let mut b = Buffer::new();
        let pa = a.adopt_temp_path("collide").unwrap();
        a.lines = vec!["first".into()];
        a.save().unwrap();
        let pb = b.adopt_temp_path("collide").unwrap();
        b.lines = vec!["second".into()];
        b.save().unwrap();
        assert_ne!(pa, pb, "the second buffer reused the first one's path");
        assert_eq!(std::fs::read_to_string(&pa).unwrap(), "first\n");
        assert_eq!(std::fs::read_to_string(&pb).unwrap(), "second\n");
        let _ = std::fs::remove_file(pa);
        let _ = std::fs::remove_file(pb);
    }

    /// A file the user chose is not temporary, so nothing warns about it.
    #[test]
    fn a_real_path_is_not_temporary() {
        let mut b = Buffer::new();
        b.path = Some(std::path::PathBuf::from("/Users/someone/notes.md"));
        assert!(!b.is_temporary());
    }
}
