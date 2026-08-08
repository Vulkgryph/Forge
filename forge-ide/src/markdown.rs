// Minimal markdown renderer for the agent chat panel.
// Handles: headings (#, ##, ###), bullet/numbered lists, fenced code blocks,
// GFM pipe tables, inline **bold** and `code`. Anything fancier (images,
// links) renders as plain text — we'll grow this as needed.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};

struct Theme {
    plain:    Color32,
    bold:     Color32,
    code_fg:  Color32,
    code_bg:  Color32,
    heading:  Color32,
    bullet:   Color32,
}

const T: Theme = Theme {
    plain:   Color32::from_rgb(216, 216, 216),
    bold:    Color32::WHITE,
    code_fg: Color32::from_rgb(206, 145, 120),
    code_bg: Color32::from_rgb(38, 38, 38),
    heading: Color32::from_rgb(86, 156, 214),
    bullet:  Color32::from_rgb(160, 160, 160),
};

/// One parsed markdown block, ready to emit into a `Ui` with no further
/// string parsing or `LayoutJob` construction. This is the cached
/// intermediate: parsing the raw text and building the inline `LayoutJob`s
/// (the `parse_inline` calls, the paragraph merging, table structure) is the
/// per-message cost that used to be paid *every frame* — see `render`'s doc
/// comment. Everything width-dependent (paragraph/label wrapping, table
/// column measurement) is deliberately left to emit time, since it can't be
/// precomputed without a `Ui`, and egui already caches the shaped `Galley`
/// for an unchanged `LayoutJob` internally.
#[derive(Clone)]
enum Block {
    /// Vertical gap (blank lines, pre-heading spacing, etc).
    Space(f32),
    /// A `#`/`##`/`###` heading — text already soft-wrapped for `max_run`.
    Heading { text: String, size: f32, space_before: f32 },
    /// A `-`/`*`/`N.` list row: marker glyph plus its wrapped content.
    Bullet { marker: String, job: LayoutJob },
    /// A fenced ``` code block (raw text, rendered monospace).
    Code(String),
    /// A GFM pipe table — kept as raw cells; column widths are measured
    /// against the live `Ui` width at emit time (can't be precomputed).
    Table { header: Vec<String>, rows: Vec<Vec<String>> },
    /// A plain paragraph (consecutive non-special lines merged).
    Paragraph(LayoutJob),
}

/// Frame-cache computer: raw `(text, max_run)` → parsed block list. egui
/// evicts any entry not requested during a frame (see `CacheStorage::update`,
/// called once per frame from `Memory`), so the single actively-streaming
/// message re-parses each frame while the rest of the conversation — whose
/// text is byte-for-byte identical frame to frame — is a pure cache hit.
#[derive(Default)]
struct MdBlockComputer;

impl egui::util::cache::ComputerMut<(&str, usize), std::sync::Arc<Vec<Block>>> for MdBlockComputer {
    fn compute(&mut self, (text, max_run): (&str, usize)) -> std::sync::Arc<Vec<Block>> {
        std::sync::Arc::new(parse_blocks(text, max_run))
    }
}

type MdBlockCache<'a> = egui::util::cache::FrameCache<std::sync::Arc<Vec<Block>>, MdBlockComputer>;

/// `max_run` is forwarded to `soft_wrap` (see its own doc comment) — applied
/// to each piece of *display* text individually, never to the raw input as a
/// whole. Applying it beforehand, to the whole raw markdown text, used to
/// corrupt structural parsing: a GFM table separator row (`|---|---|---|`)
/// is pure dashes and pipes with no whitespace at all, so any separator over
/// `max_run` characters — routine with 4+ columns — got a zero-width space
/// spliced into the middle of a dash run, failing the "every char is `-` or
/// `:`" check that recognizes it as a separator at all. The whole table then
/// silently fell back to one garbled plain-text paragraph. Wrapping only
/// the final cell/paragraph/heading text, after structure is already parsed
/// from the pristine original lines, fixes this at the root.
/// Renders `text` as markdown into `ui`.
///
/// The parse — line classification, paragraph merging, and the
/// `parse_inline` `LayoutJob` construction for every paragraph/list/table —
/// is memoized per `(text, max_run)` in an egui frame cache. It used to run
/// unconditionally every frame for every message, which scaled directly with
/// conversation length (measured ~85ms/frame for a 1442-item conversation)
/// and, because the chat panel repaints continuously while an agent streams,
/// pegged a CPU core and made typing lag behind. Now only the one message
/// whose text actually changed this frame re-parses; every stable message is
/// a cache hit, and emit-time work is just widget layout over the already-built
/// jobs (whose shaped `Galley`s egui caches on its own).
pub fn render(ui: &mut egui::Ui, text: &str, max_run: usize) {
    let blocks = ui.memory_mut(|mem| {
        mem.caches.cache::<MdBlockCache<'_>>().get((text, max_run))
    });
    emit(ui, &blocks, max_run);
}

/// Parses raw markdown into the cacheable block list. Kept free of any `Ui`
/// so it can run inside the frame-cache computer; anything that needs the
/// live `Ui` (width, font metrics) is deferred to `emit`.
fn parse_blocks(text: &str, max_run: usize) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<Block> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        // ── Pipe table (GFM-style: header row, then a |---|---| rule) ──
        if is_table_start(&lines, i) {
            let header = parse_table_row(line);
            i += 2; // skip header + separator rows
            let mut rows: Vec<Vec<String>> = Vec::new();
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(parse_table_row(lines[i]));
                i += 1;
            }
            out.push(Block::Table { header, rows });
            continue;
        }

        // ── Fenced code block ──
        if line.trim_start().starts_with("```") {
            i += 1;
            let mut code = String::new();
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            if i < lines.len() { i += 1; } // skip closing fence
            out.push(Block::Code(code.trim_end().to_string()));
            continue;
        }

        // ── Headings ──
        if let Some(rest) = line.strip_prefix("### ") {
            out.push(Block::Heading { text: crate::app::soft_wrap(rest, max_run), size: 13.5, space_before: 6.0 });
            i += 1; continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            out.push(Block::Heading { text: crate::app::soft_wrap(rest, max_run), size: 15.0, space_before: 8.0 });
            i += 1; continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            out.push(Block::Heading { text: crate::app::soft_wrap(rest, max_run), size: 17.0, space_before: 10.0 });
            i += 1; continue;
        }

        // ── Bullet list ──
        if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            let job = parse_inline(&crate::app::soft_wrap(rest, max_run), 12.5);
            out.push(Block::Bullet { marker: "•".to_string(), job });
            i += 1; continue;
        }

        // ── Numbered list ──
        if let Some((num, rest)) = parse_numbered_prefix(line) {
            let job = parse_inline(&crate::app::soft_wrap(rest, max_run), 12.5);
            out.push(Block::Bullet { marker: format!("{}.", num), job });
            i += 1; continue;
        }

        // ── Blank line ──
        if line.trim().is_empty() {
            out.push(Block::Space(4.0));
            i += 1; continue;
        }

        // ── Plain paragraph (merge consecutive non-special lines) ──
        let mut para = String::new();
        while i < lines.len() && !is_block_start(lines[i]) && !is_table_start(&lines, i) {
            if !para.is_empty() { para.push(' '); }
            para.push_str(lines[i].trim());
            i += 1;
        }
        out.push(Block::Paragraph(parse_inline(&crate::app::soft_wrap(&para, max_run), 12.5)));
    }
    out
}

/// Emits already-parsed blocks into `ui`. Cheap relative to `parse_blocks`:
/// no string parsing or job construction, just widget layout (egui caches the
/// shaped `Galley` for each unchanged `LayoutJob` on its own).
fn emit(ui: &mut egui::Ui, blocks: &[Block], max_run: usize) {
    for block in blocks {
        match block {
            Block::Space(h) => { ui.add_space(*h); }
            Block::Heading { text, size, space_before } => {
                ui.add_space(*space_before);
                ui.label(RichText::new(text).size(*size).strong().color(T.heading));
            }
            Block::Bullet { marker, job } => {
                ui.horizontal_top(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(marker).size(12.5).color(T.bullet));
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(job.clone()).wrap());
                });
            }
            Block::Code(code) => render_code_block(ui, code),
            Block::Table { header, rows } => render_table(ui, header, rows, max_run),
            Block::Paragraph(job) => { ui.add(egui::Label::new(job.clone()).wrap()); }
        }
    }
}

/// True if `lines[i]` looks like a table header row (contains a `|`)
/// immediately followed by a GFM separator row (`|---|:--:|--:|`, etc).
fn is_table_start(lines: &[&str], i: usize) -> bool {
    lines[i].contains('|')
        && i + 1 < lines.len()
        && is_table_separator(lines[i + 1])
}

/// A row of only `-`, `:` and `|` (with at least one `-` per cell) — the
/// rule GFM requires between a table's header and its body.
fn is_table_separator(line: &str) -> bool {
    let cells = parse_table_row(line);
    !cells.is_empty() && cells.iter().all(|c| {
        !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
    })
}

/// Splits `| a | b |` into `["a", "b"]`, tolerating a missing leading
/// and/or trailing pipe.
fn parse_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn render_table(ui: &mut egui::Ui, header: &[String], rows: &[Vec<String>], max_run: usize) {
    ui.add_space(4.0);
    let ncols = header.len();
    if ncols == 0 { return; }

    // Column widths: measure each column's widest content (header or any
    // cell), capped per-column so one very long unbroken value (a URL, a
    // path) can't blow a single column out on its own. `egui::Grid` sizes
    // columns to content with no way for a wrapping `Label` to claim more
    // space on its own — without this, a column ends up wrapping at
    // whatever narrow width its first layout pass happened to get (one
    // word per line), instead of using the space actually available.
    let font = FontId::proportional(12.0);
    let measure = |s: &str| -> f32 {
        ui.fonts(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x)
    };
    let available = (ui.available_width() - 16.0).max(80.0);
    let col_widths: Vec<f32> = (0..ncols).map(|c| {
        let widest = rows.iter()
            .filter_map(|r| r.get(c))
            .map(|s| measure(s))
            .fold(measure(&header[c]), f32::max);
        (widest + 14.0).clamp(40.0, 220.0)
    }).collect();

    egui::Frame::none()
        .fill(T.code_bg)
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            // A table wider than the panel — many columns, or a handful of
            // wide ones — used to squeeze only the *last* column down to
            // whatever was left over while every other column kept its own
            // full natural width regardless; with enough columns the total
            // still overflowed the panel outright. Once that happened, the
            // chat scroll area's measured content width grew to match it,
            // which fed into the *next* table's own available-width
            // measurement — a table-heavy conversation could snowball this
            // into a panel stuck wide open, unable to shrink back down.
            // A horizontally scrollable strip capped to the panel's own
            // width fixes this at the root: every column keeps a fair,
            // equally-treated natural width, and genuine overflow scrolls
            // sideways in its own contained area instead of growing
            // anything around it.
            egui::ScrollArea::horizontal()
                .max_width(available)
                .id_salt(("md_table_scroll", header, rows))
                .show(ui, |ui| {
                    // Salted by the table's actual content rather than a
                    // per-message sequential counter — the counter reliably
                    // collided ("Second use of Grid ID") once two different
                    // assistant messages each had their own "table #1",
                    // since `ui.id()` for their sibling `ui.vertical()`
                    // wrappers isn't guaranteed to differ from `Grid`'s
                    // point of view. Hashing the header+rows is unique per
                    // distinct table regardless of how many other tables
                    // exist elsewhere in the conversation.
                    egui::Grid::new(("md_table", header, rows))
                        .striped(true)
                        .spacing([16.0, 5.0])
                        .show(ui, |ui| {
                            for (i, h) in header.iter().enumerate() {
                                ui.add_sized([col_widths[i], 0.0], egui::Label::new(
                                    RichText::new(crate::app::soft_wrap(h, max_run)).strong().size(12.0).color(T.heading)
                                ));
                            }
                            ui.end_row();
                            for row in rows {
                                for c in 0..ncols {
                                    let cell = row.get(c).map(String::as_str).unwrap_or("");
                                    let job = parse_inline(&crate::app::soft_wrap(cell, max_run), 12.0);
                                    ui.add_sized([col_widths[c], 0.0], egui::Label::new(job).wrap());
                                }
                                ui.end_row();
                            }
                        });
                });
        });
    ui.add_space(4.0);
}

fn render_code_block(ui: &mut egui::Ui, code: &str) {
    ui.add_space(4.0);
    // A code block is the one piece of markdown deliberately *not* run
    // through `soft_wrap`: that inserts zero-width spaces to give egui a
    // break opportunity, which is fine for prose but silently corrupts code
    // the moment anyone copies it back out.
    //
    // Unwrapped, though, a single long line (a path, a URL, a minified blob,
    // a base64 literal) lays out wider than the panel — and the chat's
    // `ScrollArea::vertical` has horizontal scrolling *disabled* with
    // `auto_shrink[0] = false`, which egui sizes as
    // `inner_size.max(content_size)`: expand to fit content. That width
    // propagates out to the enclosing `SidePanel`, which stores its own
    // *content* rect as the panel's new width and reads it back as the width
    // next frame. So one over-wide line ratchets the agent panel open, and it
    // can never shrink back — the content is still that wide at the new
    // width. Only closing the conversation cleared it.
    //
    // This is the same failure `render_table` hit above, and takes the same
    // fix: keep the text verbatim, and let genuine overflow scroll sideways
    // inside a strip capped to the panel's own width, so it stays contained
    // instead of growing everything around it.
    let available = (ui.available_width() - 16.0).max(80.0);
    egui::Frame::none()
        .fill(T.code_bg)
        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
        .rounding(4.0)
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .max_width(available)
                // Full width even when the code is short (the block used to
                // `set_width(available_width())` for this), height to content.
                .auto_shrink([false, true])
                // Salted by content, not a per-message counter — see the
                // matching note in `render_table` for why a counter collides.
                .id_salt(("md_code_scroll", code))
                .show(ui, |ui| {
                    ui.label(RichText::new(code)
                        .monospace().size(11.5)
                        .color(Color32::from_rgb(214, 214, 214)));
                });
        });
    ui.add_space(2.0);
}

fn is_block_start(line: &str) -> bool {
    line.starts_with("# ") || line.starts_with("## ") || line.starts_with("### ")
        || line.starts_with("- ") || line.starts_with("* ")
        || line.trim_start().starts_with("```")
        || line.trim().is_empty()
        || parse_numbered_prefix(line).is_some()
}

fn parse_numbered_prefix(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let dot = trimmed.find('.')?;
    let n: usize = trimmed[..dot].parse().ok()?;
    let rest = trimmed[dot + 1..].trim_start();
    if rest.is_empty() { None } else { Some((n, rest)) }
}

fn parse_inline(text: &str, size: f32) -> LayoutJob {
    let mut job  = LayoutJob::default();
    let prop = FontId::proportional(size);
    let mono = FontId::monospace(size - 1.0);

    let fmt = |color: Color32, font: &FontId, bg: Color32| TextFormat {
        font_id:    font.clone(),
        color,
        background: bg,
        ..Default::default()
    };

    let bytes  = text.as_bytes();
    let len    = bytes.len();
    let mut i  = 0usize;
    let mut buf = String::new();
    let mut bold = false;
    let mut code = false;

    let flush = |job: &mut LayoutJob, buf: &mut String, bold: bool, code: bool| {
        if buf.is_empty() { return; }
        let f = if code {
            fmt(T.code_fg, &mono, T.code_bg)
        } else if bold {
            fmt(T.bold, &prop, Color32::TRANSPARENT)
        } else {
            fmt(T.plain, &prop, Color32::TRANSPARENT)
        };
        job.append(buf, 0.0, f);
        buf.clear();
    };

    while i < len {
        // **bold**
        if !code && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            flush(&mut job, &mut buf, bold, code);
            bold = !bold;
            i += 2;
            continue;
        }
        // `inline code`
        if !bold && bytes[i] == b'`' {
            flush(&mut job, &mut buf, bold, code);
            code = !code;
            i += 1;
            continue;
        }
        // UTF-8 safe char advance
        let end = text[i..].char_indices().nth(1).map(|(o, _)| i + o).unwrap_or(len);
        buf.push_str(&text[i..end]);
        i = end;
    }
    flush(&mut job, &mut buf, bold, code);

    job
}

