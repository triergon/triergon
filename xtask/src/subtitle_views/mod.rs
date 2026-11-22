//! Inputs and reference pins for the subtitle views worker.

mod inputs;

use std::path::Path;
use std::process::Command;

pub(crate) use inputs::private_input;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if args == ["fixtures", "corpus"] {
        return inputs::prepare(root);
    }
    if let [command, flag, output] = args
        && command == "fixtures"
        && flag == "--write-to"
    {
        let status = Command::new(crate::python::interpreter())
            .args([
                "-m",
                "harness.features.text-extraction.fixtures",
                "--output",
                output,
            ])
            .current_dir(root)
            .status()
            .map_err(|error| format!("cannot run the subtitle fixture author: {error}"))?;
        return if status.success() {
            Ok(())
        } else {
            Err(format!("subtitle fixture author failed: {status}"))
        };
    }
    crate::framing_views::command(root, args, "subtitle-views")
}
