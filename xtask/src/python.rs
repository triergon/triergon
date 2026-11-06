//! Python formatting and text semantics the measurement records were written
//! with: `str()` and `repr()` of values, `round`, path display, text-mode
//! subprocess output and `datetime.isoformat`, so the Rust runners write the
//! same bytes the earlier Python runners wrote.

use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// `json.dumps(value, sort_keys=True)`: object keys sorted at every level.
pub fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        Value::Object(members) => {
            let mut sorted: Vec<(String, Value)> = members
                .iter()
                .map(|(k, v)| (k.clone(), sort_keys(v)))
                .collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

/// `round(value, digits)` for a float.
pub fn round(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// `str(value)` of a float.
pub fn float(value: f64) -> String {
    str(&Value::float(value))
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

/// `repr()` of a list of strings.
pub fn repr_strings<S: AsRef<str>>(items: &[S]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| repr_str(s.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `datetime.now(timezone.utc).isoformat()`.
pub fn utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    isoformat(now.as_secs() as i64, now.subsec_micros())
}

fn isoformat(seconds: i64, micros: u32) -> String {
    // Days to civil date (Howard Hinnant's algorithm).
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let clock = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60
    );
    if micros == 0 {
        format!("{clock}+00:00")
    } else {
        format!("{clock}.{micros:06}+00:00")
    }
}

/// `time.time()`.
pub fn time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
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

/// `hash(n)` of an integer on a 64-bit build.
fn hash_int(n: i128) -> i64 {
    const MODULUS: i128 = (1 << 61) - 1;
    let h = (n.abs() % MODULUS) as i64;
    let h = if n < 0 { -h } else { h };
    if h == -1 { -2 } else { h }
}

/// `hash(t)` of a tuple of integers (CPython 3.8 and later, 64-bit).
pub fn hash_tuple(items: &[i128]) -> i64 {
    const PRIME_1: u64 = 11_400_714_785_074_694_791;
    const PRIME_2: u64 = 14_029_467_366_897_019_727;
    const PRIME_5: u64 = 2_870_177_450_012_600_261;
    let mut acc = PRIME_5;
    for &item in items {
        acc = acc.wrapping_add((hash_int(item) as u64).wrapping_mul(PRIME_2));
        acc = acc.rotate_left(31);
        acc = acc.wrapping_mul(PRIME_1);
    }
    acc = acc.wrapping_add(items.len() as u64 ^ (PRIME_5 ^ 0x0035_d373));
    if acc == u64::MAX {
        1_546_275_796
    } else {
        acc as i64
    }
}

/// The iteration order of a `set` built by adding `items` in order (CPython's
/// open-addressing table: linear probes of 9, perturbation shift 5, growth to
/// four times the used count once the table is three fifths full).
pub fn set_order<T: Clone + PartialEq>(items: &[T], hash: impl Fn(&T) -> i64) -> Vec<T> {
    const LINEAR_PROBES: usize = 9;
    fn insert<T>(table: &mut [Option<(i64, T)>], hash: i64, item: T) {
        let mask = table.len() - 1;
        let mut perturb = hash as u64;
        let mut i = (hash as u64 as usize) & mask;
        loop {
            let probes = if i + LINEAR_PROBES <= mask {
                LINEAR_PROBES
            } else {
                0
            };
            for j in i..=i + probes {
                if table[j].is_none() {
                    table[j] = Some((hash, item));
                    return;
                }
            }
            perturb >>= 5;
            i = (i
                .wrapping_mul(5)
                .wrapping_add(1)
                .wrapping_add(perturb as usize))
                & mask;
        }
    }
    let mut table: Vec<Option<(i64, T)>> = vec![None; 8];
    let mut used = 0usize;
    for item in items {
        let h = hash(item);
        if table.iter().flatten().any(|(eh, e)| *eh == h && e == item) {
            continue;
        }
        insert(&mut table, h, item.clone());
        used += 1;
        let mask = table.len() - 1;
        if used * 5 >= mask * 3 {
            let wanted = if used > 50_000 { used * 2 } else { used * 4 };
            let mut size = 8;
            while size <= wanted {
                size <<= 1;
            }
            let old = std::mem::replace(&mut table, vec![None; size]);
            for (h, e) in old.into_iter().flatten() {
                insert(&mut table, h, e);
            }
        }
    }
    table.into_iter().flatten().map(|(_, e)| e).collect()
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
    fn rounds_as_python() {
        // python3 -c "print(round(0.125, 2), round(1.0005, 3), round(2.675, 2), round(0.98765, 4))"
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(1.0005, 3), 1.0);
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.98765, 4), 0.9877);
        assert!(round(f64::INFINITY, 3).is_infinite());
    }

    #[test]
    fn formats_floats_as_python() {
        assert_eq!(float(1e-5), "1e-05");
    }

    #[test]
    fn reprs_as_python() {
        assert_eq!(
            repr(&crate::json::parse(r#"{"a": [1, 2.5, null, true, "it's"]}"#).unwrap()),
            r#"{'a': [1, 2.5, None, True, "it's"]}"#
        );
    }

    #[test]
    fn reprs_string_lists_as_python() {
        assert_eq!(repr_strings(&["a\\b", "c\td"]), r"['a\\b', 'c\td']");
    }

    #[test]
    fn loads_keeps_python_dict_semantics() {
        let value = loads(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(crate::json::to_python(&value, None), r#"{"a": 3, "b": 2}"#);
    }

    #[test]
    fn formats_utc_as_isoformat() {
        // python3 -c "import datetime as d; print(d.datetime.fromtimestamp(1783000000.25, d.timezone.utc).isoformat())"
        assert_eq!(
            isoformat(1_783_000_000, 250_000),
            "2026-07-02T13:46:40.250000+00:00"
        );
        assert_eq!(isoformat(0, 0), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn sets_iterate_in_cpython_order() {
        // python3 -c "print(hash((120, 5000000, 120, 5000000)))"
        assert_eq!(
            hash_tuple(&[120, 5_000_000, 120, 5_000_000]),
            -3743009442964967769
        );
        // python3 -c "print(list({(120, 5000000, 120, 5000000), (121, 5000000, 120, 5000000)}))"
        let pair = [
            [120, 5_000_000, 120, 5_000_000],
            [121, 5_000_000, 120, 5_000_000],
        ];
        assert_eq!(set_order(&pair, |t| hash_tuple(t)), [pair[1], pair[0]]);
        // python3 -c "s=set(); [s.add((i, 7, i*3, -1)) for i in range(12)]; print(list(s)[:6])"
        let items: Vec<[i128; 4]> = (0..12).map(|i| [i, 7, i * 3, -1]).collect();
        let firsts: Vec<i128> = set_order(&items, |t| hash_tuple(t))[..6]
            .iter()
            .map(|t| t[0])
            .collect();
        assert_eq!(firsts, [9, 1, 7, 8, 4, 6]);
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
