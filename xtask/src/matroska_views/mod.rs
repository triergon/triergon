//! Inputs and reference pins for the matroska views worker.

use std::path::Path;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    crate::framing_views::command(root, args, "matroska-views")
}
