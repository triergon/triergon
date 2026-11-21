//! The independent Python oracles (exact source timing, zlib content, colour
//! and catalogue), run as `python3 -m harness.oracles KIND [FILE]`.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::json::{self, Value};

#[derive(Clone, Copy)]
pub enum Kind {
    Timing,
    Content,
    /// The first track's private data and single frame, decoded through its
    /// content encoding chain.
    Chain,
    Colour,
    Catalogue,
    /// zlib streams and the outputs Python's zlib decodes from them; reads no file.
    Inflate,
    Trace(&'static str),
}

impl Kind {
    pub fn trace(name: &str) -> Result<Self, String> {
        [
            "views-smoke",
            "hvcc-prefix",
            "hvcc-arrays",
            "vpcc",
            "eac3-bsi",
            "av1-obu",
            "adts",
            "dts-header",
            "dts-unpack",
            "qt-sound-v0",
            "ddts",
            "isobmff-index",
            "store",
        ]
        .into_iter()
        .find(|kind| *kind == name)
        .map(Self::Trace)
        .ok_or_else(|| format!("unknown trace oracle: {name}"))
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Timing => "timing",
            Kind::Content => "content",
            Kind::Chain => "chain",
            Kind::Colour => "colour",
            Kind::Catalogue => "catalogue",
            Kind::Inflate => "inflate",
            Kind::Trace(name) => name,
        }
    }
}

/// The oracle's expectation for `file`, or the message of the malformed-input
/// error it raised. An oracle that fails any other way aborts the run.
pub fn run(root: &Path, kind: Kind, file: &Path) -> Result<Result<Value, String>, String> {
    invoke(root, kind, Some(file))
}

/// The expectation of an oracle that reads no input file.
pub fn generate(root: &Path, kind: Kind) -> Result<Result<Value, String>, String> {
    invoke(root, kind, None)
}

fn invoke(root: &Path, kind: Kind, file: Option<&Path>) -> Result<Result<Value, String>, String> {
    let output = Command::new(crate::python::interpreter())
        .args(["-m", "harness.oracles", kind.name()])
        .args(file)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run the {} oracle: {e}", kind.name()))?;
    if !output.status.success() {
        return Err(format!(
            "{} oracle failed on {}: {}",
            kind.name(),
            file.map_or("no input".into(), |f| f.display().to_string()),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let result = json::parse(&text).map_err(|e| format!("{} oracle output: {e}", kind.name()))?;
    Ok(
        match (
            result.get("data"),
            result.get("error").and_then(Value::as_str),
        ) {
            (Some(data), _) => Ok(data.clone()),
            (_, Some(error)) => Err(error.to_owned()),
            _ => {
                return Err(format!(
                    "{} oracle output has neither data nor error",
                    kind.name()
                ));
            }
        },
    )
}

/// Run the oracle in the same bounded subprocess as a candidate worker.
pub(super) fn trace(
    root: &Path,
    kind: Kind,
    path: &Path,
    arguments: &[OsString],
    timeout: Duration,
) -> Result<String, String> {
    let mut command = vec![
        crate::python::interpreter().into(),
        "-m".into(),
        "harness.oracles".into(),
        kind.name().into(),
        path.as_os_str().to_owned(),
    ];
    command.extend_from_slice(arguments);
    let result =
        super::worker::run_in(&command, &[], Some(root), timeout)?.ok_or("oracle timed out")?;
    if result.code != 0 {
        return Err(format!(
            "{}: {}",
            kind.name(),
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let document = json::parse(std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    render(&document)
}

fn render(document: &Value) -> Result<String, String> {
    let mut trace = String::new();
    for row in document
        .get("data")
        .and_then(Value::as_array)
        .ok_or("trace oracle needs a data array")?
    {
        let line = row
            .as_str()
            .filter(|line| !line.is_empty() && !line.contains(['\r', '\n']))
            .ok_or("oracle trace row must be one nonempty line")?;
        trace.push_str(line);
        trace.push('\n');
    }
    Ok(trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_trace_preserves_row_order_and_whitespace() {
        let parse = |input| json::parse(input).unwrap();
        assert_eq!(
            render(&parse(r#"{"data":["7  2","8 0"]}"#)).unwrap(),
            "7  2\n8 0\n"
        );
        assert_eq!(render(&parse(r#"{"data":[]}"#)).unwrap(), "");
        for input in [
            r#"{"data":["7\n8"]}"#,
            r#"{"data":[4]}"#,
            r#"{"error":"refused"}"#,
        ] {
            assert!(render(&parse(input)).is_err());
        }
    }
}
