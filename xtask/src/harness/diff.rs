//! Strict typed comparison of canonical worker output: no timestamp rounding,
//! no dropped fields. An integer equals an integral float of the same value
//! (sample rates), booleans never equal numbers.

use crate::json::Value;

pub const MAXIMUM: usize = 40;

/// Up to `MAXIMUM` differences between `actual` and `expected`, in walk order.
pub fn differences(actual: &Value, expected: &Value) -> Vec<Value> {
    differences_at(actual, expected, "", MAXIMUM)
}

pub fn differences_at(actual: &Value, expected: &Value, path: &str, maximum: usize) -> Vec<Value> {
    let mut found = Vec::new();
    walk(actual, expected, path, maximum, &mut found);
    found
}

fn shown(value: &Value) -> Value {
    if python_str_len(value, true) < 256 {
        value.clone()
    } else {
        Value::str("<large value>")
    }
}

fn walk(a: &Value, b: &Value, at: &str, maximum: usize, found: &mut Vec<Value>) {
    if found.len() >= maximum {
        return;
    }
    let is_bool = |v: &Value| matches!(v, Value::Bool(_));
    match (a, b) {
        _ if is_bool(a) != is_bool(b) => {
            found.push(Value::object([
                ("path", Value::str(at)),
                ("actual", a.clone()),
                ("expected", b.clone()),
            ]));
        }
        (Value::Object(x), Value::Object(y)) => {
            let (kx, ky) = (sorted_keys(x), sorted_keys(y));
            if kx != ky {
                found.push(Value::object([
                    ("path", Value::str(at)),
                    ("actual_keys", Value::strings(kx.iter().copied())),
                    ("expected_keys", Value::strings(ky.iter().copied())),
                ]));
            }
            for key in kx.iter().filter(|k| ky.contains(k)) {
                walk(
                    a.get(key).expect("key"),
                    b.get(key).expect("key"),
                    &format!("{at}/{key}"),
                    maximum,
                    found,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                found.push(Value::object([
                    ("path", Value::str(at)),
                    ("actual_length", Value::int(x.len() as i64)),
                    ("expected_length", Value::int(y.len() as i64)),
                ]));
            }
            for (i, (left, right)) in x.iter().zip(y).enumerate() {
                walk(left, right, &format!("{at}/{i}"), maximum, found);
            }
        }
        _ if !equal(a, b) => {
            found.push(Value::object([
                ("path", Value::str(at)),
                ("actual", shown(a)),
                ("expected", shown(b)),
            ]));
        }
        _ => {}
    }
}

fn sorted_keys(members: &[(String, Value)]) -> Vec<&str> {
    let mut keys: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Python `==` on JSON values.
pub fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.equals(y),
        (Value::Object(x), Value::Object(y)) => {
            let keys = sorted_keys(x);
            keys == sorted_keys(y)
                && keys
                    .iter()
                    .all(|k| equal(a.get(k).unwrap(), b.get(k).unwrap()))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(l, r)| equal(l, r))
        }
        // Python: True == 1; only reachable outside `walk`, which separates booleans.
        (Value::Bool(x), Value::Number(n)) | (Value::Number(n), Value::Bool(x)) => {
            n.equals(&match Value::int(i64::from(*x)) {
                Value::Number(m) => m,
                _ => unreachable!(),
            })
        }
        _ => a == b,
    }
}

/// Length of Python's `str()` of the value (`top`) or of its `repr()` (nested).
pub fn python_str_len(value: &Value, top: bool) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => match n.text() {
            "Infinity" => 3,
            "-Infinity" => 4,
            "NaN" => 3,
            text => text.len(),
        },
        Value::String(s) if top => s.chars().count(),
        Value::String(s) => repr_len(s),
        Value::Array(items) => {
            2 + items
                .iter()
                .map(|v| python_str_len(v, false))
                .sum::<usize>()
                + 2 * items.len().saturating_sub(1)
        }
        Value::Object(members) => {
            let mut seen: Vec<&str> = Vec::new();
            let mut total = 2;
            for (k, _) in members.iter().rev() {
                if !seen.contains(&k.as_str()) {
                    seen.push(k);
                }
            }
            for k in &seen {
                total += repr_len(k) + 2 + python_str_len(value.get(k).unwrap(), false);
            }
            total + 2 * seen.len().saturating_sub(1)
        }
    }
}

/// Length of Python's `repr()` of a string.
fn repr_len(s: &str) -> usize {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    2 + s
        .chars()
        .map(|c| match c {
            '\\' | '\t' | '\n' | '\r' => 2,
            c if c == quote => 2,
            c if (c as u32) < 0x20 || c as u32 == 0x7f => 4,
            c if (c as u32) < 0x7f => 1,
            c if (0x80..0xa0).contains(&(c as u32)) || c == '\u{ad}' => 4,
            c if (0xd800..0xe000).contains(&(c as u32)) => 6,
            c if matches!(c as u32, 0x2028 | 0x2029 | 0xfeff | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f) => 6,
            _ => 1,
        })
        .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn v(text: &str) -> Value {
        parse(text).unwrap()
    }

    #[test]
    fn reports_typed_differences_in_walk_order() {
        let found = differences(
            &v(r#"{"a": 1, "b": [1, 2, 3], "c": true, "d": 48000}"#),
            &v(r#"{"a": 2, "b": [1, 5], "c": 1, "d": 48000.0, "e": 0}"#),
        );
        let text: Vec<String> = found
            .iter()
            .map(|d| crate::json::to_python(d, None))
            .collect();
        assert_eq!(
            text,
            [
                r#"{"path": "", "actual_keys": ["a", "b", "c", "d"], "expected_keys": ["a", "b", "c", "d", "e"]}"#,
                r#"{"path": "/a", "actual": 1, "expected": 2}"#,
                r#"{"path": "/b", "actual_length": 3, "expected_length": 2}"#,
                r#"{"path": "/b/1", "actual": 2, "expected": 5}"#,
                r#"{"path": "/c", "actual": true, "expected": 1}"#,
            ]
        );
    }

    #[test]
    fn large_values_are_elided_and_output_bounded() {
        let long = Value::str("x".repeat(300));
        let found = differences(&long, &Value::str("y"));
        assert_eq!(
            found[0].get("actual").and_then(Value::as_str),
            Some("<large value>")
        );
        assert_eq!(found[0].get("expected").and_then(Value::as_str), Some("y"));
        let many_a = Value::Array((0..100).map(Value::int).collect());
        let many_b = Value::Array((0..100).map(|i| Value::int(i + 1)).collect());
        assert_eq!(differences(&many_a, &many_b).len(), MAXIMUM);
    }

    #[test]
    fn measures_python_str_lengths() {
        // len(str({'a': [1, 2.5, None, True, "x'y"]})) == 34
        assert_eq!(
            python_str_len(&v(r#"{"a": [1, 2.5, null, true, "x'y"]}"#), true),
            34
        );
        assert_eq!(python_str_len(&v(r#""abc""#), true), 3);
        assert_eq!(python_str_len(&v(r#"["a\nb"]"#), true), 8);
    }
}
