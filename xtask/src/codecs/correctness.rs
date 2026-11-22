//! Codec correctness grading, by the rules of `harness/codecs/numeric-rules.json`.
//!
//! Two levels per vector:
//! - decoder: the codec crate's example decoder (planar YUV, interleaved f32)
//!   against the pinned FFmpeg decoder output (`-f rawvideo -pix_fmt
//!   yuv420p|yuv420p10le`, `-f f32le`): video bit-exact per frame, audio
//!   per-channel RMS and max |d| with exact counts;
//! - output: `codec-worker` against the decode build of `harness/reference.c`:
//!   RGBA bit-exact per picture, stereo s16 within the LSB rule.
//!
//! Nothing is excluded: refusals, crashes, count mismatches and FFmpeg
//! refusals are recorded per vector as they happen. Selection (in set or not)
//! is applied afterwards by the summary, never by skipping inputs here.

mod native_audio;
mod play;
mod session;
mod video;

use super::damage;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::json::{self, Value};
use crate::process::{Frames, OUT, Stop, TempDir, run_text};
use crate::{numpy, python, sha256};

/// Correctness manifest schema written by [`manifest`].
pub const MANIFEST_SCHEMA: &str = "triergon.codecs.correctness-manifest.v1";

pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(900);
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// One output picture: 1920x1080 RGBA.
const CANVAS: usize = 1920 * 1080 * 4;
const AAC_SINGLE_SCE_STEREO_SHA256: &str =
    "04947370585296a25c63f1e31252c4290328151a0fa2ecf3271518b4d4db8796";
const AAC_CONTAINER_PRIMING: &str = "aac-container-priming";

/// The example exposes a decoder policy the device-output worker does not select.
pub(super) fn decoder_only_lane(lane: &str) -> bool {
    matches!(lane, "aac-mov" | AAC_CONTAINER_PRIMING)
        || damage::recipe_manifest(lane).is_some()
        || lane == "track-reset"
}

fn uses_initialized_aac_reference(lane: &str, name: &str, sha256: &str) -> bool {
    matches!(lane, "aac" | "session-aac" | AAC_CONTAINER_PRIMING)
        && name == "aac-sce-in-stereo.adts"
        && sha256 == AAC_SINGLE_SCE_STEREO_SHA256
}

/// Which manifest vectors a lane uses when it changes a decoder option.
pub fn vector_lane(lane: &str) -> &str {
    match lane {
        AAC_CONTAINER_PRIMING => "aac",
        lane => lane,
    }
}

/// Base manifest lanes a session comparison reuses.
pub fn session_vector_lanes(lane: &str) -> &'static [&'static str] {
    match lane {
        "session-aac" => &["aac"],
        "session-ac3" => &["ac3"],
        "session-eac3" => &["eac3"],
        "session-dts" => &["dts"],
        "session-flac" => &["flac"],
        "session-mp3" => &["mp1", "mp2", "mp3"],
        "session-vorbis" => &["vorbis"],
        "session-opus" => &["opus"],
        "session-h264" => &["h264"],
        "session-hevc" => &["hevc"],
        _ => &[],
    }
}

/// The pinned inputs of a lane that does not reuse the base manifest.
pub fn variant_manifest(lane: &str) -> Option<&'static str> {
    if play::lane(lane) {
        return Some(play::MANIFEST);
    }
    damage::recipe_manifest(lane).or_else(|| {
        (lane.starts_with("session-") || lane == "track-reset").then_some(session::CASES)
    })
}

fn decoder_lane(lane: &str) -> &str {
    match lane {
        "flac-crc-reject" => "flac",
        "ac3-crc-reject" => "ac3",
        "eac3-crc-reject" => "eac3",
        "dts-packed" => "dts",
        lane => lane,
    }
}

fn complete_single_sce_stereo(sample_bytes: usize, outputs: [(i64, usize); 2]) -> bool {
    const SAMPLES_PER_CHANNEL: usize = 217 * 1024;
    let bytes = SAMPLES_PER_CHANNEL * 2 * sample_bytes;
    outputs == [(0, bytes); 2]
}

fn video_extensions(lane: &str) -> Option<&'static [&'static str]> {
    match lane {
        "h264" => Some(&[".264", ".26l", ".avc", ".h264", ".jsv", ".jvt"]),
        "hevc" => Some(&[".bit", ".bin"]),
        _ => None,
    }
}

fn audio_suffixes(lane: &str) -> Option<&'static [&'static str]> {
    match lane {
        "ac3" => Some(&[".ac3"]),
        "eac3" => Some(&[".eac3"]),
        "aac" => Some(&[".adts"]),
        "aac-mov" => Some(&[".mov", ".mp4", ".m4a"]),
        "dts" => Some(&[".core.dts"]),
        "flac" => Some(&[".flac"]),
        "mp3" => Some(&[".bit", ".mp3"]),
        "mp2" => Some(&[".mp2"]),
        "mp1" => Some(&[".mp1"]),
        "vorbis" => Some(&[".ogg"]),
        "opus" => Some(&[".opus", ".mka"]),
        _ => None,
    }
}

/// The demuxer FFmpeg is told to use for a lane's vectors; `None` lets it probe (Opus
/// vectors come in Ogg and in Matroska).
fn audio_format(lane: &str) -> Result<Option<&'static str>, String> {
    Ok(match decoder_lane(lane) {
        "ac3" => Some("ac3"),
        "eac3" => Some("eac3"),
        "aac" | AAC_CONTAINER_PRIMING => Some("aac"),
        "aac-mov" => Some("mov"),
        "dts" => Some("dts"),
        "flac" => Some("flac"),
        "mp3" | "mp2" | "mp1" => Some("mp3"),
        "vorbis" => Some("ogg"),
        "opus" => None,
        _ => return Err(unknown(lane)),
    })
}

/// The FFmpeg decoder a lane is compared with, where the stream alone does not decide
/// it: the native decoders, and the float decoders for MPEG audio, whose Layer I and II
/// defaults are fixed point with 16-bit output.
fn reference_decoder(lane: &str) -> Option<&'static str> {
    Some(match decoder_lane(lane) {
        "aac-mov" => "aac",
        "flac" => "flac",
        "mp3" => "mp3float",
        "mp2" => "mp2float",
        "mp1" => "mp1float",
        "vorbis" => "vorbis",
        "opus" => "opus",
        _ => return None,
    })
}

fn example(lane: &str) -> Option<&'static str> {
    Some(match decoder_lane(lane) {
        "ac3-nodither" => "codec-ac3-decode",
        "track-reset" => "audio-decode",
        lane if !session_vector_lanes(lane).is_empty() => "session_decode",
        "h264" => "codec-h264-decode",
        "hevc" => "codec-hevc-decode",
        "ac3" | "eac3" => "codec-ac3-decode",
        "aac" | AAC_CONTAINER_PRIMING => "codec-aac-decode",
        "dts" => "codec-dca-decode",
        // File-format lanes decode through a container reader, as a player does.
        "aac-mov" | "flac" | "mp3" | "mp2" | "mp1" | "vorbis" | "opus" => "audio-decode",
        _ => return None,
    })
}

/// Whether the correctness runner can grade this lane.
pub fn is_lane(lane: &str) -> bool {
    example(lane).is_some() || play::lane(lane)
}

pub(super) fn output_only_lane(lane: &str) -> bool {
    play::lane(lane)
}

fn operation(lane: &str) -> Option<&'static str> {
    Some(match lane {
        "h264" => "video-h264",
        "hevc" => "video-hevc",
        "ac3" => "audio-ac3",
        "eac3" => "audio-eac3",
        "aac" => "audio-aac",
        "dts" => "audio-dts",
        "flac" => "audio-flac",
        "mp3" | "mp2" | "mp1" => "audio-mp3",
        "vorbis" => "audio-vorbis",
        "opus" => "audio-opus",
        _ => return None,
    })
}

fn unknown(lane: &str) -> String {
    format!("unknown lane {}", python::repr_str(lane))
}

/// SHA-256 of a file.
pub fn file_sha(path: &Path) -> Result<String, String> {
    sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Every path below `dir` as `Path.rglob('*')` lists it, sorted: symbolic
/// links are listed but not entered.
fn rglob(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        out.push(path.clone());
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(rglob(&path));
        }
    }
    out.sort();
    out
}

/// `manifest --out M.json LANE=DIR[,DIR...] ...`: hash every vector of each lane.
pub fn manifest(out: &Path, lanes: &[String]) -> Result<(), String> {
    let mut rows = Vec::new();
    for spec in lanes {
        let (lane, dirs) = spec
            .split_once('=')
            .ok_or_else(|| format!("{spec}: expected LANE=DIR[,DIR...]"))?;
        for dir in dirs.split(',') {
            for path in rglob(Path::new(dir)) {
                if !path.is_file() {
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(extensions) = video_extensions(lane) {
                    let suffix = suffix(&name);
                    if !extensions.contains(&suffix) {
                        continue;
                    }
                } else {
                    let want = audio_suffixes(lane)
                        .or_else(|| super::readers::suffixes(lane))
                        .ok_or_else(|| unknown(lane))?;
                    if !want.iter().any(|suffix| name.ends_with(suffix)) {
                        continue;
                    }
                }
                let bytes = fs::metadata(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?
                    .len();
                rows.push(Value::object([
                    ("lane", Value::str(lane)),
                    ("name", Value::str(name.as_str())),
                    ("path", Value::str(python::path(&path))),
                    ("bytes", Value::int(bytes)),
                    ("sha256", Value::str(file_sha(&path)?)),
                ]));
            }
        }
    }
    let count = rows.len();
    let document = Value::object([
        ("schema", Value::str(MANIFEST_SCHEMA)),
        ("vectors", Value::Array(rows)),
    ]);
    fs::write(out, json::to_python(&document, Some(1)) + "\n")
        .map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{count} vectors");
    Ok(())
}

/// `PurePath.suffix`: the final `.`-part of a name, empty for dot files.
fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(at) if at > 0 && at < name.len() - 1 => &name[at..],
        _ => "",
    }
}

/// The grading limits of `harness/codecs/numeric-rules.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    /// Per-channel RMS of the float audio difference.
    pub rms: f64,
    /// Per-sample absolute float audio difference.
    pub abs: f64,
    /// Per-sample s16 difference of the stereo output path, in LSB.
    pub lsb: i64,
    /// Lanes whose decoder output must equal the reference exactly.
    pub exact: Vec<String>,
    /// Maximum absolute difference for the dither-free AC-3 controls.
    pub ac3_dither_free_abs: f64,
}

impl Rules {
    pub fn load(root: &Path) -> Result<Rules, String> {
        let path = root.join("harness/codecs/numeric-rules.json");
        let rules = json::read(&path)?;
        let number = |section: &str, key: &str| {
            rules
                .get(section)
                .and_then(|s| s.get(key))
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("{}: missing {section}.{key}", path.display()))
        };
        let exact = rules
            .get("lossless_audio_decode")
            .and_then(|s| s.get("applies_to"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "{}: missing lossless_audio_decode.applies_to",
                    path.display()
                )
            })?
            .iter()
            .map(|lane| lane.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("{}: lossless lanes are not strings", path.display()))?;
        Ok(Rules {
            rms: number("audio_decode", "per_channel_rms_max")?,
            abs: number("audio_decode", "per_sample_abs_max")?,
            lsb: number("audio_device_path", "per_sample_abs_max_lsb")? as i64,
            exact,
            ac3_dither_free_abs: number("ac3_dither_free_exact", "per_sample_abs_max")?,
        })
    }
}

/// The arguments of `grade`.
pub struct Grade {
    pub manifest: PathBuf,
    pub root: PathBuf,
    pub bin: PathBuf,
    pub ffmpeg: String,
    pub ffprobe: String,
    /// The codec worker of each lane's operation.
    pub codec_workers: super::Candidates,
    pub reference: String,
    pub out: PathBuf,
    pub scratch: Option<PathBuf>,
    pub only: Option<String>,
    /// The lanes to grade, by exact name; all when `None`.
    pub lanes: Option<Vec<String>>,
    pub rules: Rules,
}

impl Grade {
    fn example(&self, lane: &str) -> Result<String, String> {
        Ok(python::path(
            &self.bin.join(example(lane).ok_or_else(|| unknown(lane))?),
        ))
    }
}

/// Decoder diagnostics: JSON object lines of a decoder's stderr.
fn json_lines(text: &str) -> Vec<Value> {
    python::splitlines(text)
        .into_iter()
        .map(python::strip)
        .filter(|line| line.starts_with('{'))
        .filter_map(python::loads)
        .collect()
}

/// `ffprobe` facts of the first video (or audio) stream; `None` when it finds none.
fn probe(
    ffprobe: &str,
    path: &str,
    format: Option<&str>,
    audio: bool,
) -> Result<(i64, Option<Value>), Stop> {
    let entries = if audio {
        "stream=sample_rate,channels,channel_layout"
    } else {
        "stream=width,height,pix_fmt,sample_aspect_ratio,color_range,color_space"
    };
    let mut argv = vec![ffprobe.to_owned(), "-v".into(), "error".into()];
    if let Some(format) = format {
        argv.extend(["-f".into(), format.to_owned()]);
    }
    argv.extend(
        [
            "-select_streams",
            if audio { "a:0" } else { "v:0" },
            "-show_entries",
            entries,
            "-of",
            "json",
            path,
        ]
        .map(String::from),
    );
    let done = run_text(&argv, PROBE_TIMEOUT)?;
    let stream = python::loads(&done.stdout).and_then(|v| {
        v.get("streams")
            .and_then(Value::as_array)
            .and_then(|s| s.first().cloned())
    });
    Ok((done.code, stream))
}

/// Run the planar decoder diagnostic on individual streams without a manifest.
pub(super) fn compare_hevc(
    decoder: &str,
    ffmpeg: &str,
    ffprobe: &str,
    paths: &[String],
) -> Result<i32, String> {
    let mut failed = false;
    for path in paths {
        fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
        let decoded = match video::decoder(ffprobe, ffmpeg, decoder, "hevc", path) {
            Ok(row) => row,
            Err(Stop::Timeout(argv)) => timeout_row(argv),
            Err(Stop::Fatal(message)) => return Err(message),
        };
        failed |= decoded.get("status").and_then(Value::as_str) != Some("pass");
        let row = Value::object([("path", Value::str(path)), ("decoder", decoded)]);
        println!("{}", json::to_python(&row, None));
    }
    Ok(i32::from(failed))
}

/// Little-endian `f32` samples; a length that is not a whole number of
/// samples is an error, as `np.frombuffer` makes it one.
fn f32_samples(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if !bytes.len().is_multiple_of(4) {
        return Err("buffer size must be a multiple of element size".into());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Per-channel RMS and per-channel max |d|.
type ChannelStats = (Vec<f64>, Vec<f64>);

/// Per-channel RMS and max |candidate - reference| of interleaved f32 audio,
/// reduced in numpy's order. `None` indicates incompatible lengths or non-finite samples.
pub fn channel_stats(
    candidate: &[u8],
    reference: &[u8],
    channels: i128,
) -> Result<Option<ChannelStats>, String> {
    let c = f32_samples(candidate)?;
    let r = f32_samples(reference)?;
    if channels <= 0 || c.len() != r.len() || c.len() % channels as usize != 0 {
        return Ok(None);
    }
    if c.iter().chain(&r).any(|sample| !sample.is_finite()) {
        return Ok(None);
    }
    let channels = channels as usize;
    if c.is_empty() {
        return Ok(Some((vec![0.0; channels], vec![0.0; channels])));
    }
    let d: Vec<f64> = c
        .iter()
        .zip(&r)
        .map(|(&a, &b)| f64::from(a) - f64::from(b))
        .collect();
    let squares: Vec<f64> = d.iter().map(|x| x * x).collect();
    let rms = numpy::column_means(&squares, channels)
        .into_iter()
        .map(f64::sqrt)
        .collect();
    let magnitudes: Vec<f64> = d.iter().map(|x| x.abs()).collect();
    Ok(Some((rms, numpy::column_max(&magnitudes, channels))))
}

/// `None` for a missing key or JSON null (Python's `None`).
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| !matches!(v, Value::Null))
}

/// `len(data) // 4 // channels` for a decoder-reported channel count.
fn samples_per_channel(bytes: usize, channels: Option<&Value>) -> Result<Value, String> {
    let Some(channels) = channels.filter(|v| v.truthy()) else {
        return Ok(Value::Null);
    };
    let frames = (bytes / 4) as i128;
    match channels {
        Value::Bool(_) => Ok(Value::int(frames)),
        Value::Number(n) => match n.as_i128() {
            Some(c) => Ok(Value::int(python::floor_div(frames, c))),
            None => Ok(Value::float((frames as f64 / n.as_f64()).floor())),
        },
        other => Err(format!(
            "unsupported operand type(s) for //: 'int' and {}",
            python::repr(other)
        )),
    }
}

fn crc_reject(lane: &str) -> bool {
    matches!(
        lane,
        "flac-crc-reject" | "ac3-crc-reject" | "eac3-crc-reject"
    )
}

fn reference_frames(
    a: &Grade,
    path: &str,
    format: &str,
) -> Result<(i64, Vec<Value>, String), Stop> {
    let argv = [
        a.ffprobe.as_str(),
        "-v",
        "error",
        "-f",
        format,
        "-err_detect",
        "crccheck",
        "-select_streams",
        "a:0",
        "-show_frames",
        "-show_entries",
        "frame=pts,nb_samples",
        "-of",
        "json",
        path,
    ]
    .map(String::from);
    let probe = run_text(&argv, PROBE_TIMEOUT)?;
    let frames = python::loads(&probe.stdout)
        .and_then(|doc| {
            doc.get("frames")
                .and_then(Value::as_array)
                .map(<[Value]>::to_vec)
        })
        .unwrap_or_default();
    Ok((probe.code, frames, probe.stderr))
}

fn audio_decoder(
    a: &Grade,
    lane: &str,
    path: &str,
    tmp: &Path,
    initialized_aac: bool,
    vector: &Value,
    source: Option<&str>,
) -> Result<Value, Stop> {
    let through_session = lane.starts_with("session-");
    let media_lane = if through_session || lane == "track-reset" {
        session::base(lane, vector)?
    } else {
        decoder_lane(lane).to_owned()
    };
    let format = if lane == "ac3-nodither" {
        vector.get("format").and_then(Value::as_str)
    } else {
        audio_format(&media_lane)?
    };
    let reject = crc_reject(lane);
    let coreless = matches!(lane, "dts-packed" | "session-dts")
        && vector.get("coreless") == Some(&Value::Bool(true));
    let (_, stream) = probe(&a.ffprobe, path, format, true)?;
    let (reference_path, ours_path) = (tmp.join("ref.f32"), tmp.join("ours.f32"));
    let mut reference_argv: Vec<String> = [
        a.ffmpeg.as_str(),
        "-nostdin",
        "-v",
        "error",
        "-threads",
        "1",
    ]
    .map(String::from)
    .to_vec();
    if let Some(format) = format {
        reference_argv.extend(["-f".to_owned(), format.to_owned()]);
    }
    if let Some(decoder) = reference_decoder(&media_lane) {
        reference_argv.extend(["-c:a".to_owned(), decoder.to_owned()]);
    }
    if coreless {
        reference_argv.extend(["-core_only", "1"].map(String::from));
    }
    if lane == "aac-mov" {
        // Compare the complete decoder packets from the neutral MOV view.
        reference_argv.extend(
            [
                "-xerror",
                "-flags2",
                "+skip_manual",
                "-ignore_editlist",
                "1",
            ]
            .map(String::from),
        );
    }
    if lane == AAC_CONTAINER_PRIMING {
        reference_argv.extend(["-flags2".into(), "+skip_manual".into()]);
    }
    if reject {
        reference_argv.extend(["-err_detect", "crccheck"].map(String::from));
    }
    reference_argv.extend(["-i", path].map(String::from));
    if lane == "aac-mov" {
        reference_argv.extend(["-map", "0:a:0"].map(String::from));
    }
    reference_argv.extend(
        [
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "-y",
            &python::path(&reference_path),
        ]
        .map(String::from),
    );
    let reset_reference = session::reference_args(
        a,
        &media_lane,
        path,
        &python::path(&reference_path),
        vector,
        "planar",
    );
    let reference_run = if let Some(args) = &reset_reference {
        run_text(args, RUN_TIMEOUT)?
    } else if initialized_aac {
        let worker = Path::new(&a.reference).with_file_name(super::INITIALIZED_AAC_REFERENCE);
        run_text(
            &[
                python::path(&worker),
                path.to_owned(),
                python::path(&reference_path),
            ],
            RUN_TIMEOUT,
        )?
    } else {
        run_text(&reference_argv, RUN_TIMEOUT)?
    };
    let no_noise = vector.get("noise_off") == Some(&Value::Bool(true));
    let ours_input = if no_noise {
        source.ok_or_else(|| "NoiseMode::Off needs the original stream".to_owned())?
    } else {
        path
    };
    let mut ours_argv = if through_session {
        session::args(
            a,
            &media_lane,
            ours_input,
            &python::path(&ours_path),
            tmp,
            vector,
            "planar",
        )?
    } else {
        vec![
            a.example(lane)?,
            ours_input.to_owned(),
            python::path(&ours_path),
        ]
    };
    if lane == "track-reset" {
        session::append_options(&mut ours_argv, vector);
    }
    match lane {
        "flac-crc-reject" => ours_argv.extend(["--crc", "reject"].map(String::from)),
        "ac3-crc-reject" | "eac3-crc-reject" => ours_argv.push("--reject-crc".into()),
        "ac3-nodither" if no_noise => ours_argv.push("--no-noise".into()),
        _ => {}
    }
    if lane == AAC_CONTAINER_PRIMING {
        ours_argv.extend(["--priming".into(), "container".into()]);
    }
    let ours_run = run_text(&ours_argv, RUN_TIMEOUT)?;
    let mut diagnostics = json_lines(&ours_run.stderr);
    let info = if through_session {
        diagnostics
            .iter()
            .rev()
            .find(|row| row.get("channels").is_some())
            .cloned()
    } else {
        diagnostics.pop()
    }
    .unwrap_or_else(|| Value::object::<&str>([]));
    let ours_channels = present(info.get("channels"));
    let ours_rate = match info.get("sample_rate") {
        Some(rate) => present(Some(rate)),
        None => present(info.get("rate")),
    };
    let reference = fs::read(&reference_path).unwrap_or_default();
    let candidate = fs::read(&ours_path).unwrap_or_default();
    let stream = stream.filter(Value::truthy);
    let reference_number = |key: &str| -> Result<Option<i128>, String> {
        match stream
            .as_ref()
            .and_then(|s| s.get(key))
            .filter(|v| v.truthy())
        {
            None => Ok(None),
            Some(value) => python::int(value)
                .map(Some)
                .ok_or_else(|| format!("invalid literal for int(): {}", python::repr(value))),
        }
    };
    let reference_channels = reference_number("channels")?;
    let reference_rate = reference_number("sample_rate")?;
    let optional = |v: Option<i128>| v.map_or(Value::Null, Value::int);
    let mut row = vec![
        ("ffmpeg_rate".to_owned(), optional(reference_rate)),
        (
            "ours_rate".to_owned(),
            ours_rate.cloned().unwrap_or(Value::Null),
        ),
        ("ffmpeg_channels".to_owned(), optional(reference_channels)),
        (
            "ours_channels".to_owned(),
            ours_channels.cloned().unwrap_or(Value::Null),
        ),
        (
            "ffmpeg_layout".to_owned(),
            stream
                .as_ref()
                .and_then(|s| s.get("channel_layout"))
                .cloned()
                .unwrap_or(Value::Null),
        ),
        (
            "ours_layout".to_owned(),
            info.get("layout").cloned().unwrap_or(Value::Null),
        ),
        (
            "ffmpeg_samples".to_owned(),
            match reference_channels.filter(|&c| c != 0) {
                Some(c) => Value::int(python::floor_div((reference.len() / 4) as i128, c)),
                None => Value::Null,
            },
        ),
        (
            "ours_samples".to_owned(),
            samples_per_channel(candidate.len(), ours_channels)?,
        ),
        ("ours_rc".to_owned(), Value::int(ours_run.code)),
        ("ffmpeg_rc".to_owned(), Value::int(reference_run.code)),
        ("ours_info".to_owned(), info.clone()),
        (
            "ffmpeg_stderr_tail".to_owned(),
            Value::str(python::tail(&reference_run.stderr, 300)),
        ),
    ];
    if variant_manifest(lane).is_some() {
        row.extend([
            ("ours_pcm_bytes".into(), Value::int(candidate.len() as u64)),
            (
                "ffmpeg_pcm_bytes".into(),
                Value::int(reference.len() as u64),
            ),
            (
                "ours_stderr_tail".into(),
                Value::str(python::tail(&ours_run.stderr, 500)),
            ),
        ]);
    }
    if initialized_aac {
        row.push((
            "float_reference".into(),
            Value::str(super::INITIALIZED_AAC_REFERENCE),
        ));
    }
    let status = |row: &mut Vec<(String, Value)>, status: &str| {
        row.push(("status".into(), Value::str(status)));
    };
    if coreless {
        let ours_refused = if through_session {
            ours_run.code > 0 && ours_run.stderr.contains("no DTS core sync")
        } else {
            ours_run.code == 0 && info.get("frames").and_then(python::int) == Some(0)
        };
        let both_refuse =
            candidate.is_empty() && ours_refused && no_dts_core(&reference_run, &reference);
        let result = match (both_refuse, through_session) {
            (true, true) => "ffmpeg-refused",
            (true, false) => "pass",
            (false, _) => "fail",
        };
        status(&mut row, result);
        return Ok(Value::Object(row));
    }
    if initialized_aac
        && !complete_single_sce_stereo(
            size_of::<f32>(),
            [
                (ours_run.code, candidate.len()),
                (reference_run.code, reference.len()),
            ],
        )
    {
        status(&mut row, "fail");
        return Ok(Value::Object(row));
    }
    if through_session
        && reference.is_empty()
        && candidate.is_empty()
        && ours_run.code > 0
        && reference_run.code > 0
    {
        // The two commands independently refused the file before producing PCM.
        status(&mut row, "ffmpeg-refused");
        row.push((
            "ours_stderr_tail".into(),
            Value::str(python::tail(&ours_run.stderr, 300)),
        ));
        return Ok(Value::Object(row));
    }
    let mut same_frames = true;
    if through_session {
        let reset_rows = reset_reference
            .as_ref()
            .map(|_| json_lines(&reference_run.stderr));
        let timeline =
            session::timeline(a, path, &media_lane, &diagnostics, reset_rows.as_deref())?;
        same_frames = timeline.get("status").and_then(Value::as_str) == Some("pass");
        row.push(("timeline".into(), timeline));
    }
    if reject {
        let format = format.ok_or_else(|| "CRC lane needs an explicit format".to_owned())?;
        let (code, frames, errors) = reference_frames(a, path, format)?;
        let candidate_count = info.get("frames").and_then(python::int);
        let candidate_shape = diagnostics.iter().find_map(|v| v.get("frame_samples"));
        let reference_shape = Value::Array(
            frames
                .iter()
                .map(|v| v.get("nb_samples").cloned().unwrap_or(Value::Null))
                .collect(),
        );
        same_frames = candidate_count == Some(frames.len() as i128)
            && (lane != "flac-crc-reject" || candidate_shape == Some(&reference_shape));
        row.extend([
            ("ffmpeg_frame_probe_rc".into(), Value::int(code)),
            ("ffmpeg_frames".into(), Value::Array(frames)),
            (
                "ffmpeg_frame_probe_errors".into(),
                Value::str(python::tail(&errors, 300)),
            ),
            (
                "ours_frame_samples".into(),
                candidate_shape.cloned().unwrap_or(Value::Null),
            ),
        ]);
        if reference.is_empty() && candidate.is_empty() && same_frames && ours_run.code == 0 {
            status(&mut row, "pass");
            return Ok(Value::Object(row));
        }
    }
    if reference.is_empty() {
        status(&mut row, "ffmpeg-refused");
        return Ok(Value::Object(row));
    }
    if ours_run.stderr.contains("nsupported") && candidate.is_empty() {
        status(&mut row, "refused-unsupported");
        row.push((
            "ours_stderr_tail".into(),
            Value::str(python::tail(&ours_run.stderr, 300)),
        ));
        return Ok(Value::Object(row));
    }
    let same = |ours: Option<&Value>, theirs: Option<i128>| match (ours, theirs) {
        (None, None) => true,
        (Some(v), Some(n)) => python::equals_int(v, n),
        _ => false,
    };
    let ours_layout = info.get("layout").and_then(Value::as_str);
    let reference_layout = stream
        .as_ref()
        .and_then(|s| s.get("channel_layout"))
        .and_then(Value::as_str);
    // Fixed AAC configurations 5 and 6 each have one surround pair in
    // Table 1.19. FFmpeg labels those planes as back in MOV.
    let surround_pair_labels = lane == "aac-mov"
        && matches!(
            (reference_layout, ours_layout),
            (Some("5.0"), Some("5.0(side)")) | (Some("5.1"), Some("5.1(side)"))
        );
    if lane == "aac-mov" {
        row.push((
            "surround_pair_labels".into(),
            Value::Bool(surround_pair_labels),
        ));
    }
    let exact = (!(through_session || lane == "track-reset")
        || ours_run.code == 0 && reference_run.code == 0)
        && same(ours_channels, reference_channels)
        && same(ours_rate, reference_rate)
        && candidate.len() == reference.len()
        && (lane != "aac-mov" && !initialized_aac
            || (ours_run.code == 0
                && reference_run.code == 0
                && info.get("errors").and_then(python::int) == Some(0)
                && (surround_pair_labels
                    || ours_layout.is_some() && ours_layout == reference_layout)));
    let stats = if exact {
        let channels = reference_channels.ok_or_else(|| {
            Stop::Fatal("'<=' not supported between instances of 'NoneType' and 'int'".into())
        })?;
        channel_stats(&candidate, &reference, channels)?
    } else {
        None
    };
    let floats = |values: &[f64]| Value::Array(values.iter().map(|&v| Value::float(v)).collect());
    if let Some((rms, abs)) = &stats {
        let (worst_rms, worst_abs) = (python::max(rms), python::max(abs));
        row.push((
            "stats".into(),
            Value::object([("rms", floats(rms)), ("abs", floats(abs))]),
        ));
        row.push(("worst_rms".into(), Value::float(worst_rms)));
        row.push(("worst_abs".into(), Value::float(worst_abs)));
        let pass = if lane == "ac3-nodither" && format == Some("ac3") {
            worst_abs <= a.rules.ac3_dither_free_abs
        } else if a.rules.exact.iter().any(|exact| exact == &media_lane) {
            worst_abs == 0.0
        } else {
            worst_rms <= a.rules.rms && worst_abs <= a.rules.abs
        };
        let reference_clean =
            lane != "ac3-nodither" || reference_run.code == 0 && reference_run.stderr.is_empty();
        status(
            &mut row,
            if pass && same_frames && reference_clean {
                "pass"
            } else {
                "fail"
            },
        );
    } else {
        row.push(("stats".into(), Value::Null));
        row.push(("worst_rms".into(), Value::Null));
        row.push(("worst_abs".into(), Value::Null));
        status(&mut row, "fail");
    }
    if variant_manifest(lane).is_some()
        && row
            .last()
            .is_some_and(|(key, value)| key == "status" && value.as_str() == Some("fail"))
    {
        row.extend([
            (
                "ours_pcm_sha256".into(),
                Value::str(sha256::hex(&candidate)),
            ),
            (
                "ffmpeg_pcm_sha256".into(),
                Value::str(sha256::hex(&reference)),
            ),
        ]);
    }
    Ok(Value::Object(row))
}

fn no_dts_core(reference: &crate::process::Text, pcm: &[u8]) -> bool {
    pcm.is_empty()
        && reference.code > 0
        && reference.stderr.contains("No valid DCA sub-stream found")
}

fn output_path(
    a: &Grade,
    lane: &str,
    path: &str,
    tmp: &Path,
    initialized_aac: bool,
    vector: &Value,
) -> Result<Value, Stop> {
    let through_session = lane.starts_with("session-");
    let media_lane = if through_session {
        session::base(lane, vector)?
    } else {
        lane.to_owned()
    };
    let decode = operation(&media_lane).ok_or_else(|| unknown(lane))?;
    let op = format!("{decode}-raw");
    let worker = python::path(a.codec_workers.of(decode));
    if video_extensions(&media_lane).is_some() {
        let argv = |w: &str| vec![w.to_owned(), op.clone(), path.to_owned(), OUT.to_owned()];
        let ours_argv = if through_session {
            session::args(a, &media_lane, path, OUT, tmp, vector, "device")?
        } else {
            argv(&worker)
        };
        let ours = Frames::spawn(&ours_argv, CANVAS, true, RUN_TIMEOUT)?;
        let reference_argv = session::reference_args(a, &media_lane, path, OUT, vector, "device")
            .unwrap_or_else(|| argv(&a.reference));
        let reference = Frames::spawn(&reference_argv, CANVAS, true, RUN_TIMEOUT)?;
        return if through_session {
            video::rgba(ours, reference, true)
        } else {
            video::output(ours, reference)
        };
    }
    let (o, r) = (tmp.join("ours.s16"), tmp.join("ref.s16"));
    let ours_argv = if through_session {
        session::args(
            a,
            &media_lane,
            path,
            &python::path(&o),
            tmp,
            vector,
            "device",
        )?
    } else {
        vec![worker, op.clone(), path.to_owned(), python::path(&o)]
    };
    let po = run_text(&ours_argv, RUN_TIMEOUT)?;
    let coreless = lane == "session-dts" && vector.get("coreless") == Some(&Value::Bool(true));
    let reference_argv =
        session::reference_args(a, &media_lane, path, &python::path(&r), vector, "device")
            .unwrap_or_else(|| {
                if through_session && (media_lane == "mp3" || coreless) {
                    // The CLI uses the same decoder selection as the PCM grade.
                    // For MP3, it discovers the complete gapless trim before decoding.
                    let mut args = [
                        a.ffmpeg.as_str(),
                        "-nostdin",
                        "-v",
                        "error",
                        "-threads",
                        "1",
                        "-filter_threads",
                        "1",
                        "-f",
                        if coreless { "dts" } else { "mp3" },
                        "-c:a",
                        if coreless { "dca" } else { "mp3float" },
                    ]
                    .map(String::from)
                    .to_vec();
                    if coreless {
                        args.extend(["-core_only", "1"].map(String::from));
                    }
                    args.extend(
                        [
                            "-i",
                            path,
                            "-map",
                            "0:a:0",
                            "-af",
                            "aformat=sample_fmts=s16:channel_layouts=stereo",
                            "-f",
                            "s16le",
                            "-c:a",
                            "pcm_s16le",
                            "-y",
                            &python::path(&r),
                        ]
                        .map(String::from),
                    );
                    args
                } else {
                    vec![a.reference.clone(), op, path.to_owned(), python::path(&r)]
                }
            });
    let pr = run_text(&reference_argv, RUN_TIMEOUT)?;
    let co = fs::read(&o).unwrap_or_default();
    let cr = fs::read(&r).unwrap_or_default();
    let mut row = vec![
        ("samples_ours".to_owned(), Value::int((co.len() / 4) as i64)),
        ("samples_ref".to_owned(), Value::int((cr.len() / 4) as i64)),
        ("ours_rc".to_owned(), Value::int(po.code)),
        ("reference_rc".to_owned(), Value::int(pr.code)),
        (
            "ours_stderr_tail".to_owned(),
            Value::str(python::tail(&po.stderr, 300)),
        ),
        (
            "reference_stderr_tail".to_owned(),
            Value::str(python::tail(&pr.stderr, 300)),
        ),
    ];
    if initialized_aac
        && !complete_single_sce_stereo(size_of::<i16>(), [(po.code, co.len()), (pr.code, cr.len())])
    {
        row.push(("status".into(), Value::str("fail")));
        return Ok(Value::Object(row));
    }
    if coreless {
        let both_refuse = co.is_empty()
            && po.code > 0
            && po.stderr.contains("no DTS core sync")
            && no_dts_core(&pr, &cr);
        let result = if both_refuse {
            "reference-produced-nothing"
        } else {
            "fail"
        };
        row.push(("status".into(), Value::str(result)));
        return Ok(Value::Object(row));
    }
    if cr.is_empty() {
        row.push(("status".into(), Value::str("reference-produced-nothing")));
        return Ok(Value::Object(row));
    }
    if co.len() != cr.len() || through_session && (po.code != 0 || pr.code != 0) {
        row.push(("status".into(), Value::str("fail")));
        return Ok(Value::Object(row));
    }
    if co.len() % 2 != 0 {
        return Err(Stop::Fatal(
            "buffer size must be a multiple of element size".into(),
        ));
    }
    let samples = |b: &[u8]| -> Vec<i32> {
        b.chunks_exact(2)
            .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
            .collect()
    };
    let d: Vec<i32> = samples(&co)
        .iter()
        .zip(samples(&cr))
        .map(|(x, y)| (x - y).abs())
        .collect();
    let worst = d.iter().copied().max().unwrap_or(0);
    let differing = d.iter().filter(|&&x| x != 0).count();
    row.push(("max_abs_lsb".into(), Value::int(worst)));
    row.push(("differing_samples".into(), Value::int(differing as i64)));
    row.push(("exact".into(), Value::Bool(differing == 0)));
    let pass = i64::from(worst) <= a.rules.lsb;
    row.push((
        "status".into(),
        Value::str(if pass { "pass" } else { "fail" }),
    ));
    Ok(Value::Object(row))
}

fn timeout_row(argv: Vec<String>) -> Value {
    Value::object([
        ("status", Value::str("timeout")),
        ("argv", Value::strings(argv)),
    ])
}

fn observation(value: Result<Value, Stop>, lane: &str) -> Result<Value, String> {
    match value {
        Ok(value) => Ok(value),
        Err(Stop::Timeout(argv)) => Ok(timeout_row(argv)),
        Err(Stop::Fatal(message)) if lane.starts_with("session-") || play::lane(lane) => {
            Ok(Value::object([
                ("status", Value::str("operation-error")),
                ("message", Value::str(message)),
            ]))
        }
        Err(Stop::Fatal(message)) => Err(message),
    }
}

fn vector_name(lane: &str, vector: &Value) -> Option<String> {
    if lane.starts_with("session-") && vector.get("base_lane").is_none() {
        let path = vector.get("path")?.as_str()?;
        Some(format!("{lane}:{path}"))
    } else {
        vector.get("name")?.as_str().map(str::to_owned)
    }
}

/// Vectors that differ as listed, with either an open case or a declared deviation.
/// Every name must be a vector of the manifest.
fn open_cases<'a>(manifest: &'a Value, vectors: &[Value]) -> Result<Vec<&'a str>, String> {
    let mut open = Vec::new();
    let mut admit = |case: &'a Value| -> Result<(), String> {
        let name = case.as_str().ok_or("an open case is a vector name")?;
        if open.contains(&name) {
            return Err(format!("open case {name}: listed twice"));
        }
        if !vectors.iter().any(|v| {
            v.get("name").and_then(Value::as_str) == Some(name)
                || v.get("lane")
                    .and_then(Value::as_str)
                    .and_then(session::lane)
                    .and_then(|lane| vector_name(lane, v))
                    .as_deref()
                    == Some(name)
        }) {
            return Err(format!("open case {name}: not a vector of the manifest"));
        }
        open.push(name);
        Ok(())
    };
    for case in manifest
        .get("open_cases")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        admit(case)?;
    }
    for deviation in deviations(manifest) {
        let line = |key| {
            deviation
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty() && !value.contains('\n'))
        };
        let cases = deviation
            .get("cases")
            .and_then(Value::as_array)
            .unwrap_or_default();
        if cases.is_empty()
            || !["reading", "clause", "candidate", "reference"]
                .into_iter()
                .all(line)
            || deviation.get("compat").is_some() && !line("compat")
            || deviation.get("targets").is_some()
        {
            return Err(
                "a codec deviation needs its reading, clause, both results and cases".into(),
            );
        }
        for case in cases {
            admit(case)?;
        }
    }
    Ok(open)
}

pub(super) fn deviations(manifest: &Value) -> &[Value] {
    manifest
        .get("open_deviations")
        .and_then(Value::as_array)
        .unwrap_or_default()
}

fn all_vectors(manifests: &[Value]) -> Result<Vec<Value>, String> {
    let mut vectors = Vec::new();
    for manifest in manifests {
        vectors.extend_from_slice(
            manifest
                .get("vectors")
                .and_then(Value::as_array)
                .ok_or("manifest has no vectors")?,
        );
    }
    Ok(vectors)
}

fn vector_lanes<'a>(vector: &'a Value, selected: Option<&[String]>) -> [Option<&'a str>; 3] {
    let base = vector.get("lane").and_then(Value::as_str);
    let priming = (base == Some("aac")
        && selected.is_some_and(|lanes| lanes.iter().any(|lane| lane == AAC_CONTAINER_PRIMING)))
    .then_some(AAC_CONTAINER_PRIMING);
    let session = selected.and_then(|_| base.and_then(session::lane));
    [base, priming, session]
}

/// Grade every manifest vector (or those of `--lanes` that `--only` selects),
/// appending one JSON row per vector to `--out` and printing one line per vector.
/// A row of a vector the manifest lists in `open_cases` carries `"open_case": true`.
/// A named lane without vectors fails.
pub fn grade(a: &Grade) -> Result<(), String> {
    let manifest = json::read(&a.manifest)?;
    let variant_manifests = a
        .lanes
        .iter()
        .flatten()
        .filter_map(|lane| variant_manifest(lane))
        .filter(|path| !a.manifest.ends_with(path))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|path| json::read(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut manifests = vec![manifest];
    manifests.extend(variant_manifests);
    let vectors = all_vectors(&manifests)?;
    let open = manifests
        .iter()
        .map(|manifest| open_cases(manifest, &vectors))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let in_lanes = |lane: Option<&str>| {
        a.lanes
            .as_ref()
            .is_none_or(|lanes| lane.is_some_and(|lane| lanes.iter().any(|l| l == lane)))
    };
    for lane in a.lanes.iter().flatten() {
        if !vectors
            .iter()
            .any(|v| vector_lanes(v, a.lanes.as_deref()).contains(&Some(lane)))
        {
            return Err(format!("lane {lane}: no vectors in the manifest"));
        }
    }
    let scheduled = vectors.iter().flat_map(|vector| {
        vector_lanes(vector, a.lanes.as_deref())
            .into_iter()
            .map(move |lane| (vector, lane))
    });
    let only: Option<Vec<&str>> = a.only.as_deref().map(|o| o.split(',').collect());
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&a.out)
        .map_err(|e| format!("{}: {e}", a.out.display()))?;
    let text = |v: &Value, key: &str| -> Result<String, String> {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("vector without {key}"))
    };
    let mut native_reference = None;
    for (v, lane) in scheduled.filter(|(_, lane)| lane.is_some() && in_lanes(*lane)) {
        let lane = lane.ok_or("vector without lane")?.to_owned();
        let relative = text(v, "path")?;
        let name = vector_name(&lane, v).ok_or("vector without name")?;
        if let Some(only) = &only
            && !only.iter().any(|o| name.contains(o) || *o == lane)
        {
            continue;
        }
        let tmp = TempDir::new(a.scratch.as_deref())?;
        let original = v
            .get("source")
            .and_then(Value::as_str)
            .map(|source| a.root.join(source));
        let source = original.as_deref().map(python::path);
        let path = if original.is_some() {
            tmp.0.join(
                Path::new(&relative)
                    .file_name()
                    .ok_or("variant file name")?,
            )
        } else {
            a.root.join(&relative)
        };
        let mut row = vec![
            ("lane".to_owned(), Value::str(lane.as_str())),
            ("name".to_owned(), Value::str(name.as_str())),
            (
                "path".to_owned(),
                v.get("path").cloned().unwrap_or(Value::Null),
            ),
            (
                "sha256".to_owned(),
                v.get("sha256").cloned().unwrap_or(Value::Null),
            ),
        ];
        let generated = if original.is_some() {
            damage::materialize(
                &damage::FixtureTools {
                    root: &a.root,
                    ffprobe: &a.ffprobe,
                    strip_dither: &a.bin.join("strip_dither"),
                },
                v,
                &path,
            )
        } else {
            Ok(())
        };
        if let Err(error) = generated {
            row.push((
                "decoder".into(),
                match error {
                    Stop::Timeout(argv) => timeout_row(argv),
                    Stop::Fatal(message) => Value::object([
                        ("status", Value::str("fixture-error")),
                        ("message", Value::str(message)),
                    ]),
                },
            ));
        } else if variant_manifest(&lane).is_some() && !path.is_file() {
            row.push(("status".into(), Value::str("input-missing")));
        } else if Some(file_sha(&path)?.as_str()) != v.get("sha256").and_then(Value::as_str) {
            row.push(("status".into(), Value::str("input-hash-mismatch")));
        } else if play::lane(&lane) {
            row.push((
                "output".into(),
                observation(play::grade(a, &path, &tmp.0, v), &lane)?,
            ));
        } else if let Some(base) = native_audio::selected(&lane, v) {
            if native_reference.is_none() {
                let directory = TempDir::new(a.scratch.as_deref())?;
                let executable = native_audio::build(&directory.0, &a.ffmpeg)?;
                native_reference = Some((directory, executable));
            }
            let executable = &native_reference
                .as_ref()
                .ok_or("missing native audio runner")?
                .1;
            let session = (lane != base).then_some(v);
            match native_audio::grade(a, base, &python::path(&path), &tmp.0, executable, session) {
                Ok((decoder, output)) => {
                    row.extend([("decoder".into(), decoder), ("output".into(), output)]);
                }
                Err(error) => row.push(("decoder".into(), observation(Err(error), &lane)?)),
            }
        } else {
            let path = python::path(&path);
            let sha256 = text(v, "sha256")?;
            let initialized_aac = uses_initialized_aac_reference(&lane, &text(v, "name")?, &sha256);
            let session_video = matches!(lane.as_str(), "session-h264" | "session-hevc");
            let output = if decoder_only_lane(&lane) {
                None
            } else {
                let output_dir = tmp.0.join("o");
                fs::create_dir(&output_dir).map_err(|e| e.to_string())?;
                Some(observation(
                    output_path(a, &lane, &path, &output_dir, initialized_aac, v),
                    &lane,
                )?)
            };
            let decoded = if session_video {
                session::video_timeline(a, &path, &lane, v, output.as_ref())
            } else if video_extensions(&lane).is_some() {
                video::decoder(&a.ffprobe, &a.ffmpeg, &a.example(&lane)?, &lane, &path)
            } else {
                audio_decoder(
                    a,
                    &lane,
                    &path,
                    &tmp.0,
                    initialized_aac,
                    v,
                    source.as_deref(),
                )
            };
            row.push(("decoder".into(), observation(decoded, &lane)?));
            if let Some(output) = output {
                row.push(("output".into(), output));
            }
        }
        let comparison_started = row
            .iter()
            .any(|(key, _)| key == "decoder" || play::lane(&lane) && key == "output");
        let infrastructure_error = row.iter().any(|(_, value)| {
            matches!(
                value.get("status").and_then(Value::as_str),
                Some("operation-error" | "fixture-error" | "timeout")
            )
        });
        if open.contains(&name.as_str())
            && (variant_manifest(&lane).is_none() || comparison_started && !infrastructure_error)
        {
            row.push(("open_case".into(), Value::Bool(true)));
        }
        let row = Value::Object(row);
        writeln!(out, "{}", json::to_python(&row, None))
            .and_then(|()| out.flush())
            .map_err(|e| format!("{}: {e}", a.out.display()))?;
        let status = |key: &str| row.get(key).and_then(|r| r.get("status")).cloned();
        let decoder = status("decoder")
            .or_else(|| row.get("status").cloned())
            .unwrap_or(Value::Null);
        let output = if row.get("output").is_some() {
            status("output").unwrap_or_else(|| Value::str(""))
        } else {
            Value::str("")
        };
        println!(
            "{lane} {name} {} {}",
            python::str(&decoder),
            python::str(&output)
        );
    }
    println!("DONE");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_codec_deviations_join_the_open_case_ratchet() {
        let vectors = json::parse(r#"[{"name":"one"},{"name":"two"}]"#).unwrap();
        let vectors = vectors.as_array().unwrap();
        let manifest = |name, clause| {
            json::parse(&format!(
                r#"{{"open_cases":["one"],"open_deviations":[{{"reading":"reading",
                "clause":"{clause}","candidate":"ours","reference":"theirs","cases":["{name}"]}}]}}"#
            ))
            .unwrap()
        };
        assert_eq!(
            open_cases(&manifest("two", "§4"), vectors).unwrap(),
            ["one", "two"]
        );
        assert!(open_cases(&manifest("one", "§4"), vectors).is_err());
        assert!(open_cases(&manifest("other", "§4"), vectors).is_err());
        assert!(open_cases(&manifest("two", ""), vectors).is_err());
    }

    #[test]
    fn initialized_aac_reference_requires_the_pinned_input() {
        let name = "aac-sce-in-stereo.adts";
        let pin = AAC_SINGLE_SCE_STEREO_SHA256;
        assert!(uses_initialized_aac_reference("aac", name, pin));
        assert!(!uses_initialized_aac_reference("aac-mov", name, pin));
        assert!(!uses_initialized_aac_reference("aac", "other.adts", pin));
        assert!(!uses_initialized_aac_reference("aac", name, "different"));
    }

    #[test]
    fn single_sce_comparison_requires_complete_successful_stereo_output() {
        for (width, bytes) in [(size_of::<f32>(), 1_777_664), (size_of::<i16>(), 888_832)] {
            assert!(complete_single_sce_stereo(width, [(0, bytes); 2]));
            assert!(!complete_single_sce_stereo(width, [(0, 0); 2]));
            assert!(!complete_single_sce_stereo(
                width,
                [(0, bytes - 2 * width); 2]
            ));
            assert!(!complete_single_sce_stereo(width, [(1, bytes), (0, bytes)]));
            assert!(!complete_single_sce_stereo(width, [(0, bytes), (1, bytes)]));
        }
    }

    #[test]
    fn audio_statistics_match_numpy() {
        // python3 -c "import numpy as np; c=np.array([0.5,-0.25,0.125,1e-3,0.3,0.7],'<f4'); r=np.array([0.5,-0.2500305,0.1250061,1.5e-3,0.29999,0.70001],'<f4'); d=(c.astype(np.float64)-r.astype(np.float64)).reshape(-1,2); print([float(x) for x in np.sqrt(np.mean(d*d,axis=0))], [float(x) for x in np.max(np.abs(d),axis=0)])"
        let le = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let c = le(&[0.5, -0.25, 0.125, 1e-3, 0.3, 0.7]);
        let r = le(&[0.5, -0.250_030_5, 0.125_006_1, 1.5e-3, 0.29999, 0.70001]);
        let (rms, abs) = channel_stats(&c, &r, 2).unwrap().unwrap();
        assert_eq!(rms, [6.767954748344818e-06, 0.000289269045357044]);
        assert_eq!(abs, [1.0013580322265625e-05, 0.0004999999655410647]);
        assert_eq!(channel_stats(&c, &r, 4).unwrap(), None);
        assert!(channel_stats(&c[..5], &r[..5], 1).is_err());
        let non_finite: Vec<u8> = [0.0_f32, f32::NAN]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let finite = [0_u8; 8];
        assert_eq!(channel_stats(&non_finite, &finite, 2).unwrap(), None);
        assert_eq!(channel_stats(&finite, &non_finite, 2).unwrap(), None);
    }

    #[test]
    fn vector_suffixes() {
        assert_eq!(suffix("a.core.dts"), ".dts");
        assert_eq!(suffix(".bit"), "");
    }
}
