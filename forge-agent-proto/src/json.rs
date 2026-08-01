// SPDX-License-Identifier: Apache-2.0
//! JSON, ours.
//!
//! Enough of RFC 8259 to carry the agent protocol, with no dependencies. Scoped
//! deliberately: the protocol is newline-delimited JSON objects of strings,
//! numbers, booleans, arrays and nested objects, and this handles exactly that.
//!
//! Three things it is careful about, because they are where hand-written JSON
//! usually goes wrong:
//!
//!  * **Escapes both ways.** `\uXXXX` including surrogate pairs on the way in,
//!    and on the way out every control character escaped — an unescaped newline
//!    inside a string would split a protocol frame in two and desynchronise the
//!    reader.
//!  * **Bounded recursion.** Parsing is recursive, so nesting is capped. Without
//!    it, a deeply nested document is a stack overflow, which is an abort rather
//!    than an error we can report.
//!  * **No trailing data.** `{"a":1} garbage` is rejected rather than quietly
//!    returning the first value, so a corrupt frame is noticed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How deep nesting may go before parsing gives up.
///
/// The protocol's deepest real shape is roughly message → endpoints → reasoning →
/// provider config, so single digits; 64 is far above anything legitimate and far
/// below what would exhaust the stack.
const MAX_DEPTH: usize = 64;

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// All numbers are `f64`, as JSON has one numeric type. The accessors below
    /// convert, refusing values that would not survive the round trip.
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// Ordered so serialization is deterministic — worth having when diffing
    /// captured protocol output.
    Obj(BTreeMap<String, Json>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    /// Byte offset where parsing stopped.
    pub at: usize,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.at)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

// ── Accessors ─────────────────────────────────────────────────────────────────

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// An unsigned integer, refusing anything that is not exactly one.
    ///
    /// A fractional or negative value here means the sender and this build
    /// disagree about the field, and silently truncating would hide that.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_u64().map(|n| n as usize)
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|n| u32::try_from(n).ok())
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// A string field, or empty when absent — the common case for the protocol's
    /// non-optional strings.
    pub fn str_or_empty(&self, key: &str) -> String {
        self.get(key).and_then(Json::as_str).unwrap_or("").to_string()
    }

    pub fn bool_or_false(&self, key: &str) -> bool {
        self.get(key).and_then(Json::as_bool).unwrap_or(false)
    }

    pub fn usize_or_zero(&self, key: &str) -> usize {
        self.get(key).and_then(Json::as_usize).unwrap_or(0)
    }

    pub fn u32_or_zero(&self, key: &str) -> u32 {
        self.get(key).and_then(Json::as_u32).unwrap_or(0)
    }

    pub fn u64_or_zero(&self, key: &str) -> u64 {
        self.get(key).and_then(Json::as_u64).unwrap_or(0)
    }

    /// An optional string: absent, or `null`, both give `None`.
    pub fn opt_str(&self, key: &str) -> Option<String> {
        match self.get(key) {
            None | Some(Json::Null) => None,
            Some(v) => v.as_str().map(str::to_string),
        }
    }

    pub fn opt_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            None | Some(Json::Null) => None,
            Some(v) => v.as_bool(),
        }
    }

    pub fn opt_usize(&self, key: &str) -> Option<usize> {
        match self.get(key) {
            None | Some(Json::Null) => None,
            Some(v) => v.as_usize(),
        }
    }

    pub fn opt_u64(&self, key: &str) -> Option<u64> {
        match self.get(key) {
            None | Some(Json::Null) => None,
            Some(v) => v.as_u64(),
        }
    }

    /// Every element of an array field, or an empty vector when absent.
    pub fn array_or_empty(&self, key: &str) -> &[Json] {
        self.get(key).and_then(Json::as_array).unwrap_or(&[])
    }
}

// ── Building ──────────────────────────────────────────────────────────────────

/// Numbers that can be carried by JSON.
///
/// `f64: From<usize>` does not exist, because a 64-bit `usize` cannot convert
/// losslessly — so the conversion is stated here rather than borrowed from
/// `Into`. Every protocol count is far below 2^53, where `f64` is still exact,
/// and [`Json::as_u64`] refuses anything fractional on the way back so a value
/// that did lose precision would be caught rather than silently truncated.
pub trait Num: Copy {
    fn to_f64(self) -> f64;
}

macro_rules! impl_num {
    ($($t:ty),*) => { $( impl Num for $t {
        fn to_f64(self) -> f64 { self as f64 }
    } )* };
}
impl_num!(usize, u8, u16, u32, u64, i32, i64, f32, f64);

/// Accumulates an object, skipping fields the protocol omits when empty.
#[derive(Default)]
pub struct Object(BTreeMap<String, Json>);

impl Object {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn set(mut self, key: &str, value: Json) -> Self {
        self.0.insert(key.to_string(), value);
        self
    }

    pub fn str(self, key: &str, value: &str) -> Self {
        self.set(key, Json::Str(value.to_string()))
    }

    pub fn num(self, key: &str, value: impl Num) -> Self {
        self.set(key, Json::Num(value.to_f64()))
    }

    pub fn bool(self, key: &str, value: bool) -> Self {
        self.set(key, Json::Bool(value))
    }

    /// Set only when present. The agent omits `None` optionals entirely rather
    /// than sending `null`, and a client that sent `null` where the agent expects
    /// absence would be relying on the agent's tolerance.
    pub fn opt_str(self, key: &str, value: &Option<String>) -> Self {
        match value {
            Some(v) => self.str(key, v),
            None => self,
        }
    }

    pub fn opt_num(self, key: &str, value: &Option<impl Num>) -> Self {
        match value {
            Some(v) => self.num(key, *v),
            None => self,
        }
    }

    pub fn opt_bool(self, key: &str, value: &Option<bool>) -> Self {
        match value {
            Some(v) => self.bool(key, *v),
            None => self,
        }
    }

    /// Always present, even when `None`, serialized as `null`.
    ///
    /// A few protocol fields work this way — `max_turns` is sent as `null`
    /// rather than omitted — and the difference is observable.
    pub fn nullable_num(self, key: &str, value: &Option<impl Num>) -> Self {
        match value {
            Some(v) => self.num(key, *v),
            None => self.set(key, Json::Null),
        }
    }

    pub fn arr(self, key: &str, values: Vec<Json>) -> Self {
        self.set(key, Json::Arr(values))
    }

    pub fn build(self) -> Json {
        Json::Obj(self.0)
    }
}

/// Shorthand for a tagged protocol message with no other fields.
pub fn tagged(tag: &str) -> Json {
    Object::new().str("type", tag).build()
}

// ── Writing ───────────────────────────────────────────────────────────────────

impl Json {
    /// Serialize compactly.
    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => write_number(*n, out),
            Json::Str(s) => write_string(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Numbers, preferring integer form.
///
/// `1.0` would be read back as a float by anything strict about it, and the
/// protocol's numbers are counts. JSON has no non-finite values, so those become
/// `null` rather than emitting `NaN`, which no parser accepts.
fn write_number(n: f64, out: &mut String) {
    if !n.is_finite() {
        out.push_str("null");
        return;
    }
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

/// Strings, with every character JSON requires escaped.
///
/// The control-character range matters most: a raw newline inside a string would
/// end the protocol frame early and leave the reader parsing the remainder as a
/// new message.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// Parse one complete JSON value, rejecting trailing content.
pub fn parse(input: &str) -> Result<Json> {
    let mut p = Parser { bytes: input.as_bytes(), pos: 0, depth: 0 };
    p.skip_whitespace();
    let value = p.value()?;
    p.skip_whitespace();
    if p.pos < p.bytes.len() {
        return Err(p.error("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos:   usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> Error {
        Error { message: message.to_string(), at: self.pos }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected {:?}", byte as char)))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn value(&mut self) -> Result<Json> {
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    /// Enter a nested value, refusing to recurse past [`MAX_DEPTH`].
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn array(&mut self) -> Result<Json> {
        self.expect(b'[')?;
        self.nested(|p| {
            let mut items = Vec::new();
            p.skip_whitespace();
            if p.peek() == Some(b']') {
                p.pos += 1;
                return Ok(Json::Arr(items));
            }
            loop {
                p.skip_whitespace();
                items.push(p.value()?);
                p.skip_whitespace();
                match p.peek() {
                    Some(b',') => p.pos += 1,
                    Some(b']') => {
                        p.pos += 1;
                        return Ok(Json::Arr(items));
                    }
                    _ => return Err(p.error("expected ',' or ']'")),
                }
            }
        })
    }

    fn object(&mut self) -> Result<Json> {
        self.expect(b'{')?;
        self.nested(|p| {
            let mut map = BTreeMap::new();
            p.skip_whitespace();
            if p.peek() == Some(b'}') {
                p.pos += 1;
                return Ok(Json::Obj(map));
            }
            loop {
                p.skip_whitespace();
                let key = p.string()?;
                p.skip_whitespace();
                p.expect(b':')?;
                p.skip_whitespace();
                let value = p.value()?;
                // Last wins, as most parsers do; the alternative is rejecting
                // documents that are legal JSON.
                map.insert(key, value);
                p.skip_whitespace();
                match p.peek() {
                    Some(b',') => p.pos += 1,
                    Some(b'}') => {
                        p.pos += 1;
                        return Ok(Json::Obj(map));
                    }
                    _ => return Err(p.error("expected ',' or '}'")),
                }
            }
        })
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid number"))?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| Error { message: "invalid number".into(), at: start })
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(byte) if byte < 0x20 => {
                    return Err(self.error("unescaped control character in string"));
                }
                Some(_) => {
                    // Copy one whole UTF-8 sequence. Indexing by byte would
                    // split multi-byte characters.
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| self.error("invalid UTF-8"))?;
                    let c = rest.chars().next().expect("non-empty");
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<()> {
        let byte = self.peek().ok_or_else(|| self.error("unterminated escape"))?;
        self.pos += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                // A high surrogate is only half a character; the low half must
                // follow, or the pair cannot be reassembled.
                let ch = if (0xD800..0xDC00).contains(&first) {
                    if !self.bytes[self.pos..].starts_with(b"\\u") {
                        return Err(self.error("lone high surrogate"));
                    }
                    self.pos += 2;
                    let second = self.hex4()?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(self.error("invalid low surrogate"));
                    }
                    let combined =
                        0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                    char::from_u32(combined).ok_or_else(|| self.error("invalid surrogate pair"))?
                } else if (0xDC00..0xE000).contains(&first) {
                    return Err(self.error("lone low surrogate"));
                } else {
                    char::from_u32(first).ok_or_else(|| self.error("invalid escape"))?
                };
                out.push(ch);
            }
            _ => return Err(self.error("unknown escape")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.error("truncated \\u escape"));
        }
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| self.error("invalid \\u escape"))?;
        let value = u32::from_str_radix(text, 16)
            .map_err(|_| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: &[(&str, Json)]) -> Json {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(k.to_string(), v.clone());
        }
        Json::Obj(map)
    }

    // ── Round trips ───────────────────────────────────────────────────────

    #[test]
    fn scalars_round_trip() {
        for value in [
            Json::Null,
            Json::Bool(true),
            Json::Bool(false),
            Json::Num(0.0),
            Json::Num(42.0),
            Json::Num(-7.0),
            Json::Num(1.5),
            Json::Str(String::new()),
            Json::Str("hello".into()),
        ] {
            let text = value.to_string();
            assert_eq!(parse(&text).unwrap(), value, "{text} did not round-trip");
        }
    }

    #[test]
    fn containers_round_trip() {
        let value = obj(&[
            ("empty_arr", Json::Arr(vec![])),
            ("empty_obj", obj(&[])),
            ("nested", Json::Arr(vec![obj(&[("a", Json::Num(1.0))]), Json::Null])),
        ]);
        assert_eq!(parse(&value.to_string()).unwrap(), value);
    }

    /// Counts must not come back as floats.
    #[test]
    fn whole_numbers_serialize_without_a_decimal_point() {
        assert_eq!(Json::Num(200_000.0).to_string(), "200000");
        assert_eq!(Json::Num(0.0).to_string(), "0");
        assert_eq!(Json::Num(-3.0).to_string(), "-3");
    }

    #[test]
    fn fractional_numbers_keep_their_fraction() {
        assert_eq!(Json::Num(1.5).to_string(), "1.5");
    }

    /// JSON has no NaN or infinity; emitting them would produce a document no
    /// parser accepts.
    #[test]
    fn non_finite_numbers_become_null() {
        assert_eq!(Json::Num(f64::NAN).to_string(), "null");
        assert_eq!(Json::Num(f64::INFINITY).to_string(), "null");
    }

    // ── String escaping ───────────────────────────────────────────────────

    /// The one that matters most for a newline-delimited protocol: a raw newline
    /// in a string would end the frame early and desynchronise the reader.
    #[test]
    fn control_characters_are_escaped_on_the_way_out() {
        let value = Json::Str("line one\nline two\ttabbed".into());
        let text = value.to_string();
        assert!(!text.contains('\n'), "no raw newline: {text:?}");
        assert!(!text.contains('\t'), "no raw tab");
        assert!(text.contains("\\n") && text.contains("\\t"));
        assert_eq!(parse(&text).unwrap(), value);
    }

    #[test]
    fn quotes_and_backslashes_survive() {
        let value = Json::Str(r#"a "quoted" \ backslash"#.into());
        assert_eq!(parse(&value.to_string()).unwrap(), value);
    }

    #[test]
    fn obscure_control_characters_use_the_u_form() {
        let value = Json::Str("\u{1}\u{1f}".into());
        let text = value.to_string();
        assert!(text.contains("\\u0001"), "got {text}");
        assert!(text.contains("\\u001f"));
        assert_eq!(parse(&text).unwrap(), value);
    }

    #[test]
    fn all_the_named_escapes_parse() {
        let parsed = parse(r#""\"\\\/\b\f\n\r\t""#).unwrap();
        assert_eq!(parsed, Json::Str("\"\\/\u{8}\u{c}\n\r\t".into()));
    }

    #[test]
    fn a_raw_control_character_in_a_string_is_rejected() {
        assert!(parse("\"line\nbreak\"").is_err(), "unescaped newline must fail");
    }

    // ── Unicode ───────────────────────────────────────────────────────────

    #[test]
    fn basic_multilingual_escapes_decode() {
        assert_eq!(parse(r#""é""#).unwrap(), Json::Str("é".into()));
        assert_eq!(parse(r#""日本""#).unwrap(), Json::Str("日本".into()));
    }

    /// Emoji arrive as surrogate pairs from some encoders; failing to reassemble
    /// them would corrupt exactly the text that already caused width trouble.
    #[test]
    fn surrogate_pairs_reassemble_into_one_character() {
        // U+1F468 MAN
        let parsed = parse(r#""👨""#).unwrap();
        assert_eq!(parsed, Json::Str("\u{1F468}".into()));
        assert_eq!(parsed.as_str().unwrap().chars().count(), 1);
    }

    #[test]
    fn a_lone_surrogate_is_rejected_rather_than_producing_nonsense() {
        assert!(parse(r#""\ud83d""#).is_err(), "lone high surrogate");
        assert!(parse(r#""\udc68""#).is_err(), "lone low surrogate");
        assert!(parse(r#""\ud83dA""#).is_err(), "high surrogate then a letter");
    }

    #[test]
    fn multibyte_text_passes_through_unescaped() {
        let value = Json::Str("日本語 café 👨‍👩‍👧".into());
        assert_eq!(parse(&value.to_string()).unwrap(), value);
    }

    #[test]
    fn truncated_escapes_are_errors() {
        assert!(parse(r#""\u12""#).is_err());
        assert!(parse(r#""\"#).is_err());
        assert!(parse(r#""\q""#).is_err(), "unknown escape");
    }

    // ── Malformed input ───────────────────────────────────────────────────

    #[test]
    fn trailing_content_is_rejected() {
        assert!(parse(r#"{"a":1} and then some"#).is_err());
        assert!(parse("1 2").is_err());
    }

    #[test]
    fn unterminated_containers_are_rejected() {
        for bad in [r#"{"a":1"#, "[1,2", r#""unterminated"#, "{", "["] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn empty_input_is_an_error_not_null() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
    }

    #[test]
    fn bad_literals_are_rejected() {
        for bad in ["nul", "tru", "fals", "None", "TRUE"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn whitespace_around_and_inside_is_allowed() {
        let text = "  {\n \"a\" : [ 1 , 2 ] \n} \t";
        assert_eq!(
            parse(text).unwrap(),
            obj(&[("a", Json::Arr(vec![Json::Num(1.0), Json::Num(2.0)]))]),
        );
    }

    /// Unbounded recursion on hostile input is a stack overflow, which aborts
    /// the process rather than returning an error we can report.
    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let deep = format!("{}{}", "[".repeat(1000), "]".repeat(1000));
        let err = parse(&deep).expect_err("must refuse");
        assert!(err.message.contains("deep"), "got {err}");
    }

    #[test]
    fn nesting_within_the_limit_still_parses() {
        let depth = 30;
        let text = format!("{}1{}", "[".repeat(depth), "]".repeat(depth));
        assert!(parse(&text).is_ok(), "{depth} levels should be fine");
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    #[test]
    fn integer_accessors_refuse_values_that_are_not_integers() {
        assert_eq!(Json::Num(5.0).as_u64(), Some(5));
        assert_eq!(Json::Num(5.5).as_u64(), None, "fractional is not a count");
        assert_eq!(Json::Num(-1.0).as_u64(), None, "negative is not a count");
        assert_eq!(Json::Num(f64::NAN).as_u64(), None);
        assert_eq!(Json::Str("5".into()).as_u64(), None, "a string is not a number");
    }

    #[test]
    fn u32_rejects_values_that_would_wrap() {
        assert_eq!(Json::Num(4_294_967_295.0).as_u32(), Some(u32::MAX));
        assert_eq!(Json::Num(4_294_967_296.0).as_u32(), None);
    }

    #[test]
    fn optional_accessors_treat_null_and_absent_alike() {
        let value = obj(&[("present", Json::Str("x".into())), ("nulled", Json::Null)]);
        assert_eq!(value.opt_str("present").as_deref(), Some("x"));
        assert_eq!(value.opt_str("nulled"), None);
        assert_eq!(value.opt_str("missing"), None);
    }

    #[test]
    fn defaulting_accessors_cover_absent_fields() {
        let empty = obj(&[]);
        assert_eq!(empty.str_or_empty("nope"), "");
        assert!(!empty.bool_or_false("nope"));
        assert_eq!(empty.usize_or_zero("nope"), 0);
        assert!(empty.array_or_empty("nope").is_empty());
    }

    #[test]
    fn accessors_on_the_wrong_type_return_none_rather_than_panicking() {
        let value = Json::Str("not an object".into());
        assert!(value.get("key").is_none());
        assert!(value.as_array().is_none());
        assert_eq!(value.str_or_empty("key"), "");
    }

    // ── Building ──────────────────────────────────────────────────────────

    #[test]
    fn omitted_optionals_are_absent_not_null() {
        let built = Object::new()
            .str("kept", "yes")
            .opt_str("gone", &None)
            .build();
        let text = built.to_string();
        assert!(!text.contains("gone"), "an absent optional is not written: {text}");
    }

    /// A few protocol fields are sent as `null` rather than omitted, and the
    /// difference is observable on the wire.
    #[test]
    fn nullable_fields_are_written_as_null() {
        let built = Object::new().nullable_num("max_turns", &None::<usize>).build();
        assert_eq!(built.to_string(), r#"{"max_turns":null}"#);
    }

    /// The integer types the protocol actually uses must all be accepted.
    #[test]
    fn every_protocol_number_type_can_be_written() {
        let built = Object::new()
            .num("a_usize", 200_000usize)
            .num("a_u32", 16_384u32)
            .num("a_u64", 9_000u64)
            .num("a_float", 1.5f64)
            .build();
        assert_eq!(built.get("a_usize").unwrap().as_usize(), Some(200_000));
        assert_eq!(built.get("a_u32").unwrap().as_u32(), Some(16_384));
        assert_eq!(built.get("a_u64").unwrap().as_u64(), Some(9_000));
        assert_eq!(built.get("a_float").unwrap().as_f64(), Some(1.5));
    }

    #[test]
    fn object_keys_are_ordered_so_output_is_deterministic() {
        let a = Object::new().str("zebra", "1").str("apple", "2").build();
        let b = Object::new().str("apple", "2").str("zebra", "1").build();
        assert_eq!(a.to_string(), b.to_string(), "insertion order must not matter");
        assert!(a.to_string().starts_with(r#"{"apple""#));
    }

    #[test]
    fn tagged_builds_a_bare_message() {
        assert_eq!(tagged("done").to_string(), r#"{"type":"done"}"#);
    }

    // ── Against real protocol output ──────────────────────────────────────

    /// The fixture captured from a real `forge-agent --headless` startup. If this
    /// parser cannot read what the agent actually sends, nothing else matters.
    #[test]
    fn parses_real_agent_output() {
        let fixture = include_str!("../tests/fixtures/agent_startup.jsonl");
        let mut tags = Vec::new();
        for (i, line) in fixture.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            let value = parse(line).unwrap_or_else(|e| panic!("line {i}: {e}\n{line}"));
            tags.push(value.str_or_empty("type"));
        }
        assert_eq!(tags, vec!["init", "usage_update", "usage"]);
    }

    /// And re-serializing it must produce something that parses back identically,
    /// which is the property the protocol relies on.
    #[test]
    fn real_agent_output_survives_a_round_trip() {
        let fixture = include_str!("../tests/fixtures/agent_startup.jsonl");
        for line in fixture.lines().filter(|l| !l.trim().is_empty()) {
            let once = parse(line).expect("parses");
            let twice = parse(&once.to_string()).expect("re-parses");
            assert_eq!(once, twice, "round trip changed the value");
        }
    }

    /// The init message is the largest and most nested thing the agent sends;
    /// check its interesting fields survive rather than just that it parsed.
    #[test]
    fn the_real_init_message_yields_usable_fields() {
        let fixture = include_str!("../tests/fixtures/agent_startup.jsonl");
        let init = parse(fixture.lines().next().unwrap()).unwrap();

        assert_eq!(init.str_or_empty("type"), "init");
        assert!(!init.str_or_empty("project_root").is_empty());
        assert!(init.usize_or_zero("max_context_tokens") > 0);

        let endpoints = init.array_or_empty("endpoints");
        assert!(!endpoints.is_empty(), "endpoints came through");
        let first = &endpoints[0];
        assert!(!first.str_or_empty("name").is_empty());
        assert!(first.u32_or_zero("max_output_tokens") > 0);
        // Three levels down, which is what exercises the nesting.
        let reasoning = first.get("reasoning").expect("reasoning object");
        assert!(reasoning.get("anthropic").is_some());

        // `max_turns` is the nullable case.
        let defs = init.array_or_empty("agent_definitions");
        assert!(!defs.is_empty());
        assert!(defs[0].get("max_turns").is_some(), "present, possibly null");
    }
}
