//! Inputs and reference pins for the playback views worker.

mod probe;

use std::path::Path;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if args == ["author-probes"] {
        return probe::author(root);
    }
    crate::framing_views::command(root, args, "playback-views")
}
