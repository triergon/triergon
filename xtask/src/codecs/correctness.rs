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

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::json::{self, Value};
use crate::process::{Frames, OUT, Stop, TempDir, run_text};
use crate::{numpy, python, sha256};

/// Correctness manifest schema written by [`manifest`].
pub const MANIFEST_SCHEMA: &str = "triergon.codecs.correctness-manifest.v1";

const RUN_TIMEOUT: Duration = Duration::from_secs(900);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);
/// One output picture: 1920x1080 RGBA.
const CANVAS: usize = 1920 * 1080 * 4;

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
    Ok(match lane {
        "ac3" => Some("ac3"),
        "eac3" => Some("eac3"),
        "aac" => Some("aac"),
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
    Some(match lane {
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
    Some(match lane {
        "h264" => "codec-h264-decode",
        "hevc" => "codec-hevc-decode",
        "ac3" | "eac3" => "codec-ac3-decode",
        "aac" => "codec-aac-decode",
        "dts" => "codec-dca-decode",
        // File-format lanes decode through a container reader, as a player does.
        "flac" | "mp3" | "mp2" | "mp1" | "vorbis" | "opus" => "audio-decode",
        _ => return None,
    })
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
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = sha256::Sha256::default();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finish().iter().map(|b| format!("{b:02x}")).collect())
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
                    let want = audio_suffixes(lane).ok_or_else(|| unknown(lane))?;
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
}

impl Rules {
    pub fn load(root: &Path) -> Result<Rules, String> {
        let path = root.join("harness/codecs/numeric-rules.json");
        let rules = crate::checks::read_json(&path)?;
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
            .ok_or_else(|| format!("{}: missing lossless_audio_decode.applies_to", path.display()))?
            .iter()
            .map(|lane| lane.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("{}: lossless lanes are not strings", path.display()))?;
        Ok(Rules {
            rms: number("audio_decode", "per_channel_rms_max")?,
            abs: number("audio_decode", "per_sample_abs_max")?,
            lsb: number("audio_device_path", "per_sample_abs_max_lsb")? as i64,
            exact,
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
    pub codec_worker: PathBuf,
    pub reference: String,
    pub out: PathBuf,
    pub scratch: Option<PathBuf>,
    pub only: Option<String>,
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

fn has_error(line: &Value) -> bool {
    line.get("error").is_some()
}

fn mentions_unsupported(value: &Value) -> bool {
    json::to_python(value, None).contains("nsupported")
}

/// `ffprobe` facts of the first video (or audio) stream; `None` when it finds none.
fn probe(
    ffprobe: &str,
    path: &str,
    format: Option<&str>,
    audio: bool,
) -> Result<Option<Value>, Stop> {
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
    Ok(stream)
}

/// Position of the first differing sample of two planar YUV 4:2:0 frames.
fn first_diff(
    a: &[u8],
    b: &[u8],
    width: usize,
    height: usize,
    depth: usize,
) -> Vec<(String, Value)> {
    let (cw, ch) = (width / 2, height / 2);
    let planes = [
        ("Y", 0, width, height),
        ("U", width * height, cw, ch),
        ("V", width * height + cw * ch, cw, ch),
    ];
    let clip = |data: &[u8], from: usize, to: usize| -> Vec<u8> {
        data[from.min(data.len())..to.min(data.len())].to_vec()
    };
    for (name, offset, pw, ph) in planes {
        let (from, to) = (offset * depth, (offset + pw * ph) * depth);
        let (pa, pb) = (clip(a, from, to), clip(b, from, to));
        if pa != pb {
            let k = (0..pa.len())
                .step_by(depth)
                .find(|&i| clip(&pa, i, i + depth) != clip(&pb, i, i + depth))
                .unwrap_or(0)
                / depth;
            return vec![
                ("plane".into(), Value::str(name)),
                ("x".into(), Value::int((k % pw) as i64)),
                ("y".into(), Value::int((k / pw) as i64)),
            ];
        }
    }
    Vec::new()
}

fn errors_head(errors: &[Value]) -> Value {
    Value::Array(errors.iter().take(3).cloned().collect())
}

fn video_decoder(a: &Grade, lane: &str, path: &str) -> Result<Value, Stop> {
    let stream = probe(&a.ffprobe, path, None, false)?;
    let ours_argv = vec![
        a.example(lane)?,
        path.to_owned(),
        if lane == "h264" { "/dev/stdout" } else { "-" }.to_owned(),
    ];
    let pix_fmt = stream.as_ref().and_then(|s| s.get("pix_fmt"));
    let (Some(stream), Some(pix_fmt)) = (stream.as_ref(), pix_fmt) else {
        let argv = vec![
            ours_argv[0].clone(),
            ours_argv[1].clone(),
            "/dev/null".into(),
        ];
        let done = run_text(&argv, RUN_TIMEOUT)?;
        let lines = json_lines(&done.stderr);
        let errors: Vec<Value> = lines.iter().filter(|j| has_error(j)).cloned().collect();
        return Ok(Value::object([
            ("status", Value::str("ffmpeg-refused")),
            ("ffmpeg", Value::str("ffprobe found no decodable stream")),
            ("ours_rc", Value::int(done.code)),
            (
                "ours_frames",
                Value::int(lines.iter().filter(|j| !has_error(j)).count() as i64),
            ),
            ("ours_errors", errors_head(&errors)),
            (
                "ours_stderr_tail",
                Value::str(python::tail(&done.stderr, 300)),
            ),
        ]));
    };
    let pix_name = pix_fmt
        .as_str()
        .ok_or_else(|| Stop::Fatal("pix_fmt is not a string".into()))?;
    let depth = if pix_name.contains("10") { 2 } else { 1 };
    let out_format = if depth == 2 { "yuv420p10le" } else { "yuv420p" };
    let dimension = |key: &str| -> Result<(usize, Value), Stop> {
        let value = stream
            .get(key)
            .cloned()
            .ok_or_else(|| Stop::Fatal(format!("ffprobe reported no {key}")))?;
        let number = python::int(&value)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Stop::Fatal(format!("ffprobe {key} is not a size")))?;
        Ok((number, value))
    };
    let ((w, width), (h, height)) = (dimension("width")?, dimension("height")?);
    let size = w * h * 3 / 2 * depth;
    let reference_argv: Vec<String> = [
        a.ffmpeg.as_str(),
        "-nostdin",
        "-v",
        "error",
        "-threads",
        "1",
        "-i",
        path,
        "-f",
        "rawvideo",
        "-pix_fmt",
        out_format,
        "-fps_mode",
        "passthrough",
        "-",
    ]
    .map(String::from)
    .to_vec();
    let mut reference = Frames::spawn(&reference_argv, size, false)?;
    let mut ours = Frames::spawn(&ours_argv, size, false)?;
    let (mut n, mut matched) = (0, 0);
    let mut first: Option<Value> = None;
    let (mut count_reference, mut count_ours) = (0, 0);
    loop {
        let (fa, fb) = (ours.next(), reference.next());
        count_ours += i64::from(fa.is_some());
        count_reference += i64::from(fb.is_some());
        let (Some(fa), Some(fb)) = (fa, fb) else {
            break;
        };
        if fa == fb {
            matched += 1;
        } else if first.is_none() {
            let mut fields = vec![("frame".to_owned(), Value::int(n))];
            fields.extend(first_diff(&fa, &fb, w, h, depth));
            first = Some(Value::Object(fields));
        }
        n += 1;
    }
    while ours.next().is_some() {
        count_ours += 1;
    }
    while reference.next().is_some() {
        count_reference += 1;
    }
    let (rc_ours, err_ours, ours_tail) = ours.close(RUN_TIMEOUT)?;
    let (rc_reference, err_reference, _) = reference.close(RUN_TIMEOUT)?;
    let partial = ours_tail.len();
    let lines = json_lines(&err_ours);
    let errors: Vec<Value> = lines.into_iter().filter(has_error).collect();
    let unsupported = errors.iter().any(mentions_unsupported);
    let status = if count_reference == 0 && rc_reference != 0 {
        "ffmpeg-refused"
    } else if unsupported && count_ours == 0 {
        "refused-unsupported"
    } else if first.is_none() && count_ours == count_reference && rc_ours == 0 && partial == 0 {
        "pass"
    } else {
        "fail"
    };
    Ok(Value::object([
        ("status", Value::str(status)),
        ("frames_ref", Value::int(count_reference)),
        ("frames_ours", Value::int(count_ours)),
        ("frames_equal", Value::int(matched)),
        ("first_diff", first.unwrap_or(Value::Null)),
        ("width", width),
        ("height", height),
        ("pix_fmt", pix_fmt.clone()),
        ("ours_rc", Value::int(rc_ours)),
        ("ffmpeg_rc", Value::int(rc_reference)),
        ("ours_errors", errors_head(&errors)),
        ("ours_partial_bytes", Value::int(partial as i64)),
        (
            "ffmpeg_stderr_tail",
            Value::str(python::tail(&err_reference, 300)),
        ),
    ]))
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
/// reduced in numpy's order; `None` when the layouts cannot be compared.
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

fn audio_decoder(a: &Grade, lane: &str, path: &str, tmp: &Path) -> Result<Value, Stop> {
    let format = audio_format(lane)?;
    let stream = probe(&a.ffprobe, path, format, true)?;
    let (reference_path, ours_path) = (tmp.join("ref.f32"), tmp.join("ours.f32"));
    let mut reference_argv: Vec<String> = [a.ffmpeg.as_str(), "-nostdin", "-v", "error", "-threads", "1"]
        .map(String::from)
        .to_vec();
    if let Some(format) = format {
        reference_argv.extend(["-f".to_owned(), format.to_owned()]);
    }
    if let Some(decoder) = reference_decoder(lane) {
        reference_argv.extend(["-c:a".to_owned(), decoder.to_owned()]);
    }
    reference_argv.extend(
        [
            "-i",
            path,
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "-y",
            &python::path(&reference_path),
        ]
        .map(String::from),
    );
    let reference_run = run_text(&reference_argv, RUN_TIMEOUT)?;
    let ours_run = run_text(
        &[a.example(lane)?, path.to_owned(), python::path(&ours_path)],
        RUN_TIMEOUT,
    )?;
    let info = json_lines(&ours_run.stderr)
        .pop()
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
    let status = |row: &mut Vec<(String, Value)>, status: &str| {
        row.push(("status".into(), Value::str(status)));
    };
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
    let exact = same(ours_channels, reference_channels)
        && same(ours_rate, reference_rate)
        && candidate.len() == reference.len();
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
        let pass = if a.rules.exact.iter().any(|exact| exact == lane) {
            worst_abs == 0.0
        } else {
            worst_rms <= a.rules.rms && worst_abs <= a.rules.abs
        };
        status(&mut row, if pass { "pass" } else { "fail" });
    } else {
        row.push(("stats".into(), Value::Null));
        row.push(("worst_rms".into(), Value::Null));
        row.push(("worst_abs".into(), Value::Null));
        status(&mut row, "fail");
    }
    Ok(Value::Object(row))
}

fn output_path(a: &Grade, lane: &str, path: &str, tmp: &Path) -> Result<Value, Stop> {
    let op = format!("{}-raw", operation(lane).ok_or_else(|| unknown(lane))?);
    let worker = python::path(&a.codec_worker);
    if video_extensions(lane).is_some() {
        let argv = |w: &str| vec![w.to_owned(), op.clone(), path.to_owned(), OUT.to_owned()];
        let mut ours = Frames::spawn(&argv(&worker), CANVAS, true)?;
        let mut reference = Frames::spawn(&argv(&a.reference), CANVAS, true)?;
        let (mut n, mut matched, mut worst, mut differing) = (0, 0, 0u8, 0);
        let mut first: Option<Value> = None;
        let (mut count_ours, mut count_reference) = (0, 0);
        loop {
            let (fa, fb) = (ours.next(), reference.next());
            count_ours += i64::from(fa.is_some());
            count_reference += i64::from(fb.is_some());
            let (Some(fa), Some(fb)) = (fa, fb) else {
                break;
            };
            if fa == fb {
                matched += 1;
            } else {
                let d: Vec<u8> = fa.iter().zip(&fb).map(|(x, y)| x.abs_diff(*y)).collect();
                worst = worst.max(d.iter().copied().max().unwrap_or(0));
                differing += 1;
                if first.is_none() {
                    let k = d.iter().position(|&x| x != 0).unwrap_or(0);
                    first = Some(Value::object([
                        ("frame", Value::int(n)),
                        ("x", Value::int(((k / 4) % 1920) as i64)),
                        ("y", Value::int(((k / 4) / 1920) as i64)),
                        ("channel", Value::str(&"RGBA"[k % 4..k % 4 + 1])),
                        (
                            "differing_bytes",
                            Value::int(d.iter().filter(|&&x| x != 0).count() as i64),
                        ),
                    ]));
                }
            }
            n += 1;
        }
        while ours.next().is_some() {
            count_ours += 1;
        }
        while reference.next().is_some() {
            count_reference += 1;
        }
        let (rc_ours, err_ours, _) = ours.close(RUN_TIMEOUT)?;
        let (rc_reference, err_reference, _) = reference.close(RUN_TIMEOUT)?;
        let status = if count_reference == 0 {
            "reference-produced-nothing"
        } else if first.is_none() && count_ours == count_reference {
            "pass"
        } else {
            "fail"
        };
        return Ok(Value::object([
            ("status", Value::str(status)),
            ("pictures_ref", Value::int(count_reference)),
            ("pictures_ours", Value::int(count_ours)),
            ("pictures_equal", Value::int(matched)),
            ("differing_pictures", Value::int(differing)),
            ("max_abs", Value::int(worst)),
            ("first_diff", first.unwrap_or(Value::Null)),
            ("ours_rc", Value::int(rc_ours)),
            ("reference_rc", Value::int(rc_reference)),
            ("ours_stderr_tail", Value::str(python::tail(&err_ours, 300))),
            (
                "reference_stderr_tail",
                Value::str(python::tail(&err_reference, 300)),
            ),
        ]));
    }
    let (o, r) = (tmp.join("ours.s16"), tmp.join("ref.s16"));
    let po = run_text(
        &[worker, op.clone(), path.to_owned(), python::path(&o)],
        RUN_TIMEOUT,
    )?;
    let pr = run_text(
        &[a.reference.clone(), op, path.to_owned(), python::path(&r)],
        RUN_TIMEOUT,
    )?;
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
    if cr.is_empty() {
        row.push(("status".into(), Value::str("reference-produced-nothing")));
        return Ok(Value::Object(row));
    }
    if co.len() != cr.len() {
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

/// Grade every manifest vector (or those `--only` selects), appending one JSON
/// row per vector to `--out` and printing one line per vector.
pub fn grade(a: &Grade) -> Result<(), String> {
    let manifest = crate::checks::read_json(&a.manifest)?;
    let vectors = manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("manifest has no vectors")?;
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
    for v in vectors {
        let (lane, name, relative) = (text(v, "lane")?, text(v, "name")?, text(v, "path")?);
        if let Some(only) = &only
            && !only.iter().any(|o| name.contains(o) || *o == lane)
        {
            continue;
        }
        let path = python::path(&a.root.join(&relative));
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
        if Some(file_sha(Path::new(&path))?.as_str()) != v.get("sha256").and_then(Value::as_str) {
            row.push(("status".into(), Value::str("input-hash-mismatch")));
        } else {
            let tmp = TempDir::new(a.scratch.as_deref())?;
            let decoded = if video_extensions(&lane).is_some() {
                video_decoder(a, &lane, &path)
            } else {
                audio_decoder(a, &lane, &path, &tmp.0)
            };
            row.push((
                "decoder".into(),
                match decoded {
                    Ok(value) => value,
                    Err(Stop::Timeout(argv)) => timeout_row(argv),
                    Err(Stop::Fatal(message)) => return Err(message),
                },
            ));
            let output_dir = tmp.0.join("o");
            fs::create_dir(&output_dir).map_err(|e| e.to_string())?;
            row.push((
                "output".into(),
                match output_path(a, &lane, &path, &output_dir) {
                    Ok(value) => value,
                    Err(Stop::Timeout(argv)) => timeout_row(argv),
                    Err(Stop::Fatal(message)) => return Err(message),
                },
            ));
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
    }

    #[test]
    fn first_difference_names_plane_and_position() {
        let (w, h) = (4, 2);
        let a = vec![0u8; w * h * 3 / 2];
        let mut b = a.clone();
        b[w * h + 1] = 1;
        assert_eq!(
            json::to_python(&Value::Object(first_diff(&a, &b, w, h, 1)), None),
            r#"{"plane": "U", "x": 1, "y": 0}"#
        );
        let mut c = vec![0u8; w * h * 3];
        c[2 * (w * h + 2) + 1] = 9;
        assert_eq!(
            json::to_python(
                &Value::Object(first_diff(&vec![0; w * h * 3], &c, w, h, 2)),
                None
            ),
            r#"{"plane": "V", "x": 0, "y": 0}"#
        );
        assert_eq!(suffix("a.core.dts"), ".dts");
        assert_eq!(suffix(".bit"), "");
    }
}
