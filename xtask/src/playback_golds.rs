//! `cargo xtask playback-golds DIR [--check]`: convert the archived FFprobe
//! captures of the MP4 terminal-media inputs into the playback test records.
//!
//! DIR is the unpacked evidence set [`SET`] (`cargo xtask evidence fetch`).
//! Every capture's stdout and stderr are checked against the SHA-256 its
//! capture recorded, every input against the repository fixture, and every
//! packet field against FFprobe's packet object. The command writes the Rust
//! records and a manifest of the hashes it checked; `--check` compares them
//! with the committed files instead.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use crate::json::{self, Value};
use crate::sha256;

/// The evidence set that holds the captures.
pub const SET: &str = "playback-terminal-matrix-ffprobe";

const FIXTURES: &str = "crates/playback/tests/fixtures";
const GOLDS: &str = "crates/playback/tests/fixtures/native_terminal_golds.rs";
const MANIFEST: &str = "crates/playback/tests/fixtures/terminal-matrix/captures.json";

/// The capture directories under the set's one top-level directory, in record
/// order. The matrix also reads the inputs of the first one from a seekable
/// source.
const CAPTURES: [(&str, bool); 3] = [
    ("actual/oracle-original-v1", true),
    ("actual/oracle-empty-v1", false),
    ("actual/oracle-empty-middle-v1", false),
];

/// FFprobe's arguments before the input.
const ARGUMENTS: [&str; 9] = [
    "-fflags",
    "+noparse+nofillin",
    "-show_streams",
    "-show_packets",
    "-show_format",
    "-show_data",
    "-of",
    "json",
    "-i",
];

/// FFprobe's packet keys, in its order, and whether a packet may omit each.
const KEYS: [(&str, bool); 12] = [
    ("codec_type", false),
    ("stream_index", false),
    ("pts", true),
    ("pts_time", true),
    ("dts", true),
    ("dts_time", true),
    ("duration", true),
    ("duration_time", true),
    ("size", false),
    ("pos", true),
    ("flags", false),
    ("data", false),
];

const PIPE: &str = "literal-nonseekable-pipe";
const FILE: &str = "literal-file";

type R<T> = Result<T, String>;

pub fn main(root: &Path, args: &[String]) -> R<String> {
    let (dir, check) = match args {
        [dir] => (Path::new(dir), false),
        [dir, flag] if flag == "--check" => (Path::new(dir), true),
        _ => return Err("usage: cargo xtask playback-golds DIR [--check]".into()),
    };
    let (golds, manifest) = generate(root, dir)?;
    let outputs = [(GOLDS, golds), (MANIFEST, manifest)];
    if check {
        for (path, data) in &outputs {
            let committed =
                fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
            if &committed != data {
                return Err(format!(
                    "{path} differs from the conversion of {}",
                    dir.display()
                ));
            }
        }
        return Ok(format!("{GOLDS} and {MANIFEST} match {SET}"));
    }
    for (path, data) in &outputs {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("file has a parent")).map_err(|e| e.to_string())?;
        fs::write(&path, data).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(format!("wrote {GOLDS} and {MANIFEST} from {SET}"))
}

fn read(path: &Path) -> R<Vec<u8>> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn text<'a>(value: &'a Value, key: &str) -> R<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn integer(value: &Value, key: &str) -> R<i128> {
    match value.get(key) {
        Some(Value::Number(n)) if n.is_integer() => n.as_i128().ok_or_else(|| key.to_owned()),
        _ => Err(format!("missing integer {key}")),
    }
}

/// What FFprobe printed for one read.
enum Probe {
    Packets(Vec<Packet>),
    InvalidHeader,
}

#[derive(PartialEq)]
struct Packet(Vec<(String, Field)>);

#[derive(PartialEq)]
enum Field {
    Text(String),
    Integer(i128),
    Data(Vec<u8>),
}

struct Read {
    probe: Probe,
    hashes: Value,
}

/// An input's name, pipe read, and repository path with file read, as
/// collected from its capture rows.
type Reads = (String, Option<Read>, Option<(String, Read)>);

/// One input with its pipe and file reads.
struct Case {
    name: String,
    input: String,
    pipe: Read,
    file: Read,
    seekable: bool,
}

fn generate(root: &Path, dir: &Path) -> R<(String, String)> {
    let index = crate::checks::read_json(&root.join(crate::evidence::INDEX))?;
    let set = index
        .get("sets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .find(|set| set.get("name").and_then(Value::as_str) == Some(SET))
        .ok_or_else(|| format!("{SET} is not in {}", crate::evidence::INDEX))?;
    let base = top_level(dir)?;
    let mut cases: Vec<Case> = Vec::new();
    let mut captures = Vec::new();
    let mut reference = None;
    for (directory, seekable) in CAPTURES {
        let at = base.join(directory);
        let rows_data = read(&at.join("ROWS.json"))?;
        let terminal = json::parse(&String::from_utf8_lossy(&read(&at.join("TERMINAL.json"))?))
            .map_err(|e| format!("{directory}/TERMINAL.json: {e}"))?;
        if terminal.get("complete") != Some(&Value::Bool(true)) {
            return Err(format!("{directory}: capture incomplete"));
        }
        let ffprobe = text(&terminal, "oracle_sha256")?.to_owned();
        if reference.get_or_insert_with(|| ffprobe.clone()) != &ffprobe {
            return Err(format!("{directory}: a different FFprobe binary"));
        }
        let rows = json::parse(&String::from_utf8_lossy(&rows_data))
            .map_err(|e| format!("{directory}/ROWS.json: {e}"))?;
        let rows = rows.as_array().ok_or("ROWS.json is not a list")?;
        // Each input has one pipe and one file read, in either order.
        let mut reads: Vec<Reads> = Vec::new();
        for row in rows {
            let name = text(row, "id")?;
            let mode = text(row, "mode")?;
            let observed =
                capture(&at, row).map_err(|e| format!("{directory}: {name} {mode}: {e}"))?;
            let argv: Vec<_> = row
                .get("argv")
                .and_then(Value::as_array)
                .ok_or("argv")?
                .iter()
                .map(|a| a.as_str().unwrap_or_default())
                .collect();
            let [_, arguments @ .., path] = argv.as_slice() else {
                return Err(format!("{name}: no FFprobe arguments"));
            };
            if arguments != ARGUMENTS {
                return Err(format!("{name}: unexpected FFprobe arguments"));
            }
            let sha = text(row, "input_sha256")?;
            let observed = with_input(observed, sha);
            let at = reads
                .iter()
                .position(|(n, _, _)| n == name)
                .unwrap_or_else(|| {
                    reads.push((name.to_owned(), None, None));
                    reads.len() - 1
                });
            let entry = &mut reads[at];
            match mode {
                PIPE if *path == "pipe:0" && entry.1.is_none() => entry.1 = Some(observed),
                FILE if entry.2.is_none() => {
                    let input = path
                        .split_once("/crates/playback/tests/fixtures/")
                        .ok_or_else(|| format!("{name}: input outside the fixtures"))?
                        .1;
                    let data = read(&root.join(FIXTURES).join(input))?;
                    if sha256::hex(&data) != sha {
                        return Err(format!("{input}: SHA-256 differs from the capture"));
                    }
                    entry.2 = Some((input.to_owned(), observed));
                }
                _ => return Err(format!("{name}: unexpected read {mode} of {path}")),
            }
        }
        for (name, pipe, file) in reads {
            let (Some(pipe), Some((input, file))) = (pipe, file) else {
                return Err(format!("{name}: missing a pipe or file read"));
            };
            if pipe.hashes.get("input_sha256") != file.hashes.get("input_sha256") {
                return Err(format!("{name}: pipe and file read different inputs"));
            }
            cases.push(Case {
                name,
                input,
                pipe,
                file,
                seekable,
            });
        }
        captures.push(Value::object([
            ("directory", Value::str(directory)),
            ("rows_sha256", Value::str(sha256::hex(&rows_data))),
            ("seekable", Value::Bool(seekable)),
        ]));
    }
    let golds = rust(&cases);
    let manifest = Value::object([
        ("set", Value::str(SET)),
        ("key", set.get("key").cloned().unwrap_or(Value::Null)),
        ("sha256", set.get("sha256").cloned().unwrap_or(Value::Null)),
        ("reference", Value::str("FFprobe 9.0.2")),
        ("ffprobe_sha256", Value::str(reference.unwrap_or_default())),
        ("arguments", Value::strings(ARGUMENTS)),
        ("captures", Value::Array(captures)),
        (
            "cases",
            Value::Array(
                cases
                    .iter()
                    .map(|case| {
                        Value::object([
                            ("name", Value::str(case.name.as_str())),
                            ("input", Value::str(case.input.as_str())),
                            ("pipe", case.pipe.hashes.clone()),
                            ("file", case.file.hashes.clone()),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("records", Value::str(GOLDS)),
        ("records_sha256", Value::str(sha256::hex(golds.as_bytes()))),
    ]);
    Ok((golds, json::to_python(&manifest, Some(1)) + "\n"))
}

/// The one directory an unpacked set holds.
fn top_level(dir: &Path) -> R<std::path::PathBuf> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        entries.push(entry.map_err(|e| e.to_string())?.path());
    }
    match entries.as_slice() {
        [only] if only.is_dir() => Ok(only.clone()),
        _ => Err(format!(
            "{}: expected one top-level directory",
            dir.display()
        )),
    }
}

fn with_input(observed: Read, input_sha256: &str) -> Read {
    let Value::Object(mut members) = observed.hashes else {
        unreachable!("hashes are an object")
    };
    members.insert(0, ("input_sha256".to_owned(), Value::str(input_sha256)));
    Read {
        probe: observed.probe,
        hashes: Value::Object(members),
    }
}

/// Check one FFprobe call against its recorded hashes and parse what it printed.
fn capture(dir: &Path, row: &Value) -> R<Read> {
    let base = format!("{}--{}", text(row, "id")?, text(row, "mode")?);
    let stdout = read(&dir.join(format!("{base}.stdout")))?;
    let stderr = read(&dir.join(format!("{base}.stderr")))?;
    if sha256::hex(&stdout) != text(row, "stdout_sha256")?
        || sha256::hex(&stderr) != text(row, "stderr_sha256")?
    {
        return Err("output differs from its recorded SHA-256".into());
    }
    let rc = integer(row, "returncode")?;
    let printed = json::parse(std::str::from_utf8(&stdout).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let probe = match rc {
        0 => {
            let Value::Object(members) = &printed else {
                return Err("stdout is not an object".into());
            };
            let keys: Vec<_> = members.iter().map(|(k, _)| k.as_str()).collect();
            if keys != ["packets", "streams", "format"] {
                return Err(format!("unexpected sections {keys:?}"));
            }
            let packets = printed
                .get("packets")
                .and_then(Value::as_array)
                .ok_or("packets")?;
            Probe::Packets(packets.iter().map(packet).collect::<R<_>>()?)
        }
        // FFprobe failed avformat_open_input: it printed no section, and its
        // last two lines name the header and AVERROR_INVALIDDATA.
        1 => {
            let stderr = String::from_utf8_lossy(&stderr);
            let lines: Vec<_> = stderr.lines().collect();
            let [.., header, error] = lines.as_slice() else {
                return Err("short stderr".into());
            };
            if printed != Value::Object(Vec::new())
                || !header.ends_with("] error reading header")
                || !error.ends_with(": Invalid data found when processing input")
            {
                return Err("exit 1 is not an invalid-header refusal".into());
            }
            Probe::InvalidHeader
        }
        _ => return Err(format!("exit {rc}")),
    };
    let hashes = Value::object([
        ("returncode", Value::int(rc)),
        ("stdout_sha256", Value::str(text(row, "stdout_sha256")?)),
        ("stderr_sha256", Value::str(text(row, "stderr_sha256")?)),
    ]);
    Ok(Read { probe, hashes })
}

fn packet(value: &Value) -> R<Packet> {
    let Value::Object(members) = value else {
        return Err("packet is not an object".into());
    };
    let mut keys = KEYS.iter();
    let mut fields = Vec::new();
    for (key, field) in members {
        // Keys appear in FFprobe's order; only an optional key may be absent.
        loop {
            let Some((k, optional)) = keys.next() else {
                return Err(format!("unexpected or out-of-order key {key}"));
            };
            if k == key {
                break;
            }
            if !optional {
                return Err(format!("packet without {k}"));
            }
        }
        let parsed = match (key.as_str(), field) {
            ("data", Value::String(dump)) => Field::Data(hexdump(dump)?),
            (_, Value::String(s)) => Field::Text(s.clone()),
            (_, Value::Number(n)) if n.is_integer() => {
                Field::Integer(n.as_i128().ok_or("integer")?)
            }
            _ => return Err(format!("unexpected value for {key}")),
        };
        fields.push((key.clone(), parsed));
    }
    if let Some((key, _)) = keys.find(|(_, optional)| !optional) {
        return Err(format!("packet without {key}"));
    }
    let size = fields.iter().find_map(|(k, f)| match (k.as_str(), f) {
        ("size", Field::Text(s)) => s.parse::<usize>().ok(),
        _ => None,
    });
    let data = fields.iter().find_map(|(k, f)| match (k.as_str(), f) {
        ("data", Field::Data(d)) => Some(d.len()),
        _ => None,
    });
    if size.is_none() || size != data {
        return Err("data length differs from size".into());
    }
    Ok(Packet(fields))
}

/// The bytes of FFprobe's `-show_data` dump: lines of an 8-digit offset, 16
/// bytes in hex groups of two, and their printable ASCII.
fn hexdump(dump: &str) -> R<Vec<u8>> {
    let mut out = Vec::new();
    let body = dump
        .strip_prefix('\n')
        .ok_or("dump without leading newline")?;
    for line in body.lines() {
        let (offset, rest) = line.split_once(": ").ok_or("dump line without offset")?;
        if usize::from_str_radix(offset, 16).ok() != Some(out.len()) || rest.len() < 41 {
            return Err(format!("dump line {line:?}"));
        }
        let (hex, ascii) = rest.split_at(40);
        let start = out.len();
        for group in hex.split_whitespace() {
            for pair in group.as_bytes().chunks(2) {
                let pair = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
                out.push(u8::from_str_radix(pair, 16).map_err(|_| format!("dump {pair:?}"))?);
            }
        }
        let printable: String = out[start..]
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        if ascii.strip_prefix(' ') != Some(printable.as_str()) || out.len() - start > 16 {
            return Err(format!("dump line {line:?}"));
        }
    }
    Ok(out)
}

fn rust(cases: &[Case]) -> String {
    let mut out = String::from(
        "// Generated by `cargo xtask playback-golds` from the FFprobe 9.0.2 captures in\n\
         // evidence set `playback-terminal-matrix-ffprobe`. Do not edit; see\n\
         // terminal-matrix/README.md.\n\
         pub const CASES: &[Case] = &[\n",
    );
    for case in cases {
        let _ = writeln!(out, "    Case {{");
        let _ = writeln!(out, "        name: {:?},", case.name);
        let _ = writeln!(
            out,
            "        raw: include_bytes!(\"../fixtures/{}\"),",
            case.input
        );
        let _ = write!(out, "        pipe: ");
        probe(&mut out, &case.pipe.probe, 2);
        out.push_str(",\n");
        if !case.seekable {
            out.push_str("        file: None,\n");
        } else if same(&case.file.probe, &case.pipe.probe) {
            out.push_str("        file: Some(File::SameAsPipe),\n");
        } else {
            out.push_str("        file: Some(File::Own(");
            probe(&mut out, &case.file.probe, 2);
            out.push_str(")),\n");
        }
        out.push_str("    },\n");
    }
    out.push_str("];\n");
    out
}

fn same(a: &Probe, b: &Probe) -> bool {
    match (a, b) {
        (Probe::Packets(a), Probe::Packets(b)) => a == b,
        (Probe::InvalidHeader, Probe::InvalidHeader) => true,
        _ => false,
    }
}

fn probe(out: &mut String, probe: &Probe, depth: usize) {
    let indent = "    ".repeat(depth);
    let Probe::Packets(packets) = probe else {
        out.push_str("Probe::InvalidHeader");
        return;
    };
    out.push_str("Probe::Packets(&[\n");
    for packet in packets {
        let _ = writeln!(out, "{indent}    Packet {{");
        for (key, _) in KEYS {
            let field = packet.0.iter().find(|(k, _)| k == key).map(|(_, f)| f);
            let value = match field {
                None => "None".to_owned(),
                Some(Field::Data(data)) => bytes(data),
                Some(Field::Integer(n)) if key == "stream_index" => n.to_string(),
                Some(Field::Integer(n)) => format!("Some({n})"),
                Some(Field::Text(s)) if matches!(key, "codec_type" | "size" | "flags") => {
                    format!("{s:?}")
                }
                Some(Field::Text(s)) => format!("Some({s:?})"),
            };
            let _ = writeln!(out, "{indent}        {key}: {value},");
        }
        let _ = writeln!(out, "{indent}    }},");
    }
    let _ = write!(out, "{indent}])");
}

/// A Rust byte string: printable ASCII as itself, everything else escaped.
fn bytes(data: &[u8]) -> String {
    let mut out = String::from("b\"");
    for &b in data {
        if (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\' {
            out.push(b as char);
        } else {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_decode_with_their_ascii_column() {
        let dump = "\n00000000: 0005 4865 6c6c 6f                        ..Hello\n";
        assert_eq!(hexdump(dump).unwrap(), b"\0\x05Hello");
        assert_eq!(hexdump("\n").unwrap(), b"");
        let wrong = "\n00000000: 0005 4865 6c6c 6f                        ..Hellp\n";
        assert!(hexdump(wrong).is_err());
        assert_eq!(bytes(b"\"a\\\x7f "), "b\"\\x22a\\x5c\\x7f \"");
    }

    #[test]
    fn packets_keep_ffprobe_key_order() {
        let parse = |text: &str| packet(&json::parse(text).unwrap());
        let ok = r#"{"codec_type":"video","stream_index":0,"size":"0","flags":"K__","data":"\n"}"#;
        assert!(parse(ok).is_ok());
        let swapped =
            r#"{"stream_index":0,"codec_type":"video","size":"0","flags":"K__","data":"\n"}"#;
        assert!(parse(swapped).is_err());
        let extra = r#"{"codec_type":"video","stream_index":0,"size":"0","flags":"K__","data":"\n","side_data_list":[]}"#;
        assert!(parse(extra).is_err());
        let short =
            r#"{"codec_type":"video","stream_index":0,"size":"1","flags":"K__","data":"\n"}"#;
        assert!(parse(short).is_err());
    }
}
