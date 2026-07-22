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
    /// Lazily decoded/uploaded from `image_bytes` the first time it's drawn —
    /// texture upload needs an `egui::Context`, which load time doesn't have.
    pub image_tex: Option<egui::TextureHandle>,
}

fn is_image_ext(ext: &str) -> bool {
    ext.eq_ignore_ascii_case("png")
}

impl Buffer {
    pub fn new() -> Self {
        Self { path: None, lines: vec![String::new()], cursor: (0, 0), modified: false, diff: None,
               undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true,
               image_bytes: None, image_tex: None }
    }

    pub fn from_file(path: PathBuf) -> Result<Self, String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if is_image_ext(ext) {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            return Ok(Self {
                path: Some(path), lines: vec![String::new()], cursor: (0, 0), modified: false,
                diff: None, undo_stack: Vec::new(), redo_stack: Vec::new(), trailing_newline: true,
                image_bytes: Some(bytes), image_tex: None,
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
                  image_bytes: None, image_tex: None })
    }

    /// A read-only diff tab for `path`, holding precomputed diff rows.
    pub fn diff_view(path: PathBuf, rows: Vec<crate::git::DiffRow>) -> Self {
        Self { path: Some(path), lines: vec![String::new()], cursor: (0, 0),
               modified: false, diff: Some(rows), undo_stack: Vec::new(),
               redo_stack: Vec::new(), trailing_newline: true,
               image_bytes: None, image_tex: None }
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
        if self.image_bytes.is_some() {
            self.image_bytes = Some(std::fs::read(&path)
                .map_err(|e| format!("reload {}: {e}", path.display()))?);
            self.image_tex = None; // force re-decode/re-upload with the new bytes
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
