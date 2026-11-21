//! Inputs and reference pins for the codec views worker.

mod bits;
mod budget;

use std::path::Path;

pub(crate) use bits::fetch_opus;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if let [command, flags @ .., id] = args
        && command == "fixtures"
        && (flags.is_empty() || flags == ["--write-manifests"])
    {
        let id = Path::new(id)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|_| id.ends_with("/contract.json"))
            .unwrap_or(id);
        let write = !flags.is_empty();
        if bits::IDS.contains(&id) {
            bits::fixtures(root, id, write)?;
        } else if id == "core-budgets" {
            budget::fixtures(root, write)?;
        }
        if write {
            return Ok(());
        }
    }
    crate::framing_views::command(root, args, "codec-views")
}
