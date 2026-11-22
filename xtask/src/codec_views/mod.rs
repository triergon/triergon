//! Inputs and reference pins for the codec views worker.

mod audio;
mod bits;
mod blocks;
mod budget;
mod time;
mod video;

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
        } else {
            match id {
                "core-budgets" => budget::fixtures(root, write)?,
                "crc-values" => blocks::fixtures(root, write)?,
                "time-rescale" => time::fixtures(root, write)?,
                "adts-sync" => audio::adts_fixtures(root, write)?,
                "ac3-features" => audio::audblk_fixtures(root, write)?,
                "ac3-frame-header" | "ac3-frame-info" | "codec-layouts" => {
                    audio::fixtures(root, id, write)?;
                }
                "hevc-access-units"
                | "picture-properties"
                | "video-nal-input"
                | "h264-stream-inspection" => {
                    video::fixtures(root, id, write)?;
                }
                _ => {}
            }
        }
        if write {
            return Ok(());
        }
    }
    crate::framing_views::command(root, args, "codec-views")
}
