//! Pinned static mpg123 for the free-format transport comparison.
use crate::fetch::Network;
use crate::json::Value;
use crate::prepare::{PinnedRelease, unpack_release, verify_release_tree};
use crate::process::{output, run};
use std::path::Path;
use std::process::Command;

pub const CACHE: &str = ".research/mpg123";
const RELEASE: PinnedRelease<'static> = PinnedRelease {
    file: "mpg123-1.33.7.tar.bz2",
    directory: "mpg123-1.33.7",
    url: "https://www.mpg123.org/download/mpg123-1.33.7.tar.bz2",
    mirrors: &[],
    sha256: "31d0e35a4ca567ec9b5ebda6c3062bb4435d6d3eacd6ef0d95cadd7854dc03ee",
};

fn configure(prefix: &Path) -> Vec<String> {
    vec![
        format!("--prefix={}", prefix.display()),
        "--disable-shared".into(),
        "--enable-static".into(),
        "--disable-components".into(),
        "--enable-libmpg123".into(),
        "CFLAGS=-O2 -ffp-contract=off".into(),
    ]
}

pub fn verify(record: &Value) -> Result<(), String> {
    let args = configure(Path::new(""));
    let actual = record
        .get("configure_argv")
        .and_then(Value::as_array)
        .unwrap_or_default();
    if record.get("archive_sha256").and_then(Value::as_str) == Some(RELEASE.sha256)
        && actual.len() == args.len()
        && actual
            .iter()
            .skip(1)
            .zip(args.iter().skip(1))
            .all(|(a, b)| a.as_str() == Some(b))
    {
        Ok(())
    } else {
        Err("mpg123 build mismatch; run cargo xtask prepare-mpg123".into())
    }
}

pub fn prepare(root: &Path, args: &[String]) -> Result<(), String> {
    let jobs = match args {
        [] => 4,
        [option, jobs] if option == "--jobs" => jobs.parse::<u32>().map_err(|e| e.to_string())?,
        _ => return Err("usage: cargo xtask prepare-mpg123 [--jobs N]".into()),
    };
    if !(1..=8).contains(&jobs) {
        return Err("jobs must be 1..8".into());
    }
    let build = crate::references::build("mpg123")?;
    let result = build.prepare(root, || {
        let cache = root.join(CACHE);
        let prefix = cache.join("install");
        let source = unpack_release(&cache, &RELEASE, &Network)?;
        let argv = configure(&prefix);
        run(Command::new("./configure").args(&argv).current_dir(&source))?;
        run(Command::new("make")
            .arg(format!("-j{jobs}"))
            .current_dir(&source))?;
        run(Command::new("make").arg("install").current_dir(&source))?;
        verify_release_tree(&cache.join(RELEASE.file), &cache)?;
        let library = prefix.join("lib/libmpg123.a");
        let hash =
            crate::sha256::file_hex(&library).map_err(|e| format!("{}: {e}", library.display()))?;
        let identity = Value::object([
            ("schema", Value::int(1)),
            ("release", Value::str("1.33.7")),
            ("archive_sha256", Value::str(RELEASE.sha256)),
            ("configure_argv", Value::strings(argv)),
            (
                "compiler",
                Value::str(output(Command::new("cc").arg("--version"))?),
            ),
            (
                "installed",
                Value::object([("install/lib/libmpg123.a", Value::str(hash))]),
            ),
        ]);
        crate::corpus::write_json(&cache.join("build.json"), &identity)?;
        Ok("Prepared static mpg123".into())
    })?;
    println!("{result}");
    Ok(())
}
