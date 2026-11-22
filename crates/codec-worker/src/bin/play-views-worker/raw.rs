use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};

use triergon::session::{Canvas, PcmBlock, Published};
use triergon::{Budget, Bytes, PacketInfo, RandomAccess, Result};
use triergon_harness_protocol::digest::signed;
use triergon_harness_protocol::md5;

#[allow(
    dead_code,
    reason = "the shared example also contains its CLI summary and ASS rendering"
)]
#[path = "../../../../triergon/examples/play/pipeline.rs"]
mod pipeline;

use super::protocol::{Operation, Work};
use pipeline::{Observer, Options};

fn options(arguments: &[OsString]) -> std::result::Result<Options, String> {
    let parse_track = |value: &OsStr| {
        if value == "-" {
            Ok(None)
        } else {
            value
                .to_str()
                .and_then(|text| text.parse::<usize>().ok())
                .filter(|index| i32::try_from(*index).is_ok())
                .map(Some)
                .ok_or("invalid track index")
        }
    };
    let (size, video, audio) = match arguments {
        [] => ("1920x1080", None, None),
        [size] => (size.to_str().ok_or("invalid canvas")?, None, None),
        [size, video, audio] => (
            size.to_str().ok_or("invalid canvas")?,
            parse_track(video)?,
            parse_track(audio)?,
        ),
        _ => return Err("expected [WxH [VIDEO_TRACK AUDIO_TRACK]]".into()),
    };
    let (width, height) = size.split_once('x').ok_or("invalid canvas")?;
    let width = width.parse::<usize>().map_err(|_| "invalid canvas")?;
    let height = height.parse::<usize>().map_err(|_| "invalid canvas")?;
    let canvas = Canvas::new(width, height).ok_or("invalid canvas")?;
    Ok(Options {
        canvas,
        format: None,
        video,
        audio,
    })
}

struct Raw {
    video: BufWriter<File>,
    audio: BufWriter<File>,
    packets: BufWriter<File>,
}

struct Output<'w, 't> {
    raw: Option<Raw>,
    trace: Option<&'t mut dyn Write>,
    work: &'w mut Work,
}

impl Observer for Output<'_, '_> {
    fn packet(&mut self, index: usize, packet: &impl PacketInfo) -> Result<()> {
        let base = packet.time_base();
        let (pts, dts, duration) = (packet.pts(), packet.dts(), packet.duration());
        let flags = u64::from(packet.random_access() == RandomAccess::Sync);
        let data = packet.payload();
        if let Some(raw) = &mut self.raw {
            writeln!(
                raw.packets,
                "{index} {}/{} {} {} {} {flags} {} {}",
                base.numerator(),
                base.denominator(),
                tick(pts),
                tick(dts),
                tick(duration),
                data.len(),
                md5::hex_digest(data),
            )?;
        } else {
            self.work.record(
                1,
                &[
                    index as u64,
                    base.numerator(),
                    base.denominator(),
                    word(pts),
                    word(dts),
                    word(duration),
                    flags,
                    data.len() as u64,
                ],
                data,
            );
        }
        Ok(())
    }

    fn picture(&mut self, picture: &Published) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            writeln!(trace, "v {}", tick(picture.pts))?;
        }
        if let Some(raw) = &mut self.raw {
            raw.video.write_all(&picture.rgba)?;
        } else {
            self.work.record(4, &[word(picture.pts)], &picture.rgba);
        }
        Ok(())
    }

    fn pcm(&mut self, block: &PcmBlock) -> Result<()> {
        if let Some(trace) = &mut self.trace {
            writeln!(trace, "a {} {}", block.sample_rate, block.samples.len() / 2)?;
        }
        if let Some(raw) = &mut self.raw {
            for sample in &block.samples {
                raw.audio.write_all(&sample.to_le_bytes())?;
            }
        } else {
            for stereo in block.samples.chunks_exact(2) {
                let [l0, l1] = stereo[0].to_le_bytes();
                let [r0, r1] = stereo[1].to_le_bytes();
                self.work
                    .record(3, &[u64::from(block.sample_rate)], &[l0, l1, r0, r1]);
            }
        }
        Ok(())
    }
}

fn tick(value: Option<i64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

fn word(value: Option<i64>) -> u64 {
    signed(value.unwrap_or(i64::MIN))
}

fn play(
    input: &[u8],
    options: Options,
    output: &mut Output<'_, '_>,
) -> std::result::Result<(), String> {
    let pool = Budget::new(384 << 20).map_err(|error| error.to_string())?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(|error| error.to_string())?;
    pipeline::play_with(&pool, bytes, options, output).map_err(|error| error.to_string())
}

pub(super) fn operation<'a>(
    _prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> std::result::Result<Operation<'a>, String> {
    let opts = options(arguments)?;
    Ok(Operation {
        name: "play",
        run: Box::new(move |work, trace| {
            play(
                input,
                opts,
                &mut Output {
                    raw: None,
                    trace,
                    work,
                },
            )
        }),
    })
}

pub(super) fn command(arguments: &[OsString]) -> std::result::Result<(), String> {
    let [file, video, audio, packets, arguments @ ..] = arguments else {
        return Err(
            "usage: play-raw FILE VIDEO AUDIO PACKETS [WxH [VIDEO_TRACK AUDIO_TRACK]]".into(),
        );
    };
    let run = || -> std::result::Result<(), Box<dyn std::error::Error>> {
        let opts = options(arguments)?;
        let mut input = Vec::new();
        File::open(file)?
            .take((256 << 20) + 1)
            .read_to_end(&mut input)?;
        if input.len() > 256 << 20 {
            return Err("input exceeds the worker limit".into());
        }
        let mut stdout = io::stdout().lock();
        let mut work = Work::default();
        let mut output = Output {
            raw: Some(Raw {
                video: BufWriter::new(File::create(video)?),
                audio: BufWriter::new(File::create(audio)?),
                packets: BufWriter::new(File::create(packets)?),
            }),
            trace: Some(&mut stdout),
            work: &mut work,
        };
        let result = play(&input, opts, &mut output);
        if let Some(raw) = &mut output.raw {
            raw.video.flush()?;
            raw.audio.flush()?;
            raw.packets.flush()?;
        }
        result?;
        Ok(())
    };
    run().map_err(|error| error.to_string())
}
