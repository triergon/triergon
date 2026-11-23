use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use triergon::mpegts::{
    Reader, ReaderLimits,
    psi::{DescriptorNamespace, DvbNamespaceEvidence},
};
use triergon::playback::Format;
use triergon::render::ass::{Change, Frame, ImageKind};
use triergon::session::{Canvas, PcmBlock, Published};
use triergon::{
    AnyReader, Budget, Bytes, Error, PacketInfo, RandomAccess, Result, SourceCapabilities, SourceId,
};
use triergon_harness_protocol::digest::signed;
use triergon_harness_protocol::md5;

use super::pipeline::{self, Observer, Options};
use triergon_harness_runner::{Operation, Totals};

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
    let (size, video, audio, atsc) = match arguments {
        [] => ("1920x1080", None, None, false),
        [size] => (size.to_str().ok_or("invalid canvas")?, None, None, false),
        [size, video, audio, namespace @ ..]
            if namespace.is_empty() || namespace == [OsStr::new("atsc")] =>
        {
            (
                size.to_str().ok_or("invalid canvas")?,
                parse_track(video)?,
                parse_track(audio)?,
                !namespace.is_empty(),
            )
        }
        _ => return Err("expected [WxH [VIDEO_TRACK AUDIO_TRACK [atsc]]]".into()),
    };
    let (width, height) = size.split_once('x').ok_or("invalid canvas")?;
    let width = width.parse::<usize>().map_err(|_| "invalid canvas")?;
    let height = height.parse::<usize>().map_err(|_| "invalid canvas")?;
    let canvas = Canvas::new(width, height)
        .filter(|canvas| canvas.width().max(canvas.height()) <= 8192)
        .ok_or("invalid canvas")?;
    Ok(Options {
        canvas,
        video,
        audio,
        transport: if atsc { open_atsc } else { open_dvb },
    })
}

fn open_atsc(
    pool: &Budget,
    id: SourceId,
    caps: SourceCapabilities,
    format: Format,
) -> Result<AnyReader> {
    open_ts(pool, id, caps, format, DescriptorNamespace::Atsc)
}

fn open_dvb(
    pool: &Budget,
    id: SourceId,
    caps: SourceCapabilities,
    format: Format,
) -> Result<AnyReader> {
    open_ts(
        pool,
        id,
        caps,
        format,
        DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
    )
}

fn open_ts(
    pool: &Budget,
    id: SourceId,
    caps: SourceCapabilities,
    format: Format,
    descriptor_namespace: DescriptorNamespace,
) -> Result<AnyReader> {
    let Format::Ts(format) = format else {
        return Err(Error::Invalid("expected a transport stream"));
    };
    let limits = ReaderLimits {
        format,
        descriptor_namespace,
        ..ReaderLimits::default()
    };
    Reader::new(pool, id, caps, limits, 64 << 10).map(AnyReader::MpegTs)
}

struct Raw {
    video: BufWriter<File>,
    audio: BufWriter<File>,
    packets: BufWriter<File>,
    subtitles: Option<BufWriter<File>>,
}

struct Output<'w, 't> {
    raw: Option<Raw>,
    trace: Option<&'t mut dyn Write>,
    work: &'w mut Totals,
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

    fn subtitle(&mut self, now_ms: i64, frame: Frame<'_>) -> Result<()> {
        let out = self
            .raw
            .as_mut()
            .and_then(|raw| raw.subtitles.as_mut())
            .ok_or(Error::Invalid("missing subtitle output"))?;
        let change = match frame.change {
            Change::None => 0,
            Change::Positions => 1,
            Change::Content => 2,
        };
        writeln!(out, "frame {now_ms} {change} {}", frame.images.len())?;
        for image in frame.images {
            let kind = match image.kind {
                ImageKind::Fill => 0,
                ImageKind::Border => 1,
                ImageKind::Shadow => 2,
                ImageKind::Background => 3,
            };
            write!(
                out,
                "image {kind} {} {} {} {} {:08x}",
                image.x,
                image.y,
                image.width(),
                image.height(),
                image.colour.0
            )?;
            for row in image.rows() {
                out.write_all(b" ")?;
                for byte in row {
                    write!(out, "{byte:02x}")?;
                }
            }
            writeln!(out)?;
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

fn preload(input: &[u8]) -> std::result::Result<(Budget, Bytes), String> {
    let pool = Budget::new(384 << 20).map_err(|error| error.to_string())?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(|error| error.to_string())?;
    Ok((pool, bytes))
}

pub(super) fn operation<'a>(
    _prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> std::result::Result<Operation<'a>, String> {
    let opts = options(arguments)?;
    let (pool, bytes) = preload(input)?;
    Ok(Operation {
        name: "play",
        run: Box::new(move |work, trace| {
            pipeline::play_with(
                &pool,
                bytes.clone(),
                opts,
                &mut Output {
                    raw: None,
                    trace,
                    work,
                },
            )
            .map_err(|error| error.to_string())
        }),
    })
}

pub(super) fn command(arguments: &[OsString]) -> std::result::Result<(), String> {
    let [file, video, audio, packets, arguments @ ..] = arguments else {
        return Err("usage: play-raw FILE VIDEO AUDIO PACKETS [WxH [VIDEO_TRACK AUDIO_TRACK [atsc]]] [--ass SCRIPT OUTPUT TIME...]".into());
    };
    let run = || -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (arguments, subtitles) = match arguments.iter().position(|arg| arg == "--ass") {
            Some(index) => {
                let (_, subtitle_args) = arguments.split_at(index);
                let [_, script, output, times @ ..] = subtitle_args else {
                    return Err("expected --ass SCRIPT OUTPUT TIME...".into());
                };
                let times = times
                    .iter()
                    .map(|time| time.to_str().and_then(|text| text.parse::<i64>().ok()))
                    .collect::<Option<Vec<_>>>()
                    .filter(|times| !times.is_empty())
                    .ok_or("expected signed subtitle times in milliseconds")?;
                (&arguments[..index], Some((script, output, times)))
            }
            None => (arguments, None),
        };
        let opts = options(arguments)?;
        let mut input = Vec::new();
        File::open(file)?
            .take((256 << 20) + 1)
            .read_to_end(&mut input)?;
        if input.len() > 256 << 20 {
            return Err("input exceeds the worker limit".into());
        }
        let mut stdout = io::stdout().lock();
        let mut work = Totals::default();
        let mut output = Output {
            raw: Some(Raw {
                video: BufWriter::new(File::create(video)?),
                audio: BufWriter::new(File::create(audio)?),
                packets: BufWriter::new(File::create(packets)?),
                subtitles: subtitles
                    .as_ref()
                    .map(|(_, path, _)| File::create(path).map(BufWriter::new))
                    .transpose()?,
            }),
            trace: Some(&mut stdout),
            work: &mut work,
        };
        let (pool, bytes) = preload(&input)?;
        let result = if let Some((script, _, times)) = subtitles {
            pipeline::play_with_subtitles(
                &pool,
                bytes,
                opts,
                Path::new(script),
                &times,
                &mut output,
            )
        } else {
            pipeline::play_with(&pool, bytes, opts, &mut output)
        };
        if let Some(raw) = &mut output.raw {
            raw.video.flush()?;
            raw.audio.flush()?;
            raw.packets.flush()?;
            if let Some(subtitles) = &mut raw.subtitles {
                subtitles.flush()?;
            }
        }
        result?;
        Ok(())
    };
    run().map_err(|error| error.to_string())
}
