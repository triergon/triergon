//! The pinned FFmpeg reference: download, verify, configure, build and list the
//! configure-expanded FATE cases. No sample is downloaded and no upstream test
//! is executed.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::fetch::{self, Transport};
use crate::json::{self, Value};
use crate::sha256;

pub const REVISION: &str = "d32b387f2b0a484599d4587d651891f0c63c4238";
pub const ARCHIVE_SHA256: &str = "8a830a34bfaf98514b5d45cf6c01b1fe78b38d5e4c10eab0de2531b783c15f90";
const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
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
}

impl Profile {
    /// The cache directory under the repository root.
    pub fn cache(self) -> &'static str {
        match self {
            Profile::Matroska => ".research/reference",
            Profile::Formats => ".research/reference-formats",
            Profile::Video => ".research/reference-video",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Matroska => "matroska",
            Profile::Formats => "formats",
            Profile::Video => "video",
        }
    }
}

pub fn configure_argv(prefix: &Path, profile: Profile) -> Vec<String> {
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
    .map(|s| s.to_string())
    .collect();
    if profile == Profile::Formats {
        for flag in &mut argv {
            *flag = match flag.as_str() {
                "--enable-demuxer=matroska" => "--enable-demuxer=matroska,mov,mpegts,h264,hevc,srt,ass,webvtt,sup,vobsub",
                "--enable-decoder=pcm_s16le" => {
                    "--enable-decoder=pcm_s16le,pgssub,dvdsub,dvbsub,ass,srt,subrip,text,mov_text,webvtt"
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

fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|e| format!("{command:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?}: {status}"))
    }
}

fn output(command: &mut Command) -> Result<String, String> {
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
        let data = net.https(&url, "triergon-reference-builder", ARCHIVE_LIMIT)?;
        if data.len() > ARCHIVE_LIMIT {
            return Err("source archive size limit".into());
        }
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
            .arg(&cache))?;
    }
    verify_tree(&archive, &cache)?;

    let prefix = cache.join("install");
    let argv = configure_argv(&prefix, profile);
    let record = cache.join("configure-argv.json");
    let recorded = crate::checks::read_json(&record).ok();
    let wanted = Value::strings(argv.iter().cloned());
    if !source.join("config.h").exists() || recorded.as_ref() != Some(&wanted) {
        run(Command::new(&argv[0]).args(&argv[1..]).current_dir(&source))?;
        fs::write(&record, json::to_python(&wanted, None))
            .map_err(|e| format!("{}: {e}", record.display()))?;
    }
    run(Command::new("make")
        .arg(format!("-j{jobs}"))
        .current_dir(&source))?;
    run(Command::new("make").arg("install").current_dir(&source))?;
    let samples = root.join(".research/fate-samples");
    let listing = output(
        Command::new("make")
            .args(["-s", "fate-list"])
            .arg(format!("SAMPLES={}", samples.display()))
            .current_dir(&source),
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

#[cfg(test)]
mod tests {
    use super::*;

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
