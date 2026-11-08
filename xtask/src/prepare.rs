//! The pinned FFmpeg references: download, verify, configure, build and, for the
//! container profiles, list the configure-expanded FATE cases. No sample is
//! downloaded and no upstream test is executed.
//!
//! Two sources are pinned. The container profiles build git revision
//! [`REVISION`] on the 9.0 branch; the decoder and capture profiles build the
//! FFmpeg [`RELEASE`] release tarball, which those comparisons were measured with.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fetch::{self, Transport};
use crate::json::{self, Value};
use crate::sha256;

pub const REVISION: &str = "d32b387f2b0a484599d4587d651891f0c63c4238";
pub const ARCHIVE_SHA256: &str = "8a830a34bfaf98514b5d45cf6c01b1fe78b38d5e4c10eab0de2531b783c15f90";
const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
pub const RELEASE: &str = "9.0.2";
pub const RELEASE_SHA256: &str = "8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e";
/// Bound on the unpacked source tree read back for verification.
const SOURCE_LIMIT: usize = 1024 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// The Matroska reference used by the container harness.
    Matroska,
    /// An isolated MOV/TS/subtitle reference; the Matroska build is untouched.
    Formats,
    /// The Matroska reference plus the H.264 and HEVC decoders, which stream
    /// discovery uses to learn the B-picture delay; the Matroska build is untouched.
    Video,
    /// FFmpeg's default configuration (static libraries, `ffmpeg`, `ffprobe`):
    /// the decoder comparison reference, and the independent decoder and demuxer
    /// of the capture comparison.
    Decode,
    /// The gating capture reference: the statically linked component set (`mpeg1video`, MPEG-TS, `swscale`).
    Capture,
    /// The same capture component set built with `--enable-small`, reported
    /// alongside the gating reference.
    CaptureSmall,
}

/// Which pinned source a profile builds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Revision,
    Release,
}

impl Profile {
    /// The cache directory under the repository root.
    pub fn cache(self) -> &'static str {
        match self {
            Profile::Matroska => ".research/reference",
            Profile::Formats => ".research/reference-formats",
            Profile::Video => ".research/reference-video",
            Profile::Decode => ".research/reference-decode",
            Profile::Capture => ".research/reference-capture-static",
            Profile::CaptureSmall => ".research/reference-capture-small-static",
        }
    }

    fn source(self) -> Source {
        match self {
            Profile::Matroska | Profile::Formats | Profile::Video => Source::Revision,
            Profile::Decode | Profile::Capture | Profile::CaptureSmall => Source::Release,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [
            Profile::Matroska,
            Profile::Formats,
            Profile::Video,
            Profile::Decode,
            Profile::Capture,
            Profile::CaptureSmall,
        ]
        .into_iter()
        .find(|profile| profile.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Matroska => "matroska",
            Profile::Formats => "formats",
            Profile::Video => "video",
            Profile::Decode => "decode",
            Profile::Capture => "capture",
            Profile::CaptureSmall => "capture-small",
        }
    }
}

/// The capture comparison's component set (static libraries, no programs).
const CAPTURE_ARGS: [&str; 22] = [
    "--disable-shared",
    "--enable-static",
    "--enable-pic",
    "--extra-cflags=-Dstatic_assert=_Static_assert",
    "--disable-programs",
    "--disable-doc",
    "--disable-avdevice",
    "--disable-avfilter",
    "--disable-network",
    "--disable-swresample",
    "--disable-debug",
    "--disable-everything",
    "--disable-autodetect",
    "--enable-zlib",
    "--enable-demuxer=matroska,mov,mpegts,h264,hevc",
    "--enable-parser=h264,hevc,aac,ac3,flac,dvdsub,dvbsub",
    "--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata",
    "--enable-decoder=pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt",
    "--enable-decoder=aac,ac3,eac3,dca,mp3,mp2,flac,opus,vorbis,truehd,mlp,alac,pcm_s16le,pcm_s24le,pcm_s16be,pcm_s24be,pcm_s32le,pcm_s32be",
    "--enable-protocol=file",
    "--enable-encoder=mpeg1video",
    "--enable-muxer=mpegts",
];

/// The decoder comparison's configuration: FFmpeg's defaults without
/// documentation and debug symbols, as the comparison's static reference and
/// its `ffmpeg` and `ffprobe` were built.
const DECODE_ARGS: [&str; 2] = ["--disable-doc", "--disable-debug"];

pub fn configure_argv(prefix: &Path, profile: Profile) -> Vec<String> {
    let release = |args: &[&str]| -> Vec<String> {
        [
            "./configure".to_owned(),
            format!("--prefix={}", prefix.display()),
        ]
        .into_iter()
        .chain(args.iter().map(ToString::to_string))
        .collect()
    };
    match profile {
        Profile::Capture => return release(&CAPTURE_ARGS),
        Profile::CaptureSmall => {
            let mut args = CAPTURE_ARGS.to_vec();
            let at = args
                .iter()
                .position(|a| *a == "--disable-debug")
                .expect("listed")
                + 1;
            args.insert(at, "--enable-small");
            return release(&args);
        }
        Profile::Decode => return release(&DECODE_ARGS),
        Profile::Matroska | Profile::Formats | Profile::Video => {}
    }
    let mut argv: Vec<String> = [
        "./configure",
        &format!("--prefix={}", prefix.display()),
        "--disable-everything",
        "--disable-autodetect",
        "--disable-doc",
        "--disable-debug",
        "--enable-protocol=file,pipe",
        "--enable-demuxer=matroska",
        "--enable-muxer=framecrc",
        "--enable-decoder=pcm_s16le",
        "--enable-encoder=pcm_s16le,rawvideo",
        "--enable-parser=h264,hevc,aac,ac3,vorbis",
        "--enable-ffmpeg",
        "--enable-ffprobe",
        "--enable-pic",
        "--enable-zlib",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    if profile == Profile::Formats {
        for flag in &mut argv {
            *flag = match flag.as_str() {
                "--enable-demuxer=matroska" => "--enable-demuxer=matroska,mov,mpegts,h264,hevc,srt,ass,webvtt,sup,vobsub",
                "--enable-decoder=pcm_s16le" => {
                    "--enable-decoder=pcm_s16le,pgssub,dvdsub,dvbsub,ass,srt,subrip,text,movtext,webvtt"
                }
                "--enable-parser=h264,hevc,aac,ac3,vorbis" => "--enable-parser=h264,hevc,aac,ac3,vorbis,dvdsub,dvbsub",
                other => other,
            }
            .to_owned();
        }
        argv.push("--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata".into());
    }
    if profile == Profile::Video {
        for flag in &mut argv {
            if flag == "--enable-decoder=pcm_s16le" {
                *flag = "--enable-decoder=pcm_s16le,h264,hevc".into();
            }
        }
    }
    argv
}

/// The sorted, unique `fate-*` case names in `make fate-list` output.
pub fn fate_list(listing: &str) -> String {
    let mut names: Vec<&str> = listing
        .lines()
        .filter(|line| {
            line.strip_prefix("fate-").is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            })
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names.join("\n") + "\n"
}

pub fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|e| format!("{command:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?}: {status}"))
    }
}

pub fn output(command: &mut Command) -> Result<String, String> {
    let out = command.output().map_err(|e| format!("{command:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{command:?}: {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// Every regular file of the archive must still match the unpacked tree, so the
/// build cannot use a source that only carries the revision label.
pub fn verify_tree(archive: &[u8], cache: &Path) -> Result<usize, String> {
    let tar = crate::archive::gunzip(archive, SOURCE_LIMIT)?;
    let files = crate::archive::tar_files(&tar)?;
    for file in &files {
        let path = cache.join(&file.path);
        let same = fs::read(&path).is_ok_and(|data| sha256::hex(&data) == sha256::hex(file.data));
        if !same {
            return Err(format!("Pinned source was changed: {}", file.path));
        }
    }
    Ok(files.len())
}

pub fn prepare(
    root: &Path,
    jobs: u32,
    profile: Profile,
    net: &dyn Transport,
) -> Result<String, String> {
    if !(1..=8).contains(&jobs) {
        return Err("jobs must be 1..8".into());
    }
    let cache = root.join(profile.cache());
    fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let (source, archive_sha) = match profile.source() {
        Source::Revision => revision_source(root, &cache, profile, net)?,
        Source::Release => release_source(root, &cache, net)?,
    };
    let prefix = cache.join("install");
    let argv = configure_argv(&prefix, profile);
    let record = cache.join("configure-argv.json");
    let recorded = crate::checks::read_json(&record).ok();
    let wanted = Value::strings(argv.iter().cloned());
    let configuration = Value::object([
        ("arguments", wanted.clone()),
        ("git_discovery", Value::str("reference-source")),
    ]);
    // Keep version discovery inside the unpacked reference source.
    let build = |program: &str| {
        let mut command = Command::new(program);
        command
            .current_dir(&source)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CEILING_DIRECTORIES", &cache);
        command
    };
    let configured = source.join("config.h").exists();
    if !configured || recorded.as_ref() != Some(&configuration) {
        if configured {
            run(build("make").arg("distclean"))?;
        }
        run(build(&argv[0]).args(&argv[1..]))?;
        fs::write(&record, json::to_python(&configuration, None))
            .map_err(|e| format!("{}: {e}", record.display()))?;
    }
    run(build("make").arg(format!("-j{jobs}")))?;
    run(build("make").arg("install"))?;
    if profile.source() == Source::Release {
        return release_manifest(&cache, &prefix, profile, &archive_sha, wanted);
    }
    let samples = root.join(".research/fate-samples");
    let listing = output(
        build("make")
            .args(["-s", "fate-list"])
            .arg(format!("SAMPLES={}", samples.display())),
    )?;
    let list = fate_list(&listing);
    fs::write(cache.join("fate-list.txt"), &list).map_err(|e| e.to_string())?;
    let mut manifest = vec![
        ("schema", Value::int(1)),
        ("revision", Value::str(REVISION)),
        ("archive_sha256", Value::str(archive_sha)),
        ("configure_argv", wanted),
        ("fate_list_sha256", Value::str(sha256::hex(list.as_bytes()))),
        (
            "fate_enabled_count",
            Value::int(list.split_whitespace().count() as i64),
        ),
        (
            "note",
            Value::str(
                "Configured case identities only; upstream FATE cases have not been executed.",
            ),
        ),
    ];
    if profile != Profile::Matroska {
        let lib = prefix.join("lib");
        let mut archives = Vec::new();
        let mut entries: Vec<_> = fs::read_dir(&lib)
            .map_err(|e| format!("{}: {e}", lib.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "a"))
            .collect();
        entries.sort();
        for path in entries {
            let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let key = path
                .strip_prefix(&cache)
                .expect("library under the cache")
                .display()
                .to_string();
            archives.push((key, Value::str(sha256::hex(&data))));
        }
        manifest.push(("profile", Value::str(profile.name())));
        manifest.push(("reference_archives", Value::object(archives)));
        manifest.push((
            "compiler",
            Value::str(output(Command::new("cc").arg("--version"))?),
        ));
    }
    let path = cache.join("build.json");
    crate::corpus::write_json(&path, &Value::object(manifest))?;
    Ok(path.display().to_string())
}

/// Download (once), hash-check and unpack the pinned git revision; the unpacked
/// tree is verified against the archive on every run.
fn revision_source(
    root: &Path,
    cache: &Path,
    profile: Profile,
    net: &dyn Transport,
) -> Result<(PathBuf, String), String> {
    let name = format!("{REVISION}.tar.gz");
    let shared = root.join(".research/reference").join(&name);
    // The other builds reuse the Matroska build's verified download read-only.
    let archive_path = if profile != Profile::Matroska && shared.is_file() {
        shared
    } else {
        cache.join(&name)
    };
    if !archive_path.exists() {
        let url = format!("https://codeload.github.com/FFmpeg/FFmpeg/tar.gz/{REVISION}");
        let data = fetch::pinned(
            net,
            &[fetch::Source::Url(&url)],
            ARCHIVE_SHA256,
            ARCHIVE_LIMIT,
            "triergon-reference-builder",
        )?;
        fetch::publish(&archive_path, &data)?;
    }
    let archive = fetch::read_bounded(&archive_path, ARCHIVE_LIMIT)?;
    let archive_sha = sha256::hex(&archive);
    if archive_sha != ARCHIVE_SHA256 {
        return Err("Pinned source archive hash mismatch".into());
    }
    let source = cache.join(format!("FFmpeg-{REVISION}"));
    if !source.exists() {
        run(Command::new("tar")
            .arg("-xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(cache))?;
    }
    verify_tree(&archive, cache)?;
    Ok((source, archive_sha))
}

/// Download (once), hash-check and unpack the pinned release tarball. It is
/// xz-compressed, which xtask does not decode itself, so the unpacked tree is
/// verified by comparing the concatenated contents of every regular file in
/// archive order (as `tar` streams them) with the same files on disk.
fn release_source(
    root: &Path,
    cache: &Path,
    net: &dyn Transport,
) -> Result<(PathBuf, String), String> {
    let archive_path = root
        .join(".research/ffmpeg-release")
        .join(format!("ffmpeg-{RELEASE}.tar.xz"));
    if !archive_path.exists() {
        let url = format!("https://ffmpeg.org/releases/ffmpeg-{RELEASE}.tar.xz");
        let data = fetch::pinned(
            net,
            &[fetch::Source::Url(&url)],
            RELEASE_SHA256,
            ARCHIVE_LIMIT,
            "triergon-reference-builder",
        )?;
        let parent = archive_path.parent().expect("archive has a parent");
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        fetch::publish(&archive_path, &data)?;
    }
    let archive = fetch::read_bounded(&archive_path, ARCHIVE_LIMIT)?;
    let archive_sha = sha256::hex(&archive);
    if archive_sha != RELEASE_SHA256 {
        return Err("Pinned release archive hash mismatch".into());
    }
    let source = cache.join(format!("ffmpeg-{RELEASE}"));
    if !source.exists() {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&archive_path)
            .arg("-C")
            .arg(cache))?;
    }
    verify_xz_tree(&archive_path, cache)?;
    Ok((source, archive_sha))
}

/// A release tarball pinned by URL and SHA-256.
pub struct PinnedRelease<'a> {
    /// The tarball's file name, which is also the cache entry.
    pub file: &'a str,
    /// The top-level directory it unpacks to.
    pub directory: &'a str,
    /// The release's own address, tried first.
    pub url: &'a str,
    /// Further sources of the same bytes, tried in order when `url` fails.
    pub mirrors: &'a [fetch::Source<'a>],
    pub sha256: &'a str,
}

impl<'a> PinnedRelease<'a> {
    /// Where the archive is fetched from, in order: the release's own address,
    /// then its mirrors.
    pub fn sources(&self) -> Vec<fetch::Source<'a>> {
        std::iter::once(fetch::Source::Url(self.url))
            .chain(self.mirrors.iter().copied())
            .collect()
    }
}

/// Downloads `release` into `cache` once, checks its hash, unpacks it and verifies
/// the unpacked tree. Returns the source directory.
pub fn unpack_release(
    cache: &Path,
    release: &PinnedRelease<'_>,
    net: &dyn Transport,
) -> Result<PathBuf, String> {
    fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let archive_path = cache.join(release.file);
    if !archive_path.exists() {
        let data = fetch::pinned(
            net,
            &release.sources(),
            release.sha256,
            ARCHIVE_LIMIT,
            "triergon-reference-builder",
        )?;
        fetch::publish(&archive_path, &data)?;
    }
    let archive = fetch::read_bounded(&archive_path, ARCHIVE_LIMIT)?;
    if sha256::hex(&archive) != release.sha256 {
        return Err(format!("Pinned archive hash mismatch: {}", release.file));
    }
    let source = cache.join(release.directory);
    if !source.exists() {
        run(Command::new("tar")
            .arg("-xJf")
            .arg(&archive_path)
            .arg("-C")
            .arg(cache))?;
    }
    verify_xz_tree(&archive_path, cache)?;
    Ok(source)
}

/// Every regular file of an xz-compressed release archive must still match the tree
/// unpacked under `cache`. xtask does not decode xz itself, so the concatenated
/// contents of the files, in archive order as `tar` streams them, are compared
/// with the same files on disk.
pub fn verify_xz_tree(archive_path: &Path, cache: &Path) -> Result<(), String> {
    let listing = output(Command::new("tar").arg("-tvJf").arg(archive_path))?;
    let streamed = Command::new("tar")
        .arg("-xJOf")
        .arg(archive_path)
        .output()
        .map_err(|e| format!("tar: {e}"))?;
    if !streamed.status.success() {
        return Err(format!("tar -xJOf: {}", streamed.status));
    }
    let mut unpacked = Vec::with_capacity(streamed.stdout.len());
    for line in listing.lines().filter(|line| line.starts_with('-')) {
        let name = line
            .split_whitespace()
            .last()
            .ok_or_else(|| format!("unreadable listing line: {line}"))?;
        let path = cache.join(name);
        let data =
            fs::read(&path).map_err(|e| format!("Pinned release was changed: {name}: {e}"))?;
        unpacked.extend(data);
    }
    if sha256::hex(&unpacked) != sha256::hex(&streamed.stdout) {
        return Err("Pinned release tree was changed".into());
    }
    Ok(())
}

/// The identity of a release profile build: archive, configure line, the
/// installed libraries and programs, and the compiler.
fn release_manifest(
    cache: &Path,
    prefix: &Path,
    profile: Profile,
    archive_sha: &str,
    configure: Value,
) -> Result<String, String> {
    let mut installed = Vec::new();
    for directory in ["lib", "bin"] {
        let Ok(entries) = fs::read_dir(prefix.join(directory)) else {
            continue;
        };
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && !p.is_symlink())
            .collect();
        paths.sort();
        for path in paths {
            let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let key = path
                .strip_prefix(cache)
                .expect("installed under the cache")
                .display()
                .to_string();
            installed.push((key, Value::str(sha256::hex(&data))));
        }
    }
    let manifest = Value::object(vec![
        ("schema", Value::int(1)),
        ("release", Value::str(RELEASE)),
        ("archive_sha256", Value::str(archive_sha)),
        ("configure_argv", configure),
        ("profile", Value::str(profile.name())),
        ("installed", Value::object(installed)),
        (
            "compiler",
            Value::str(output(Command::new("cc").arg("--version"))?),
        ),
    ]);
    let path = cache.join("build.json");
    crate::corpus::write_json(&path, &manifest)?;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_fetched_from_its_address_then_its_mirrors() {
        let release = PinnedRelease {
            file: "x.tar.xz",
            directory: "x",
            url: "https://example.org/x.tar.xz",
            mirrors: &[fetch::Source::GitHubAsset {
                repo: "owner/x",
                id: 7,
            }],
            sha256: "0",
        };
        let sources = format!("{:?}", release.sources());
        assert_eq!(
            sources,
            r#"[Url("https://example.org/x.tar.xz"), GitHubAsset { repo: "owner/x", id: 7 }]"#
        );
    }

    #[test]
    fn lists_unique_sorted_fate_names_only() {
        let listing = "make: noise\nfate-b\nfate-a.x_1\nfate-b\nfate-\nfate-bad name\n  fate-indented\nfate-c-d\n";
        assert_eq!(fate_list(listing), "fate-a.x_1\nfate-b\nfate-c-d\n");
    }

    #[test]
    fn formats_profile_extends_the_matroska_configuration() {
        let prefix = Path::new("/p/install");
        let matroska = configure_argv(prefix, Profile::Matroska);
        let formats = configure_argv(prefix, Profile::Formats);
        assert_eq!(matroska[1], "--prefix=/p/install");
        assert_eq!(matroska.len() + 1, formats.len());
        assert!(formats.contains(
            &"--enable-demuxer=matroska,mov,mpegts,h264,hevc,srt,ass,webvtt,sup,vobsub".to_owned()
        ));
        assert_eq!(
            formats.last().unwrap(),
            "--enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata"
        );
    }

    #[test]
    fn capture_profiles_differ_only_in_size_optimization() {
        let prefix = Path::new("/p/install");
        let speed = configure_argv(prefix, Profile::Capture);
        let small = configure_argv(prefix, Profile::CaptureSmall);
        assert!(speed.iter().any(|arg| arg == "--enable-static"));
        assert!(speed.iter().any(|arg| arg == "--disable-shared"));
        assert_eq!(
            speed,
            small
                .into_iter()
                .filter(|arg| arg != "--enable-small")
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn verifies_the_unpacked_tree_against_the_archive() {
        let dir = crate::walk::scratch("tree");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.c"), b"int a;").unwrap();
        let mut header = vec![0u8; 512];
        header[..7].copy_from_slice(b"src/a.c");
        header[124..135].copy_from_slice(b"00000000006");
        header[156] = b'0';
        let mut tar = header;
        tar.extend(b"int a;");
        tar.resize(1024, 0);
        tar.extend([0; 1024]);
        let deflated = crate::zlib::compress(&tar, 6, crate::zlib::Strategy::Default).unwrap();
        let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
        gz.extend_from_slice(&deflated[2..deflated.len() - 4]);
        gz.extend(crate::zlib::crc32(&tar).to_le_bytes());
        gz.extend((tar.len() as u32).to_le_bytes());
        assert_eq!(verify_tree(&gz, &dir).unwrap(), 1);
        fs::write(dir.join("src/a.c"), b"int b;").unwrap();
        assert_eq!(
            verify_tree(&gz, &dir).unwrap_err(),
            "Pinned source was changed: src/a.c"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn video_profile_adds_only_the_h264_and_hevc_decoders() {
        let prefix = Path::new("/p");
        let matroska = configure_argv(prefix, Profile::Matroska);
        let video = configure_argv(prefix, Profile::Video);
        let changed: Vec<_> = matroska
            .iter()
            .zip(&video)
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(matroska.len(), video.len());
        assert_eq!(
            changed,
            [(
                &"--enable-decoder=pcm_s16le".to_owned(),
                &"--enable-decoder=pcm_s16le,h264,hevc".to_owned()
            )]
        );
    }
}
