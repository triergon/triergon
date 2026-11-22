//! Inputs and reference pins for the ISO BMFF views worker.

use std::path::Path;

pub(crate) mod index;
mod packets;
mod seek;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let author_indexes = matches!(args, [command] if command == "author-indexes");
    if author_indexes
        || matches!(args, [command, rest @ ..] if command == "fixtures"
            && rest.first().is_none_or(|id| index::IDS.contains(&id.as_str())))
    {
        println!(
            "{}",
            index::fixtures(
                &crate::corpus::Paths {
                    root: root.into(),
                    out: root.into(),
                },
                author_indexes,
            )?
        );
        if author_indexes {
            return Ok(());
        }
    }
    if let [command, destination] = args
        && command == "author-packets"
    {
        return packets::author(root, &root.join(destination));
    }
    if let [command, destination] = args
        && command == "author-seek"
    {
        return seek::author(root, &root.join(destination));
    }
    crate::framing_views::command(root, args, "isobmff-views")
}
