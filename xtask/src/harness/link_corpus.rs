//! `cargo xtask link-order corpus OUT`: the link-order training corpus, made with the
//! `ffmpeg` and `ffprobe` found on `PATH` from FFmpeg's own video and audio generators.
//! Another FFmpeg build may encode differently; the SHA-256 pins of
//! [`super::link_training::MANIFEST`] then no longer match, and `link-order separate`
//! refuses the corpus.

use std::fs;
use std::path::Path;

use crate::json::Value;
use crate::sha256;

type R<T> = Result<T, String>;

/// The `ffmpeg` options before the inputs: quiet, overwriting.
const FFMPEG: [&str; 5] = ["-hide_banner", "-nostdin", "-y", "-v", "error"];

/// The output options of every file: bit-exact, without metadata.
const BITEXACT: [&str; 8] = [
    "-map_metadata",
    "-1",
    "-fflags",
    "+bitexact",
    "-flags:v",
    "+bitexact",
    "-flags:a",
    "+bitexact",
];

/// Video and audio sources, from FFmpeg's own generators.
const MANDELBROT: &str = "mandelbrot=size=320x180:rate=25:maxiter=256";
const LIFE: &str = "life=size=256x144:rate=30:mold=10:ratio=0.1:seed=7";
const CELLS: &str = "cellauto=size=320x176:rate=24:seed=11:rule=110";
const CHORD_48K: &str = "aevalsrc=0.3*sin(2*PI*523*t)|0.3*sin(2*PI*659*t):s=48000";
const CHORD_44K: &str = "aevalsrc=0.3*sin(2*PI*392*t)|0.2*sin(2*PI*587*t):s=44100";

/// The video encoders, on one thread so their output does not depend on the host,
/// with closed GOPs.
const AVC: &[&str] = &[
    "-c:v", "libx264", "-preset", "veryfast", "-g", "25", "-bf", "2", "-pix_fmt", "yuv420p",
    "-threads", "1",
];
const HEVC: &[&str] = &[
    "-c:v",
    "libx265",
    "-preset",
    "ultrafast",
    "-x265-params",
    "pools=none:frame-threads=1:log-level=error:keyint=30:open-gop=0",
    "-pix_fmt",
    "yuv420p",
];
const NO_VIDEO: &[&str] = &[];

/// A subtitle script, the text track of `avc-aac-text.mkv`.
const CUES: (&str, &str) = (
    "cues.srt",
    "1\n00:00:00,400 --> 00:00:01,600\nA training cue\n\n\
     2\n00:00:02,100 --> 00:00:03,900\nAnother line\nwith two rows\n\n\
     3\n00:00:04,250 --> 00:00:05,500\nThe last cue\n",
);

const FRAGMENTED: &str = "frag_keyframe+empty_moov+default_base_moof";

/// One `ffmpeg` run: its output file, its inputs (lavfi graphs, or files made before
/// it), each `seconds` long, and the output options: a video encoder, then the rest.
struct Recipe {
    file: &'static str,
    inputs: &'static [&'static str],
    seconds: &'static str,
    video: &'static [&'static str],
    options: &'static [&'static str],
}

const RECIPES: &[Recipe] = &[
    Recipe {
        file: "avc-aac-text.mkv",
        inputs: &[MANDELBROT, CHORD_48K, CUES.0],
        seconds: "6",
        video: AVC,
        options: &["-c:a", "aac", "-b:a", "96k", "-c:s", "srt"],
    },
    Recipe {
        file: "hevc-opus.mkv",
        inputs: &[LIFE, CHORD_48K],
        seconds: "5",
        video: HEVC,
        options: &["-c:a", "libopus", "-b:a", "64k"],
    },
    Recipe {
        file: "avc-three-audio.mkv",
        inputs: &[CELLS, CHORD_48K],
        seconds: "4",
        video: AVC,
        options: &[
            "-map", "0:v", "-map", "1:a", "-map", "1:a", "-map", "1:a", "-c:a:0", "aac", "-c:a:1",
            "ac3", "-c:a:2", "eac3",
        ],
    },
    Recipe {
        file: "avc-aac.mp4",
        inputs: &[CELLS, CHORD_48K],
        seconds: "6",
        video: AVC,
        options: &["-c:a", "aac", "-b:a", "96k"],
    },
    Recipe {
        file: "avc-aac-front.mp4",
        inputs: &[MANDELBROT, CHORD_44K],
        seconds: "5",
        video: AVC,
        options: &["-c:a", "aac", "-movflags", "+faststart"],
    },
    Recipe {
        file: "hevc-aac-fragmented.mp4",
        inputs: &[LIFE, CHORD_44K],
        seconds: "6",
        video: HEVC,
        options: &["-tag:v", "hvc1", "-c:a", "aac", "-movflags", FRAGMENTED],
    },
    Recipe {
        file: "avc-aac-fragmented.mp4",
        inputs: &[CELLS, CHORD_44K],
        seconds: "5",
        video: AVC,
        options: &["-c:a", "aac", "-movflags", FRAGMENTED],
    },
    Recipe {
        file: "avc-opus.mp4",
        inputs: &[MANDELBROT, CHORD_48K],
        seconds: "4",
        video: AVC,
        options: &["-c:a", "libopus"],
    },
    Recipe {
        file: "program.ts",
        inputs: &[CELLS, CHORD_48K],
        seconds: "5",
        video: AVC,
        options: &["-c:a", "aac", "-f", "mpegts", "-mpegts_start_pid", "481"],
    },
    Recipe {
        file: "program-hevc-aac.ts",
        inputs: &[LIFE, CHORD_48K],
        seconds: "4",
        video: HEVC,
        options: &["-c:a", "aac", "-f", "mpegts", "-mpegts_start_pid", "768"],
    },
    Recipe {
        file: "service-5.ts",
        inputs: &[MANDELBROT, CHORD_44K],
        seconds: "4",
        video: AVC,
        options: &["-c:a", "aac", "-f", "mpegts", "-mpegts_service_id", "5"],
    },
    Recipe {
        file: "service-9-dvb.ts",
        inputs: &[LIFE, CHORD_48K],
        seconds: "4",
        video: HEVC,
        options: &[
            "-c:a",
            "eac3",
            "-f",
            "mpegts",
            "-mpegts_flags",
            "system_b",
            "-mpegts_service_id",
            "9",
        ],
    },
    Recipe {
        file: "audio-6.ts",
        inputs: &[CHORD_44K],
        seconds: "5",
        video: NO_VIDEO,
        options: &["-c:a", "aac", "-f", "mpegts", "-mpegts_service_id", "6"],
    },
    Recipe {
        file: "video.h264",
        inputs: &[CELLS],
        seconds: "3",
        video: AVC,
        options: &["-f", "h264"],
    },
    Recipe {
        file: "video.hevc",
        inputs: &[LIFE],
        seconds: "3",
        video: HEVC,
        options: &["-f", "hevc"],
    },
];

/// The HLS segments the corpus keeps: the second segment of a playlist of each video
/// encoder and audio codec, named `NAME.ts`.
const SEGMENTS: [(&[&str], &str, &str, &str); 2] = [
    (AVC, "aac", "avc", "hls-avc-aac"),
    (HEVC, "ac3", "hevc", "hls-hevc-ac3"),
];

/// The playback capsules: each Matroska file's startup and its seeks after discovery
/// and after the end, at the given milliseconds.
const CAPSULES: [(&str, &str, [i64; 2]); 2] = [
    ("avc-aac-text.mkv", "avc", [1900, 3300]),
    ("hevc-opus.mkv", "hevc", [2300, 900]),
];

/// The packet-framing scripts and the movies whose first 48 video packets they hold.
const SCRIPTS: [(&str, &str); 2] = [
    ("avc-aac.mp4", "avc.bsf"),
    ("hevc-aac-fragmented.mp4", "hevc.bsf"),
];

/// The files only made to make others: the subtitle script and the fragmented HEVC
/// movie, besides the playlists and their other segments.
const MEANS: [&str; 2] = [CUES.0, "hevc-aac-fragmented.mp4"];

fn ffmpeg(out: &Path, inputs: &[&str], seconds: &str, options: &[&str], file: &str) -> R<()> {
    let mut command = std::process::Command::new("ffmpeg");
    command.args(FFMPEG);
    for input in inputs {
        if input.contains('=') {
            command.args(["-f", "lavfi", "-t", seconds, "-i", input]);
        } else {
            command.arg("-i").arg(out.join(input));
        }
    }
    let status = command
        .args(BITEXACT)
        .args(options)
        .arg(out.join(file))
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg {file}: {status}"))
    }
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) -> R<()> {
    fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Makes the training corpus in `out` and prints each kept file's name and SHA-256 as
/// the manifest lists them.
pub fn corpus(out: &Path) -> R<()> {
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    write(&out.join(CUES.0), CUES.1)?;
    let mut made = Vec::new();
    for recipe in RECIPES {
        let options = [recipe.video, recipe.options].concat();
        ffmpeg(out, recipe.inputs, recipe.seconds, &options, recipe.file)?;
        made.push(recipe.file.to_owned());
    }
    for (video, audio, codec, name) in SEGMENTS {
        let segments = out
            .join(format!("{codec}-segment-%d.ts"))
            .display()
            .to_string();
        let hls = [
            "-c:a",
            audio,
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_segment_filename",
            &segments,
        ];
        let playlist = format!("{codec}.m3u8");
        ffmpeg(
            out,
            &[CELLS, CHORD_48K],
            "6",
            &[video, &hls].concat(),
            &playlist,
        )?;
        let file = format!("{name}.ts");
        fs::rename(out.join(format!("{codec}-segment-1.ts")), out.join(&file))
            .map_err(|e| format!("{name}: {e}"))?;
        remove(out, &playlist)?;
        for index in 0.. {
            let segment = format!("{codec}-segment-{index}.ts");
            if !out.join(&segment).exists() {
                break;
            }
            remove(out, &segment)?;
        }
        made.push(file);
    }
    for (source, prefix, seeks) in CAPSULES {
        let data = fs::read(out.join(source)).map_err(|e| format!("{source}: {e}"))?;
        for (operation, target, name) in [
            ("startup", 0, "startup"),
            ("seek-after-discovery", seeks[0], "seek-discovery"),
            ("seek-after-end", seeks[1], "seek-end"),
        ] {
            let file = format!("{prefix}-{name}.case");
            write(
                &out.join(&file),
                crate::corpus::playback_capsule(operation, target, &data)?,
            )?;
            made.push(file);
        }
    }
    for (source, script) in SCRIPTS {
        write(&out.join(script), framing_script(&out.join(source), 48)?)?;
        made.push(script.to_owned());
    }
    for name in MEANS {
        remove(out, name)?;
        made.retain(|file| file != name);
    }
    made.sort();
    for name in made {
        let hash = sha256::file_hex(&out.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        println!("{{\"file\": \"{name}\", \"sha256\": \"{hash}\"}}");
    }
    Ok(())
}

fn remove(out: &Path, name: &str) -> R<()> {
    fs::remove_file(out.join(name)).map_err(|e| format!("{name}: {e}"))
}

fn integer(value: &Value, key: &str) -> R<i64> {
    value
        .get(key)
        .and_then(crate::python::int)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| format!("ffprobe: no 64-bit {key}"))
}

fn be32(value: usize) -> R<[u8; 4]> {
    u32::try_from(value)
        .map(u32::to_be_bytes)
        .map_err(|_| format!("{value} does not fit a packet-framing script"))
}

/// A packet-framing script (`SMBSF001`) of the first `count` packets of the first
/// video stream of `source`, then a drain.
fn framing_script(source: &Path, count: usize) -> R<Vec<u8>> {
    let data = fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let probe = crate::process::check_json(
        Path::new("ffprobe"),
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,extradata:packet=pts,dts,duration,pos,size,flags",
            "-show_data",
            "-of",
            "json",
            &source.display().to_string(),
        ],
    )?;
    let Some([stream]) = probe.get("streams").and_then(Value::as_array) else {
        return Err(format!("{}: no video stream", source.display()));
    };
    let codec = match stream.get("codec_name").and_then(Value::as_str) {
        Some("h264") => 1u8,
        Some("hevc") => 2,
        other => return Err(format!("{}: video codec {other:?}", source.display())),
    };
    let configuration = crate::ffprobe::data_bytes(
        stream
            .get("extradata")
            .and_then(Value::as_str)
            .ok_or("ffprobe: no extradata")?,
    )?;
    let packets = probe
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default();
    let packets = &packets[..packets.len().min(count)];
    let mut script = b"SMBSF001".to_vec();
    script.push(codec);
    script.extend_from_slice(&be32(configuration.len())?);
    script.extend_from_slice(&configuration);
    script.extend_from_slice(&be32(packets.len() + 1)?);
    for packet in packets {
        let position = integer(packet, "pos")?;
        let size = integer(packet, "size")?;
        let bytes = usize::try_from(position)
            .ok()
            .zip(usize::try_from(size).ok())
            .and_then(|(start, size)| data.get(start..start.checked_add(size)?))
            .ok_or_else(|| format!("{}: packet outside the file", source.display()))?;
        script.push(0);
        for key in ["pts", "dts", "duration"] {
            script.extend_from_slice(&integer(packet, key)?.to_be_bytes());
        }
        script.extend_from_slice(&position.to_be_bytes());
        let key = packet
            .get("flags")
            .and_then(Value::as_str)
            .is_some_and(|flags| flags.starts_with('K'));
        script.extend_from_slice(&u32::from(key).to_be_bytes());
        script.extend_from_slice(&0i32.to_be_bytes());
        script.extend_from_slice(&be32(bytes.len())?);
        script.extend_from_slice(&u32::MAX.to_be_bytes());
        script.extend_from_slice(bytes);
    }
    script.push(2);
    Ok(script)
}
