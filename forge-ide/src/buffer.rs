use std::path::PathBuf;

/// One undo step, stored as the lines it replaced and the lines that replaced
/// them — not as a copy of the whole file.
///
/// A full copy per step is what the stack used to hold, and the cap was a count
/// rather than a size: a hundred steps on an eighteen-thousand-line file came to
/// about 136 MB of undo history for one buffer. Nearly every edit touches a line
/// or two, so a step is bounded by what actually changed instead of by how large
/// the file happens to be.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    /// The first line the step changed.
    at: usize,
    /// What was there, which undo puts back.
    before: Vec<String>,
    /// What replaced it, which redo puts back.
    after: Vec<String>,
}

impl Edit {
    /// The difference between two versions of a buffer, as the one region that
    /// differs: matching lines at the start and end are not stored.
    fn between(old: &[String], new: &[String]) -> Self {
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        // Measured from the ends, and never past the prefix — otherwise a file
        // whose edit repeats surrounding lines can overlap the two and produce a
        // negative-length region.
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = (0..max_suffix)
            .take_while(|i| old[old.len() - 1 - i] == new[new.len() - 1 - i])
            .count();
        Self {
            at: prefix,
            before: old[prefix..old.len() - suffix].to_vec(),
            after: new[prefix..new.len() - suffix].to_vec(),
        }
    }

    /// Put `before` back — the undo direction.
    fn revert(&self, lines: &mut Vec<String>) {
        let end = (self.at + self.after.len()).min(lines.len());
        lines.splice(self.at.min(lines.len())..end, self.before.iter().cloned());
    }

    /// Put `after` back — the redo direction.
    fn apply(&self, lines: &mut Vec<String>) {
        let end = (self.at + self.before.len()).min(lines.len());
        lines.splice(self.at.min(lines.len())..end, self.after.iter().cloned());
    }

    /// The buffer as it was before this step, given the buffer as it is now.
    ///
    /// Used to re-cut a step that is still being typed into: the step has to
    /// describe everything typed since it opened, not only the latest keystroke.
    fn base_of(&self, current: &[String]) -> Vec<String> {
        let mut base = current.to_vec();
        self.revert(&mut base);
        base
    }
}

/// Undo steps kept when nothing says otherwise. Configurable —
/// `Settings::undo_steps`.
pub const DEFAULT_UNDO_STEPS: usize = 200;

pub struct Buffer {
    pub path:     Option<PathBuf>,
    pub lines:    Vec<String>,
    pub cursor:   (usize, usize), // (line, col)
    pub modified: bool,
    /// When `Some`, this is a read-only diff view (HEAD ↔ working tree) rather
    /// than an editable text buffer.  `lines` is unused in that case.
    pub diff:     Option<Vec<crate::git::DiffRow>>,
    undo_stack:   Vec<Edit>,
    redo_stack:   Vec<Edit>,
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
    pub fn set_text_from_editor(&mut self, text: &str, undo_steps: usize) {
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
            self.push_edit(Edit::between(&self.lines, &next), undo_steps);
        } else if let Some(open) = self.undo_stack.pop() {
            // Still the same burst: re-cut the open step so it spans from where
            // the burst began to the text as it now is. Without this the step
            // would describe only the last keystroke, and undo would step back
            // one character at a time through everything typed since.
            let base = open.base_of(&self.lines);
            self.undo_stack.push(Edit::between(&base, &next));
        } else {
            self.push_edit(Edit::between(&self.lines, &next), undo_steps);
        }
        self.redo_stack.clear();
        self.last_edit_at = Some(std::time::Instant::now());

        self.lines = next;
        self.modified = true;
    }

    fn push_edit(&mut self, edit: Edit, undo_steps: usize) {
        // At least one step, whatever the setting says: a cap of zero would mean
        // an editor with no undo, which is not a thing anyone wants configured
        // by accident.
        let cap = undo_steps.max(1);
        if self.undo_stack.len() >= cap {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(edit);
    }

    pub fn undo(&mut self) {
        if let Some(edit) = self.undo_stack.pop() {
            edit.revert(&mut self.lines);
            self.redo_stack.push(edit);
            self.after_history_move();
        }
    }

    pub fn redo(&mut self) {
        if let Some(edit) = self.redo_stack.pop() {
            edit.apply(&mut self.lines);
            self.undo_stack.push(edit);
            self.after_history_move();
        }
    }

    fn after_history_move(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let row = self.cursor.0.min(self.lines.len().saturating_sub(1));
        let col = self.cursor.1.min(self.lines[row].len());
        self.cursor = (row, col);
        self.modified = true;
        // The next keystroke starts a step of its own rather than being folded
        // into the one just undone.
        self.last_edit_at = None;
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
    use super::{Buffer, DEFAULT_UNDO_STEPS};

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
        b.set_text_from_editor("a\nb\n", DEFAULT_UNDO_STEPS);
        assert_eq!(b.lines, vec!["a", "b", ""], "the new empty line was dropped");
    }

    /// And repeatedly, since a run of blank lines is the case that made it
    /// obvious.
    #[test]
    fn several_blank_lines_in_a_row_all_survive() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\n\n\n", DEFAULT_UNDO_STEPS);
        assert_eq!(b.lines, vec!["a", "", "", ""]);
    }

    /// Enter in the middle still splits, which always worked and must keep
    /// working.
    #[test]
    fn enter_in_the_middle_splits_the_line() {
        let mut b = buf(&["hello world"]);
        b.set_text_from_editor("hello \nworld", DEFAULT_UNDO_STEPS);
        assert_eq!(b.lines, vec!["hello ", "world"]);
    }

    /// Emptied entirely, a buffer still has one line to put a caret on.
    #[test]
    fn an_empty_buffer_keeps_one_line() {
        let mut b = buf(&["a", "b"]);
        b.set_text_from_editor("", DEFAULT_UNDO_STEPS);
        assert_eq!(b.lines, vec![""]);
    }

    /// Splitting must not double the newline a file ended with on disk: that
    /// one is remembered separately, not stored as a line.
    #[test]
    fn the_file_s_own_trailing_newline_is_not_doubled() {
        let mut b = buf(&["a", "b"]);
        assert!(b.trailing_newline, "a fresh buffer assumes one");
        b.set_text_from_editor("a\nb", DEFAULT_UNDO_STEPS);
        assert_eq!(b.text_for_disk(), "a\nb\n");
        // Now the user adds a blank line at the end.
        b.set_text_from_editor("a\nb\n", DEFAULT_UNDO_STEPS);
        assert_eq!(b.text_for_disk(), "a\nb\n\n", "the added line reaches disk");
    }
}

#[cfg(test)]
mod undo_tests {
    use super::{Buffer, DEFAULT_UNDO_STEPS};

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
        b.set_text_from_editor("hello world", DEFAULT_UNDO_STEPS);
        b.undo();
        assert_eq!(b.lines, vec!["hello"], "the edit was never recorded");
    }

    /// Adding a line is structural, so it always starts its own entry — undo
    /// after Enter puts the line back, whatever the timing.
    #[test]
    fn adding_a_line_is_its_own_undo_step() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\n", DEFAULT_UNDO_STEPS);
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
            b.set_text_from_editor(text, DEFAULT_UNDO_STEPS);
        }
        assert_eq!(b.lines, vec!["hello"]);
        b.undo();
        assert_eq!(b.lines, vec![""], "each keystroke became its own entry");
    }

    /// Redo still works across the new path.
    #[test]
    fn redo_puts_it_back() {
        let mut b = buf(&["a"]);
        b.set_text_from_editor("a\nb", DEFAULT_UNDO_STEPS);
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
        b.set_text_from_editor("a", DEFAULT_UNDO_STEPS);
        b.set_text_from_editor("a", DEFAULT_UNDO_STEPS);
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

#[cfg(test)]
mod undo_storage_tests {
    use super::{Buffer, Edit, DEFAULT_UNDO_STEPS};

    fn big(lines: usize) -> Buffer {
        let mut b = Buffer::new();
        b.lines = (0..lines).map(|i| format!("    let value_{i} = compute({i});")).collect();
        b
    }

    /// What a step costs, in bytes of stored text.
    fn undo_bytes(b: &Buffer) -> usize {
        b.undo_stack.iter()
            .map(|e| e.before.iter().chain(&e.after).map(|l| l.len() + 24).sum::<usize>())
            .sum()
    }

    /// The point of the change: a step is bounded by the edit, not by the file.
    ///
    /// A step used to be a copy of the whole buffer, capped at a hundred of
    /// them — about 136 MB of history for one eighteen-thousand-line file.
    #[test]
    fn a_step_stores_the_edit_not_the_file() {
        let mut b = big(18_000);
        let file_bytes: usize = b.lines.iter().map(|l| l.len() + 24).sum();

        // Two hundred separate edits, each touching one line. `last_edit_at`
        // is cleared between them because that is what a pause does: without
        // one, a burst of typing is deliberately folded into a single step.
        for i in 0..200 {
            let mut text = b.lines.clone();
            text[i * 50] = format!("    let value_{i} = compute({i}); // touched");
            b.last_edit_at = None;
            b.set_text_from_editor(&text.join("\n"), DEFAULT_UNDO_STEPS);
        }

        let stored = undo_bytes(&b);
        let old_way = file_bytes * 200;
        eprintln!(
            "200 steps on a {:.1} MB file: {:.1} KB stored, against {:.0} MB as whole copies",
            file_bytes as f64 / 1e6, stored as f64 / 1e3, old_way as f64 / 1e6
        );
        assert_eq!(b.undo_stack.len(), 200, "every edit should be its own step");
        assert!(
            stored * 1000 < old_way,
            "only {}x smaller: {stored} vs {old_way}", old_way / stored.max(1)
        );
    }

    /// And it still undoes correctly, all the way back, in order.
    #[test]
    fn deep_history_undoes_back_to_the_start() {
        let mut b = big(500);
        let original = b.lines.clone();
        let mut expected = vec![original.clone()];
        for i in 0..150 {
            let mut text = b.lines.clone();
            text[i * 3] = format!("changed {i}");
            b.last_edit_at = None;   // a pause between edits, so each is a step
            b.set_text_from_editor(&text.join("\n"), DEFAULT_UNDO_STEPS);
            expected.push(b.lines.clone());
        }
        // Walk back through every state, then forward again.
        for want in expected.iter().rev().skip(1) {
            b.undo();
            assert_eq!(&b.lines, want, "undo did not reproduce the previous state");
        }
        for want in expected.iter().skip(1) {
            b.redo();
            assert_eq!(&b.lines, want, "redo did not reproduce the next state");
        }
    }

    /// Structural edits — a paste, a whole-file replacement, deleting
    /// everything — are where a region-based step could most easily go wrong.
    #[test]
    fn structural_edits_round_trip() {
        for (from, to) in [
            ("a\nb\nc",        "a\nb\nc\nd\ne"),        // append
            ("a\nb\nc",        "a\nc"),                 // delete a middle line
            ("a\nb\nc",        ""),                     // delete everything
            ("",               "one\ntwo\nthree"),      // paste into an empty buffer
            ("x\ny",           "y\nx"),                 // reorder
            ("a\na\na",        "a\na\na\na"),            // repeated lines
        ] {
            let mut b = Buffer::new();
            b.set_text_from_editor(from, DEFAULT_UNDO_STEPS);
            let before = b.lines.clone();
            b.undo_stack.clear();
            b.set_text_from_editor(to, DEFAULT_UNDO_STEPS);
            let after = b.lines.clone();
            b.undo();
            assert_eq!(b.lines, before, "undo of {from:?} -> {to:?}");
            b.redo();
            assert_eq!(b.lines, after, "redo of {from:?} -> {to:?}");
        }
    }

    /// The cap is a setting, and it is honoured.
    #[test]
    fn the_cap_is_configurable_and_enforced() {
        let mut b = big(50);
        for i in 0..30 {
            let mut text = b.lines.clone();
            text[0] = format!("edit {i}");
            b.last_edit_at = None;
            b.set_text_from_editor(&text.join("\n"), 10);
        }
        assert_eq!(b.undo_stack.len(), 10, "the cap was not applied");
        // The oldest steps are the ones dropped, so undo still walks back the
        // ten most recent.
        for _ in 0..10 { b.undo(); }
        assert_eq!(b.lines[0], "edit 19", "the wrong end of the history was kept");
    }

    /// A cap of zero would mean an editor with no undo at all.
    #[test]
    fn a_cap_of_zero_still_keeps_one_step() {
        let mut b = big(10);
        let original = b.lines.clone();
        let mut text = b.lines.clone();
        text[0] = "changed".into();
        b.set_text_from_editor(&text.join("\n"), 0);
        b.undo();
        assert_eq!(b.lines, original);
    }

    /// The region is the smallest one that differs — matching lines around the
    /// edit are not stored.
    #[test]
    fn only_the_changed_region_is_stored() {
        let old: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[42] = "line 42 edited".into();
        let e = Edit::between(&old, &new);
        assert_eq!(e.at, 42);
        assert_eq!(e.before, vec!["line 42".to_string()]);
        assert_eq!(e.after, vec!["line 42 edited".to_string()]);
    }
}

#[cfg(test)]
mod undo_coverage_tests {
    use super::{Buffer, DEFAULT_UNDO_STEPS};

    /// Every path that changes a buffer has to record an undo step.
    ///
    /// This has now been wrong twice, the same way each time: a path wrote
    /// `lines` directly instead of going through `set_text_from_editor`, so the
    /// edit happened and no step was recorded — and Ctrl+Z silently skipped
    /// past it to whatever came before. First it was ordinary typing (only the
    /// Tab handler took a snapshot); then it was multi-cursor editing.
    ///
    /// This asserts the property rather than the call site: an edit made
    /// through the editor's write path is undoable, whatever produced the text.
    #[test]
    fn an_edit_through_the_editor_path_is_undoable() {
        let mut b = Buffer::new();
        b.set_text_from_editor("one\ntwo\nthree", DEFAULT_UNDO_STEPS);
        let before = b.lines.clone();
        b.last_edit_at = None;

        // What a multi-cursor insert produces: the whole text, rebuilt.
        b.set_text_from_editor("Xone\nXtwo\nXthree", DEFAULT_UNDO_STEPS);
        assert_eq!(b.lines, vec!["Xone", "Xtwo", "Xthree"]);

        b.undo();
        assert_eq!(b.lines, before, "the edit was not undoable");
        b.redo();
        assert_eq!(b.lines, vec!["Xone", "Xtwo", "Xthree"], "redo did not restore it");
    }

    /// A paste over a selection replaces a region with a different number of
    /// lines — the case where a region-based step is most likely to be cut
    /// wrongly.
    #[test]
    fn a_paste_over_a_selection_round_trips() {
        let mut b = Buffer::new();
        b.set_text_from_editor("a\nb\nc\nd\ne", DEFAULT_UNDO_STEPS);
        let before = b.lines.clone();
        b.last_edit_at = None;

        // Lines b..d replaced by two different ones.
        b.set_text_from_editor("a\nX\nY\ne", DEFAULT_UNDO_STEPS);
        let after = b.lines.clone();
        assert_eq!(after, vec!["a", "X", "Y", "e"]);

        b.undo();
        assert_eq!(b.lines, before, "undo of a shrinking replacement");
        b.redo();
        assert_eq!(b.lines, after, "redo of a shrinking replacement");

        // And the growing direction.
        b.last_edit_at = None;
        b.set_text_from_editor("a\nX\n1\n2\n3\nY\ne", DEFAULT_UNDO_STEPS);
        let grown = b.lines.clone();
        b.undo();
        assert_eq!(b.lines, after, "undo of a growing replacement");
        b.redo();
        assert_eq!(b.lines, grown, "redo of a growing replacement");
    }
}
