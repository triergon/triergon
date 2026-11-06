//! A TOML reader for Cargo manifests, producing the same structure as Python's
//! `tomllib` (tables in document order, integers, floats, booleans, strings and
//! arrays). Dates and times are not supported.

use crate::json::Value;

pub fn parse(text: &str) -> Result<Value, String> {
    let mut root = Value::Object(Vec::new());
    let mut current: Vec<String> = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((number, raw)) = lines.next() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let fail = |message: &str| format!("line {}: {message}", number + 1);
        if let Some(header) = line.strip_prefix("[[") {
            let path = keys(
                header
                    .strip_suffix("]]")
                    .ok_or_else(|| fail("unterminated table header"))?,
            )
            .map_err(|e| fail(&e))?;
            let (last, parent) = path
                .split_last()
                .ok_or_else(|| fail("empty table header"))?;
            let table = table_at(&mut root, parent).map_err(|e| fail(&e))?;
            let array = entry(table, last, || Value::Array(Vec::new()));
            let Value::Array(items) = array else {
                return Err(fail("not an array of tables"));
            };
            items.push(Value::Object(Vec::new()));
            current = path;
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            current = keys(
                header
                    .strip_suffix(']')
                    .ok_or_else(|| fail("unterminated table header"))?,
            )
            .map_err(|e| fail(&e))?;
            table_at(&mut root, &current).map_err(|e| fail(&e))?;
            continue;
        }
        let (key, rest) = split_key(line).map_err(|e| fail(&e))?;
        let mut value_text = rest.to_owned();
        // Arrays and strings may continue over several lines.
        while !balanced(&value_text) {
            let (_, next) = lines.next().ok_or_else(|| fail("unterminated value"))?;
            value_text.push('\n');
            value_text.push_str(strip_comment(next));
        }
        let mut parser = Inline {
            text: value_text.trim(),
            pos: 0,
        };
        let value = parser.value().map_err(|e| fail(&e))?;
        parser.space();
        if parser.pos != parser.text.len() {
            return Err(fail("trailing characters after value"));
        }
        let (last, parent) = key.split_last().expect("non-empty key");
        let mut path = current.clone();
        path.extend_from_slice(parent);
        let table = table_at(&mut root, &path).map_err(|e| fail(&e))?;
        if table.iter().any(|(k, _)| k == last) {
            return Err(fail("duplicate key"));
        }
        table.push((last.clone(), value));
    }
    Ok(root)
}

/// The members of the table at `path`, created as needed; an array of tables
/// on the path resolves to its last table.
fn table_at<'a>(
    root: &'a mut Value,
    path: &[String],
) -> Result<&'a mut Vec<(String, Value)>, String> {
    let mut node = root;
    for key in path {
        let next = match node {
            Value::Object(members) => entry(members, key, || Value::Object(Vec::new())),
            _ => return Err(format!("{key} is not a table")),
        };
        node = match next {
            Value::Array(items) => items.last_mut().ok_or("empty array of tables")?,
            other => other,
        };
    }
    match node {
        Value::Object(members) => Ok(members),
        _ => Err("not a table".into()),
    }
}

fn entry<'a>(
    members: &'a mut Vec<(String, Value)>,
    key: &str,
    new: impl FnOnce() -> Value,
) -> &'a mut Value {
    let index = if let Some(index) = members.iter().position(|(k, _)| k == key) {
        index
    } else {
        members.push((key.to_owned(), new()));
        members.len() - 1
    };
    &mut members[index].1
}

fn strip_comment(line: &str) -> &str {
    let (mut quote, mut escaped) = (None, false);
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(q), c) if c == q && !escaped => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
        escaped = false;
    }
    line
}

fn balanced(text: &str) -> bool {
    let (mut depth, mut quote, mut escaped) = (0i32, None, false);
    for c in text.chars() {
        match (quote, c) {
            (Some('"'), '\\') if !escaped => {
                escaped = true;
                continue;
            }
            (Some(q), c) if c == q && !escaped => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '[' | '{') => depth += 1,
            (None, ']' | '}') => depth -= 1,
            _ => {}
        }
        escaped = false;
    }
    depth <= 0 && quote.is_none()
}

fn split_key(line: &str) -> Result<(Vec<String>, &str), String> {
    let mut parser = Inline { text: line, pos: 0 };
    let key = parser.key()?;
    parser.space();
    if !parser.eat('=') {
        return Err("expected '='".into());
    }
    Ok((key, &line[parser.pos..]))
}

fn keys(text: &str) -> Result<Vec<String>, String> {
    let mut parser = Inline {
        text: text.trim(),
        pos: 0,
    };
    let key = parser.key()?;
    parser.space();
    if parser.pos != parser.text.len() {
        return Err("invalid table header".into());
    }
    Ok(key)
}

struct Inline<'a> {
    text: &'a str,
    pos: usize,
}

impl Inline<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn space(&mut self) {
        while self
            .peek()
            .is_some_and(|c| c == ' ' || c == '\t' || c == '\n' || c == '\r')
        {
            self.pos += 1;
        }
    }

    fn key(&mut self) -> Result<Vec<String>, String> {
        let mut parts = Vec::new();
        loop {
            self.space();
            let part = match self.peek() {
                Some('"') => self.basic()?,
                Some('\'') => self.literal()?,
                _ => {
                    let start = self.pos;
                    while self
                        .peek()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    {
                        self.pos += 1;
                    }
                    if self.pos == start {
                        return Err("expected a key".into());
                    }
                    self.text[start..self.pos].to_owned()
                }
            };
            parts.push(part);
            self.space();
            if !self.eat('.') {
                return Ok(parts);
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.space();
        match self.peek() {
            Some('"') => Ok(Value::String(self.basic()?)),
            Some('\'') => Ok(Value::String(self.literal()?)),
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.space();
                    if self.eat(']') {
                        return Ok(Value::Array(items));
                    }
                    items.push(self.value()?);
                    self.space();
                    if !self.eat(',') {
                        self.space();
                        return if self.eat(']') {
                            Ok(Value::Array(items))
                        } else {
                            Err("expected ',' or ']'".into())
                        };
                    }
                }
            }
            Some('{') => {
                self.pos += 1;
                let mut table = Value::Object(Vec::new());
                self.space();
                if self.eat('}') {
                    return Ok(table);
                }
                loop {
                    let key = self.key()?;
                    if !self.eat('=') {
                        return Err("expected '=' in inline table".into());
                    }
                    let value = self.value()?;
                    let (last, parent) = key.split_last().expect("key");
                    table_at(&mut table, parent)?.push((last.clone(), value));
                    self.space();
                    if self.eat('}') {
                        return Ok(table);
                    }
                    if !self.eat(',') {
                        return Err("expected ',' or '}'".into());
                    }
                }
            }
            _ => self.scalar(),
        }
    }

    fn scalar(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| !matches!(c, ',' | ']' | '}' | ' ' | '\t' | '\n' | '\r'))
        {
            self.pos += self.peek().map_or(1, char::len_utf8);
        }
        let word = &self.text[start..self.pos];
        match word {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "inf" | "+inf" => return Ok(Value::float(f64::INFINITY)),
            "-inf" => return Ok(Value::float(f64::NEG_INFINITY)),
            "nan" | "+nan" | "-nan" => return Ok(Value::float(f64::NAN)),
            _ => {}
        }
        let digits = word.replace('_', "");
        for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
            if let Some(rest) = digits.strip_prefix(prefix) {
                return i128::from_str_radix(rest, radix)
                    .map(Value::int)
                    .map_err(|_| format!("invalid integer {word}"));
            }
        }
        if let Ok(int) = digits.parse::<i128>() {
            return Ok(Value::int(int));
        }
        digits
            .parse::<f64>()
            .map(Value::float)
            .map_err(|_| format!("unsupported value {word}"))
    }

    fn literal(&mut self) -> Result<String, String> {
        if self.text[self.pos..].starts_with("'''") {
            self.pos += 3;
            let rest = &self.text[self.pos..];
            let end = rest.find("'''").ok_or("unterminated literal string")?;
            let body = rest[..end]
                .strip_prefix('\n')
                .unwrap_or(&rest[..end])
                .to_owned();
            self.pos += end + 3;
            return Ok(body);
        }
        self.pos += 1;
        let rest = &self.text[self.pos..];
        let end = rest.find('\'').ok_or("unterminated literal string")?;
        self.pos += end + 1;
        Ok(rest[..end].to_owned())
    }

    fn basic(&mut self) -> Result<String, String> {
        let multi = self.text[self.pos..].starts_with("\"\"\"");
        self.pos += if multi { 3 } else { 1 };
        if multi {
            self.eat('\n');
        }
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            if multi && self.text[self.pos..].starts_with("\"\"\"") {
                self.pos += 3;
                return Ok(out);
            }
            self.pos += c.len_utf8();
            match c {
                '"' if !multi => return Ok(out),
                '\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.pos += e.len_utf8();
                    match e {
                        'b' => out.push('\u{8}'),
                        't' => out.push('\t'),
                        'n' => out.push('\n'),
                        'f' => out.push('\u{c}'),
                        'r' => out.push('\r'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'u' | 'U' => {
                            let len = if e == 'u' { 4 } else { 8 };
                            let hex = self
                                .text
                                .get(self.pos..self.pos + len)
                                .ok_or("short unicode escape")?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| "invalid unicode escape")?;
                            out.push(char::from_u32(code).ok_or("invalid unicode scalar")?);
                            self.pos += len;
                        }
                        c if multi && c.is_whitespace() => {
                            while self.peek().is_some_and(char::is_whitespace) {
                                self.pos += 1;
                            }
                        }
                        _ => return Err("invalid escape".into()),
                    }
                }
                '\n' if !multi => return Err("newline in string".into()),
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::to_python;

    #[test]
    fn reads_the_workspace_manifest_like_tomllib() {
        let text = r#"
[workspace]
members = ["crates/core", # first
  "xtask",
]
resolver = "3"

[workspace.package]
edition = "2024"
publish = false
repository.workspace = true

[workspace.lints.clippy]
needless_range_loop = "allow" # comment
'quoted key' = 'lit#eral'

[profile.dev.package.xtask]
opt-level = 3
ratio = 1.5
big = 1_000
inline = { a = 1, b.c = "x" }
"#;
        let value = parse(text).unwrap();
        assert_eq!(
            to_python(&value, None),
            r#"{"workspace": {"members": ["crates/core", "xtask"], "resolver": "3", "package": {"edition": "2024", "publish": false, "repository": {"workspace": true}}, "lints": {"clippy": {"needless_range_loop": "allow", "quoted key": "lit#eral"}}}, "profile": {"dev": {"package": {"xtask": {"opt-level": 3, "ratio": 1.5, "big": 1000, "inline": {"a": 1, "b": {"c": "x"}}}}}}}"#
        );
    }

    #[test]
    fn reads_arrays_of_tables() {
        let value = parse("[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\n").unwrap();
        assert_eq!(
            to_python(&value, None),
            r#"{"bin": [{"name": "a"}, {"name": "b"}]}"#
        );
    }

    #[test]
    fn rejects_malformed_documents() {
        for text in ["[a", "a = ", "a = [1, 2", "a = 1\na = 2", "a = \"x", "= 1"] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn matches_the_repository_manifest_structure() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let value = parse(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
        assert!(
            value
                .get("workspace")
                .and_then(|w| w.get("members"))
                .is_some()
        );
    }
}
