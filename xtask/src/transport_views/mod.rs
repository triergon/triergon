//! Inputs and reference pins for the transport views worker.

mod catalogue;
mod mux;
mod packets;
mod time;

use std::fs;
use std::path::Path;

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    if args == ["fixtures", "catalogue-packets"] {
        let paths = crate::corpus::Paths {
            root: root.into(),
            out: root.into(),
        };
        println!("{}", fixtures(&paths)?);
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "fixtures") {
        fixtures(&crate::corpus::Paths {
            root: root.into(),
            out: root.into(),
        })?;
    }
    crate::framing_views::command(root, args, "transport-views")
}

pub(crate) fn fixtures(paths: &crate::corpus::Paths) -> Result<String, String> {
    let dir = paths.fixtures("fixtures-transport-views")?;
    catalogue::fixtures(&dir)?;
    packets::fixtures(&dir)?;
    time::fixtures(&paths.root, &dir)?;
    mux::fixtures(paths)?;
    Ok("Generated MPEG-TS catalogue, carrier and mux fixtures".into())
}

fn write(dir: &Path, name: &str, data: &[u8]) -> Result<(), String> {
    let path = dir.join(name);
    if fs::read(&path).as_deref().ok() != Some(data) {
        // Multiple host prepares can share the same fixture directory.
        let pending = path.with_extension(format!("{}.partial", std::process::id()));
        fs::write(&pending, data).map_err(|e| format!("{}: {e}", pending.display()))?;
        fs::rename(&pending, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}
