//! Inputs and reference pins for the play views worker.

mod seek;

use std::path::Path;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if let [command, args @ ..] = args
        && command == "seek"
    {
        return seek::main(root, args);
    }
    crate::framing_views::command(root, args, "play-views")
}
