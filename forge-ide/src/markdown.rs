// Minimal markdown renderer for the agent chat panel.
// Handles: headings (#, ##, ###), bullet/numbered lists, fenced code blocks,
// GFM pipe tables, inline **bold**, `code` and [links](target). Anything
// fancier (images) renders as plain text — we'll grow this as needed.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, RichText};

struct Theme {
    plain:    Color32,
    bold:     Color32,
    code_fg:  Color32,
    code_bg:  Color32,
    link:     Color32,
    heading:  Color32,
    bullet:   Color32,
}

const T: Theme = Theme {
    plain:   Color32::from_rgb(216, 216, 216),
    bold:    Color32::WHITE,
    code_fg: Color32::from_rgb(206, 145, 120),
    code_bg: Color32::from_rgb(38, 38, 38),
    link:    Color32::from_rgb(106, 162, 222),
    heading: Color32::from_rgb(86, 156, 214),
    bullet:  Color32::from_rgb(160, 160, 160),
};

/// Where a link sits in a rendered job, and what it points at.
///
/// Char indices into `LayoutJob::text`, so a click can be mapped back to a
/// target: `Galley::cursor_from_pos` gives a char index and this says which
/// link, if any, contains it. Carried beside the job rather than inside it
/// because `LayoutJob` has nowhere to put it.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub range: std::ops::Range<usize>,
    pub target: String,
}

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
    Bullet { marker: String, job: LayoutJob, links: Vec<Link> },
    /// A fenced ``` code block (raw text, rendered monospace).
    Code(String),
    /// A GFM pipe table — kept as raw cells; column widths are measured
    /// against the live `Ui` width at emit time (can't be precomputed).
    Table { header: Vec<String>, rows: Vec<Vec<String>> },
    /// A plain paragraph (consecutive non-special lines merged).
    Paragraph(LayoutJob, Vec<Link>),
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
/// Where a clicked link target is left for the application to collect.
///
/// Stashed in egui memory rather than returned, because `render` is called
/// from inside nested closures that already borrow the app — threading a
/// value back out of those would mean a local and an `if let` at every call
/// site, and a new one would be easy to forget. One well-known slot, read
/// once a frame by `take_clicked_link`, cannot be forgotten at a call site
/// that does not know about it.
fn clicked_link_slot() -> egui::Id {
    egui::Id::new("forge_markdown_clicked_link")
}

/// The link target clicked since this was last called, if any.
///
/// Taken rather than read: a click is an event, and leaving it in memory
/// would reopen the file on every frame after.
pub fn take_clicked_link(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|d| d.remove_temp::<String>(clicked_link_slot()))
}

pub fn render(ui: &mut egui::Ui, text: &str, max_run: usize) {
    let blocks = ui.memory_mut(|mem| {
        mem.caches.cache::<MdBlockCache<'_>>().get((text, max_run))
    });
    if let Some(target) = emit(ui, &blocks, max_run) {
        ui.data_mut(|d| d.insert_temp(clicked_link_slot(), target));
    }
}

/// Draw one job, and report a link target if one was clicked.
///
/// `Label::layout_in_ui` rather than `ui.add(Label)`, because it hands back
/// the galley — and the galley is what turns a click position into a
/// character index, which `links` turns into a target. Laying the paragraph
/// out as one job keeps the text wrapping identical to before; splitting it
/// into a widget per span would have made every link a wrap opportunity.
fn label_with_links(
    ui: &mut egui::Ui,
    job: &LayoutJob,
    links: &[Link],
) -> Option<String> {
    let label = egui::Label::new(job.clone()).wrap();
    if links.is_empty() {
        ui.add(label);
        return None;
    }

    let (pos, galley, response) = label.sense(egui::Sense::click()).layout_in_ui(ui);
    ui.painter().galley(pos, galley.clone(), T.plain);

    let at = |p: egui::Pos2| -> Option<&Link> {
        let idx = galley.cursor_from_pos(p - pos).ccursor.index;
        links.iter().find(|l| l.range.contains(&idx))
    };

    // Hover shows where it goes, which is the half of this that matters even
    // when nobody clicks: the target is no longer printed inline, so hover is
    // the only way to see it.
    if let Some(hovered) = response.hover_pos().and_then(at) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        response.clone().on_hover_text(hovered.target.clone());
    }
    if response.clicked() {
        if let Some(clicked) = response.interact_pointer_pos().and_then(at) {
            return Some(clicked.target.clone());
        }
    }
    None
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
            let (job, links) = parse_inline(&crate::app::soft_wrap(rest, max_run), 12.5);
            out.push(Block::Bullet { marker: "•".to_string(), job, links });
            i += 1; continue;
        }

        // ── Numbered list ──
        if let Some((num, rest)) = parse_numbered_prefix(line) {
            let (job, links) = parse_inline(&crate::app::soft_wrap(rest, max_run), 12.5);
            out.push(Block::Bullet { marker: format!("{}.", num), job, links });
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
        let (job, links) = parse_inline(&crate::app::soft_wrap(&para, max_run), 12.5);
            out.push(Block::Paragraph(job, links));
    }
    out
}

/// Emits already-parsed blocks into `ui`. Cheap relative to `parse_blocks`:
/// no string parsing or job construction, just widget layout (egui caches the
/// shaped `Galley` for each unchanged `LayoutJob` on its own).
fn emit(ui: &mut egui::Ui, blocks: &[Block], max_run: usize) -> Option<String> {
    let mut clicked = None;
    for block in blocks {
        match block {
            Block::Space(h) => { ui.add_space(*h); }
            Block::Heading { text, size, space_before } => {
                ui.add_space(*space_before);
                ui.label(RichText::new(text).size(*size).strong().color(T.heading));
            }
            Block::Bullet { marker, job, links } => {
                ui.horizontal_top(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(marker).size(12.5).color(T.bullet));
                    ui.add_space(4.0);
                    clicked = label_with_links(ui, job, links).or(clicked.take());
                });
            }
            Block::Code(code) => render_code_block(ui, code),
            Block::Table { header, rows } => render_table(ui, header, rows, max_run),
            Block::Paragraph(job, links) => {
                clicked = label_with_links(ui, job, links).or(clicked.take());
            }
        }
    }
    clicked
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
                                    ui.add_sized([col_widths[c], 0.0], egui::Label::new(job.0).wrap());
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

/// `[label](target)` at the start of `text`: the label, and how many bytes the
/// whole construct took.
///
/// `None` when it is not a link, so `[` keeps its ordinary meaning —
/// `vec[0]`, `[WARN]` and an unclosed bracket all have to survive untouched.
/// Nested brackets inside the label are counted rather than ended on, so
/// `[see [note]](x)` takes the whole label.
fn parse_link(text: &str) -> Option<(&str, &str, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let mut depth = 0usize;
    let mut close = None;
    for (idx, b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(idx);
                    break;
                }
            }
            // A newline inside the brackets means this was never a link.
            b'\n' => return None,
            _ => {}
        }
    }
    let close = close?;
    if bytes.get(close + 1) != Some(&b'(') {
        return None;
    }
    let mut end = None;
    for (idx, b) in bytes.iter().enumerate().skip(close + 2) {
        match b {
            b')' => {
                end = Some(idx);
                break;
            }
            b'\n' => return None,
            _ => {}
        }
    }
    let end = end?;
    let label = &text[1..close];
    if label.is_empty() {
        return None;
    }
    Some((label, &text[close + 2..end], end + 1))
}

fn parse_inline(text: &str, size: f32) -> (LayoutJob, Vec<Link>) {
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
    let mut links: Vec<Link> = Vec::new();
    // Characters appended to the job so far, which is what a click maps to.
    let mut chars_out = 0usize;

    let flush = |job: &mut LayoutJob, buf: &mut String, bold: bool, code: bool| -> usize {
        if buf.is_empty() { return 0; }
        let f = if code {
            fmt(T.code_fg, &mono, T.code_bg)
        } else if bold {
            fmt(T.bold, &prop, Color32::TRANSPARENT)
        } else {
            fmt(T.plain, &prop, Color32::TRANSPARENT)
        };
        let n = buf.chars().count();
        job.append(buf, 0.0, f);
        buf.clear();
        n
    };

    while i < len {
        // [text](target)
        //
        // Rendered as its text, in the link colour and underlined, with the
        // target dropped. It used to fall through to the literal-text branch,
        // so a link to a file came out as the whole of
        // `[language design directive](path/to/some/long/file.md)` mid-sentence
        // — reported from the agent panel, where a sentence naming three files
        // becomes unreadable.
        //
        // The target is not shown. For a file path that is a real loss, and
        // the honest fix is making these clickable so it does not have to be
        // shown; that needs hit-testing a galley range rather than a colour,
        // and is not this change. Dropping it is still better than printing it
        // inline, because the agent names the file in prose when it matters.
        if !code && bytes[i] == b'[' {
            if let Some((label, target, took)) = parse_link(&text[i..]) {
                chars_out += flush(&mut job, &mut buf, bold, code);
                let start = chars_out;
                job.append(
                    label,
                    0.0,
                    TextFormat {
                        font_id: prop.clone(),
                        color: T.link,
                        background: Color32::TRANSPARENT,
                        underline: egui::Stroke::new(1.0_f32, T.link),
                        ..Default::default()
                    },
                );
                chars_out += label.chars().count();
                links.push(Link { range: start..chars_out, target: target.to_string() });
                i += took;
                continue;
            }
        }
        // **bold**
        if !code && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            chars_out += flush(&mut job, &mut buf, bold, code);
            bold = !bold;
            i += 2;
            continue;
        }
        // `inline code`
        if !bold && bytes[i] == b'`' {
            chars_out += flush(&mut job, &mut buf, bold, code);
            code = !code;
            i += 1;
            continue;
        }
        // UTF-8 safe char advance
        let end = text[i..].char_indices().nth(1).map(|(o, _)| i + o).unwrap_or(len);
        buf.push_str(&text[i..end]);
        i = end;
    }
    chars_out += flush(&mut job, &mut buf, bold, code);
    let _ = chars_out;

    (job, links)
}
#[cfg(test)]
mod link_tests {
    use super::*;

    /// The reported case, verbatim from the agent panel.
    ///
    /// A link to a file rendered as the whole of
    /// `[language design directive](CascadeProjects/.../vulkgryph-design-directive.md)`
    /// in the middle of a sentence, because the inline parser knew only bold
    /// and code and everything else fell through to literal text.
    #[test]
    fn a_link_renders_as_its_text() {
        let (label, target, took) = parse_link(
            "[language design directive](CascadeProjects/Bastion_Vulkgryph/specs/vulkgryph-design-directive.md) says",
        )
        .expect("that is a link");
        assert_eq!(label, "language design directive");
        assert_eq!(
            target,
            "CascadeProjects/Bastion_Vulkgryph/specs/vulkgryph-design-directive.md"
        );
        // Everything up to and including the closing paren.
        assert_eq!(
            took,
            "[language design directive](CascadeProjects/Bastion_Vulkgryph/specs/vulkgryph-design-directive.md)".len()
        );
    }

    /// `[` has an ordinary meaning and most of its uses are not links.
    #[test]
    fn brackets_that_are_not_links_are_left_alone() {
        for not_a_link in [
            "[WARN] something happened",
            "vec[0] is the first",
            "[unclosed",
            "[label] followed by prose",
            "[label]  (space before the paren)",
            "[](empty label)",
            "[spans\na newline](x)",
            "[label](unclosed",
        ] {
            assert!(
                parse_link(not_a_link).is_none(),
                "{not_a_link:?} was taken for a link"
            );
        }
    }

    /// Brackets inside the label are counted, not ended on.
    #[test]
    fn a_nested_bracket_does_not_end_the_label() {
        let (label, target, _) = parse_link("[see [note] here](x.md)").expect("a link");
        assert_eq!(target, "x.md");
        assert_eq!(label, "see [note] here");
    }

    /// The whole construct disappears from the rendered text, leaving the
    /// label — which is the thing the report was about.
    #[test]
    fn the_target_is_not_in_the_rendered_output() {
        let (job, links) = parse_inline(
            "see the [design directive](specs/vulkgryph-design-directive.md) for why",
            14.0,
        );
        let rendered = job.text.clone();
        // The target left the text, but not the page: it is what a click follows.
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].target, "specs/vulkgryph-design-directive.md");
        assert_eq!(&rendered[links[0].range.clone()], "design directive");
        assert!(rendered.contains("design directive"), "{rendered:?}");
        assert!(
            !rendered.contains("specs/vulkgryph-design-directive.md"),
            "the target is still being printed: {rendered:?}"
        );
        assert!(!rendered.contains('['), "a bracket survived: {rendered:?}");
        assert!(!rendered.contains("]("), "the markdown survived: {rendered:?}");
        assert!(rendered.starts_with("see the "), "{rendered:?}");
        assert!(rendered.ends_with(" for why"), "{rendered:?}");
    }

    /// Code spans still win: a link inside backticks is literal text, because
    /// that is someone showing the markdown rather than using it.
    #[test]
    fn a_link_inside_code_stays_literal() {
        let (job, links) = parse_inline("write `[a](b)` to link", 14.0);
        assert!(
            job.text.contains("[a](b)"),
            "a link inside a code span was rendered: {:?}",
            job.text
        );
        assert!(links.is_empty(), "{links:?}");
    }

    /// Lays `text` out in a headless context and clicks at `at`, returning
    /// whatever target the click left behind.
    ///
    /// Two frames: the first registers the label so egui knows its rect, the
    /// second delivers the press and release against it.
    fn click_at(text: &str, at: Option<egui::Pos2>) -> Option<String> {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0));
        let draw = |ctx: &egui::Context| {
            egui::CentralPanel::default().show(ctx, |ui| render(ui, text, 80));
        };

        let base = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        let _ = ctx.run(base.clone(), |ctx| draw(ctx));

        let events = at
            .map(|pos| {
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ]
            })
            .unwrap_or_default();
        let _ = ctx.run(egui::RawInput { events, ..base }, |ctx| draw(ctx));
        take_clicked_link(&ctx)
    }

    /// The point of the whole exercise: a person clicks the link and the
    /// application is handed the file to open.
    #[test]
    fn clicking_a_link_yields_its_target() {
        // The entire paragraph is the link, so any point inside the laid-out
        // text is inside the link's range — the test is about the click
        // arriving, not about hit-testing a few pixels of it.
        let text = "[the whole of this line is one link](docs/spec.md)";
        assert_eq!(
            click_at(text, Some(egui::pos2(40.0, 14.0))),
            Some("docs/spec.md".to_string())
        );
    }

    /// The discriminating half: the same frame without the click must not
    /// produce a target. Otherwise the test above would pass on a renderer
    /// that reported a link whenever it drew one.
    #[test]
    fn drawing_a_link_without_clicking_it_yields_nothing() {
        let text = "[the whole of this line is one link](docs/spec.md)";
        assert_eq!(click_at(text, None), None);
        // Clicking well below the text is outside the label entirely.
        assert_eq!(click_at(text, Some(egui::pos2(40.0, 300.0))), None);
    }

    /// A click is an event, not a state. Left in memory it would reopen the
    /// file on every frame for as long as the message stayed on screen.
    #[test]
    fn a_click_is_delivered_once() {
        let ctx = egui::Context::default();
        ctx.data_mut(|d| d.insert_temp(clicked_link_slot(), "docs/spec.md".to_string()));
        assert_eq!(take_clicked_link(&ctx), Some("docs/spec.md".to_string()));
        assert_eq!(take_clicked_link(&ctx), None);
    }
}


