//! Python formatting and text semantics the measurement records were written
//! with: `str()` and `repr()` of values, `round`, path display, text-mode
//! subprocess output and `datetime.isoformat`, so the Rust runners write the
//! same bytes the earlier Python runners wrote.

use std::path::{Component, Path};

use crate::json::Value;

/// `str(pathlib.Path(path))`: `.` components and repeated or trailing
/// separators removed; `.` for an empty path.
pub fn path(path: &Path) -> String {
    let mut parts = Vec::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => parts.push("..".into()),
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_string_lossy().into()),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// The last `count` characters of `text` (`text[-count:]`).
pub fn tail(text: &str, count: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(count)).collect()
}

/// Output of a `subprocess.run(..., text=True)` stream: UTF-8 with universal
/// newlines (`\r\n` and `\r` read as `\n`).
pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// `bytes.decode(errors="replace")`.
pub fn decode(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `str.splitlines()`.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let breaks = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !breaks {
            continue;
        }
        lines.push(&text[start..at]);
        start = at + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
            chars.next();
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Characters `str.strip()` removes.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// A parsed JSON document as Python's `json.loads` returns it: a repeated
/// object key keeps its first position and its last value.
pub fn loads(text: &str) -> Option<Value> {
    crate::json::parse(text).ok().map(dict)
}

fn dict(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(dict).collect()),
        Value::Object(members) => {
            let mut out: Vec<(String, Value)> = Vec::with_capacity(members.len());
            for (key, item) in members {
                let item = dict(item);
                match out.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = item,
                    None => out.push((key, item)),
                }
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// `str()` of a value `json.loads` returned.
pub fn str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// `repr()` of a value `json.loads` returned.
pub fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => match n.text() {
            "Infinity" => "inf".into(),
            "-Infinity" => "-inf".into(),
            "NaN" => "nan".into(),
            text => text.into(),
        },
        Value::String(s) => repr_str(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(k, v)| format!("{}: {}", repr_str(k), repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `repr()` of a string.
pub fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa1).contains(&(c as u32)) || c == '\u{ad}' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's `a // b` for integers.
pub fn floor_div(a: i128, b: i128) -> i128 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Python `==` between a JSON value and an integer (`2 == 2.0`, `True == 1`).
pub fn equals_int(value: &Value, number: i128) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_))
        && crate::harness::diff::equal(value, &Value::int(number))
}

/// Python's `int()` of a JSON value: integers, integral strings and
/// truncated floats.
pub fn int(value: &Value) -> Option<i128> {
    match value {
        Value::Bool(b) => Some(i128::from(*b)),
        Value::Number(n) => n.as_i128().or_else(|| {
            Some(n.as_f64())
                .filter(|f| f.is_finite())
                .map(|f| f.trunc() as i128)
        }),
        Value::String(s) => s.trim().replace('_', "").parse().ok(),
        _ => None,
    }
}

/// `max()` of a non-empty sequence of floats: the first item no later item
/// is greater than.
pub fn max(values: &[f64]) -> f64 {
    let mut best = values[0];
    for &v in &values[1..] {
        if v > best {
            best = v;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_paths_as_pathlib() {
        assert_eq!(path(Path::new("./a//b/./c/")), "a/b/c");
        assert_eq!(path(Path::new("/x/./y")), "/x/y");
        assert_eq!(path(Path::new("./")), ".");
        assert_eq!(path(Path::new("a/../b")), "a/../b");
    }

    #[test]
    fn splits_and_strips_as_python() {
        // python3 -c "print('a\r\nb\rc\x1cd\n\ne'.splitlines())"
        assert_eq!(
            splitlines("a\r\nb\rc\u{1c}d\n\ne"),
            ["a", "b", "c", "d", "", "e"]
        );
        assert_eq!(splitlines("x\n"), ["x"]);
        assert_eq!(strip("\u{1f} {a} \u{a0}"), "{a}");
        assert_eq!(text(b"a\r\nb\rc"), "a\nb\nc");
        assert_eq!(tail("h\u{e9}llo", 4), "\u{e9}llo");
    }

    #[test]
    fn reprs_as_python() {
        assert_eq!(
            repr(&crate::json::parse(r#"{"a": [1, 2.5, null, true, "it's"]}"#).unwrap()),
            r#"{'a': [1, 2.5, None, True, "it's"]}"#
        );
    }

    #[test]
    fn loads_keeps_python_dict_semantics() {
        let value = loads(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(crate::json::to_python(&value, None), r#"{"a": 3, "b": 2}"#);
    }

    #[test]
    fn integer_semantics() {
        assert_eq!(floor_div(-7, 2), -4);
        assert_eq!(floor_div(7, 2), 3);
        assert!(equals_int(&Value::float(2.0), 2));
        assert!(equals_int(&Value::Bool(false), 0));
        assert_eq!(int(&Value::str(" 48000 ")), Some(48000));
        assert_eq!(max(&[1.0, f64::NAN, 3.0]), 3.0);
        assert!(max(&[f64::NAN, 1.0]).is_nan());
    }
}
