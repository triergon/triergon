//! `triergon-font` against the pinned FreeType reference: build the reference
//! worker, fetch the font corpus and compare every face line by line.
//!
//! FreeType is built from its hash-checked release tarball with recorded configure
//! flags, as the FFmpeg references are. The worker (`harness/freetype-font.c`)
//! links the static library and prints the same lines as the crate's `font-dump`
//! example. The comparison is exact.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::fetch::{self, Transport};
use crate::json::Value;
use crate::prepare::{output, run, verify_xz_tree};
use crate::sha256;

pub const USAGE: &str = "usage: cargo xtask fonts prepare [--jobs N]
       cargo xtask fonts fetch
       cargo xtask fonts compare [--output DIR] [FILTER]
       cargo xtask fonts fuzz [--iters N]

  prepare  build the pinned FreeType release and its reference worker into
           .research/freetype
  fetch    fetch and verify the font corpus (harness/fonts/corpus.json) into
           .research/fonts
  compare  run font-dump and the FreeType worker on every corpus file whose path
           contains FILTER and compare their output: face data and unscaled
           outlines, then what libass obtains at ten sizes (--scaled); with
           --output, write DIR/report.json
  fuzz     run the crate's mutation smoke test (examples/fuzz.rs, dev profile, so
           overflow is checked) over every corpus file, N mutations each
           (default 40)";

pub const VERSION: &str = "2.15.0";
pub const ARCHIVE_SHA256: &str = "93cd47eb719c325107f2d3cf087d06299e944c3b48a40b55537302304dea1dd5";
const ARCHIVE_LIMIT: usize = 16 * 1024 * 1024;
const FONT_LIMIT: usize = 64 * 1024 * 1024;
const CACHE: &str = ".research/freetype";
const FONTS: &str = ".research/fonts";
const CORPUS: &str = "harness/fonts/corpus.json";
const WORKER_SOURCE: &str = "harness/freetype-font.c";
/// Mismatching lines kept per file in the report.
const KEPT_MISMATCHES: usize = 5;

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
        [command, rest @ ..] if command == "compare" => {
            let (mut report, mut filter) = (None, None);
            let mut rest = rest.iter();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--output" => report = Some(PathBuf::from(rest.next().ok_or(USAGE)?)),
                    _ if filter.is_none() && !arg.starts_with('-') => filter = Some(arg.as_str()),
                    _ => return Err(USAGE.into()),
                }
            }
            compare(root, report.as_deref(), filter)
        }
        [command, rest @ ..] if command == "fuzz" => {
            let iterations = match rest {
                [] => 40,
                [flag, value] if flag == "--iters" => {
                    value.parse().map_err(|_| "iterations must be a number")?
                }
                _ => return Err(USAGE.into()),
            };
            fuzz(root, iterations)
        }
        _ => Err(USAGE.into()),
    }
}

/// The crate's fuzz example over every corpus file.
fn fuzz(root: &Path, iterations: u32) -> Result<(), String> {
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
        .arg("--iters")
        .arg(iterations.to_string())
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

/// Download, verify, configure, build and install FreeType, then build the worker.
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
    let pkg_config = |flag: &str| -> Result<Vec<String>, String> {
        let text = output(
            Command::new("pkg-config")
                .args(["--static", flag, "freetype2"])
                .env("PKG_CONFIG_PATH", prefix.join("lib/pkgconfig")),
        )?;
        Ok(text.split_whitespace().map(str::to_owned).collect())
    };
    let (cflags, libs) = (pkg_config("--cflags")?, pkg_config("--libs")?);
    let worker = cache.join("freetype-font");
    run(Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .args(&cflags)
        .arg(root.join(WORKER_SOURCE))
        .args(&libs)
        .arg("-o")
        .arg(&worker))?;
    let hash = |path: &Path| {
        fs::read(path)
            .map(|d| sha256::hex(&d))
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    let manifest = Value::object([
        ("schema", Value::int(1)),
        ("release", Value::str(VERSION)),
        ("archive_sha256", Value::str(ARCHIVE_SHA256)),
        ("configure_argv", wanted),
        (
            "library_sha256",
            Value::str(hash(&prefix.join("lib/libfreetype.a"))?),
        ),
        ("link", Value::strings(libs)),
        (
            "worker_source_sha256",
            Value::str(hash(&root.join(WORKER_SOURCE))?),
        ),
        ("worker_sha256", Value::str(hash(&worker)?)),
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

/// The 26.6 heights the scaled comparison requests, as libass requests them
/// (`FT_SIZE_REQUEST_TYPE_REAL_DIM`): 256 pixels, what libass asks for every
/// glyph with hinting off; then font sizes scaled to frames, which it asks for
/// with hinting on: 75 and 82.5 (ASS sizes 20 and 22 at PlayResY 288 on 1080
/// lines), 50 (20 at 288 on 720), 48 (48 at 1080 on 1080), 37.5 and 33.75 (10
/// and 9 at 288 on 1080), 13.33 (20 at 1080 on 720), 24.5 (below the 25 pixels
/// per em where kerning shrinks) and 8.4.
const SIZES: [i32; 10] = [16384, 4800, 5280, 3200, 3072, 2400, 2160, 853, 1568, 538];

/// The two comparisons run on every corpus file.
#[derive(Clone, Copy)]
enum Pass {
    /// Face data and unscaled outlines.
    Faces,
    /// What libass obtains at each of [`SIZES`].
    Scaled,
}

impl Pass {
    const ALL: [Self; 2] = [Self::Faces, Self::Scaled];

    const fn name(self) -> &'static str {
        match self {
            Self::Faces => "faces",
            Self::Scaled => "scaled",
        }
    }

    fn args(self) -> Vec<String> {
        match self {
            Self::Faces => Vec::new(),
            Self::Scaled => {
                let sizes: Vec<String> = SIZES.iter().map(i32::to_string).collect();
                vec!["--scaled".into(), sizes.join(",")]
            }
        }
    }
}

/// The comparison of one corpus file in one pass.
struct Outcome {
    lines: usize,
    faces: usize,
    sizes: usize,
    glyphs: usize,
    mismatches: usize,
    /// Lines where the crate declares the value unsupported instead of giving
    /// one; counted apart from mismatches, never as agreement.
    unsupported: usize,
    kept: Vec<(usize, String, String)>,
    exits: (i32, i32),
}

impl Outcome {
    const fn pass(&self) -> bool {
        self.mismatches == 0 && self.exits.0 == 0 && self.exits.1 == 0
    }
}

fn compare(root: &Path, report: Option<&Path>, filter: Option<&str>) -> Result<(), String> {
    let reference = root.join(CACHE).join("freetype-font");
    if !reference.exists() {
        return Err("no FreeType reference worker: run `cargo xtask fonts prepare`".into());
    }
    run(Command::new("cargo")
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "triergon-font",
            "--example",
            "font-dump",
        ])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root))?;
    let candidate = root.join("target/release/examples/font-dump");
    let files = root.join(FONTS).join("files");
    let mut entries = Vec::new();
    let (mut failed, mut compared, mut unsupported) = (0, 0, 0);
    for file in corpus(root)?.into_iter().flat_map(|(_, list)| list) {
        if filter.is_some_and(|f| !file.path.contains(f)) {
            continue;
        }
        let path = files.join(&file.path);
        let data = fetch::read_bounded(&path, FONT_LIMIT)
            .map_err(|e| format!("{e} (run `cargo xtask fonts fetch`)"))?;
        if sha256::hex(&data) != file.sha256 {
            return Err(format!("{}: hash mismatch", file.path));
        }
        compared += 1;
        let mut passes = Vec::new();
        let mut file_failed = false;
        for pass in Pass::ALL {
            let outcome = compare_file(&reference, &candidate, &pass.args(), &path)?;
            file_failed |= !outcome.pass();
            unsupported += outcome.unsupported;
            println!(
                "{} {} {}: {} faces, {} sizes, {} glyphs, {} lines, {} differing, {} unsupported",
                if outcome.pass() { "PASS" } else { "FAIL" },
                pass.name(),
                file.path,
                outcome.faces,
                outcome.sizes,
                outcome.glyphs,
                outcome.lines,
                outcome.mismatches,
                outcome.unsupported
            );
            for (line, expected, actual) in &outcome.kept {
                println!("  line {line}\n    freetype: {expected}\n    triergon: {actual}");
            }
            passes.push(Value::object([
                ("pass", Value::str(pass.name())),
                ("faces", Value::int(outcome.faces as i64)),
                ("sizes", Value::int(outcome.sizes as i64)),
                ("glyphs", Value::int(outcome.glyphs as i64)),
                ("lines", Value::int(outcome.lines as i64)),
                ("differing_lines", Value::int(outcome.mismatches as i64)),
                ("unsupported_lines", Value::int(outcome.unsupported as i64)),
                ("reference_exit", Value::int(outcome.exits.0)),
                ("candidate_exit", Value::int(outcome.exits.1)),
                (
                    "first_differences",
                    Value::Array(
                        outcome
                            .kept
                            .iter()
                            .map(|(line, a, b)| {
                                Value::object([
                                    ("line", Value::int(*line as i64)),
                                    ("freetype", Value::str(a.clone())),
                                    ("triergon", Value::str(b.clone())),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]));
        }
        if file_failed {
            failed += 1;
        }
        entries.push(Value::object([
            ("path", Value::str(file.path.clone())),
            ("sha256", Value::str(file.sha256.clone())),
            ("passes", Value::Array(passes)),
        ]));
    }
    if compared == 0 {
        return Err("no corpus file matched".into());
    }
    if let Some(dir) = report {
        let hash = |path: &Path| {
            fs::read(path)
                .map(|d| sha256::hex(&d))
                .map_err(|e| format!("{}: {e}", path.display()))
        };
        let build = crate::checks::read_json(&root.join(CACHE).join("build.json"))?;
        let document = Value::object([
            ("schema", Value::int(2)),
            ("reference_build", build),
            ("reference_worker_sha256", Value::str(hash(&reference)?)),
            ("candidate_sha256", Value::str(hash(&candidate)?)),
            ("corpus_sha256", Value::str(hash(&root.join(CORPUS))?)),
            (
                "sizes_26_6",
                Value::Array(SIZES.iter().map(|&s| Value::int(i64::from(s))).collect()),
            ),
            ("files", Value::Array(entries)),
        ]);
        crate::corpus::write_json(&dir.join("report.json"), &document)?;
    }
    println!(
        "{} of {compared} corpus files match FreeType {VERSION}; {unsupported} lines unsupported",
        compared - failed
    );
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} corpus files differ"))
    }
}

/// Streams both workers' output for `font` and compares it line by line. A
/// candidate line ending in `unsupported` where the lines differ is counted as
/// unsupported.
fn compare_file(
    reference: &Path,
    candidate: &Path,
    args: &[String],
    font: &Path,
) -> Result<Outcome, String> {
    let spawn = |program: &Path| {
        Command::new(program)
            .args(args)
            .arg(font)
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))
    };
    let (mut a, mut b) = (spawn(reference)?, spawn(candidate)?);
    let mut ra = BufReader::new(a.stdout.take().ok_or("no reference output")?);
    let mut rb = BufReader::new(b.stdout.take().ok_or("no candidate output")?);
    let mut outcome = Outcome {
        lines: 0,
        faces: 0,
        sizes: 0,
        glyphs: 0,
        mismatches: 0,
        unsupported: 0,
        kept: Vec::new(),
        exits: (0, 0),
    };
    let (mut la, mut lb) = (Vec::new(), Vec::new());
    loop {
        la.clear();
        lb.clear();
        let na = ra.read_until(b'\n', &mut la).map_err(|e| e.to_string())?;
        let nb = rb.read_until(b'\n', &mut lb).map_err(|e| e.to_string())?;
        if na == 0 && nb == 0 {
            break;
        }
        outcome.lines += 1;
        if la.starts_with(b"glyph ") {
            outcome.glyphs += 1;
        } else if la.starts_with(b"face ") {
            outcome.faces += 1;
        } else if la.starts_with(b"size ") {
            outcome.sizes += 1;
        }
        if la == lb {
            continue;
        }
        if lb.trim_ascii_end().ends_with(b" unsupported") {
            outcome.unsupported += 1;
            continue;
        }
        outcome.mismatches += 1;
        if outcome.kept.len() < KEPT_MISMATCHES {
            let show = |line: &[u8]| {
                let text = String::from_utf8_lossy(line).trim_end().to_owned();
                text.chars().take(160).collect::<String>()
            };
            outcome.kept.push((outcome.lines, show(&la), show(&lb)));
        }
    }
    let code = |child: &mut std::process::Child| {
        child
            .wait()
            .map(|s| s.code().unwrap_or(-1))
            .map_err(|e| e.to_string())
    };
    outcome.exits = (code(&mut a)?, code(&mut b)?);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::{compare_file, configure_argv};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn differing_lines_and_exit_codes_are_reported() {
        let dir = crate::walk::scratch("fonts-compare");
        fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let a = script("a", "printf 'face 0\\nglyph 1 x\\nglyph 2 y\\n'");
        let b = script("b", "printf 'face 0\\nglyph 1 z\\n'; exit 3");
        let same = compare_file(&a, &a, &[], Path::new("font")).unwrap();
        assert_eq!(
            (same.lines, same.faces, same.glyphs, same.mismatches),
            (3, 1, 2, 0)
        );
        let outcome = compare_file(&a, &b, &[], Path::new("font")).unwrap();
        assert_eq!((outcome.mismatches, outcome.exits), (2, (0, 3)));
        assert_eq!(
            outcome.kept[0],
            (2, "glyph 1 x".to_owned(), "glyph 1 z".to_owned())
        );
        assert_eq!(outcome.kept[1].2, "");
        let c = script("c", "printf 'face 0\\nglyph 1 x\\nhinted 2 unsupported\\n'");
        let declined = compare_file(&a, &c, &[], Path::new("font")).unwrap();
        assert_eq!((declined.mismatches, declined.unsupported), (0, 1));
        assert!(declined.pass());
        fs::remove_dir_all(dir).unwrap();
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
