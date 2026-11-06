//! JSON for harness data files and worker output, with Python's `json` semantics.
//!
//! Objects keep their key order. Numbers keep integer/float identity: integers
//! keep their digits, floats are held in Python's shortest `repr`, so a document
//! read and written back is byte-identical to Python's `json.loads`/`json.dumps`.
//! `NaN`, `Infinity` and `-Infinity` are accepted, as Python accepts them.

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// A JSON number as written in the source.
#[derive(Clone, Debug, PartialEq)]
pub struct Number(String);

impl Number {
    /// An integer (as opposed to a float) in Python's sense.
    pub fn is_integer(&self) -> bool {
        !self.0.contains(['.', 'e', 'E', 'N', 'I'])
    }

    pub fn as_i128(&self) -> Option<i128> {
        if self.is_integer() {
            self.0.parse().ok()
        } else {
            None
        }
    }

    /// Numeric equality as Python's `==` on ints, floats and `Decimal`s: exact
    /// decimal values, so `48000 == 48000.0` and `1.50 == 1.5`, while an integer
    /// beyond binary64 never equals a nearby float.
    pub fn equals(&self, other: &Number) -> bool {
        match (decimal(&self.0), decimal(&other.0)) {
            (Some(a), Some(b)) => a == b,
            _ => match (self.0.as_str(), other.0.as_str()) {
                ("NaN", _) | (_, "NaN") => false,
                (a, b) => a == b,
            },
        }
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    pub fn as_f64(&self) -> f64 {
        match self.0.as_str() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            text => text.parse().unwrap_or(f64::NAN),
        }
    }
}

impl Value {
    /// Member `key` of an object; `None` for other values or a missing key.
    /// A repeated key resolves to its last occurrence, as in Python.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Python truthiness: false, null, zero, and empty strings, arrays and objects
    /// are false.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64() != 0.0,
            Value::String(s) => !s.is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(members) => !members.is_empty(),
        }
    }
}

impl Value {
    pub fn int(value: impl Into<i128>) -> Value {
        Value::Number(Number(value.into().to_string()))
    }

    /// A float written as Python's `repr` writes it (`1.0`, `0.95`, `1e-05`).
    pub fn float(value: f64) -> Value {
        Value::Number(Number(python_float(value)))
    }

    pub fn str(value: impl Into<String>) -> Value {
        Value::String(value.into())
    }

    pub fn object<K: Into<String>>(members: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Object(members.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn strings<S: Into<String>>(items: impl IntoIterator<Item = S>) -> Value {
        Value::Array(items.into_iter().map(|s| Value::String(s.into())).collect())
    }
}

/// A finite JSON number as (negative, significant digits, exponent), normalised
/// so equal values compare equal; `None` for `NaN` and infinities.
fn decimal(text: &str) -> Option<(bool, String, i64)> {
    let (negative, body) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], body[at + 1..].parse::<i64>().ok()?),
        None => (body, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let mut exponent = exponent - fraction.len() as i64;
    let digits = digits.trim_start_matches('0');
    let trimmed = digits.trim_end_matches('0');
    exponent += (digits.len() - trimmed.len()) as i64;
    if trimmed.is_empty() {
        return Some((false, String::new(), 0));
    }
    Some((negative, trimmed.to_owned(), exponent))
}

/// Python's `str(Decimal(text))` for a JSON number's text.
pub fn decimal_text(text: &str) -> String {
    let (negative, body) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], body[at + 1..].parse::<i64>().unwrap_or(0)),
        None => (body, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let raw = format!("{whole}{fraction}");
    let coefficient = raw.trim_start_matches('0');
    let coefficient = if coefficient.is_empty() {
        "0"
    } else {
        coefficient
    };
    let exponent = exponent - fraction.len() as i64;
    let adjusted = exponent + coefficient.len() as i64 - 1;
    let sign = if negative { "-" } else { "" };
    let text = if exponent <= 0 && adjusted >= -6 {
        if exponent == 0 {
            coefficient.to_owned()
        } else {
            let point = coefficient.len() as i64 + exponent;
            if point > 0 {
                format!(
                    "{}.{}",
                    &coefficient[..point as usize],
                    &coefficient[point as usize..]
                )
            } else {
                format!("0.{}{coefficient}", "0".repeat((-point) as usize))
            }
        }
    } else {
        let (first, rest) = coefficient.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        format!(
            "{first}{fraction}E{}{adjusted}",
            if adjusted >= 0 { "+" } else { "" }
        )
    };
    format!("{sign}{text}")
}

/// The text Python writes back for a JSON number it read.
fn canonical_number(raw: &str) -> String {
    if !raw.contains(['.', 'e', 'E']) {
        // An integer; only "-0" changes ("0").
        return if raw.trim_start_matches('-').bytes().all(|b| b == b'0') {
            "0".into()
        } else {
            raw.into()
        };
    }
    python_float(raw.parse().unwrap_or(f64::NAN))
}

/// Python's shortest round-trip float `repr`.
fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent form");
    let exponent: i32 = exponent.parse().expect("integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let text = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        format!("{sign}{text}")
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{first}{fraction}e{exp_sign}{:02}", exponent.abs())
    }
}

/// `json.dumps(value, indent=indent)` as Python writes it, ASCII-only.
pub fn to_python(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_value(&mut out, value, indent, 0);
    out
}

/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`: the canonical
/// form whose SHA-256 identifies a document.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
            keys.sort_unstable();
            keys.dedup();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_canonical(out, value.get(key).expect("key"));
            }
            out.push('}');
        }
        other => write_value(out, other, None, 0),
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(Number(raw)) => out.push_str(raw),
        Value::String(s) => write_string(out, s),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(members) if members.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                separator(out, indent, level + 1, i == 0);
                write_value(out, item, indent, level + 1);
            }
            close(out, indent, level, ']');
        }
        Value::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                separator(out, indent, level + 1, i == 0);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item, indent, level + 1);
            }
            close(out, indent, level, '}');
        }
    }
}

fn separator(out: &mut String, indent: Option<usize>, level: usize, first: bool) {
    match indent {
        Some(width) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * level));
        }
        None if !first => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, indent: Option<usize>, level: usize, bracket: char) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * level));
    }
    out.push(bracket);
}

fn write_string(out: &mut String, s: &str) {
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
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[derive(Debug, PartialEq)]
pub struct Error {
    pub message: String,
    pub line: usize,
    pub column: usize,
    /// Character offset, as Python reports it.
    pub offset: usize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.offset == usize::MAX {
            // A protocol rule, not a syntax position.
            return f.write_str(&self.message);
        }
        write!(
            f,
            "{}: line {} column {} (char {})",
            self.message, self.line, self.column, self.offset
        )
    }
}

/// Parse one JSON document; surrounding whitespace is allowed, trailing data is not.
pub fn parse(text: &str) -> Result<Value, Error> {
    parse_with(text, false)
}

/// Strict protocol parsing: duplicate keys, `NaN` and `Infinity` and floats
/// beyond binary64 are errors, and floats keep their exact decimal text.
pub fn parse_strict(text: &str) -> Result<Value, Error> {
    parse_with(text, true)
}

fn parse_with(text: &str, strict: bool) -> Result<Value, Error> {
    let mut parser = Parser {
        text: text.as_bytes(),
        pos: 0,
        depth: 0,
        strict,
    };
    parser.whitespace();
    let value = parser.value()?;
    parser.whitespace();
    if parser.pos != parser.text.len() {
        return Err(parser.error("Extra data"));
    }
    Ok(value)
}

const MAX_DEPTH: usize = 512;

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
    depth: usize,
    strict: bool,
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> Error {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        // Python counts characters, not bytes.
        let chars = |bytes: &[u8]| bytes.iter().filter(|&&b| b & 0xc0 != 0x80).count();
        Error {
            message: message.into(),
            line,
            column: chars(&before[start..]) + 1,
            offset: chars(before),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.text[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, Error> {
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'-' | b'0'..=b'9' | b'N' | b'I') => self.number(),
            _ if self.literal("true") => Ok(Value::Bool(true)),
            _ if self.literal("false") => Ok(Value::Bool(false)),
            _ if self.literal("null") => Ok(Value::Null),
            _ => Err(self.error("Expecting value")),
        }
    }

    fn nested(&mut self, f: fn(&mut Self) -> Result<Value, Error>) -> Result<Value, Error> {
        if self.depth == MAX_DEPTH {
            return Err(self.error("Nesting too deep"));
        }
        self.depth += 1;
        let value = f(self);
        self.depth -= 1;
        value
    }

    fn object(&mut self) -> Result<Value, Error> {
        self.pos += 1;
        let mut members = Vec::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("Expecting property name enclosed in double quotes"));
            }
            let key = self.string()?;
            self.whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("Expecting ':' delimiter"));
            }
            self.pos += 1;
            self.whitespace();
            if self.strict && members.iter().any(|(k, _)| *k == key) {
                return Err(Error {
                    message: format!("duplicate JSON field: {key}"),
                    line: 0,
                    column: 0,
                    offset: usize::MAX,
                });
            }
            members.push((key, self.value()?));
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, Error> {
        self.pos += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.whitespace();
            items.push(self.value()?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.pos;
        for word in ["NaN", "Infinity", "-Infinity"] {
            if self.literal(word) {
                if self.strict {
                    return Err(Error {
                        message: format!("non-finite JSON constant: {word}"),
                        line: 0,
                        column: 0,
                        offset: usize::MAX,
                    });
                }
                return Ok(Value::Number(Number(word.to_owned())));
            }
        }
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => {
                self.pos = start;
                return Err(self.error("Expecting value"));
            }
        }
        if self.peek() == Some(b'.') && self.text.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            self.pos += 1;
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mark = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.digits();
            } else {
                self.pos = mark;
            }
        }
        let raw = std::str::from_utf8(&self.text[start..self.pos]).expect("ASCII number");
        if self.strict {
            let float = raw.contains(['.', 'e', 'E']);
            if float && !raw.parse::<f64>().is_ok_and(f64::is_finite) {
                return Err(Error {
                    message: "JSON number exceeds the finite binary64 protocol range".into(),
                    line: 0,
                    column: 0,
                    offset: usize::MAX,
                });
            }
            return Ok(Value::Number(Number(raw.to_owned())));
        }
        Ok(Value::Number(Number(canonical_number(raw))))
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Result<String, Error> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let run = self.pos;
            while self
                .peek()
                .is_some_and(|b| b != b'"' && b != b'\\' && b >= 0x20)
            {
                self.pos += 1;
            }
            // The input is a &str, so any run between ASCII delimiters is valid UTF-8.
            out.push_str(std::str::from_utf8(&self.text[run..self.pos]).expect("UTF-8 run"));
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.error("Invalid control character")),
                None => return Err(self.error("Unterminated string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), Error> {
        let c = match self.peek() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                self.pos += 1;
                let first = self.hex4()?;
                let code = if (0xd800..0xdc00).contains(&first)
                    && self.text[self.pos..].starts_with(b"\\u")
                {
                    let mark = self.pos;
                    self.pos += 2;
                    let second = self.hex4()?;
                    if (0xdc00..0xe000).contains(&second) {
                        0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                    } else {
                        self.pos = mark;
                        first
                    }
                } else {
                    first
                };
                // Lone surrogates cannot be held in a Rust string.
                out.push(char::from_u32(code).ok_or_else(|| self.error("Unpaired surrogate"))?);
                return Ok(());
            }
            _ => return Err(self.error("Invalid \\escape")),
        };
        self.pos += 1;
        out.push(c);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("Invalid \\uXXXX escape"))?;
        let mut value = 0;
        for &d in digits {
            let digit = char::from(d)
                .to_digit(16)
                .ok_or_else(|| self.error("Invalid \\uXXXX escape"))?;
            value = value * 16 + digit;
        }
        self.pos += 4;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(raw: &str) -> Value {
        Value::Number(Number(raw.to_owned()))
    }

    #[test]
    fn parses_nested_documents_in_order() {
        let v = parse(r#" {"b": [1, -2.5e3, true, null], "a": {"x": "y"}} "#).unwrap();
        let Value::Object(members) = &v else { panic!() };
        assert_eq!(members[0].0, "b");
        assert_eq!(
            v.get("b").unwrap(),
            &Value::Array(vec![
                num("1"),
                num("-2500.0"),
                Value::Bool(true),
                Value::Null
            ])
        );
        assert_eq!(v.get("a").unwrap().get("x").unwrap().as_str(), Some("y"));
        assert_eq!(
            v.get("b").unwrap().as_array().unwrap()[1].as_f64(),
            Some(-2500.0)
        );
    }

    #[test]
    fn numbers_read_back_as_python_writes_them() {
        for (text, back) in [
            ("-0", "0"),
            ("1.50", "1.5"),
            ("1E3", "1000.0"),
            ("1e400", "Infinity"),
            ("12345678901234567890123", "12345678901234567890123"),
            ("0.1e-7", "1e-08"),
        ] {
            assert_eq!(to_python(&parse(text).unwrap(), None), back, "{text}");
        }
        let n = |t: &str| match parse(t).unwrap() {
            Value::Number(n) => n,
            _ => unreachable!(),
        };
        assert!(n("48000").equals(&n("48000.0")));
        assert!(!n("9007199254740993").equals(&n("9007199254740992.0")));
        assert!(!n("1").equals(&n("1.5")));
    }

    #[test]
    fn decimals_print_and_compare_like_python_decimal() {
        for (text, shown) in [
            ("1.50", "1.50"),
            ("0.05", "0.05"),
            ("1e3", "1E+3"),
            ("-0.0", "-0.0"),
            ("1.0", "1.0"),
            ("0.0000001", "1E-7"),
            ("123.456e2", "12345.6"),
            ("0", "0"),
            ("-12", "-12"),
        ] {
            assert_eq!(decimal_text(text), shown, "{text}");
        }
        let n = |t: &str| match parse_strict(t).unwrap() {
            Value::Number(n) => n,
            _ => unreachable!(),
        };
        assert!(
            n("1.50").equals(&n("1.5")) && n("0").equals(&n("-0.0")) && n("1e3").equals(&n("1000"))
        );
        assert!(!n("0.1000000000000000055511151231257827").equals(&n("0.1")));
        assert!(
            parse_strict(r#"{"a": 1, "a": 2}"#).is_err()
                && parse_strict("NaN").is_err()
                && parse_strict("1e400").is_err()
        );
        assert_eq!(
            parse_strict("[1.50]").unwrap(),
            Value::Array(vec![Value::Number(Number("1.50".into()))])
        );
    }

    #[test]
    fn repeated_keys_resolve_to_the_last() {
        assert_eq!(
            parse(r#"{"a": 1, "a": 2}"#).unwrap().get("a"),
            Some(&num("2"))
        );
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        let v = parse(r#""a\"\\\/\b\f\n\r\t\u00e9\ud83d\ude00""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"\\/\u{8}\u{c}\n\r\t\u{e9}\u{1f600}"));
    }

    #[test]
    fn accepts_python_special_numbers() {
        assert!(parse("NaN").unwrap().as_f64().unwrap().is_nan());
        assert_eq!(
            parse("-Infinity").unwrap().as_f64(),
            Some(f64::NEG_INFINITY)
        );
    }

    #[test]
    fn rejects_malformed_documents() {
        for text in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "{1: 2}",
            "01",
            "\"\t\"",
            "\"\\x\"",
            "[1] 2",
            "tru",
            "-",
            "\"\\ud800\"",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
        assert_eq!(
            parse("[1,\n x]").unwrap_err(),
            Error {
                message: "Expecting value".into(),
                line: 2,
                column: 2,
                offset: 5
            }
        );
    }

    #[test]
    fn bounds_nesting_depth() {
        assert!(parse(&"[".repeat(MAX_DEPTH + 1)).is_err());
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn truthiness_follows_python() {
        for (text, truth) in [
            ("0", false),
            ("0.0", false),
            ("1", true),
            ("\"\"", false),
            ("[]", false),
            ("{}", false),
            ("null", false),
            ("true", true),
        ] {
            assert_eq!(parse(text).unwrap().truthy(), truth, "{text}");
        }
    }

    #[test]
    fn canonical_form_sorts_keys_compactly() {
        let v = parse(r#"{"b": [1, {"d": 2, "c": "\u00e9"}], "a": null}"#).unwrap();
        assert_eq!(canonical(&v), r#"{"a":null,"b":[1,{"c":"\u00e9","d":2}]}"#);
    }

    #[test]
    fn writes_floats_like_python_repr() {
        for (value, text) in [
            (1.2, "1.2"),
            (1.0, "1.0"),
            (0.95, "0.95"),
            (0.8, "0.8"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.0, "123456789012345.0"),
            (-0.5, "-0.5"),
            (0.0, "0.0"),
            (2.5e-300, "2.5e-300"),
            (f64::INFINITY, "Infinity"),
        ] {
            assert_eq!(python_float(value), text);
        }
    }

    #[test]
    fn writes_documents_like_python_json_dumps() {
        let v = Value::object([
            ("a", Value::int(1)),
            ("b", Value::Array(vec![])),
            ("c", Value::object(Vec::<(String, Value)>::new())),
            ("d", Value::strings(["x\u{e9}\u{1f600}\"\n\u{1}"])),
            (
                "e",
                Value::Array(vec![Value::Null, Value::Bool(true), Value::float(0.5)]),
            ),
        ]);
        assert_eq!(
            to_python(&v, Some(2)),
            "{\n  \"a\": 1,\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    \"x\\u00e9\\ud83d\\ude00\\\"\\n\\u0001\"\n  ],\n  \"e\": [\n    null,\n    true,\n    0.5\n  ]\n}"
        );
        assert_eq!(
            to_python(&v, None),
            "{\"a\": 1, \"b\": [], \"c\": {}, \"d\": [\"x\\u00e9\\ud83d\\ude00\\\"\\n\\u0001\"], \"e\": [null, true, 0.5]}"
        );
        assert_eq!(parse(&to_python(&v, Some(2))).unwrap(), v);
    }
}
