use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ── File icons ───────────────────────────────────────────────────────────────
//
// All file-type icons are rendered from bundled PNGs via `crate::icons`
// (Devicon + Codicons, both MIT) and tinted with each language's brand color.

// ── Actions returned to app.rs ────────────────────────────────────────────────

pub enum TreeAction {
    Open(PathBuf),
    OpenInTerminal(PathBuf),
    OpenFolderDialog,
    AddFolderDialog,
}

// ── Inline creation state ─────────────────────────────────────────────────────

enum Creating { File(PathBuf), Folder(PathBuf) }

// ── FileTree ──────────────────────────────────────────────────────────────────

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
        let mut entries: Vec<PathBuf> = iter.flatten().map(|e| e.path()).collect();
        entries.sort_by(|a, b| {
            b.is_dir().cmp(&a.is_dir()).then(a.file_name().cmp(&b.file_name()))
        });
        for path in entries {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') || name == "node_modules" { continue; }
            }
            let is_dir = path.is_dir();
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

        egui::ScrollArea::vertical()
            .id_salt("filetree_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 1.0;

                // Root-level create (panel background / a top-level file's
                // context menu both target `self.root`) — `self.root` never
                // has an `Entry` of its own in `self.entries` (only its
                // children do; see `refresh`/`walk`), so the per-row check
                // below can never match it. Render it here instead, as the
                // would-be first row, before anything else.
                self.draw_create_row(ui, &mut action, &mut refresh, &self.root.clone(), 0);

                for i in 0..self.entries.len() {
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
                    let row_h    = ui.text_style_height(&egui::TextStyle::Body) + 7.0;
                    let avail_w  = ui.available_width();
                    let (row_rect, resp) = ui.allocate_exact_size(
                        egui::vec2(avail_w, row_h), egui::Sense::click(),
                    );

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
                            egui::Stroke::new(1.0, egui::Color32::from_gray(38)),
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

        if let Some(dir) = toggle {
            if self.expanded.contains(&dir) { self.expanded.remove(&dir); }
            else                            { self.expanded.insert(dir);  }
            self.refresh();
        }
        if refresh { self.refresh(); }
        action
    }
}
