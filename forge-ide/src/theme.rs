//! Color schemes. Built-in palettes plus user TOML overrides in
//! ~/.config/forge-ide/themes/*.toml.

use egui::Color32;
use std::path::PathBuf;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Palette {
    pub name:          String,
    pub dark:          bool,
    // Editor chrome
    pub editor_bg:     [u8; 3],
    pub cur_line_bg:   [u8; 3],
    pub gutter_fg:     [u8; 3],
    pub gutter_cur_fg: [u8; 3],
    // Syntax
    pub default_fg:    [u8; 3],
    pub keyword:       [u8; 3],
    pub type_col:      [u8; 3],
    pub string:        [u8; 3],
    pub comment:       [u8; 3],
    pub number:        [u8; 3],
    pub macro_pre:     [u8; 3],
    pub func:          [u8; 3],
    // Bracket-pair colorization (rotating by nesting depth)
    pub brackets:      Vec<[u8; 3]>,
    pub indent_guide:  [u8; 3],
}

fn c(rgb: [u8; 3]) -> Color32 { Color32::from_rgb(rgb[0], rgb[1], rgb[2]) }

impl Palette {
    pub fn editor_bg_c(&self) -> Color32     { c(self.editor_bg) }
    pub fn cur_line_bg_c(&self) -> Color32   { c(self.cur_line_bg) }
    pub fn gutter_fg_c(&self) -> Color32     { c(self.gutter_fg) }
    pub fn gutter_cur_fg_c(&self) -> Color32 { c(self.gutter_cur_fg) }
    pub fn default_fg_c(&self) -> Color32    { c(self.default_fg) }
    pub fn keyword_c(&self) -> Color32       { c(self.keyword) }
    pub fn type_c(&self) -> Color32          { c(self.type_col) }
    pub fn string_c(&self) -> Color32        { c(self.string) }
    pub fn comment_c(&self) -> Color32       { c(self.comment) }
    pub fn number_c(&self) -> Color32        { c(self.number) }
    pub fn macro_c(&self) -> Color32         { c(self.macro_pre) }
    pub fn func_c(&self) -> Color32          { c(self.func) }
    pub fn indent_guide_c(&self) -> Color32  { c(self.indent_guide) }
    pub fn bracket_c(&self, depth: usize) -> Color32 {
        if self.brackets.is_empty() { return self.default_fg_c(); }
        c(self.brackets[depth % self.brackets.len()])
    }
}

impl Default for Palette {
    fn default() -> Self { dark_plus() }
}

pub fn dark_plus() -> Palette {
    Palette {
        name:          "Dark+".into(),
        dark:          true,
        editor_bg:     [30, 30, 30],
        cur_line_bg:   [42, 45, 46],
        gutter_fg:     [133, 133, 133],
        gutter_cur_fg: [198, 198, 198],
        default_fg:    [212, 212, 212],
        keyword:       [86, 156, 214],
        type_col:      [78, 201, 176],
        string:        [206, 145, 120],
        comment:       [106, 153, 85],
        number:        [181, 206, 168],
        macro_pre:     [197, 134, 192],
        func:          [220, 220, 170],
        brackets:      vec![[255, 216, 102], [218, 112, 214], [23, 159, 255]],
        indent_guide:  [60, 60, 60],
    }
}

pub fn light_plus() -> Palette {
    Palette {
        name:          "Light+".into(),
        dark:          false,
        editor_bg:     [255, 255, 255],
        cur_line_bg:   [232, 242, 254],
        gutter_fg:     [110, 110, 110],
        gutter_cur_fg: [20, 20, 20],
        default_fg:    [30, 30, 30],
        keyword:       [0, 0, 255],
        type_col:      [38, 127, 153],
        string:        [163, 21, 21],
        comment:       [0, 128, 0],
        number:        [9, 134, 88],
        macro_pre:     [175, 0, 219],
        func:          [121, 94, 38],
        brackets:      vec![[4, 49, 250], [50, 140, 50], [123, 56, 20]],
        indent_guide:  [210, 210, 210],
    }
}

pub fn monokai() -> Palette {
    Palette {
        name:          "Monokai".into(),
        dark:          true,
        editor_bg:     [39, 40, 34],
        cur_line_bg:   [62, 61, 50],
        gutter_fg:     [144, 144, 138],
        gutter_cur_fg: [248, 248, 242],
        default_fg:    [248, 248, 242],
        keyword:       [249, 38, 114],
        type_col:      [102, 217, 239],
        string:        [230, 219, 116],
        comment:       [117, 113, 94],
        number:        [174, 129, 255],
        macro_pre:     [166, 226, 46],
        func:          [166, 226, 46],
        brackets:      vec![[248, 248, 242], [253, 151, 31], [102, 217, 239]],
        indent_guide:  [64, 64, 58],
    }
}

pub fn one_dark() -> Palette {
    Palette {
        name:          "One Dark".into(),
        dark:          true,
        editor_bg:     [40, 44, 52],
        cur_line_bg:   [44, 49, 60],
        gutter_fg:     [99, 109, 131],
        gutter_cur_fg: [171, 178, 191],
        default_fg:    [171, 178, 191],
        keyword:       [198, 120, 221],
        type_col:      [229, 192, 123],
        string:        [152, 195, 121],
        comment:       [92, 99, 112],
        number:        [209, 154, 102],
        macro_pre:     [86, 182, 194],
        func:          [97, 175, 239],
        brackets:      vec![[209, 154, 102], [198, 120, 221], [86, 182, 194]],
        indent_guide:  [58, 63, 75],
    }
}

/// Built-in palettes shown in the picker (before user themes are appended).
pub fn builtins() -> Vec<Palette> {
    vec![dark_plus(), light_plus(), monokai(), one_dark()]
}

fn themes_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("forge-ide")
        .join("themes")
}

/// All available palettes: built-ins + any TOML files in the themes dir.
pub fn all() -> Vec<Palette> {
    let mut out = builtins();
    if let Ok(rd) = std::fs::read_dir(themes_dir()) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(pal) = toml::from_str::<Palette>(&text) {
                    out.push(pal);
                }
            }
        }
    }
    out
}

pub fn by_name(name: &str) -> Palette {
    all().into_iter().find(|p| p.name == name).unwrap_or_default()
}

pub fn theme_names() -> Vec<String> {
    all().into_iter().map(|p| p.name).collect()
}
