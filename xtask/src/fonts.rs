//! The FreeType reference and the font corpus: build the pinned FreeType release,
//! fetch the corpus, and run the font features. `cargo xtask build-workers` links
//! the reference worker (`harness/reference_font.h`) against the installed
//! library; the features `font-outlines`, `font-scaled` and `font-hinted` run
//! through `cargo xtask report`, `trace-identities` and `fuzz --contract` like
//! every other feature, and `fonts compare` reports all three.
//!
//! FreeType is built from its hash-checked release tarball with recorded configure
//! flags, as the FFmpeg references are.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch::{self, Transport};
use crate::json::Value;
use crate::prepare::{output, run, verify_xz_tree};
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask fonts prepare [--jobs N]
       cargo xtask fonts fetch
       cargo xtask fonts compare [REPORT OPTION]...
       cargo xtask fonts fuzz [--iters N] [--timeout SECONDS]

  prepare  build the pinned FreeType release into .research/freetype; then
           `cargo xtask build-workers` builds its reference worker
  fetch    fetch and verify the font corpus (harness/fonts/corpus.json) into
           .research/fonts, writing the authored fonts with the crate's
           authored-fonts example
  compare  `cargo xtask report` on each font feature (font-outlines, font-scaled,
           font-hinted), with any further report options; each report goes to
           OUTPUT/<feature> (OUTPUT: --output, default .research/font-report)
  fuzz     run the crate's mutation smoke test (examples/fuzz.rs, dev profile, so
           overflow is checked; an input past the timeout is a hang) over every
           corpus file, N mutations each (default 40)";

pub const VERSION: &str = "2.15.0";
pub const ARCHIVE_SHA256: &str = "93cd47eb719c325107f2d3cf087d06299e944c3b48a40b55537302304dea1dd5";
const ARCHIVE_LIMIT: usize = 16 * 1024 * 1024;
const FONT_LIMIT: usize = 64 * 1024 * 1024;
const CACHE: &str = ".research/freetype";
const FONTS: &str = ".research/fonts";
const CORPUS: &str = "harness/fonts/corpus.json";
/// The font features: face data and unscaled outlines, what libass loads at a
/// size, and default-load (hinted) advances.
pub const FEATURES: [&str; 3] = ["font-outlines", "font-scaled", "font-hinted"];

/// The `cargo xtask report` arguments `fonts compare ARGS` stands for, one list
/// per feature; `None` for the other commands.
pub fn reports(args: &[String]) -> Option<Vec<(&'static str, Vec<String>)>> {
    let (command, rest) = args.split_first()?;
    if command != "compare" {
        return None;
    }
    let mut output = PathBuf::from(".research/font-report");
    let mut options = Vec::new();
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        match (arg.as_str(), rest.clone().next()) {
            ("--output", Some(value)) => {
                output = PathBuf::from(value);
                rest.next();
            }
            _ => options.push(arg.clone()),
        }
    }
    Some(
        FEATURES
            .iter()
            .map(|&feature| {
                let path = |file: &str| format!("harness/features/{feature}/{file}.json");
                let mut list = vec![
                    "--contract".to_owned(),
                    path("contract"),
                    "--manifest".to_owned(),
                    path("corpus"),
                    "--output".to_owned(),
                    output.join(feature).display().to_string(),
                ];
                list.extend(options.iter().cloned());
                (feature, list)
            })
            .collect(),
    )
}

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [help] if help == "--help" || help == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        [command, rest @ ..] if command == "prepare" => {
            let jobs = match rest {
                [] => 4,
                [flag, value] if flag == "--jobs" => {
                    value.parse().map_err(|_| "jobs must be 1..8")?
                }
                _ => return Err(USAGE.into()),
            };
            println!("{}", prepare(root, jobs, &fetch::Network)?.display());
            Ok(())
        }
        [command] if command == "fetch" => {
            println!(
                "Verified {} corpus files",
                fetch_corpus(root, &fetch::Network)?
            );
            Ok(())
        }
        [command, rest @ ..] if command == "fuzz" => {
            let (mut iterations, mut timeout) = (40, 10);
            let mut rest = rest.iter();
            while let Some(flag) = rest.next() {
                let value: u32 = rest
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| format!("{flag} needs a number"))?;
                match flag.as_str() {
                    "--iters" => iterations = value,
                    "--timeout" => timeout = value,
                    _ => return Err(USAGE.into()),
                }
            }
            fuzz(root, iterations, timeout)
        }
        _ => Err(USAGE.into()),
    }
}

/// The crate's fuzz example over every corpus file.
fn fuzz(root: &Path, iterations: u32, timeout: u32) -> Result<(), String> {
    run(Command::new("cargo")
        .args([
            "build",
            "--locked",
            "-p",
            "triergon-font",
            "--example",
            "fuzz",
        ])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root))?;
    let files = root.join(FONTS).join("files");
    let paths: Vec<PathBuf> = corpus(root)?
        .into_iter()
        .flat_map(|(_, list)| list)
        .map(|file| files.join(file.path))
        .collect();
    if let Some(missing) = paths.iter().find(|p| !p.exists()) {
        return Err(format!(
            "{}: missing (run `cargo xtask fonts fetch`)",
            missing.display()
        ));
    }
    run(Command::new(root.join("target/debug/examples/fuzz"))
        .args([
            "--iters",
            &iterations.to_string(),
            "--timeout",
            &timeout.to_string(),
        ])
        .args(&paths)
        .current_dir(root.join(FONTS)))
}

pub fn configure_argv(prefix: &Path) -> Vec<String> {
    [
        "./configure".to_owned(),
        format!("--prefix={}", prefix.display()),
        "--disable-shared".into(),
        "--enable-static".into(),
        "--with-zlib=no".into(),
        "--with-bzip2=no".into(),
        "--with-png=no".into(),
        "--with-harfbuzz=no".into(),
        "--with-brotli=no".into(),
        "--with-librsvg=no".into(),
    ]
    .into()
}

/// Download, verify, configure, build and install FreeType, and record the build in
/// `.research/freetype/build.json`.
fn prepare(root: &Path, jobs: u32, net: &dyn Transport) -> Result<PathBuf, String> {
    if !(1..=8).contains(&jobs) {
        return Err("jobs must be 1..8".into());
    }
    let cache = root.join(CACHE);
    fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let archive_path = cache.join(format!("freetype-{VERSION}.tar.xz"));
    if !archive_path.exists() {
        let url = format!(
            "https://download.savannah.gnu.org/releases/freetype/freetype-{VERSION}.tar.xz"
        );
        let data = net.https(&url, "triergon-reference-builder", ARCHIVE_LIMIT)?;
        if data.len() > ARCHIVE_LIMIT {
            return Err("FreeType archive size limit".into());
        }
        fetch::publish(&archive_path, &data)?;
    }
    let archive = fetch::read_bounded(&archive_path, ARCHIVE_LIMIT)?;
    if sha256::hex(&archive) != ARCHIVE_SHA256 {
        return Err("Pinned FreeType archive hash mismatch".into());
    }
    let source = cache.join(format!("freetype-{VERSION}"));
    if !source.exists() {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&cache))?;
    }
    verify_xz_tree(&archive_path, &cache)?;
    let prefix = cache.join("install");
    let argv = configure_argv(&prefix);
    let wanted = Value::strings(argv.iter().cloned());
    let record = cache.join("configure-argv.json");
    let configured = source.join("config.mk").exists();
    if !configured || crate::checks::read_json(&record).ok().as_ref() != Some(&wanted) {
        if configured {
            run(Command::new("make").arg("distclean").current_dir(&source))?;
        }
        run(Command::new(&argv[0]).args(&argv[1..]).current_dir(&source))?;
        crate::corpus::write_json(&record, &wanted)?;
    }
    run(Command::new("make")
        .arg(format!("-j{jobs}"))
        .current_dir(&source))?;
    run(Command::new("make").arg("install").current_dir(&source))?;
    verify_xz_tree(&archive_path, &cache)?;
    let libs = output(
        Command::new("pkg-config")
            .args(["--static", "--libs", "freetype2"])
            .env("PKG_CONFIG_LIBDIR", prefix.join("lib/pkgconfig")),
    )?;
    let library = prefix.join("lib/libfreetype.a");
    let library = fs::read(&library).map_err(|e| format!("{}: {e}", library.display()))?;
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("release", Value::str(VERSION)),
        ("archive_sha256", Value::str(ARCHIVE_SHA256)),
        ("configure_argv", wanted),
        ("library_sha256", Value::str(sha256::hex(&library))),
        (
            "link",
            Value::strings(libs.split_whitespace().map(str::to_owned)),
        ),
        (
            "compiler",
            Value::str(output(Command::new("cc").arg("--version"))?),
        ),
    ]);
    let path = cache.join("build.json");
    crate::corpus::write_json(&path, &manifest)?;
    Ok(path)
}

/// One corpus file: its path under `.research/fonts/files` and its SHA-256.
struct CorpusFile {
    path: String,
    sha256: String,
}

fn corpus(root: &Path) -> Result<Vec<(Value, Vec<CorpusFile>)>, String> {
    let manifest =
        crate::checks::read_json(&root.join(CORPUS)).map_err(|e| format!("{CORPUS}: {e}"))?;
    let sources = manifest
        .get("sources")
        .and_then(Value::as_array)
        .ok_or("corpus without sources")?;
    let mut result = Vec::new();
    for source in sources {
        let files = source
            .get("files")
            .and_then(Value::as_array)
            .ok_or("source without files")?;
        let mut list = Vec::new();
        for file in files {
            let field = |key| file.get(key).and_then(Value::as_str).map(str::to_owned);
            let (Some(path), Some(sha256)) = (field("path"), field("sha256")) else {
                return Err("corpus file without path or hash".into());
            };
            if path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err(format!("invalid corpus path {path}"));
            }
            list.push(CorpusFile { path, sha256 });
        }
        result.push((source.clone(), list));
    }
    Ok(result)
}

/// Download each source once, verify it, and unpack its files.
fn fetch_corpus(root: &Path, net: &dyn Transport) -> Result<usize, String> {
    let downloads = root.join(FONTS).join("downloads");
    let files = root.join(FONTS).join("files");
    fs::create_dir_all(&downloads).map_err(|e| format!("{}: {e}", downloads.display()))?;
    fs::create_dir_all(&files).map_err(|e| format!("{}: {e}", files.display()))?;
    let mut verified = 0;
    for (source, list) in corpus(root)? {
        if source.get("generator").and_then(Value::as_str) == Some("authored-fonts") {
            // Written by the font crate's own builder, then checked like any file.
            run(Command::new("cargo")
                .args([
                    "run",
                    "--quiet",
                    "--locked",
                    "-p",
                    "triergon-font",
                    "--example",
                    "authored-fonts",
                    "--",
                ])
                .arg(files.join("authored"))
                .env("CARGO_INCREMENTAL", "0")
                .current_dir(root))?;
            for file in &list {
                let data = fetch::read_bounded(&files.join(&file.path), FONT_LIMIT)?;
                if sha256::hex(&data) != file.sha256 {
                    return Err(format!(
                        "{}: authored font changed; update its hash",
                        file.path
                    ));
                }
                verified += 1;
            }
            continue;
        }
        let text = |key| {
            source
                .get(key)
                .and_then(Value::as_str)
                .ok_or(format!("source without {key}"))
        };
        let (name, url, expected) = (text("name")?, text("url")?, text("sha256")?);
        if name.contains('/') || name.starts_with('.') {
            return Err(format!("invalid source name {name}"));
        }
        let archive = downloads.join(name);
        if !archive.exists() {
            let data = net.https(url, "triergon-font-corpus", FONT_LIMIT)?;
            if data.len() > FONT_LIMIT {
                return Err(format!("{name} exceeds the download bound"));
            }
            fetch::publish(&archive, &data)?;
        }
        let data = fetch::read_bounded(&archive, FONT_LIMIT)?;
        if sha256::hex(&data) != expected {
            return Err(format!("{name}: hash mismatch"));
        }
        if name.ends_with(".tar.bz2") {
            run(Command::new("tar")
                .arg("-xjf")
                .arg(&archive)
                .arg("-C")
                .arg(&files)
                .args(list.iter().map(|f| f.path.as_str())))?;
        } else {
            fs::copy(&archive, files.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
        for file in &list {
            let data = fetch::read_bounded(&files.join(&file.path), FONT_LIMIT)?;
            if sha256::hex(&data) != file.sha256 {
                return Err(format!("{}: hash mismatch", file.path));
            }
            verified += 1;
        }
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::{FEATURES, configure_argv, reports};
    use std::path::Path;

    #[test]
    fn compare_reports_every_font_feature() {
        let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        let found = reports(&args(&["compare", "--output", "out", "--memory"])).unwrap();
        assert_eq!(found.len(), FEATURES.len());
        let (feature, scaled) = &found[1];
        assert_eq!(*feature, "font-scaled");
        assert_eq!(
            *scaled,
            args(&[
                "--contract",
                "harness/features/font-scaled/contract.json",
                "--manifest",
                "harness/features/font-scaled/corpus.json",
                "--output",
                "out/font-scaled",
                "--memory",
            ])
        );
        assert!(reports(&args(&["fuzz"])).is_none());
    }

    #[test]
    fn freetype_builds_without_optional_libraries() {
        let argv = configure_argv(Path::new("/p/install"));
        assert_eq!(argv[1], "--prefix=/p/install");
        for library in ["zlib", "bzip2", "png", "harfbuzz", "brotli", "librsvg"] {
            assert!(argv.contains(&format!("--with-{library}=no")), "{library}");
        }
    }
}
