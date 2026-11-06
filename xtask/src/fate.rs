//! A lexical inventory of FFmpeg's FATE definitions: case identities, their
//! definition lines, command hashes and literal sample references. Make is never
//! executed and no Make function, generated target or conditional is evaluated,
//! so an unresolved path can never become an acquired input or an executed test.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::json::Value;
use crate::sha256;

fn case_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Length of a case name (`fate-[A-Za-z0-9][A-Za-z0-9_.-]*`) at the start of `text`.
fn case_at(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if !text.starts_with("fate-") || !bytes.get(5).is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    Some(6 + bytes[6..].iter().take_while(|&&c| case_char(c)).count())
}

/// Every case name that starts at a word boundary, left to right.
fn case_names(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = line[from..].find("fate-") {
        let at = from + found;
        let boundary = !line[..at].chars().next_back().is_some_and(word_char);
        match case_at(&line[at..]).filter(|_| boundary) {
            Some(len) => {
                out.push(&line[at..at + len]);
                from = at + len;
            }
            None => from = at + 1,
        }
    }
    out
}

/// `NAME : CMD|SRC = VALUE` at the start of a line.
fn assignment(line: &str) -> Option<(&str, &'static str, &str)> {
    let len = case_at(line)?;
    let rest = line[len..].trim_start_matches(is_space).strip_prefix(':')?;
    let rest = rest.trim_start_matches(is_space);
    let (key, rest) = if let Some(r) = rest.strip_prefix("CMD") {
        ("CMD", r)
    } else {
        ("SRC", rest.strip_prefix("SRC")?)
    };
    let value = rest
        .trim_start_matches(is_space)
        .strip_prefix('=')?
        .trim_start_matches(is_space);
    Some((&line[..len], key, value))
}

fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Literal paths after `$(TARGET_SAMPLES)/`, up to whitespace or a quote.
fn samples(text: &str) -> Vec<&str> {
    const PREFIX: &str = "$(TARGET_SAMPLES)/";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(PREFIX) {
        let start = from + found + PREFIX.len();
        let len = text[start..]
            .find(|c: char| is_space(c) || c == '"' || c == '\'')
            .unwrap_or(text.len() - start);
        if len > 0 {
            out.push(&text[start..start + len]);
            from = start + len;
        } else {
            from += found + 1;
        }
    }
    out
}

/// Python's `str.splitlines()` boundaries.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' if chars.peek().is_some_and(|&(_, n)| n == '\n') => {
                chars.next();
                i + 2
            }
            '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}'
            | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        lines.push(&text[start..i]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

#[derive(Default)]
struct Case {
    definition_files: Vec<String>,
    assignments: Vec<(&'static str, String)>,
}

fn push_unique(list: &mut Vec<Value>, item: &str) {
    if !list.iter().any(|v| v.as_str() == Some(item)) {
        list.push(Value::str(item));
    }
}

pub fn inventory(source: &Path) -> Result<Value, String> {
    let dir = source.join("tests/fate");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "mak"))
        .collect();
    files.sort();
    let mut cases: BTreeMap<String, Case> = BTreeMap::new();
    let mut definitions = Vec::new();
    for path in files {
        let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let relative = format!(
            "tests/fate/{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        definitions.push((relative.clone(), Value::str(sha256::hex(&data))));
        let text = String::from_utf8(data).map_err(|e| format!("{relative}: {e}"))?;
        for (number, line) in split_lines(&text).into_iter().enumerate() {
            if line.trim_start_matches(is_space).starts_with('#') {
                continue;
            }
            for name in case_names(line) {
                let location = format!("{relative}:{}", number + 1);
                let case = cases.entry(name.to_owned()).or_default();
                if !case.definition_files.contains(&location) {
                    case.definition_files.push(location);
                }
            }
        }
        for line in split_lines(&text.replace("\\\n", " ")) {
            if let Some((name, key, value)) = assignment(line)
                && let Some(case) = cases.get_mut(name)
            {
                case.assignments.push((key, value.to_owned()));
            }
        }
    }
    let mut rows = Vec::new();
    for (name, case) in cases {
        let (mut commands, mut resolved, mut unresolved) = (Vec::new(), Vec::new(), Vec::new());
        let sources: Vec<&str> = case
            .assignments
            .iter()
            .filter(|(k, _)| *k == "SRC")
            .map(|(_, v)| v.as_str())
            .collect();
        for (key, value) in &case.assignments {
            let texts: Vec<String> = if *key == "CMD" {
                commands.push(Value::str(sha256::hex(value.as_bytes())));
                if sources.is_empty() {
                    vec![value.clone()]
                } else {
                    sources.iter().map(|s| value.replace("$(SRC)", s)).collect()
                }
            } else {
                vec![value.clone()]
            };
            for text in &texts {
                for sample in samples(text) {
                    push_unique(
                        if sample.contains('$') {
                            &mut unresolved
                        } else {
                            &mut resolved
                        },
                        sample,
                    );
                }
            }
        }
        rows.push(Value::object([
            ("id", Value::str(name)),
            ("definition_files", Value::strings(case.definition_files)),
            ("command_sha256", Value::Array(commands)),
            ("sample_references", Value::Array(resolved)),
            ("unresolved_sample_references", Value::Array(unresolved)),
            ("status", Value::str("unmapped")),
            ("rights", Value::str("unreviewed")),
        ]));
    }
    Ok(Value::object([
        ("schema_version", Value::int(1)),
        (
            "status",
            Value::str("identity inventory only; not the configure-expanded runnable FATE suite"),
        ),
        (
            "source_root_role",
            Value::str("externally cached FFmpeg snapshot"),
        ),
        ("definition_sha256", Value::object(definitions)),
        ("cases", Value::Array(rows)),
    ]))
}

/// Mark which identities a configured reference enables; nothing is executed.
pub fn annotate(report: &mut Value, configured_list: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(configured_list).map_err(|e| e.to_string())?;
    let enabled: std::collections::BTreeSet<&str> =
        text.split(is_space).filter(|s| !s.is_empty()).collect();
    let Value::Object(members) = report else {
        return Err("inventory is not an object".into());
    };
    for (key, value) in members.iter_mut() {
        if let (true, Value::Array(rows)) = (key == "cases", value) {
            for row in rows {
                let id = row
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Value::Object(fields) = row {
                    fields.push((
                        "configured".into(),
                        Value::Bool(enabled.contains(id.as_str())),
                    ));
                }
            }
        }
    }
    members.push((
        "configured_list".into(),
        Value::object([
            ("sha256", Value::str(sha256::hex(configured_list))),
            ("identities", Value::int(enabled.len() as i64)),
            ("executed", Value::Bool(false)),
        ]),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_of(definition: &str) -> Value {
        let root = crate::walk::scratch("fate");
        fs::create_dir_all(root.join("tests/fate")).unwrap();
        fs::write(root.join("tests/fate/authored.mak"), definition).unwrap();
        let report = inventory(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
        report
    }

    fn only_case(report: &Value) -> &Value {
        let cases = report.get("cases").and_then(Value::as_array).unwrap();
        assert_eq!(cases.len(), 1);
        &cases[0]
    }

    fn strings(value: &Value, key: &str) -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn case_local_source_variable_resolves_the_required_external_sample() {
        let report = inventory_of(
            "fate-transport-change: SRC = $(TARGET_SAMPLES)/transport/change.ts\n\
             fate-transport-change: CMD = probe -i \"$(SRC)\"\n",
        );
        let case = only_case(&report);
        assert_eq!(strings(case, "sample_references"), ["transport/change.ts"]);
        assert_eq!(case.get("status").and_then(Value::as_str), Some("unmapped"));
        assert_eq!(
            case.get("rights").and_then(Value::as_str),
            Some("unreviewed")
        );
        assert_eq!(
            strings(case, "definition_files"),
            ["tests/fate/authored.mak:1", "tests/fate/authored.mak:2"]
        );
    }

    #[test]
    fn continued_commands_keep_all_literal_samples_once() {
        let report = inventory_of(
            "fate-two-inputs: CMD = compare $(TARGET_SAMPLES)/a/input.mov \\\n \
             $(TARGET_SAMPLES)/b/input.ts $(TARGET_SAMPLES)/a/input.mov\n",
        );
        assert_eq!(
            strings(only_case(&report), "sample_references"),
            ["a/input.mov", "b/input.ts"]
        );
    }

    #[test]
    fn dynamic_sample_names_stay_unresolved_without_executing_make() {
        let report = inventory_of(
            "fate-dynamic: SRC = $(TARGET_SAMPLES)/folder/$(UNEXPANDED).ts\n\
             fate-dynamic: CMD = probe -i \"$(SRC)\" $(shell exit 99)\n",
        );
        let case = only_case(&report);
        assert!(strings(case, "sample_references").is_empty());
        assert_eq!(
            strings(case, "unresolved_sample_references"),
            ["folder/$(UNEXPANDED).ts"]
        );
    }

    #[test]
    fn names_need_a_word_boundary_and_comments_define_nothing() {
        assert_eq!(
            case_names("x fate-a.b-c,fate-d xfate-e (fate-f) fate-"),
            ["fate-a.b-c", "fate-d", "fate-f"]
        );
        let report = inventory_of("# fate-commented: CMD = x\nFATE += fate-real\n");
        assert_eq!(
            only_case(&report).get("id").and_then(Value::as_str),
            Some("fate-real")
        );
    }

    #[test]
    fn annotates_configured_identities() {
        let mut report = inventory_of("FATE += fate-a fate-b\n");
        annotate(&mut report, b"fate-b\nfate-c\n").unwrap();
        let cases = report.get("cases").and_then(Value::as_array).unwrap();
        assert_eq!(cases[0].get("configured"), Some(&Value::Bool(false)));
        assert_eq!(cases[1].get("configured"), Some(&Value::Bool(true)));
        assert_eq!(
            report
                .get("configured_list")
                .and_then(|c| c.get("identities")),
            Some(&Value::int(2))
        );
    }

    #[test]
    fn splits_lines_like_python() {
        assert_eq!(
            split_lines("a\r\nb\rc\nd\u{c}e\n"),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(split_lines("a\n\nb"), ["a", "", "b"]);
    }
}
