//! File-type icons, drawn at runtime with egui's painter.
//!
//! Every icon in this module is original geometry — there are no embedded
//! third-party assets and no attribution requirements.  Categories with a
//! distinctive shape (image, audio, video, archive, lock, folder) get their
//! own painter routine; everything else is a generic document silhouette with
//! a short label and the language's brand color.

use std::path::Path;

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2, pos2, vec2};

fn hex(r: u8, g: u8, b: u8) -> Color32 { Color32::from_rgb(r, g, b) }

/// What shape to draw.  `Doc` carries a 1–3 char label rendered inside the
/// document silhouette; the tint color comes from `classify`.
enum Kind {
    Folder,
    Doc(&'static str),
    Image,
    Audio,
    Video,
    Archive,
    Lock,
    Font,
}

fn classify(path: &Path, is_dir: bool) -> (Kind, Color32) {
    if is_dir {
        return (Kind::Folder, hex(144, 175, 207));
    }

    // Special-cased filenames take priority.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        match name {
            ".gitignore" | ".gitattributes" | ".gitmodules"
                => return (Kind::Doc("GIT"), hex(241,  80,  47)),
            "Cargo.toml" | "Cargo.lock"
                => return (Kind::Doc("RS"),  hex(222, 117,  49)),
            "package.json" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock"
                => return (Kind::Doc("NPM"), hex(203,  56,  55)),
            "Makefile" | "makefile" | "GNUmakefile"
                => return (Kind::Doc("MK"),  hex(109, 128, 134)),
            "Dockerfile" | "docker-compose.yml" | "docker-compose.yaml" | ".dockerignore"
                => return (Kind::Doc("DKR"), hex( 73, 154, 186)),
            "LICENSE" | "LICENSE.md" | "LICENSE.txt" | "COPYING" | "NOTICE"
                => return (Kind::Doc("L"),   hex(203, 203,  65)),
            ".editorconfig"
                => return (Kind::Doc("EC"),  hex(161, 211, 236)),
            _ => {}
        }
    }

    match path.extension().and_then(|e| e.to_str()) {
        // Systems
        Some("rs")                       => (Kind::Doc("RS"),  hex(222, 117,  49)),
        Some("c" | "cc")                 => (Kind::Doc("C"),   hex(  3,  90, 158)),
        Some("cpp" | "cxx" | "c++" | "cu") => (Kind::Doc("C+"), hex(  3,  90, 158)),
        Some("h" | "hpp" | "hh" | "hxx") => (Kind::Doc("H"),   hex(109, 128, 134)),
        Some("cs")                       => (Kind::Doc("C#"),  hex( 86, 156, 214)),
        Some("go")                       => (Kind::Doc("GO"),  hex(  0, 175, 215)),
        Some("java")                     => (Kind::Doc("JV"),  hex(176,  61,  52)),
        Some("kt" | "kts")               => (Kind::Doc("KT"),  hex(247, 120,  37)),
        Some("swift")                    => (Kind::Doc("SW"),  hex(252,  93,  76)),
        Some("zig")                      => (Kind::Doc("ZIG"), hex(247, 164,  29)),
        Some("dart")                     => (Kind::Doc("DT"),  hex(  3, 169, 244)),

        // Scripting
        Some("py" | "pyw" | "pyi")       => (Kind::Doc("PY"),  hex(255, 209,  74)),
        Some("rb")                       => (Kind::Doc("RB"),  hex(204,  52,  45)),
        Some("php")                      => (Kind::Doc("PHP"), hex(119, 123, 180)),
        Some("lua")                      => (Kind::Doc("LUA"), hex( 96, 117, 234)),
        Some("pl" | "pm")                => (Kind::Doc("PL"),  hex(204,  52,  45)),
        Some("r" | "R")                  => (Kind::Doc("R"),   hex( 19, 142, 230)),
        Some("sh" | "bash" | "zsh" | "fish")
                                         => (Kind::Doc("SH"),  hex(143, 188, 187)),

        // Web / JS family
        Some("js" | "mjs" | "cjs")       => (Kind::Doc("JS"),  hex(247, 223,  30)),
        Some("jsx")                      => (Kind::Doc("JSX"), hex( 97, 218, 251)),
        Some("ts")                       => (Kind::Doc("TS"),  hex( 48, 117, 196)),
        Some("tsx")                      => (Kind::Doc("TSX"), hex( 97, 218, 251)),
        Some("vue")                      => (Kind::Doc("VUE"), hex( 65, 184, 131)),
        Some("svelte")                   => (Kind::Doc("SV"),  hex(255,  62,   0)),
        Some("html" | "htm")             => (Kind::Doc("HT"),  hex(228,  79,  38)),
        Some("css")                      => (Kind::Doc("CSS"), hex( 39, 110, 188)),
        Some("scss" | "sass")            => (Kind::Doc("SCS"), hex(205, 103, 153)),

        // Data / config
        Some("json" | "jsonc")           => (Kind::Doc("JSN"), hex(203, 203,  65)),
        Some("yaml" | "yml")             => (Kind::Doc("YML"), hex(203,  79,  65)),
        Some("toml")                     => (Kind::Doc("TML"), hex(109, 128, 134)),
        Some("xml" | "plist")            => (Kind::Doc("XML"), hex(228,  79,  38)),
        Some("env")                      => (Kind::Doc("EN"),  hex(203, 203,  65)),
        Some("ini" | "cfg" | "conf")     => (Kind::Doc("CFG"), hex(109, 128, 134)),

        // Docs
        Some("md" | "markdown")          => (Kind::Doc("MD"),  hex( 81, 154, 186)),
        Some("tex")                      => (Kind::Doc("TEX"), hex( 40, 100, 170)),
        Some("pdf")                      => (Kind::Doc("PDF"), hex(204,  52,  45)),
        Some("txt")                      => (Kind::Doc("TXT"), hex(161, 211, 236)),

        // Categorical shapes
        Some("svg")                      => (Kind::Image,      hex(255, 178,  56)),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "bmp" | "tiff")
                                         => (Kind::Image,      hex(161, 211, 236)),
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a")
                                         => (Kind::Audio,      hex(205, 103, 153)),
        Some("mp4" | "mov" | "avi" | "mkv" | "webm")
                                         => (Kind::Video,      hex(205, 103, 153)),
        Some("ttf" | "otf" | "woff" | "woff2")
                                         => (Kind::Font,       hex(203, 203,  65)),
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar")
                                         => (Kind::Archive,    hex(176,  61,  52)),
        Some("lock")                     => (Kind::Lock,       hex(161, 211, 236)),

        _                                => (Kind::Doc(""),    hex(161, 211, 236)),
    }
}

/// Render the appropriate icon for `path` inside the given rect.
pub fn paint(p: &Painter, rect: Rect, path: &Path, is_dir: bool) {
    let (kind, color) = classify(path, is_dir);
    paint_kind(p, rect, &kind, color);
}

/// Render an icon by category key — used by the inline new-file/new-folder row.
/// Recognized keys: "folder", "default".
pub fn paint_key(p: &Painter, rect: Rect, key: &str) {
    let color = hex(144, 175, 207);
    match key {
        "folder"  => paint_folder(p, rect, color),
        _         => paint_doc(p, rect, "", color),
    }
}

fn paint_kind(p: &Painter, r: Rect, kind: &Kind, color: Color32) {
    match kind {
        Kind::Folder       => paint_folder(p, r, color),
        Kind::Doc(label)   => paint_doc(p, r, label, color),
        Kind::Image        => paint_image(p, r, color),
        Kind::Audio        => paint_audio(p, r, color),
        Kind::Video        => paint_video(p, r, color),
        Kind::Archive      => paint_archive(p, r, color),
        Kind::Lock         => paint_lock(p, r, color),
        Kind::Font         => paint_font(p, r, color),
    }
}

// ── Individual icon renderers ───────────────────────────────────────────────
//
// All take a target Rect and tint Color32.  Strokes are drawn at 1.5px and
// fills are flat.  Geometry is laid out for ~15px icons but scales linearly.

fn paint_folder(p: &Painter, r: Rect, color: Color32) {
    // Classic folder: small tab on the upper-left, wider body underneath.
    let s   = Stroke::new(1.4, color);
    let w   = r.width();
    let h   = r.height();
    let tab_w = w * 0.35;
    let tab_h = h * 0.15;
    let body_y = r.min.y + tab_h;
    // Tab
    let tab_tl = pos2(r.min.x + w * 0.05, r.min.y + h * 0.20);
    let tab_tr = pos2(tab_tl.x + tab_w, tab_tl.y);
    let tab_br = pos2(tab_tr.x + tab_h, body_y);
    p.line_segment([tab_tl, tab_tr], s);
    p.line_segment([tab_tr, tab_br], s);
    // Body
    let body_tl = pos2(r.min.x + w * 0.05, body_y);
    let body_br = pos2(r.max.x - w * 0.05, r.max.y - h * 0.10);
    p.line_segment([body_tl, pos2(body_br.x, body_tl.y)], s);
    p.line_segment([pos2(body_br.x, body_tl.y), body_br], s);
    p.line_segment([body_br, pos2(body_tl.x, body_br.y)], s);
    p.line_segment([pos2(body_tl.x, body_br.y), body_tl], s);
}

fn paint_doc(p: &Painter, r: Rect, label: &str, color: Color32) {
    // Document silhouette with a folded upper-right corner, label centered.
    let s    = Stroke::new(1.4, color);
    let fold = (r.height() * 0.28).min(5.0);
    let pad  = r.width() * 0.08;
    let tl   = pos2(r.min.x + pad,            r.min.y + pad);
    let tr   = pos2(r.max.x - pad - fold,     r.min.y + pad);
    let cr   = pos2(r.max.x - pad,            r.min.y + pad + fold);
    let br   = pos2(r.max.x - pad,            r.max.y - pad);
    let bl   = pos2(r.min.x + pad,            r.max.y - pad);
    let fc   = pos2(r.max.x - pad - fold,     r.min.y + pad + fold);
    p.line_segment([tl, tr], s);
    p.line_segment([tr, fc], s);
    p.line_segment([fc, cr], s);
    p.line_segment([cr, br], s);
    p.line_segment([br, bl], s);
    p.line_segment([bl, tl], s);
    p.line_segment([tr, cr], s); // diagonal of the fold

    if !label.is_empty() {
        // Scale font with rect size; nudge down so the label sits below the fold.
        let font_px = (r.height() * 0.42).clamp(6.0, 9.5);
        let centre  = pos2(r.center().x - fold * 0.25, r.center().y + fold * 0.3);
        p.text(
            centre,
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(font_px),
            color,
        );
    }
}

fn paint_image(p: &Painter, r: Rect, color: Color32) {
    // Outline frame + mountain + sun.
    let s   = Stroke::new(1.4, color);
    let pad = r.width() * 0.08;
    let inner = Rect::from_min_max(
        pos2(r.min.x + pad, r.min.y + pad),
        pos2(r.max.x - pad, r.max.y - pad),
    );
    p.rect_stroke(inner, 1.0, s);

    let iw = inner.width();
    let ih = inner.height();
    // Mountain
    let base_y = inner.max.y - ih * 0.15;
    let peak   = pos2(inner.center().x - iw * 0.05, inner.min.y + ih * 0.40);
    let bl     = pos2(inner.min.x + iw * 0.10, base_y);
    let br     = pos2(inner.max.x - iw * 0.15, base_y);
    p.add(egui::Shape::convex_polygon(vec![bl, peak, br], color, Stroke::NONE));
    // Sun
    let sun_r = iw * 0.10;
    p.circle_filled(pos2(inner.max.x - iw * 0.25, inner.min.y + ih * 0.27), sun_r, color);
}

fn paint_audio(p: &Painter, r: Rect, color: Color32) {
    // Quarter note: vertical stem with a filled head at the bottom-left.
    let stem_top = pos2(r.center().x + r.width() * 0.18,  r.min.y + r.height() * 0.18);
    let stem_bot = pos2(r.center().x + r.width() * 0.18,  r.max.y - r.height() * 0.28);
    p.line_segment([stem_top, stem_bot], Stroke::new(1.8, color));
    // Flag
    let flag_end = pos2(stem_top.x + r.width() * 0.18, stem_top.y + r.height() * 0.18);
    p.line_segment([stem_top, flag_end], Stroke::new(1.8, color));
    // Note head (filled ellipse approximation via circle)
    let head_c = pos2(stem_bot.x - r.width() * 0.13, stem_bot.y);
    p.circle_filled(head_c, r.width() * 0.14, color);
}

fn paint_video(p: &Painter, r: Rect, color: Color32) {
    // Outline frame + filled play triangle.
    let s   = Stroke::new(1.4, color);
    let pad = r.width() * 0.08;
    let inner = Rect::from_min_max(
        pos2(r.min.x + pad, r.min.y + pad),
        pos2(r.max.x - pad, r.max.y - pad),
    );
    p.rect_stroke(inner, 1.0, s);
    // Triangle
    let cx = inner.center().x;
    let cy = inner.center().y;
    let h  = inner.height();
    p.add(egui::Shape::convex_polygon(
        vec![
            pos2(cx - h * 0.18, cy - h * 0.22),
            pos2(cx - h * 0.18, cy + h * 0.22),
            pos2(cx + h * 0.22, cy),
        ],
        color, Stroke::NONE,
    ));
}

fn paint_archive(p: &Painter, r: Rect, color: Color32) {
    // Three stacked horizontal bars suggesting compressed layers.
    let pad   = r.width() * 0.12;
    let bar_w = r.width()  - pad * 2.0;
    let bar_h = r.height() * 0.16;
    let gap   = r.height() * 0.08;
    let top_y = r.min.y + (r.height() - (bar_h * 3.0 + gap * 2.0)) * 0.5;
    for i in 0..3 {
        let y = top_y + i as f32 * (bar_h + gap);
        p.rect_filled(
            Rect::from_min_size(pos2(r.min.x + pad, y), vec2(bar_w, bar_h)),
            1.0, color,
        );
    }
}

fn paint_lock(p: &Painter, r: Rect, color: Color32) {
    // U-shackle on top, solid body underneath.
    let s    = Stroke::new(1.6, color);
    let cx   = r.center().x;
    let w    = r.width();
    let h    = r.height();
    let shackle_w = w * 0.50;
    let shackle_h = h * 0.35;
    let shackle_top_y = r.min.y + h * 0.20;
    let body_top_y    = r.min.y + h * 0.50;
    // Shackle (left vertical, top horizontal, right vertical)
    let l = pos2(cx - shackle_w * 0.5, shackle_top_y + shackle_h);
    let tl = pos2(cx - shackle_w * 0.5, shackle_top_y);
    let tr = pos2(cx + shackle_w * 0.5, shackle_top_y);
    let rr = pos2(cx + shackle_w * 0.5, shackle_top_y + shackle_h);
    p.line_segment([l, tl], s);
    p.line_segment([tl, tr], s);
    p.line_segment([tr, rr], s);
    // Body
    let body_w = w * 0.70;
    let body_h = h * 0.40;
    let body_rect = Rect::from_center_size(
        pos2(cx, body_top_y + body_h * 0.5),
        vec2(body_w, body_h),
    );
    p.rect_filled(body_rect, 1.5, color);
}

fn paint_font(p: &Painter, r: Rect, color: Color32) {
    // Aa glyph centered in the rect — classic font-file icon idea.
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        "Aa",
        FontId::proportional(r.height() * 0.55),
        color,
    );
}

// Suppress dead-code warnings for items that aren't yet exercised in all
// build configurations.
#[allow(dead_code)]
fn _suppress_warnings(_: Vec2, _: Pos2) {}
