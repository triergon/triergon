//! The independent Python oracles (exact source timing, zlib content, colour
//! and catalogue), run as `python3 -m harness.oracles KIND [FILE]`.

use std::path::Path;
use std::process::Command;

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
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Timing => "timing",
            Kind::Content => "content",
            Kind::Chain => "chain",
            Kind::Colour => "colour",
            Kind::Catalogue => "catalogue",
            Kind::Inflate => "inflate",
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
