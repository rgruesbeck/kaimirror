//! Just enough JSON for the debugging protocol.
//!
//! Hand-rolled for the same reason the argument parser is: the protocol needs
//! to *read* a handful of fields out of replies and *write* one string into a
//! request, and that is a page of code against a dependency this workspace
//! otherwise does not have -- one that would also have to cross-compile.
//!
//! Reading is total: anything malformed is `None`, and a field that is
//! missing or the wrong shape reads as absent rather than panicking.  The
//! protocol is a moving target across Gecko versions, so a reply that does
//! not look the way this expects should produce a diagnosis, not a crash.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// A field of an object, or None for anything else.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A string field, empty when it is missing or is not a string.
    pub fn text(&self, key: &str) -> &str {
        self.get(key).and_then(Json::str).unwrap_or("")
    }

    /// The elements of an array field, empty when there is no such array.
    pub fn list(&self, key: &str) -> &[Json] {
        match self.get(key) {
            Some(Json::Arr(items)) => items,
            _ => &[],
        }
    }

    pub fn is_true(&self, key: &str) -> bool {
        matches!(self.get(key), Some(Json::Bool(true)))
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(Json::Num(n)) => Some(*n),
            _ => None,
        }
    }
}

/// Render a Rust string as a JSON string literal, escapes and all.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn parse(input: &str) -> Option<Json> {
    let bytes = input.as_bytes();
    let mut at = 0;
    let value = value(bytes, &mut at)?;
    skip_space(bytes, &mut at);
    // Trailing garbage means we did not understand the document, and reading
    // half of it is worse than reading none.
    if at == bytes.len() { Some(value) } else { None }
}

fn skip_space(b: &[u8], at: &mut usize) {
    while *at < b.len() && matches!(b[*at], b' ' | b'\t' | b'\n' | b'\r') {
        *at += 1;
    }
}

fn literal(b: &[u8], at: &mut usize, word: &str) -> Option<()> {
    if b[*at..].starts_with(word.as_bytes()) {
        *at += word.len();
        Some(())
    } else {
        None
    }
}

fn value(b: &[u8], at: &mut usize) -> Option<Json> {
    skip_space(b, at);
    match *b.get(*at)? {
        b'{' => object(b, at),
        b'[' => array(b, at),
        b'"' => string(b, at).map(Json::Str),
        b't' => literal(b, at, "true").map(|_| Json::Bool(true)),
        b'f' => literal(b, at, "false").map(|_| Json::Bool(false)),
        b'n' => literal(b, at, "null").map(|_| Json::Null),
        _ => number(b, at),
    }
}

fn object(b: &[u8], at: &mut usize) -> Option<Json> {
    *at += 1; // '{'
    let mut fields = Vec::new();
    skip_space(b, at);
    if *b.get(*at)? == b'}' {
        *at += 1;
        return Some(Json::Obj(fields));
    }
    loop {
        skip_space(b, at);
        let key = string(b, at)?;
        skip_space(b, at);
        if *b.get(*at)? != b':' {
            return None;
        }
        *at += 1;
        fields.push((key, value(b, at)?));
        skip_space(b, at);
        match *b.get(*at)? {
            b',' => *at += 1,
            b'}' => {
                *at += 1;
                return Some(Json::Obj(fields));
            }
            _ => return None,
        }
    }
}

fn array(b: &[u8], at: &mut usize) -> Option<Json> {
    *at += 1; // '['
    let mut items = Vec::new();
    skip_space(b, at);
    if *b.get(*at)? == b']' {
        *at += 1;
        return Some(Json::Arr(items));
    }
    loop {
        items.push(value(b, at)?);
        skip_space(b, at);
        match *b.get(*at)? {
            b',' => *at += 1,
            b']' => {
                *at += 1;
                return Some(Json::Arr(items));
            }
            _ => return None,
        }
    }
}

fn string(b: &[u8], at: &mut usize) -> Option<String> {
    if *b.get(*at)? != b'"' {
        return None;
    }
    *at += 1;
    let mut out = String::new();
    loop {
        let c = *b.get(*at)?;
        *at += 1;
        match c {
            b'"' => return Some(out),
            b'\\' => {
                let esc = *b.get(*at)?;
                *at += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => out.push(unicode(b, at)?),
                    _ => return None,
                }
            }
            // Anything else is a literal character: one byte of ASCII, or a
            // whole multi-byte sequence copied across in one piece.
            c => {
                let start = *at - 1;
                let len = if c < 0x80 { 1 } else { utf8_len(c)? };
                out.push_str(std::str::from_utf8(b.get(start..start + len)?).ok()?);
                *at = start + len;
            }
        }
    }
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

/// A `\uXXXX` escape, joining a surrogate pair when one follows.
fn unicode(b: &[u8], at: &mut usize) -> Option<char> {
    let hex = |at: &mut usize| -> Option<u32> {
        let text = std::str::from_utf8(b.get(*at..*at + 4)?).ok()?;
        *at += 4;
        u32::from_str_radix(text, 16).ok()
    };
    let first = hex(at)?;
    if (0xd800..0xdc00).contains(&first) {
        if b.get(*at) == Some(&b'\\') && b.get(*at + 1) == Some(&b'u') {
            *at += 2;
            let second = hex(at)?;
            let combined = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
            return char::from_u32(combined);
        }
        return Some('\u{fffd}');
    }
    char::from_u32(first)
}

fn number(b: &[u8], at: &mut usize) -> Option<Json> {
    let start = *at;
    while *at < b.len() && matches!(b[*at], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        *at += 1;
    }
    std::str::from_utf8(&b[start..*at]).ok()?.parse().ok().map(Json::Num)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Json {
        parse(s).expect("should parse")
    }

    #[test]
    fn reads_the_shapes_the_protocol_sends() {
        let pkt = p(r#"{"from":"root","tabs":[{"actor":"a1","url":"http://x/","selected":false}],
                        "count":2,"nothing":null}"#);
        assert_eq!(pkt.text("from"), "root");
        assert_eq!(pkt.num("count"), Some(2.0));
        assert_eq!(pkt.list("tabs").len(), 1);
        assert_eq!(pkt.list("tabs")[0].text("actor"), "a1");
        assert!(!pkt.list("tabs")[0].is_true("selected"));
        assert_eq!(pkt.get("nothing"), Some(&Json::Null));
    }

    #[test]
    fn missing_and_mistyped_fields_read_as_absent() {
        let pkt = p(r#"{"a":1}"#);
        assert_eq!(pkt.text("b"), "");
        assert_eq!(pkt.list("b"), &[]);
        assert_eq!(pkt.text("a"), "");        // present, but not a string
        assert!(!pkt.is_true("a"));
        assert_eq!(Json::Num(1.0).get("a"), None);
    }

    #[test]
    fn escapes_survive_a_round_trip() {
        // The app URLs on the phone carry percent-encoded JSON in their hash,
        // and a snapshot comes back full of newlines and quotes.
        let text = "line\n\"quoted\" \\ tab\tunicode \u{2713} emoji \u{1f600}";
        let doc = format!(r#"{{"text":{}}}"#, quote(text));
        assert_eq!(p(&doc).text("text"), text);
        assert_eq!(p(r#"{"s":"✓ 😀 \/"}"#).text("s"), "\u{2713} \u{1f600} /");
    }

    #[test]
    fn malformed_input_is_none_rather_than_a_panic() {
        for bad in ["", "{", "{\"a\"}", "[1,]", "{\"a\":1}trailing", "\"unterminated"] {
            assert_eq!(parse(bad), None, "{bad:?} should not parse");
        }
    }
}
