//! A small, tolerant JSON reader for the build report artifacts.
//!
//! Two of the artifacts the dashboard reads are JSON written by Zephyr's own
//! Python scripts: `zephyr/.config-trace.json` (an array of 6-tuples, one per
//! Kconfig symbol) and `<build>/dashboard/{all,ram,rom}_report.json` (an
//! arbitrarily nested symbol tree from `scripts/footprint/size_report`). The
//! other three artifacts are line-oriented text and are scanned directly by
//! their own modules.
//!
//! The crate has no serde and gains none here (`AGENTS.md`: confirm the
//! standard library cannot reasonably solve it first). The flat, one-shape
//! listings elsewhere in the tree are read by targeted scanners ---
//! [`crate::backend::micropython::packages`] walks the index with a brace
//! counter and pulls named fields out of each object's slice. That technique
//! does not survive a *recursive* shape: the memory report nests symbol nodes
//! to whatever depth the source tree has, and "find the next `"children"`"
//! cannot tell which node it belongs to. So this is a real value reader
//! instead --- ~200 lines, no dependency, and the two callers then read their
//! own shape out of a [`Json`] tree.
//!
//! Tolerance means the same thing it means for the config parsers: a file
//! that does not parse yields `None` and the tab says so, never a panic. The
//! recursion is bounded ([`MAX_DEPTH`]) so a truncated or corrupt artifact
//! cannot overflow the stack --- these files are generated, but they are also
//! read straight off disk after an interrupted build.

/// One JSON value.
///
/// Objects keep their pairs in a `Vec` rather than a map: the artifacts'
/// objects carry at most a handful of keys, a linear scan beats hashing at
/// that size, and the source order survives for anything that wants to show
/// the fields as written.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// An integral number. Every number these artifacts carry --- sizes,
    /// addresses, line numbers --- is an integer, so this is the common case
    /// and it keeps full 64-bit precision.
    Int(i64),
    /// A number that is not integral, or one too large for [`i64`]. Kept so
    /// an unexpected value is still *parsed* (the reader stays tolerant)
    /// rather than failing the whole file.
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// How deep a value may nest before the reader gives up. The deepest real
/// artifact is the memory tree, whose depth follows the source tree's own
/// (tens, not hundreds); the limit exists for corrupt input, not for them.
const MAX_DEPTH: usize = 256;

impl Json {
    /// The string behind a `Str`, or `None` for every other kind.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value.as_str()),
            _ => None,
        }
    }

    /// The value as a signed integer. A `Float` converts when it is
    /// integral, so a generator that writes `4.0` still answers 4.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            Self::Float(value) if value.fract() == 0.0 => Some(*value as i64),
            _ => None,
        }
    }

    /// The value as an unsigned integer --- sizes and addresses. A negative
    /// number is not one.
    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|value| u64::try_from(value).ok())
    }

    /// The elements behind an `Array`, or `None` for every other kind.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// The value of `key` in an `Object`. `None` when the value is not an
    /// object or the key is absent --- the caller decides which of those
    /// two matters.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(pairs) => pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Whether this is `null`. Both artifacts use `null` as a real answer
    /// (an unset Kconfig symbol has no value and no location), so telling it
    /// apart from an absent key matters.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// Reads one JSON document. `None` when the text is not valid JSON, is
/// empty, nests past [`MAX_DEPTH`], or carries trailing content after the
/// top-level value.
pub fn parse(text: &str) -> Option<Json> {
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    let value = read_value(bytes, &mut cursor, 0)?;
    skip_whitespace(bytes, &mut cursor);
    (cursor == bytes.len()).then_some(value)
}

fn skip_whitespace(bytes: &[u8], cursor: &mut usize) {
    while *cursor < bytes.len() && matches!(bytes[*cursor], b' ' | b'\t' | b'\n' | b'\r') {
        *cursor += 1;
    }
}

fn read_value(bytes: &[u8], cursor: &mut usize, depth: usize) -> Option<Json> {
    if depth > MAX_DEPTH {
        return None;
    }
    skip_whitespace(bytes, cursor);
    match *bytes.get(*cursor)? {
        b'{' => read_object(bytes, cursor, depth),
        b'[' => read_array(bytes, cursor, depth),
        b'"' => read_string(bytes, cursor).map(Json::Str),
        b't' => read_literal(bytes, cursor, b"true", Json::Bool(true)),
        b'f' => read_literal(bytes, cursor, b"false", Json::Bool(false)),
        b'n' => read_literal(bytes, cursor, b"null", Json::Null),
        _ => read_number(bytes, cursor),
    }
}

fn read_literal(bytes: &[u8], cursor: &mut usize, word: &[u8], value: Json) -> Option<Json> {
    if bytes.len() < *cursor + word.len() || &bytes[*cursor..*cursor + word.len()] != word {
        return None;
    }
    *cursor += word.len();
    Some(value)
}

fn read_number(bytes: &[u8], cursor: &mut usize) -> Option<Json> {
    let start = *cursor;
    if matches!(bytes.get(*cursor), Some(b'-' | b'+')) {
        *cursor += 1;
    }
    let mut integral = true;
    while let Some(byte) = bytes.get(*cursor) {
        match byte {
            b'0'..=b'9' => {}
            b'.' | b'e' | b'E' | b'-' | b'+' => integral = false,
            _ => break,
        }
        *cursor += 1;
    }
    if *cursor == start {
        return None;
    }
    // The slice is ASCII digits and number punctuation by construction, so
    // the UTF-8 boundary check cannot fail --- but `get` keeps it total.
    let text = std::str::from_utf8(bytes.get(start..*cursor)?).ok()?;
    if integral && let Ok(value) = text.parse::<i64>() {
        return Some(Json::Int(value));
    }
    text.parse::<f64>().ok().map(Json::Float)
}

/// Reads a quoted string, resolving the escapes JSON defines. `\u` is
/// decoded including surrogate pairs, which is what a Windows path or a
/// non-ASCII symbol name in a report would arrive as.
fn read_string(bytes: &[u8], cursor: &mut usize) -> Option<String> {
    if *bytes.get(*cursor)? != b'"' {
        return None;
    }
    *cursor += 1;
    let mut value = String::new();
    loop {
        match *bytes.get(*cursor)? {
            b'"' => {
                *cursor += 1;
                return Some(value);
            }
            b'\\' => {
                *cursor += 1;
                match *bytes.get(*cursor)? {
                    b'"' => value.push('"'),
                    b'\\' => value.push('\\'),
                    b'/' => value.push('/'),
                    b'b' => value.push('\u{8}'),
                    b'f' => value.push('\u{c}'),
                    b'n' => value.push('\n'),
                    b'r' => value.push('\r'),
                    b't' => value.push('\t'),
                    b'u' => {
                        *cursor += 1;
                        value.push(read_escaped_char(bytes, cursor)?);
                        // The `\u` arm consumed its own digits; skip the
                        // shared trailing bump below.
                        continue;
                    }
                    _ => return None,
                }
                *cursor += 1;
            }
            byte => {
                // Multi-byte UTF-8 passes through whole. The character's
                // length comes from its *leading byte*, so the slice handed
                // to `from_utf8` is exactly one character --- validating the
                // whole remaining buffer here instead (`bytes[cursor..]`)
                // costs O(n) per character and made a 415 KB
                // `.config-trace.json` take seconds rather than milliseconds.
                let len = utf8_len(byte);
                let text = std::str::from_utf8(bytes.get(*cursor..*cursor + len)?).ok()?;
                value.push_str(text);
                *cursor += len;
            }
        }
    }
}

/// Decodes the four hex digits of a `\u` escape, joining a surrogate pair
/// with the `\u` escape that must follow it.
fn read_escaped_char(bytes: &[u8], cursor: &mut usize) -> Option<char> {
    let first = read_hex4(bytes, cursor)?;
    if !(0xD800..0xDC00).contains(&first) {
        return char::from_u32(first);
    }
    // A high surrogate is only a character together with its low half.
    if bytes.get(*cursor) != Some(&b'\\') || bytes.get(*cursor + 1) != Some(&b'u') {
        return None;
    }
    *cursor += 2;
    let second = read_hex4(bytes, cursor)?;
    if !(0xDC00..0xE000).contains(&second) {
        return None;
    }
    char::from_u32(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00))
}

/// How many bytes the UTF-8 character starting with `byte` occupies. A
/// continuation or invalid byte answers 1, which lets `from_utf8` refuse it
/// on the next line rather than making this function fallible.
fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn read_hex4(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let text = std::str::from_utf8(bytes.get(*cursor..*cursor + 4)?).ok()?;
    let value = u32::from_str_radix(text, 16).ok()?;
    *cursor += 4;
    Some(value)
}

fn read_array(bytes: &[u8], cursor: &mut usize, depth: usize) -> Option<Json> {
    *cursor += 1; // the '['
    let mut items = Vec::new();
    skip_whitespace(bytes, cursor);
    if bytes.get(*cursor) == Some(&b']') {
        *cursor += 1;
        return Some(Json::Array(items));
    }
    loop {
        items.push(read_value(bytes, cursor, depth + 1)?);
        skip_whitespace(bytes, cursor);
        match *bytes.get(*cursor)? {
            b',' => *cursor += 1,
            b']' => {
                *cursor += 1;
                return Some(Json::Array(items));
            }
            _ => return None,
        }
    }
}

fn read_object(bytes: &[u8], cursor: &mut usize, depth: usize) -> Option<Json> {
    *cursor += 1; // the '{'
    let mut pairs = Vec::new();
    skip_whitespace(bytes, cursor);
    if bytes.get(*cursor) == Some(&b'}') {
        *cursor += 1;
        return Some(Json::Object(pairs));
    }
    loop {
        skip_whitespace(bytes, cursor);
        let key = read_string(bytes, cursor)?;
        skip_whitespace(bytes, cursor);
        if *bytes.get(*cursor)? != b':' {
            return None;
        }
        *cursor += 1;
        pairs.push((key, read_value(bytes, cursor, depth + 1)?));
        skip_whitespace(bytes, cursor);
        match *bytes.get(*cursor)? {
            b',' => *cursor += 1,
            b'}' => {
                *cursor += 1;
                return Some(Json::Object(pairs));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip() {
        assert_eq!(parse("null"), Some(Json::Null));
        assert_eq!(parse("true"), Some(Json::Bool(true)));
        assert_eq!(parse("false"), Some(Json::Bool(false)));
        assert_eq!(parse("  -12 "), Some(Json::Int(-12)));
        assert_eq!(parse("\"hi\""), Some(Json::Str("hi".into())));
    }

    /// Sizes and addresses must keep full precision: an `f64` round-trip
    /// silently rounds past 2^53, and a 64-bit address is past it.
    #[test]
    fn large_integers_keep_their_precision() {
        let value = parse("9007199254740993").expect("parses");
        assert_eq!(value.as_i64(), Some(9_007_199_254_740_993));
    }

    #[test]
    fn a_non_integral_number_still_parses() {
        assert_eq!(parse("1.5"), Some(Json::Float(1.5)));
        // ... and an integral float still answers as an integer, so a
        // generator writing `4.0` does not lose the tab a row.
        assert_eq!(parse("4.0").and_then(|v| v.as_i64()), Some(4));
    }

    #[test]
    fn escapes_are_resolved() {
        let value = parse(r#""a\"b\\c\ndéA""#).expect("parses");
        assert_eq!(value.as_str(), Some("a\"b\\c\nd\u{e9}A"));
    }

    /// A surrogate pair is one character, not two lone halves --- what a
    /// non-BMP symbol name in a generated report arrives as.
    #[test]
    fn a_surrogate_pair_becomes_one_character() {
        let value = parse(r#""😀""#).expect("parses");
        assert_eq!(value.as_str(), Some("\u{1f600}"));
    }

    #[test]
    fn a_lone_high_surrogate_is_refused_rather_than_guessed() {
        assert_eq!(parse(r#""\ud83d""#), None);
    }

    #[test]
    fn multi_byte_text_passes_through_whole() {
        let value = parse("\"caf\u{e9} \u{2014} fim\"").expect("parses");
        assert_eq!(value.as_str(), Some("caf\u{e9} \u{2014} fim"));
    }

    #[test]
    fn nested_containers_and_lookups() {
        let value = parse(
            r#"{ "symbols": { "name": "Root", "size": 12,
                              "children": [ { "name": "a", "size": 5 } ] },
                "total_size": 40 }"#,
        )
        .expect("parses");
        assert_eq!(value.get("total_size").and_then(Json::as_u64), Some(40));
        let symbols = value.get("symbols").expect("symbols");
        assert_eq!(symbols.get("name").and_then(Json::as_str), Some("Root"));
        let children = symbols
            .get("children")
            .and_then(Json::as_array)
            .expect("children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].get("size").and_then(Json::as_u64), Some(5));
    }

    /// The `.config-trace.json` shape: an array of fixed-arity arrays whose
    /// last element is either a `[path, line]` pair, a list of expression
    /// strings, or `null`.
    #[test]
    fn the_config_trace_shape_reads_back() {
        let value = parse(
            r#"[["CONFIG_A","y","bool","y","assign",["/p/.config",41]],
                ["CONFIG_B","y","bool",null,"unset",null]]"#,
        )
        .expect("parses");
        let rows = value.as_array().expect("array");
        assert_eq!(rows.len(), 2);
        let first = rows[0].as_array().expect("row");
        assert_eq!(first[0].as_str(), Some("CONFIG_A"));
        let loc = first[5].as_array().expect("loc");
        assert_eq!(loc[0].as_str(), Some("/p/.config"));
        assert_eq!(loc[1].as_u64(), Some(41));
        let second = rows[1].as_array().expect("row");
        assert!(second[3].is_null());
        assert!(second[5].is_null());
    }

    #[test]
    fn empty_containers_are_values_not_failures() {
        assert_eq!(parse("[]"), Some(Json::Array(Vec::new())));
        assert_eq!(parse("{}"), Some(Json::Object(Vec::new())));
        assert_eq!(parse(" [ ] "), Some(Json::Array(Vec::new())));
    }

    /// A build interrupted mid-write leaves a half-file on disk; the tab has
    /// to say "unreadable", not panic or invent a tree.
    #[test]
    fn truncated_and_malformed_input_yields_none() {
        for text in [
            "",
            "   ",
            "{",
            "[1, 2",
            r#"{"a": }"#,
            r#"{"a" 1}"#,
            r#"{a: 1}"#,
            "[1] trailing",
            "tru",
            r#""unterminated"#,
        ] {
            assert_eq!(parse(text), None, "expected None for {text:?}");
        }
    }

    /// Deep nesting is bounded rather than recursed into: the reader must
    /// answer `None` where a plain recursive descent would overflow.
    #[test]
    fn nesting_past_the_limit_is_refused_not_overflowed() {
        let deep = format!("{}{}", "[".repeat(4096), "]".repeat(4096));
        assert_eq!(parse(&deep), None);
    }

    #[test]
    fn accessors_answer_none_off_shape() {
        let value = parse(r#"{"a": 1}"#).expect("parses");
        assert_eq!(value.as_str(), None);
        assert_eq!(value.as_array(), None);
        assert_eq!(value.get("missing"), None);
        assert_eq!(Json::Int(-1).as_u64(), None);
        assert_eq!(Json::Str("x".into()).get("a"), None);
    }
}
