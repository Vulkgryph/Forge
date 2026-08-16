// SPDX-License-Identifier: Apache-2.0
//! Syntax highlighting for fenced code blocks.
//!
//! The TypeScript client used `cli-highlight`, which wraps highlight.js and its
//! two hundred grammars. That is not available to us and reproducing it is not
//! the goal: what makes code readable in a chat transcript is separating
//! comments, strings, numbers and keywords from everything else. A lexer that
//! does those four things across the languages an agent actually emits gets
//! almost all of the benefit.
//!
//! Deliberately lexical, not syntactic — no parsing, no grammar. It cannot tell a
//! type from a variable, and does not try. The rule it follows instead is that
//! **wrong colour is worse than no colour**: an unknown language gets no
//! highlighting rather than a guess, and anything the lexer is unsure of stays
//! plain.
//!
//! State carries across lines, because block comments and triple-quoted strings
//! do. Highlighting each line independently would end the colouring at the first
//! newline inside a comment.

use crate::screen::Style;

/// What a run of characters is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Plain,
    Keyword,
    /// Strings and characters.
    Str,
    Number,
    Comment,
    /// Attributes, decorators, preprocessor lines, macros.
    Meta,
}

impl Token {
    /// The colour for a token, from the terminal's own palette so it follows the
    /// user's theme.
    pub fn style(self) -> Style {
        match self {
            Token::Plain => Style::default(),
            // Blue, not magenta: nothing in Forge is pink. Distinct from the
            // green strings, yellow numbers and cyan meta around it.
            Token::Keyword => Style::fg(12), // blue
            Token::Str => Style::fg(10),     // green
            Token::Number => Style::fg(11),  // yellow
            Token::Comment => Style::fg(244),
            Token::Meta => Style::fg(14),    // cyan
        }
    }
}

/// How a language is lexed.
struct Syntax {
    keywords:      &'static [&'static str],
    /// Sequences that begin a comment running to end of line.
    line_comment:  &'static [&'static str],
    /// Whether `/* */` nests through lines.
    block_comment: bool,
    /// Quote characters that start a string.
    quotes:        &'static [char],
    /// Whether a `#[...]`-style attribute or `@decorator` is meta.
    meta_prefix:   &'static [char],
}

const RUST: Syntax = Syntax {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while",
    ],
    line_comment: &["//"],
    block_comment: true,
    quotes: &['"', '\''],
    meta_prefix: &['#'],
};

const JS: Syntax = Syntax {
    keywords: &[
        "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
        "delete", "do", "else", "export", "extends", "false", "finally", "for", "from",
        "function", "if", "import", "in", "instanceof", "interface", "let", "new", "null",
        "of", "return", "static", "super", "switch", "this", "throw", "true", "try",
        "type", "typeof", "undefined", "var", "void", "while", "yield",
    ],
    line_comment: &["//"],
    block_comment: true,
    quotes: &['"', '\'', '`'],
    meta_prefix: &['@'],
};

const PYTHON: Syntax = Syntax {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return",
        "True", "try", "while", "with", "yield",
    ],
    line_comment: &["#"],
    block_comment: false,
    quotes: &['"', '\''],
    meta_prefix: &['@'],
};

const GO: Syntax = Syntax {
    keywords: &[
        "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
        "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
        "return", "select", "struct", "switch", "type", "var", "nil", "true", "false",
    ],
    line_comment: &["//"],
    block_comment: true,
    quotes: &['"', '`', '\''],
    meta_prefix: &[],
};

const C_LIKE: Syntax = Syntax {
    keywords: &[
        "auto", "break", "case", "char", "class", "const", "continue", "default", "do",
        "double", "else", "enum", "extern", "float", "for", "goto", "if", "inline", "int",
        "long", "namespace", "new", "nullptr", "private", "protected", "public", "return",
        "short", "signed", "sizeof", "static", "struct", "switch", "template", "this",
        "throw", "try", "typedef", "union", "unsigned", "using", "virtual", "void", "while",
    ],
    line_comment: &["//"],
    block_comment: true,
    quotes: &['"', '\''],
    meta_prefix: &['#'],
};

const SHELL: Syntax = Syntax {
    keywords: &[
        "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
        "if", "in", "local", "return", "then", "until", "while",
    ],
    line_comment: &["#"],
    block_comment: false,
    quotes: &['"', '\''],
    meta_prefix: &[],
};

const SQL: Syntax = Syntax {
    keywords: &[
        "and", "as", "asc", "by", "create", "delete", "desc", "distinct", "drop", "from",
        "group", "having", "index", "inner", "insert", "into", "join", "left", "limit",
        "not", "null", "on", "or", "order", "outer", "select", "set", "table", "union",
        "update", "values", "where",
    ],
    line_comment: &["--"],
    block_comment: true,
    quotes: &['\'', '"'],
    meta_prefix: &[],
};

/// Data formats: no keywords, but strings and numbers still carry most of the
/// meaning.
const DATA: Syntax = Syntax {
    keywords: &["true", "false", "null"],
    line_comment: &["#"],
    block_comment: false,
    quotes: &['"', '\''],
    meta_prefix: &[],
};

const JSON: Syntax = Syntax {
    keywords: &["true", "false", "null"],
    line_comment: &[],
    block_comment: false,
    quotes: &['"'],
    meta_prefix: &[],
};

/// The syntax for a fence's language tag, or `None` when it is unknown.
///
/// Unknown means no highlighting at all. Guessing at a grammar produces
/// confidently wrong colours, which reads worse than plain text.
fn syntax_for(lang: &str) -> Option<&'static Syntax> {
    Some(match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => &RUST,
        "ts" | "typescript" | "js" | "javascript" | "tsx" | "jsx" | "mjs" => &JS,
        "py" | "python" => &PYTHON,
        "go" | "golang" => &GO,
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "cs" | "swift" | "kt" => &C_LIKE,
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" => &SHELL,
        "sql" => &SQL,
        "json" | "jsonc" | "jsonl" => &JSON,
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "env" | "properties" => &DATA,
        _ => return None,
    })
}

/// Whether a language tag is recognised.
pub fn supports(lang: &str) -> bool {
    syntax_for(lang).is_some()
}

/// Lexer state that carries between lines.
#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    in_block_comment: bool,
}

/// Split one line into styled runs.
///
/// An unknown or absent language returns the line as a single plain run, so the
/// caller needs no special case for it.
pub fn line(text: &str, lang: &str, state: &mut State) -> Vec<(String, Token)> {
    let Some(syntax) = syntax_for(lang) else {
        return vec![(text.to_string(), Token::Plain)];
    };
    lex(text, syntax, state)
}

fn lex(text: &str, syntax: &Syntax, state: &mut State) -> Vec<(String, Token)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, Token)> = Vec::new();
    let mut i = 0;

    // A block comment opened on an earlier line runs until it closes.
    if state.in_block_comment {
        match find(&chars, i, "*/") {
            Some(end) => {
                push(&mut out, &chars[i..end + 2], Token::Comment);
                state.in_block_comment = false;
                i = end + 2;
            }
            None => {
                push(&mut out, &chars[i..], Token::Comment);
                return out;
            }
        }
    }

    while i < chars.len() {
        // Line comment: everything to the end.
        if let Some(marker) = syntax
            .line_comment
            .iter()
            .find(|m| starts_with(&chars, i, m))
        {
            let _ = marker;
            push(&mut out, &chars[i..], Token::Comment);
            return out;
        }

        // Block comment.
        if syntax.block_comment && starts_with(&chars, i, "/*") {
            match find(&chars, i + 2, "*/") {
                Some(end) => {
                    push(&mut out, &chars[i..end + 2], Token::Comment);
                    i = end + 2;
                }
                None => {
                    push(&mut out, &chars[i..], Token::Comment);
                    state.in_block_comment = true;
                    return out;
                }
            }
            continue;
        }

        let c = chars[i];

        // String, to its closing quote. An unterminated one is coloured to the
        // end of the line rather than swallowing the rest of the block, because
        // guessing that a quote spans lines is wrong more often than right.
        if syntax.quotes.contains(&c) {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2; // an escape, whatever follows
                    continue;
                }
                if chars[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let end = j.min(chars.len());
            push(&mut out, &chars[i..end], Token::Str);
            i = end;
            continue;
        }

        // Attribute, decorator or preprocessor line.
        if syntax.meta_prefix.contains(&c) && at_line_start_or_after_space(&chars, i) {
            let end = chars.len();
            push(&mut out, &chars[i..end], Token::Meta);
            return out;
        }

        // Number.
        if c.is_ascii_digit() {
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
                j += 1;
            }
            push(&mut out, &chars[i..j], Token::Number);
            i = j;
            continue;
        }

        // Word: a keyword, or plain.
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let token = if syntax.keywords.contains(&word.as_str()) {
                Token::Keyword
            } else {
                Token::Plain
            };
            push_str(&mut out, word, token);
            i = j;
            continue;
        }

        // Anything else: punctuation and whitespace, merged into plain runs.
        push(&mut out, &chars[i..i + 1], Token::Plain);
        i += 1;
    }

    out
}

/// Append, merging with the previous run when the token matches — otherwise
/// every character of punctuation would be its own span.
fn push(out: &mut Vec<(String, Token)>, chars: &[char], token: Token) {
    push_str(out, chars.iter().collect(), token);
}

fn push_str(out: &mut Vec<(String, Token)>, text: String, token: Token) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((existing, t)) if *t == token => existing.push_str(&text),
        _ => out.push((text, token)),
    }
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    if at + needle.len() > chars.len() {
        return false;
    }
    chars[at..at + needle.len()] == needle[..]
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| starts_with(chars, i, needle))
}

/// True when only whitespace precedes this position, or the previous character
/// is a space — so `#` in `#[derive]` is meta but in `a#b` is not.
fn at_line_start_or_after_space(chars: &[char], at: usize) -> bool {
    chars[..at].iter().all(|c| c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lex a line and return the runs as (text, token) pairs.
    fn lex_line(text: &str, lang: &str) -> Vec<(String, Token)> {
        let mut state = State::default();
        line(text, lang, &mut state)
    }

    /// Find the token a substring was given.
    fn token_of(runs: &[(String, Token)], needle: &str) -> Token {
        runs.iter()
            .find(|(text, _)| text.contains(needle))
            .unwrap_or_else(|| panic!("no run containing {needle:?} in {runs:?}"))
            .1
    }

    fn plain(runs: &[(String, Token)]) -> String {
        runs.iter().map(|(t, _)| t.as_str()).collect()
    }

    // ── The invariant that matters most ───────────────────────────────────

    /// Highlighting must never change the text. A lexer that drops or duplicates
    /// a character corrupts the code it is meant to make readable.
    #[test]
    fn lexing_never_alters_the_text() {
        let samples = [
            ("rust", r#"fn main() { let x = "hi"; /* c */ 42 } // done"#),
            ("python", "def f(a, b):  # comment\n    return 'x'"),
            ("json", r#"{"a": 1, "b": [true, null]}"#),
            ("sh", "for f in *.txt; do echo \"$f\"; done # loop"),
            ("sql", "SELECT * FROM t WHERE a = 'b' -- note"),
            ("go", "func main() { s := `raw` }"),
            ("unknownlang", "anything at all $%^&*"),
            ("rust", "日本語 = \"テキスト\"; // 語"),
            ("rust", ""),
        ];
        for (lang, text) in samples {
            for line_text in text.lines() {
                let runs = lex_line(line_text, lang);
                assert_eq!(
                    plain(&runs), line_text,
                    "{lang} lexing changed the text",
                );
            }
        }
    }

    /// An unknown language gets no highlighting rather than a guess — a wrong
    /// colour reads worse than none.
    #[test]
    fn an_unknown_language_is_left_plain() {
        let runs = lex_line("fn main() { }", "brainfuck");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, Token::Plain);
        assert!(!supports("brainfuck"));
    }

    #[test]
    fn an_empty_language_tag_is_left_plain() {
        let runs = lex_line("let x = 1;", "");
        assert_eq!(runs[0].1, Token::Plain);
    }

    // ── Per-language ──────────────────────────────────────────────────────

    #[test]
    fn rust_keywords_strings_numbers_and_comments() {
        let runs = lex_line(r#"let x = "hi"; // note"#, "rust");
        assert_eq!(token_of(&runs, "let"), Token::Keyword);
        assert_eq!(token_of(&runs, "\"hi\""), Token::Str);
        assert_eq!(token_of(&runs, "// note"), Token::Comment);

        let runs = lex_line("const N: u32 = 42;", "rust");
        assert_eq!(token_of(&runs, "42"), Token::Number);
        assert_eq!(token_of(&runs, "const"), Token::Keyword);
    }

    #[test]
    fn language_aliases_resolve() {
        for lang in ["rust", "rs"] {
            assert_eq!(token_of(&lex_line("fn f()", lang), "fn"), Token::Keyword);
        }
        for lang in ["ts", "typescript", "js", "tsx"] {
            assert_eq!(token_of(&lex_line("const a = 1", lang), "const"), Token::Keyword);
        }
        for lang in ["py", "python"] {
            assert_eq!(token_of(&lex_line("def f():", lang), "def"), Token::Keyword);
        }
    }

    #[test]
    fn python_uses_hash_comments_not_slashes() {
        let runs = lex_line("x = 1  # note", "python");
        assert_eq!(token_of(&runs, "# note"), Token::Comment);

        // `//` is integer division in Python, not a comment.
        let runs = lex_line("y = a // b", "python");
        assert_ne!(token_of(&runs, "//"), Token::Comment);
    }

    #[test]
    fn sql_uses_double_dash_comments() {
        let runs = lex_line("SELECT 1 -- note", "sql");
        assert_eq!(token_of(&runs, "-- note"), Token::Comment);
    }

    #[test]
    fn shell_keywords_and_comments() {
        let runs = lex_line("for f in *; do echo hi; done # loop", "bash");
        assert_eq!(token_of(&runs, "for"), Token::Keyword);
        assert_eq!(token_of(&runs, "# loop"), Token::Comment);
    }

    #[test]
    fn json_strings_and_literals() {
        let runs = lex_line(r#"{"key": true, "n": 12}"#, "json");
        assert_eq!(token_of(&runs, "\"key\""), Token::Str);
        assert_eq!(token_of(&runs, "true"), Token::Keyword);
        assert_eq!(token_of(&runs, "12"), Token::Number);
    }

    /// JSON has no comments, so a `#` must stay plain rather than greying out the
    /// rest of a line.
    #[test]
    fn json_has_no_comments() {
        let runs = lex_line(r#"{"a": "b#c"}"#, "json");
        assert_ne!(token_of(&runs, "#"), Token::Comment);
    }

    // ── Multi-line state ──────────────────────────────────────────────────

    /// A block comment spans lines, so the state has to carry — otherwise the
    /// colouring stops at the first newline.
    #[test]
    fn a_block_comment_continues_across_lines() {
        let mut state = State::default();
        let first = line("/* opening", "rust", &mut state);
        assert_eq!(first[0].1, Token::Comment);
        assert!(state.in_block_comment, "still open");

        let middle = line("still inside let fn", "rust", &mut state);
        assert!(
            middle.iter().all(|(_, t)| *t == Token::Comment),
            "the whole line is comment, keywords included: {middle:?}",
        );

        let last = line("closing */ let x = 1;", "rust", &mut state);
        assert_eq!(last[0].1, Token::Comment);
        assert!(!state.in_block_comment, "closed");
        assert_eq!(token_of(&last, "let"), Token::Keyword, "code after it resumes");
    }

    /// A language without block comments must not treat `/*` as one.
    #[test]
    fn a_language_without_block_comments_ignores_them() {
        let mut state = State::default();
        let runs = line("x = a /* not a comment", "python", &mut state);
        assert!(!state.in_block_comment);
        assert_ne!(token_of(&runs, "/*"), Token::Comment);
    }

    /// An unterminated string must not swallow the rest of the block.
    #[test]
    fn an_unterminated_string_ends_at_the_line() {
        let mut state = State::default();
        line("let s = \"unclosed", "rust", &mut state);
        let next = line("let y = 1;", "rust", &mut state);
        assert_eq!(token_of(&next, "let"), Token::Keyword, "the next line is code again");
    }

    #[test]
    fn escaped_quotes_do_not_end_a_string() {
        let runs = lex_line(r#"let s = "a\"b"; let t = 1;"#, "rust");
        assert_eq!(token_of(&runs, r#""a\"b""#), Token::Str);
        // Two `let`s, both keywords — so the string did not swallow the second
        // statement. Searching for "let t" would fail regardless, since `let` and
        // ` t` are separate runs by design.
        let keywords = runs.iter().filter(|(t, k)| *k == Token::Keyword && t == "let").count();
        assert_eq!(keywords, 2, "code after the string is still code: {runs:?}");
    }

    // ── Meta ──────────────────────────────────────────────────────────────

    #[test]
    fn rust_attributes_and_python_decorators_are_meta() {
        assert_eq!(token_of(&lex_line("#[derive(Debug)]", "rust"), "#["), Token::Meta);
        assert_eq!(token_of(&lex_line("@property", "python"), "@"), Token::Meta);
    }

    /// A `#` in the middle of a line is not an attribute.
    #[test]
    fn a_hash_mid_line_is_not_meta_in_rust() {
        let runs = lex_line("let a = b # c", "rust");
        assert_ne!(token_of(&runs, "#"), Token::Meta);
    }

    // ── Output shape ──────────────────────────────────────────────────────

    /// Adjacent runs of one token must merge, or punctuation becomes one span
    /// per character and the renderer emits an escape sequence for each.
    #[test]
    fn adjacent_runs_of_the_same_token_merge() {
        let runs = lex_line("a + b - c * d", "rust");
        assert!(runs.len() <= 3, "over-split into {} runs: {runs:?}", runs.len());
    }

    #[test]
    fn every_token_has_a_distinct_colour_except_plain() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for token in [Token::Keyword, Token::Str, Token::Number, Token::Comment, Token::Meta] {
            let style = token.style();
            assert!(style.fg.is_some(), "{token:?} has no colour");
            assert!(seen.insert(style.fg), "{token:?} reuses a colour");
        }
        assert_eq!(Token::Plain.style(), Style::default(), "plain is the default");
    }

    /// Nothing may panic on any input, whatever the language.
    #[test]
    fn no_input_panics() {
        let long = "a".repeat(500);
        let inputs: [&str; 18] = [
            "", " ", "\t", "\\", "\"", "'", "`", "/*", "*/", "//", "#", "@", "--",
            "\"\\", "0x", "1.2.3", "日本語 🙂", &long,
        ];
        for lang in ["rust", "python", "json", "sh", "sql", "go", "c", "toml", "nope"] {
            for input in inputs {
                let mut state = State::default();
                let _ = line(input, lang, &mut state);
            }
        }
    }
}
