use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ── File icons ───────────────────────────────────────────────────────────────
//
// All file-type icons are drawn at runtime by `crate::icons` from original
// geometry — no embedded third-party assets, no attribution required.

// ── Actions returned to app.rs ────────────────────────────────────────────────

pub enum TreeAction {
    Open(PathBuf),
    OpenInTerminal(PathBuf),
    OpenFolderDialog,
    AddFolderDialog,
    /// Files dropped from outside the app (Finder, another editor) onto a row.
    /// `dir` is where they should land: the hovered folder, or the hovered
    /// file's parent. The caller decides what "land" means — a local workspace
    /// copies, an SSH one uploads.
    DropFiles { dir: PathBuf, paths: Vec<PathBuf> },
}

// ── Inline creation state ─────────────────────────────────────────────────────

enum Creating { File(PathBuf), Folder(PathBuf) }

// ── FileTree ──────────────────────────────────────────────────────────────────

/// Cap on entries listed per directory. A folder with hundreds of thousands of
/// siblings is not navigable as a tree anyway, and every entry costs a row in
/// `entries` plus work in `walk`.
const MAX_DIR_ENTRIES: usize = 10_000;

pub struct FileTree {
    pub root:     PathBuf,
    /// Additional workspace roots (multi-root workspace).
    pub extra_roots: Vec<PathBuf>,
    entries:      Vec<Entry>,
    pub expanded: HashSet<PathBuf>,
    pub selected: Option<PathBuf>,
    creating:     Option<Creating>,
    create_name:  String,
}

struct Entry { path: PathBuf, depth: usize, is_dir: bool }

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root: root.clone(), extra_roots: Vec::new(), entries: Vec::new(),
            expanded: HashSet::new(), selected: None,
            creating: None, create_name: String::new(),
        };
        tree.expanded.insert(root);
        tree.refresh();
        tree
    }

    pub fn set_root(&mut self, root: PathBuf) {
        self.root = root.clone();
        self.extra_roots.clear();
        self.expanded.clear();
        self.expanded.insert(root);
        self.selected = None;
        self.creating = None;
        self.refresh();
    }

    pub fn add_root(&mut self, root: PathBuf) {
        if root == self.root || self.extra_roots.contains(&root) { return; }
        self.expanded.insert(root.clone());
        self.extra_roots.push(root);
        self.refresh();
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        self.walk(&self.root.clone(), 0);
        for root in self.extra_roots.clone() {
            self.entries.push(Entry { path: root.clone(), depth: 0, is_dir: true });
            if self.expanded.contains(&root) {
                self.walk(&root, 1);
            }
        }
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(iter) = std::fs::read_dir(dir) else { return };

        // Resolve directory-ness exactly once per entry, up front.
        //
        // This used to call `path.is_dir()` from inside the sort comparator,
        // which is a `stat` per comparison — O(n log n) syscalls for an
        // n-entry directory instead of O(n), plus one more `stat` per entry in
        // the loop below. On a 924-entry directory that measured 84ms against
        // 0.6ms for this version, and `refresh` runs on the event-loop thread
        // on every file-watch event.
        let mut entries: Vec<(PathBuf, bool)> = Vec::new();
        for entry in iter.flatten() {
            if entries.len() >= MAX_DIR_ENTRIES { break; }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') || name == "node_modules" { continue; }
            // `file_type()` comes from the directory read itself on most
            // platforms, so it is far cheaper than a fresh `stat`. Symlinks
            // report as neither file nor dir, so resolve just those.
            let is_dir = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => entry.path().is_dir(),
                Ok(ft)                    => ft.is_dir(),
                Err(_)                    => continue,
            };
            entries.push((entry.path(), is_dir));
        }
        entries.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| a.0.file_name().cmp(&b.0.file_name()))
        });

        for (path, is_dir) in entries {
            self.entries.push(Entry { path: path.clone(), depth, is_dir });
            if is_dir && self.expanded.contains(&path) {
                self.walk(&path, depth + 1);
            }
        }
    }

    fn parent_dir(path: &Path) -> PathBuf {
        if path.is_dir() { path.to_path_buf() }
        else             { path.parent().unwrap_or(path).to_path_buf() }
    }

    /// Renders the inline "new file/folder" name box if `self.creating`
    /// targets `target` — as a would-be first child, indented to
    /// `child_depth`. Shared by the root-level case (called once, right
    /// before the entries loop — `self.root` never has an `Entry` of its
    /// own to match against inside that loop, so the box could never
    /// render at all for "New File" from the panel background/a top-level
    /// file's context menu) and the per-row case (called after a folder's
    /// own row, so it reads as nested inside — not above — that folder).
    fn draw_create_row(
        &mut self, ui: &mut egui::Ui,
        action: &mut Option<TreeAction>, refresh: &mut bool,
        target: &Path, child_depth: usize,
    ) {
        let Some(creating) = &self.creating else { return };
        let parent = match creating { Creating::File(p) | Creating::Folder(p) => p.clone() };
        if parent != target { return; }
        let is_new_folder = matches!(creating, Creating::Folder(_));

        let child_indent = child_depth as f32 * 14.0;
        let icon_key = if is_new_folder { "folder" } else { "default" };
        ui.horizontal(|ui| {
            ui.add_space(child_indent);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(15.0, 15.0), egui::Sense::hover(),
            );
            crate::icons::paint_key(ui.painter(), rect, icon_key);
            ui.add_space(2.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.create_name)
                    .desired_width(140.0)
                    .hint_text(if is_new_folder { "folder name" } else { "file name" })
            );
            resp.request_focus();

            let commit = resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));

            if commit && !self.create_name.is_empty() {
                let new_path = parent.join(&self.create_name);
                if is_new_folder {
                    let _ = std::fs::create_dir_all(&new_path);
                } else {
                    if let Some(p) = new_path.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::File::create(&new_path);
                    *action = Some(TreeAction::Open(new_path.clone()));
                }
                self.creating = None;
                self.create_name.clear();
                *refresh = true;
            } else if cancel {
                self.creating = None;
                self.create_name.clear();
            }
        });
    }

    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        git: Option<&crate::git::GitState>,
    ) -> Option<TreeAction> {
        let mut action: Option<TreeAction> = None;
        let mut toggle: Option<PathBuf>    = None;
        let mut refresh                    = false;
        // Row geometry, kept so an external file drop can be attributed to the
        // row it landed on (see the drop handling at the end of this function).
        let mut row_rects: Vec<(egui::Rect, PathBuf)> = Vec::new();

        // Capture full panel rect BEFORE the header is drawn so the context
        // menu covers the entire sidebar including the "Forge-IDE" title row.
        let full_rect = ui.max_rect();

        // Root header (matches VS Code: just the workspace name, no icons)
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let root_name = self.root.file_name().and_then(|n| n.to_str()).unwrap_or("/");
            ui.label(egui::RichText::new(root_name).strong().color(egui::Color32::from_gray(200)));
        });
        ui.add_space(4.0);

        // Empty-space context menu (right-click anywhere in the panel that
        // isn't covered by a file/folder row)
        let panel_resp = ui.interact(
            full_rect,
            ui.id().with("panel_ctx"),
            egui::Sense::click(),
        );
        panel_resp.context_menu(|ui| {
            ui.set_min_width(180.0);
            if ui.button("New File…").clicked() {
                self.expanded.insert(self.root.clone());
                self.creating    = Some(Creating::File(self.root.clone()));
                self.create_name = String::new();
                ui.close_menu();
            }
            if ui.button("New Folder…").clicked() {
                self.expanded.insert(self.root.clone());
                self.creating    = Some(Creating::Folder(self.root.clone()));
                self.create_name = String::new();
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Open Folder…").clicked() {
                action = Some(TreeAction::OpenFolderDialog);
                ui.close_menu();
            }
            if ui.button("Add Folder to Workspace…").clicked() {
                action = Some(TreeAction::AddFolderDialog);
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Reveal in Finder").clicked() {
                let _ = std::process::Command::new("open").arg(&self.root).spawn();
                ui.close_menu();
            }
            if ui.button("Open in Terminal").clicked() {
                action = Some(TreeAction::OpenInTerminal(self.root.clone()));
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Refresh").clicked() {
                refresh = true;
                ui.close_menu();
            }
        });

        // Render only the rows actually on screen. Rows are uniform height, so
        // `show_rows` can map the scroll offset straight to an index range —
        // this used to be `for i in 0..self.entries.len()`, which built a
        // widget (plus a `PathBuf` clone and a `String`) for every entry in the
        // tree every frame. Expanding a large folder was enough to stall the
        // UI; the Source Control panel next door already virtualizes for the
        // same reason.
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 7.0;
        let total_rows = self.entries.len();
        egui::ScrollArea::vertical()
            .id_salt("filetree_scroll")
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total_rows, |ui, row_range| {
                ui.style_mut().spacing.item_spacing.y = 1.0;

                // Root-level create (panel background / a top-level file's
                // context menu both target `self.root`) — `self.root` never
                // has an `Entry` of its own in `self.entries` (only its
                // children do; see `refresh`/`walk`), so the per-row check
                // below can never match it. Render it here instead, as the
                // would-be first row, before anything else. Costs no height
                // unless a create is actually in progress, which is what keeps
                // the virtual row mapping above exact.
                self.draw_create_row(ui, &mut action, &mut refresh, &self.root.clone(), 0);

                for i in row_range {
                    let path     = self.entries[i].path.clone();
                    let depth    = self.entries[i].depth;
                    let is_dir   = self.entries[i].is_dir;
                    let expanded = self.expanded.contains(&path);
                    let name     = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
                    let selected = self.selected.as_ref() == Some(&path);

                    // Files get a painter-drawn icon (folders render as the
                    // chevron only, matching VS Code).
                    let show_file_icon = !is_dir;

                    // Full-width row — allocate across the entire available width
                    // so the click target and hover highlight match VS Code.
                    // Height must match the `row_h` handed to `show_rows`, or
                    // the virtual range and the real layout drift apart.
                    let avail_w  = ui.available_width();
                    let (row_rect, resp) = ui.allocate_exact_size(
                        egui::vec2(avail_w, row_h), egui::Sense::click(),
                    );
                    row_rects.push((row_rect, path.clone()));

                    // Hover / selection highlight across the full row
                    if selected {
                        ui.painter().rect_filled(
                            row_rect, 0.0, egui::Color32::from_rgb(40, 40, 60),
                        );
                    } else if resp.hovered() {
                        ui.painter().rect_filled(
                            row_rect, 0.0, egui::Color32::from_rgb(35, 35, 35),
                        );
                    }
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }

                    let indent_px = depth as f32 * 16.0;
                    let p         = ui.painter();
                    let left      = row_rect.left();
                    let cy        = row_rect.center().y;

                    // Indent guide lines
                    for d in 0..depth {
                        let gx = left + d as f32 * 16.0 + 8.0;
                        p.line_segment(
                            [egui::pos2(gx, row_rect.top()),
                             egui::pos2(gx, row_rect.bottom())],
                            egui::Stroke::new(1.0_f32, egui::Color32::from_gray(38)),
                        );
                    }

                    // Arrow triangle — sized like VS Code's chevron (≈ 10px)
                    let ax = left + indent_px + 6.0;
                    if is_dir {
                        let color = egui::Color32::from_gray(170);
                        let pts = if expanded {
                            // ▾ downward-pointing
                            vec![egui::pos2(ax - 5.0, cy - 2.5),
                                 egui::pos2(ax + 5.0, cy - 2.5),
                                 egui::pos2(ax,       cy + 3.5)]
                        } else {
                            // ▸ right-pointing
                            vec![egui::pos2(ax - 3.0, cy - 5.0),
                                 egui::pos2(ax + 4.0, cy),
                                 egui::pos2(ax - 3.0, cy + 5.0)]
                        };
                        p.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
                    }

                    // File-type icon (bundled PNG, tinted) — only for files;
                    // folders read fine with just the chevron, matching VS
                    // Code's default explorer.  Same column geometry as before:
                    // icon left edge at indent_px + 16, name 18px further right.
                    let text_x = if is_dir {
                        left + indent_px + 24.0
                    } else {
                        let icon_x = left + indent_px + 16.0;
                        if show_file_icon {
                            // 15px square centered vertically on the row.
                            let icon_rect = egui::Rect::from_min_size(
                                egui::pos2(icon_x, cy - 7.5),
                                egui::vec2(15.0, 15.0),
                            );
                            crate::icons::paint(p, icon_rect, &path, is_dir);
                        }
                        icon_x + 18.0
                    };

                    // Git status: color the filename if this entry is changed,
                    // OR if it's a folder whose subtree contains changes (then
                    // we apply a softer tint so the user sees "something inside
                    // is modified").
                    let git_status = git.and_then(|g| {
                        if is_dir {
                            // Folder rollup — show a subtle "M" hue if anything inside changed.
                            if g.folder_has_changes(&path) {
                                Some(crate::git::FileStatus::Modified)
                            } else {
                                None
                            }
                        } else {
                            g.status_for(&path)
                        }
                    });

                    let name_color = if selected {
                        egui::Color32::WHITE
                    } else if let Some(st) = git_status {
                        if is_dir { st.color().linear_multiply(0.85) } else { st.color() }
                    } else {
                        egui::Color32::from_gray(210)
                    };

                    p.text(
                        egui::pos2(text_x, cy),
                        egui::Align2::LEFT_CENTER,
                        &name,
                        egui::FontId::proportional(13.5),
                        name_color,
                    );

                    // Status letter badge (M, U, A, D, R, !) for files only — pinned to the
                    // row's right edge like VS Code's explorer, not positioned relative to
                    // the filename. A per-character width estimate (the previous approach)
                    // is unreliable for a proportional font and overlaps the name for some
                    // lengths; anchoring to the row edge is exact regardless of name length.
                    if !is_dir {
                        if let Some(st) = git_status {
                            p.text(
                                egui::pos2(row_rect.right() - 14.0, cy),
                                egui::Align2::RIGHT_CENTER,
                                st.letter(),
                                egui::FontId::proportional(11.0),
                                st.color(),
                            );
                        }
                    }

                    // Left-click: open / expand
                    if resp.clicked() {
                        self.selected = Some(path.clone());
                        if is_dir { toggle = Some(path.clone()); }
                        else      { action = Some(TreeAction::Open(path.clone())); }
                    }

                    // Right-click / two-finger click: context menu
                    resp.context_menu(|ui| {
                        self.selected = Some(path.clone());
                        ui.set_min_width(200.0);

                        let dir = Self::parent_dir(&path);

                        if ui.button("New File…").clicked() {
                            self.expanded.insert(dir.clone());
                            self.creating    = Some(Creating::File(dir.clone()));
                            self.create_name = String::new();
                            ui.close_menu();
                        }
                        if ui.button("New Folder…").clicked() {
                            self.expanded.insert(dir.clone());
                            self.creating    = Some(Creating::Folder(dir.clone()));
                            self.create_name = String::new();
                            ui.close_menu();
                        }

                        ui.separator();

                        if ui.button("Reveal in Finder").clicked() {
                            let _ = std::process::Command::new("open")
                                .arg("-R").arg(&path).spawn();
                            ui.close_menu();
                        }

                        if ui.button("Open in Terminal").clicked() {
                            action = Some(TreeAction::OpenInTerminal(dir.clone()));
                            ui.close_menu();
                        }

                        ui.separator();

                        if ui.button("Copy Path").clicked() {
                            ui.output_mut(|o| o.copied_text = path.to_string_lossy().to_string());
                            ui.close_menu();
                        }

                        let rel = path.strip_prefix(&self.root).unwrap_or(&path);
                        if ui.button("Copy Relative Path").clicked() {
                            ui.output_mut(|o| o.copied_text = rel.to_string_lossy().to_string());
                            ui.close_menu();
                        }

                        ui.separator();

                        let del_label = if is_dir { "Delete Folder" } else { "Delete File" };
                        if ui.button(egui::RichText::new(del_label)
                            .color(egui::Color32::from_rgb(240, 80, 80))).clicked()
                        {
                            if is_dir {
                                let _ = std::fs::remove_dir_all(&path);
                            } else {
                                let _ = std::fs::remove_file(&path);
                            }
                            if self.selected.as_ref() == Some(&path) {
                                self.selected = None;
                            }
                            ui.close_menu();
                            refresh = true;
                        }
                    });

                    // If this row is the target of an in-progress create,
                    // render the name box right after it — reads as nested
                    // inside this folder (its first child), not above it.
                    if is_dir {
                        self.draw_create_row(ui, &mut action, &mut refresh, &path, depth + 1);
                    }
                }
            });

        // ── External file drop ────────────────────────────────────────────
        // Only OS-level drops arrive as `dropped_files` (Finder and friends);
        // dragging a row *within* the tree is a separate egui mechanism and is
        // not handled here. So this is always an import, never a move.
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if !dropped.is_empty() {
            if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                if full_rect.contains(pos) {
                    // Land in the row under the cursor: that folder, or the
                    // parent of that file. Falling back to the root means a drop
                    // on empty space below the rows still does the obvious thing.
                    let target = row_rects.iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .map(|(_, path)| Self::parent_dir(path))
                        .unwrap_or_else(|| self.root.clone());
                    action = Some(TreeAction::DropFiles { dir: target, paths: dropped });
                }
            }
        }

        if let Some(dir) = toggle {
            if self.expanded.contains(&dir) { self.expanded.remove(&dir); }
            else                            { self.expanded.insert(dir);  }
            self.refresh();
        }
        if refresh { self.refresh(); }
        action
    }
}

#[cfg(test)]
mod walk_tests {
    use super::{FileTree, MAX_DIR_ENTRIES};
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-tree-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.entries.iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Directories first, then by name — the ordering the old comparator gave,
    /// preserved now that `file_type()` is captured once instead of `is_dir()`
    /// being called per comparison.
    #[test]
    fn orders_directories_before_files() {
        let root = scratch("order");
        for d in ["zeta_dir", "alpha_dir"] { std::fs::create_dir(root.join(d)).unwrap(); }
        for f in ["b.txt", "a.txt"] { std::fs::write(root.join(f), "").unwrap(); }

        let tree = FileTree::new(root.clone());
        assert_eq!(names(&tree), vec!["alpha_dir", "zeta_dir", "a.txt", "b.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hides_dotfiles_and_node_modules() {
        let root = scratch("hide");
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("keep.rs"), "").unwrap();

        let tree = FileTree::new(root.clone());
        assert_eq!(names(&tree), vec!["keep.rs"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A symlinked directory still reads as a directory, so it stays expandable.
    #[test]
    #[cfg(unix)]
    fn symlinked_directory_is_still_a_directory() {
        let root = scratch("symdir");
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        std::fs::write(root.join("f.txt"), "").unwrap();

        let tree = FileTree::new(root.clone());
        // Both dirs sort ahead of the file, so the link kept its dir-ness.
        assert_eq!(names(&tree), vec!["link", "real", "f.txt"]);
        assert!(tree.entries.iter().find(|e| e.path.ends_with("link")).unwrap().is_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only the expanded root is listed — collapsed subdirectories are not
    /// walked, so tree cost doesn't scale with the whole tree.
    #[test]
    fn does_not_walk_collapsed_directories() {
        let root = scratch("collapsed");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/hidden_from_tree.txt"), "").unwrap();

        let tree = FileTree::new(root.clone());
        assert_eq!(names(&tree), vec!["sub"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_entries_per_directory() {
        let root = scratch("cap");
        for i in 0..(MAX_DIR_ENTRIES + 50) {
            std::fs::write(root.join(format!("f{i:06}.txt")), "").unwrap();
        }
        let tree = FileTree::new(root.clone());
        assert_eq!(tree.entries.len(), MAX_DIR_ENTRIES);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Timing check for the walk that used to `stat` inside its sort comparator.
/// Ignored by default (machine-specific); run with:
///   cargo test -p forge-ide tree_walk_is_fast -- --ignored --nocapture
#[cfg(test)]
mod walk_bench {
    #[test]
    #[ignore]
    fn tree_walk_is_fast() {
        for d in ["/usr/bin", "/System/Library/Fonts", "/"] {
            let p = std::path::PathBuf::from(d);
            if !p.is_dir() { continue; }
            let t = std::time::Instant::now();
            let tree = super::FileTree::new(p);
            eprintln!("FileTree::new({d}): {:?} ({} rows)", t.elapsed(), tree.entries.len());
        }
    }
}
