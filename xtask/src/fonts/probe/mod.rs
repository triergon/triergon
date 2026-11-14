//! `cargo xtask fonts assemble` and `cargo xtask fonts probe`: TrueType faces
//! assembled from short descriptions (`font`), with programs in TrueType
//! assembly (`asm`), loaded through the reference and the candidate font
//! workers to establish and check how a face's programs run.
//!
//! A probe is a description file `NAME.probe`; its leading comment says what it
//! shows. Next to it, `NAME.dump` records the reference's default load of every
//! glyph at each of the probe's heights (`font-sized-dump FILE default
//! HEIGHT`), as the reference gives it on 32-bit ARMv7. `probe` compares the
//! reference with that record and the candidate with the reference.

mod asm;
mod font;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{Stop, run_text};
use font::Description;

pub(super) const ASSEMBLE_USAGE: &str = "usage: cargo xtask fonts assemble DESCRIPTION OUTPUT

  Write the TrueType font DESCRIPTION describes (see xtask/src/fonts/probe/font.rs)
  to OUTPUT.";

pub(super) const PROBE_USAGE: &str =
    "usage: cargo xtask fonts probe --reference WORKER [--candidate WORKER]
                              [--record] [--output DIR] PROBE...

  Assemble each PROBE (a .probe description, or a directory of them, such as
  harness/fonts/probes) into DIR (default target/probes) and load it with the
  font workers at each of its heights (`font-sized-dump FONT default HEIGHT`).
  The reference must match the probe's recorded .dump, which --record rewrites
  (record on 32-bit ARMv7); the candidate must match the reference. Fails on any
  difference.";

/// The longest a worker may take to load one probe face.
const TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn assemble_command(args: &[String]) -> Result<(), String> {
    let [description, output] = args else {
        return Err(ASSEMBLE_USAGE.into());
    };
    let text = fs::read_to_string(description).map_err(|e| format!("{description}: {e}"))?;
    let font = Description::parse(&text)
        .and_then(|d| d.build())
        .map_err(|e| format!("{description}: {e}"))?;
    fs::write(output, font).map_err(|e| format!("{output}: {e}"))
}

struct Options {
    reference: String,
    candidate: Option<String>,
    record: bool,
    output: PathBuf,
    probes: Vec<PathBuf>,
}

fn options(root: &Path, args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        reference: String::new(),
        candidate: None,
        record: false,
        output: root.join("target/probes"),
        probes: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or_else(|| PROBE_USAGE.to_owned());
        match arg.as_str() {
            "--reference" => o.reference = value()?,
            "--candidate" => o.candidate = Some(value()?),
            "--output" => o.output = PathBuf::from(value()?),
            "--record" => o.record = true,
            flag if flag.starts_with("--") => return Err(PROBE_USAGE.into()),
            path => o.probes.extend(probe_files(Path::new(path))?),
        }
    }
    if o.reference.is_empty() || o.probes.is_empty() {
        return Err(PROBE_USAGE.into());
    }
    Ok(o)
}

/// A probe file, or the `.probe` files of a directory in name order.
fn probe_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let entries = fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let file = entry.map_err(|e| e.to_string())?.path();
        if file.extension().is_some_and(|e| e == "probe") {
            files.push(file);
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn probe_command(root: &Path, args: &[String]) -> Result<(), String> {
    let o = options(root, args)?;
    fs::create_dir_all(&o.output).map_err(|e| format!("{}: {e}", o.output.display()))?;
    let mut failed = Vec::new();
    for probe in &o.probes {
        let name = probe
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("{}: not a probe name", probe.display()))?;
        let text = fs::read_to_string(probe).map_err(|e| format!("{}: {e}", probe.display()))?;
        let description = Description::parse(&text).map_err(|e| format!("{name}: {e}"))?;
        let font_path = o.output.join(format!("{name}.ttf"));
        let font = description.build().map_err(|e| format!("{name}: {e}"))?;
        fs::write(&font_path, font).map_err(|e| format!("{}: {e}", font_path.display()))?;
        let reference = dump(&o.reference, &font_path, &description.heights)?;
        let record = probe.with_extension("dump");
        let mut differences = Vec::new();
        if o.record {
            fs::write(&record, &reference).map_err(|e| format!("{}: {e}", record.display()))?;
        } else {
            match fs::read_to_string(&record) {
                Ok(recorded) => differences.extend(compare("record", &recorded, &reference)),
                Err(_) => differences.push(format!("no record {}", record.display())),
            }
        }
        if let Some(candidate) = &o.candidate {
            let candidate = dump(candidate, &font_path, &description.heights)?;
            differences.extend(compare("reference", &reference, &candidate));
        }
        if differences.is_empty() {
            println!("{name}: same");
        } else {
            println!("{name}: differs");
            for line in &differences {
                println!("  {line}");
            }
            failed.push(name.to_owned());
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} probes differ: {}",
            failed.len(),
            failed.join(", ")
        ))
    }
}

/// The worker's default-load dump of `font` at each height, each after a line
/// `height H`; a failing run is recorded as its exit code and error output.
fn dump(worker: &str, font: &Path, heights: &[i32]) -> Result<String, String> {
    let mut out = String::new();
    for height in heights {
        let argv = [
            worker.to_owned(),
            "font-sized-dump".to_owned(),
            font.display().to_string(),
            "default".to_owned(),
            height.to_string(),
        ];
        out.push_str(&format!("height {height}\n"));
        match run_text(&argv, TIMEOUT) {
            Ok(done) if done.code == 0 => out.push_str(&done.stdout),
            Ok(done) => {
                out.push_str(&done.stdout);
                let error = done.stderr.lines().next().unwrap_or("").trim();
                out.push_str(&format!("exit {} {error}\n", done.code));
            }
            Err(Stop::Timeout(_)) => out.push_str("timeout\n"),
            Err(Stop::Fatal(message)) => return Err(format!("{worker}: {message}")),
        }
    }
    Ok(out)
}

/// The lines in which `actual` differs from `expected`; for two records of
/// the same kind, also the values that differ, by position.
fn compare(expected_name: &str, expected: &str, actual: &str) -> Vec<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (expected.lines().collect(), actual.lines().collect());
    let width = expected_name.len();
    let mut out = Vec::new();
    for i in 0..a.len().max(b.len()) {
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                out.push(format!("{expected_name}: {x}"));
                out.push(format!("{:width$}  {y}", "got"));
                if x.split(' ').next() == y.split(' ').next() {
                    out.push(format!("{:width$}  {}", "", value_differences(x, y)));
                }
            }
            (Some(x), None) => out.push(format!("missing: {x}")),
            (None, Some(y)) => out.push(format!("extra: {y}")),
            (None, None) => {}
        }
    }
    out
}

/// The positions and values where two dump lines differ, word by word.
fn value_differences(a: &str, b: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
    let mut out = Vec::new();
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or("-"),
            b.get(i).copied().unwrap_or("-"),
        );
        if x != y {
            out.push(format!("[{i}] {x}->{y}"));
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::compare;

    #[test]
    fn differences_name_the_values_that_changed() {
        let expected = "height 16384\nface 0\ndefault 0 640 0 0 64 64 1 | 1 | 0 0 1\n";
        assert!(compare("reference", expected, expected).is_empty());
        let actual = "height 16384\nface 0\ndefault 0 704 0 0 64 64 1 | 1 | 0 0 1\nmore\n";
        let differences = compare("reference", expected, actual);
        assert_eq!(differences.len(), 4);
        assert_eq!(differences[2].trim(), "[2] 640->704");
        assert_eq!(differences[3], "extra: more");
    }
}
