//! `cargo xtask codecs aac-ps-tables`: rerun the clean-room recovery of the Parametric
//! Stereo tables against the pinned FFmpeg 9.0.2 decoder and regenerate the Rust tables
//! of triergon-codec-aac, or check the committed ones.
//!
//! `harness/codecs/aac/ps/recover_tables.py` crafts HE-AAC v2 streams, decodes them with
//! the reference binary (a black box: only its output and messages are read), fits the
//! tables and writes them as JSON: the Huffman books (exact) and the signal tables, each
//! with its values as Rust expressions and the precision its fit established. This
//! command writes `crates/codec-aac/src/huffman_probed.rs` and
//! `crates/codec-aac/src/sbr/ps/tables.rs` from that JSON. A write reuses the steps whose
//! JSON an earlier run left in the work directory. With `--check` every step runs again
//! from an emptied directory, and the result is compared with the committed files instead
//! of written: the books and every non-numeric token exactly, each fitted number within
//! its table's precision, the precision its doc states (a refit moves the last digits).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Value};

const BOOKS: &str = "crates/codec-aac/src/huffman_probed.rs";
const TABLES: &str = "crates/codec-aac/src/sbr/ps/tables.rs";

pub(super) struct Options {
    pub check: bool,
    pub ffmpeg: Option<PathBuf>,
    pub python: String,
    pub work: Option<PathBuf>,
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd:?} failed: {status}"))
    }
}

pub(super) fn main(root: &Path, opts: &Options) -> Result<i32, String> {
    let ffmpeg = opts
        .ffmpeg
        .clone()
        .unwrap_or_else(|| root.join(".research/reference-decode/install/bin/ffmpeg"));
    let version = Command::new(&ffmpeg)
        .arg("-version")
        .output()
        .map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
    if !String::from_utf8_lossy(&version.stdout).starts_with("ffmpeg version 9.0.2 ") {
        return Err(format!("{} is not FFmpeg 9.0.2", ffmpeg.display()));
    }
    let work = opts
        .work
        .clone()
        .unwrap_or_else(|| root.join("target/aac-ps-tables"));
    fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let work = fs::canonicalize(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    // this decoder's PS input, for the fits of the signal side
    run(
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(root)
            .env("CARGO_INCREMENTAL", "0")
            .args([
                "build",
                "--quiet",
                "--release",
                "-p",
                "triergon-codec-aac",
                "--features",
                "inspect",
                "--example",
                "ps_input",
            ]),
    )?;
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let ps_input = work.join("ps_input");
    fs::copy(target.join("release/examples/ps_input"), &ps_input)
        .map_err(|e| format!("{}: {e}", ps_input.display()))?;
    let out = work.join("tables.json");
    let mut recover = Command::new(&opts.python);
    recover
        .current_dir(root)
        .env("FFMPEG", &ffmpeg)
        .env("PS_INPUT", &ps_input)
        .arg("-s")
        .arg(root.join("harness/codecs/aac/ps/recover_tables.py"));
    if opts.check {
        recover.arg("--rerun");
    }
    run(recover.arg(&work).arg(&out))?;
    let text = fs::read_to_string(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let value = json::parse(&text).map_err(|e| format!("{}: {e:?}", out.display()))?;
    let books = render_books(&value)?;
    let (tables, precision) = render_tables(&value)?;
    let mut status = 0;
    for (path, fresh, tolerances) in [(BOOKS, books, Vec::new()), (TABLES, tables, precision)] {
        let fresh = rustfmt(&work, &fresh)?;
        let target = root.join(path);
        if opts.check {
            let committed = fs::read_to_string(&target).map_err(|e| format!("{path}: {e}"))?;
            match compare(&committed, &fresh, &tolerances) {
                Ok(()) => println!("matches    {path}"),
                Err(why) => {
                    println!("DIFFERS    {path}: {why}");
                    status = 1;
                }
            }
        } else {
            fs::write(&target, fresh).map_err(|e| format!("{path}: {e}"))?;
            println!("wrote      {path}");
        }
    }
    Ok(status)
}

fn rustfmt(work: &Path, text: &str) -> Result<String, String> {
    let path = work.join("generated.rs");
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    run(Command::new("rustfmt")
        .args(["--edition", "2024"])
        .arg(&path))?;
    fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

fn str_of<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("tables.json: missing string {key}"))
}

fn array_of<'a>(v: &'a Value, key: &str) -> Result<&'a [Value], String> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("tables.json: missing array {key}"))
}

fn doc(out: &mut String, text: &str, prefix: &str) {
    for line in text.lines() {
        if line.is_empty() {
            out.push_str(prefix.trim_end());
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
        out.push('\n');
    }
}

fn render_books(v: &Value) -> Result<String, String> {
    let mut out = String::new();
    doc(&mut out, str_of(v, "books_doc")?, "//! ");
    for book in array_of(v, "books")? {
        let codes = array_of(book, "codes")?;
        out.push('\n');
        doc(&mut out, str_of(book, "doc")?, "/// ");
        out.push_str(&format!(
            "pub(crate) static {}: [(u32, u8); {}] = [\n",
            str_of(book, "name")?,
            codes.len()
        ));
        for c in codes {
            let pair = c
                .as_array()
                .filter(|p| p.len() == 2)
                .ok_or("tables.json: bad codeword")?;
            let (code, len) = (
                pair[0].as_f64().unwrap_or(-1.0),
                pair[1].as_f64().unwrap_or(-1.0),
            );
            if !(0.0..=32.0).contains(&len) || code < 0.0 || code >= 2f64.powf(len) {
                return Err("tables.json: codeword out of range".into());
            }
            out.push_str(&format!("    (0x{:x}, {}),\n", code as u64, len as u64));
        }
        out.push_str("];\n");
    }
    Ok(out)
}

/// The tables file and, per table, (name, precision) for the check.
fn render_tables(v: &Value) -> Result<(String, Vec<(String, f64)>), String> {
    let mut out = String::new();
    doc(&mut out, str_of(v, "tables_doc")?, "//! ");
    let mut precision = Vec::new();
    for t in array_of(v, "tables")? {
        let name = str_of(t, "name")?;
        let ty = str_of(t, "type")?;
        out.push('\n');
        doc(&mut out, str_of(t, "doc")?, "/// ");
        if let Some(items) = t.get("items").and_then(Value::as_array) {
            out.push_str(&format!("pub(super) const {name}: {ty} = [\n"));
            for item in items {
                out.push_str(&format!(
                    "    {},\n",
                    item.as_str().ok_or("tables.json: item")?
                ));
            }
            out.push_str("];\n");
        } else {
            out.push_str(&format!(
                "pub(super) const {name}: {ty} = {};\n",
                str_of(t, "value")?
            ));
        }
        precision.push((
            name.to_owned(),
            t.get("precision").and_then(Value::as_f64).unwrap_or(0.0),
        ));
    }
    Ok((out, precision))
}

/// Tokens: runs of letters, digits, `_` and `.` (a number when it starts with a digit;
/// the generated files write no exponents) and single other characters.
fn tokens(text: &str) -> Vec<(bool, &str)> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let n = if word(c) {
            rest.find(|c: char| !word(c)).unwrap_or(rest.len())
        } else {
            c.len_utf8()
        };
        if !c.is_whitespace() {
            out.push((c.is_ascii_digit(), &rest[..n]));
        }
        rest = &rest[n..];
    }
    out
}

fn number(s: &str) -> Option<f64> {
    let s = s.replace('_', "");
    if let Some(h) = s.strip_prefix("0x") {
        return u64::from_str_radix(h, 16).ok().map(|v| v as f64);
    }
    s.trim_end_matches("f64")
        .trim_end_matches("f32")
        .parse()
        .ok()
}

/// The committed file against the fresh one: equal tokens, numbers of a table within its
/// precision (the table is the last `const NAME` seen).
fn compare(committed: &str, fresh: &str, precision: &[(String, f64)]) -> Result<(), String> {
    let (a, b) = (tokens(committed), tokens(fresh));
    if a.len() != b.len() {
        return Err(format!(
            "{} tokens, the recovery gives {}",
            a.len(),
            b.len()
        ));
    }
    let mut table = String::new();
    let mut prev = String::new();
    for ((na, ta), (nb, tb)) in a.iter().zip(&b) {
        if prev == "const" {
            (*ta).clone_into(&mut table);
        }
        (*ta).clone_into(&mut prev);
        if ta == tb {
            continue;
        }
        let tol = precision
            .iter()
            .find(|(n, _)| *n == table)
            .map_or(0.0, |(_, p)| *p);
        match (na, nb, number(ta), number(tb)) {
            (true, true, Some(x), Some(y)) if (x - y).abs() <= tol => {}
            _ => return Err(format!("{table}: committed {ta}, recovered {tb}")),
        }
    }
    Ok(())
}
