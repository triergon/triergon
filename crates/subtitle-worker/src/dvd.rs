//! The `dvd` operation: preparation and the timed decode.
//!
//! Preparation reads one DVD subtitle stream as the reference's demuxers present
//! it: a VobSub `.idx` with its sibling `.sub` ([`VobSubIndex`]), a program
//! stream (`.vob`, or a bare `.sub`) through the DVD subtitle parser
//! ([`SubpictureParser`]), a Matroska `S_VOBSUB` track or an MP4 `mp4s` track
//! (whose colour table becomes the reference's `palette:` configuration). The
//! operation runs the receiver ([`Decoder`]) over every packet and reports each
//! subtitle it outputs.

use crate::container::{self, Packet, Result, Track};
use crate::sink::{Bitmap, EventHead, RectHead, RectKind, Sink};
use crate::{Args, Prepared};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use triergon::{CodecId, TimeBase};
use triergon_core::{Budget, BudgetVec, WorkBudget};
use triergon_subtitles::compat::dvd::{
    Decoder, Limits, Settings, Status, SubpictureParser, SubpictureStreams, VobSubIndex,
    mp4_configuration,
};
use triergon_subtitles::dvd::program_stream::{Pes, Unit, Units};

/// The decoder's pool: one reassembled unit and one decoded area.
const POOL_BYTES: usize = 64 << 20;

struct Dvd {
    track: Track,
    options: Options,
    pool: Budget,
}

/// The decoder options, applied when the timed operation opens the decoder.
struct Options {
    palette: Option<String>,
    ifo_palette: Option<String>,
    forced_only: bool,
}

impl Options {
    /// The receiver's settings, as opening the reference decoder makes them:
    /// the configuration's palette, then the IFO file's, then the option's.
    fn settings(&self, configuration: &[u8]) -> Settings {
        let mut settings = Settings::from_configuration(configuration);
        if let Some(path) = &self.ifo_palette {
            settings.apply_ifo(std::fs::read(path).ok().as_deref());
        }
        if let Some(text) = &self.palette {
            settings.apply_palette_option(text.as_bytes());
        }
        settings.forced_only = self.forced_only;
        settings
    }
}

pub fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    args.only(&["palette", "ifo_palette", "forced_subs_only"])?;
    let mut head = [0; 8];
    let length = File::open(file)?.read(&mut head)?;
    let head = &head[..length];
    let track = if file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("idx"))
    {
        read_vobsub(file, args.stream)?
    } else if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        container::read_file_track(file, CodecId::VobSub, args.stream)?
    } else if head.get(4..8) == Some(b"ftyp") {
        let mut track = container::read_file_track(file, CodecId::VobSub, args.stream)?;
        track.configuration = mp4_configuration(&track.configuration)
            .map(|text| text.to_vec())
            .unwrap_or_default();
        track
    } else if head.starts_with(&[0, 0, 1, 0xba]) {
        read_program_stream(file, args.stream)?
    } else {
        return Err("dvd: unrecognised input".into());
    };
    let options = Options {
        palette: args.option("palette").map(str::to_owned),
        ifo_palette: args.option("ifo_palette").map(str::to_owned),
        forced_only: match args.option("forced_subs_only") {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => return Err(format!("forced_subs_only={other}").into()),
        },
    };
    Ok(Box::new(Dvd {
        track,
        options,
        pool: Budget::new(POOL_BYTES)?,
    }))
}

impl Prepared for Dvd {
    fn run(&self, sink: &mut Sink<'_>) -> Result<()> {
        let settings = self.options.settings(&self.track.configuration);
        let mut decoder = Decoder::new(&self.pool, settings, Limits::default());
        sink.header(self.track.packets.len(), None)?;
        for (index, packet) in self.track.packets.iter().enumerate() {
            let status = decoder.decode(&packet.data, &mut WorkBudget::new(u64::MAX))?;
            let Status::Subtitle(subtitle) = status else {
                continue;
            };
            let subtitle = match packet.duration {
                Some(duration) if duration > 0 => {
                    subtitle.ending_with_packet(milliseconds(duration, self.track.time_base))
                }
                _ => subtitle,
            };
            let bitmap = subtitle.bitmap;
            sink.begin(EventHead {
                packet: index as u64,
                pts: packet
                    .pts
                    .and_then(|pts| container::microseconds(pts, self.track.time_base)),
                start_ms: subtitle.start_ms,
                end_ms: subtitle.end_ms,
                format: 0,
                rects: 1,
            })?;
            sink.rect(
                RectHead {
                    kind: RectKind::Bitmap,
                    x: i32::from(bitmap.x),
                    y: i32::from(bitmap.y),
                    w: i32::from(bitmap.width),
                    h: i32::from(bitmap.height),
                    colors: bitmap.palette.len() as i32,
                    flags: i32::from(subtitle.forced),
                },
                Some(&Bitmap {
                    palette: bitmap.palette,
                    pixels: bitmap.pixels,
                    stride: bitmap.stride,
                }),
                None,
                None,
            )?;
            sink.end()?;
        }
        Ok(())
    }

    fn pool(&self) -> &Budget {
        &self.pool
    }

    fn input(&self) -> Option<&Track> {
        Some(&self.track)
    }
}

/// `value` ticks of `base` in milliseconds, rounded to nearest with ties away
/// from zero and saturated.
fn milliseconds(value: i64, base: TimeBase) -> u32 {
    let num = i128::from(value) * i128::from(base.numerator()) * 1000;
    let den = i128::from(base.denominator());
    u32::try_from((num + den / 2) / den).unwrap_or(u32::MAX)
}

fn packet(pts: Option<i64>, data: &[u8]) -> Packet {
    Packet {
        pts,
        dts: pts,
        duration: None,
        data: data.to_vec(),
    }
}

/// The units of the `stream`th track of a VobSub index, read from the sibling
/// `.sub` file, on the index's millisecond clock.
fn read_vobsub(file: &Path, stream: usize) -> Result<Track> {
    let pool = Budget::new(1 << 30)?;
    let index = VobSubIndex::parse(&pool, &std::fs::read(file)?)?;
    if stream >= index.streams().len() {
        return Err("dvd: no such stream".into());
    }
    let mut sub = File::open(file.with_extension("sub"))?;
    let length = sub.metadata()?.len();
    let mut extent = Vec::new();
    let mut data = BudgetVec::new(&pool);
    let mut packets = Vec::new();
    for entry in index.entries(stream) {
        // The extent, and room for the packet that ends it.
        let end = entry
            .end
            .map_or(length, |end| end.saturating_add(MAX_PES).min(length));
        extent.resize(usize::try_from(end.saturating_sub(entry.filepos))?, 0);
        sub.seek(SeekFrom::Start(entry.filepos.min(length)))?;
        sub.read_exact(&mut extent)?;
        if entry.read(&extent, &mut data)? {
            packets.push(packet(Some(entry.milliseconds), data.as_slice()));
        }
    }
    Ok(Track {
        configuration: index.configuration().to_vec(),
        time_base: TimeBase::new(1, 1000)?,
        packets,
    })
}

/// The largest PES packet: a start code, a length word and 65535 bytes.
const MAX_PES: u64 = 6 + 65535;

/// Every unit of a program stream file, read in bounded windows.
fn for_each_unit(file: &Path, mut f: impl FnMut(&Pes<'_>) -> Result<()>) -> Result<()> {
    // With the largest PES carried over, the window stays below the
    // allocator's mmap threshold, so both passes reuse the same heap pages.
    const WINDOW: usize = 32 * 1024;
    let mut input = File::open(file)?;
    let mut window = Vec::with_capacity(WINDOW + MAX_PES as usize);
    loop {
        let kept = window.len();
        window.resize(kept + WINDOW, 0);
        let got = input.read(&mut window[kept..])?;
        window.truncate(kept + got);
        let mut units = Units::new(&window);
        for unit in units.by_ref() {
            if let Unit::Pes(pes) = unit {
                f(&pes)?;
            }
        }
        let rest = units.position();
        if got == 0 {
            return Ok(());
        }
        window.drain(..rest);
    }
}

/// The units of the `stream`th subpicture stream of a program stream, joined
/// by the reference's parser, on the 90 kHz clock.
fn read_program_stream(file: &Path, stream: usize) -> Result<Track> {
    let mut streams = SubpictureStreams::default();
    for_each_unit(file, |pes| {
        streams.note(pes);
        Ok(())
    })?;
    let number = *streams
        .as_slice()
        .get(stream)
        .ok_or("dvd: no such stream")?;
    let pool = Budget::new(1 << 30)?;
    let mut parser = SubpictureParser::new(&pool);
    let mut packets = Vec::new();
    for_each_unit(file, |pes| {
        if let Some((_, data)) = pes.subpicture().filter(|(n, _)| *n == number)
            && let Some(unit) = parser.push(data, pes.pts)?
        {
            packets.push(packet(unit.pts.map(i64::try_from).transpose()?, unit.data));
        }
        Ok(())
    })?;
    Ok(Track {
        configuration: Vec::new(),
        time_base: TimeBase::new(1, 90_000)?,
        packets,
    })
}
