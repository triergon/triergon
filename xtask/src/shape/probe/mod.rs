//! `cargo xtask shape assemble` and `cargo xtask shape probe`: CFF and CFF2
//! faces assembled from short descriptions (`cff`), measured through the
//! reference and the candidate shaping workers' `extents` operation to
//! establish and check the glyph extents of the OpenType font functions.
//!
//! A probe is a description file `NAME.probe`; its leading comment says what it
//! shows. Next to it, `NAME.dump` records the reference's `extents-dump` of the
//! face, as the reference gives it on 32-bit ARMv7. `probe` compares the
//! reference with that record and the candidate with the reference.

mod cff;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{Stop, run_text};
use cff::Description;

pub(super) const ASSEMBLE_USAGE: &str = "usage: cargo xtask shape assemble DESCRIPTION OUTPUT

  Write the CFF or CFF2 font DESCRIPTION describes (see
  xtask/src/shape/probe/cff.rs) to OUTPUT.";

pub(super) const PROBE_USAGE: &str =
    "usage: cargo xtask shape probe --reference WORKER [--candidate WORKER]
                              [--record] [--output DIR] PROBE...

  Assemble each PROBE (a .probe description, or a directory of them, such as
  harness/shape/probes) into DIR (default target/shape-probes) and measure it
  with the shaping workers (`extents-dump FONT`). The reference must match the
  probe's recorded .dump, which --record rewrites (record on 32-bit ARMv7); the
  candidate must match the reference. Fails on any difference.";

/// The longest a worker may take to measure one probe face.
const TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn assemble_command(args: &[String]) -> Result<(), String> {
    let [description, output] = args else {
        return Err(ASSEMBLE_USAGE.into());
    };
    let text = fs::read_to_string(description).map_err(|e| format!("{description}: {e}"))?;
    let font = Description::parse(&text).map_err(|e| format!("{description}: {e}"))?;
    fs::write(output, font.build()).map_err(|e| format!("{output}: {e}"))
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
        output: root.join("target/shape-probes"),
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
        let font_path = o.output.join(format!("{name}.otf"));
        fs::write(&font_path, description.build())
            .map_err(|e| format!("{}: {e}", font_path.display()))?;
        let reference = dump(&o.reference, &font_path)?;
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
            let candidate = dump(candidate, &font_path)?;
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

/// The worker's extents dump of `font`; a failing run is recorded as its exit
/// code and error output.
fn dump(worker: &str, font: &Path) -> Result<String, String> {
    let argv = [
        worker.to_owned(),
        "extents-dump".to_owned(),
        font.display().to_string(),
    ];
    match run_text(&argv, TIMEOUT) {
        Ok(done) if done.code == 0 => Ok(done.stdout),
        Ok(done) => Ok(format!(
            "{}exit {} {}\n",
            done.stdout,
            done.code,
            crate::fonts::probe::first_error(&done.stderr, font)
        )),
        Err(Stop::Timeout(_)) => Err(format!(
            "{worker}: {} timed out after {TIMEOUT:?}",
            font.display()
        )),
        Err(Stop::Fatal(message)) => Err(format!("{worker}: {message}")),
    }
}

/// The lines in which `actual` differs from `expected`.
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
            }
            (Some(x), None) => out.push(format!("missing: {x}")),
            (None, Some(y)) => out.push(format!("extra: {y}")),
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::compare;

    #[test]
    fn differences_name_the_lines_that_changed() {
        let expected = "face 0\nglyphs 1\nextents 0 1 0 10 20 -10\n";
        assert!(compare("reference", expected, expected).is_empty());
        let actual = "face 0\nglyphs 1\nextents 0 1 0 11 20 -10\nmore\n";
        let differences = compare("reference", expected, actual);
        assert_eq!(differences.len(), 3);
        assert!(differences[1].ends_with("  extents 0 1 0 11 20 -10"));
        assert_eq!(differences[2], "extra: more");
    }
}
