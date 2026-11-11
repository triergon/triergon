//! The `cc` operation: preparation and the timed decode.
//!
//! `source=packets` reads an SCC file as FFmpeg's `scc` demuxer does.
//! `source=video` (the default) takes the caption data of the Nth video stream
//! without decoding it: MPEG-2 video from a raw elementary stream or MPEG-TS,
//! H.264 and HEVC from MPEG-TS, Matroska or MP4, the containers read through
//! [`container::read_file_video_track`]. Each output frame carrying
//! caption data becomes one packet, in FFmpeg's output order and with its
//! times, from a [`CaptionExtractor`] fed packet by packet.

use crate::container::{self, Packet, Result, Track};
use crate::sink::{EventHead, RectHead, RectKind, Sink};
use crate::{Args, At, Prepared};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use triergon::{Budget, CodecId, TimeBase, WorkBudget};
use triergon_mpegts::PacketFormat;
use triergon_subtitles::cc::video::nal::{Codec, Framing};
use triergon_subtitles::compat::cc::{
    CaptionExtractor, Clock, DataField, Decoder, Options, SccPackets, Subtitle, VideoCoding,
    VideoPacket,
};

/// The reference drains with at most this many empty packets.
const MAX_DRAIN: usize = 64;

pub fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    let options = options(args)?;
    let track = if args.from_video {
        video_track(file, args.stream)?
    } else {
        scc_track(&std::fs::read(file)?)?
    };
    Ok(Box::new(Captions {
        track,
        options,
        pool: Budget::new(POOL_BYTES)?,
    }))
}

fn options(args: &Args) -> Result<Options> {
    args.only(&["real_time", "real_time_latency_msec", "data_field"])?;
    let mut options = Options::default();
    if let Some(value) = args.option("real_time") {
        options.real_time = match value {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => return Err(format!("cc: real_time {other}").into()),
        };
    }
    if let Some(value) = args.option("real_time_latency_msec") {
        let ms: u16 = value.parse()?;
        if ms > 500 {
            return Err("cc: real_time_latency_msec above 500".into());
        }
        options.real_time_latency_ms = ms;
    }
    if let Some(value) = args.option("data_field") {
        options.data_field = match value {
            "auto" | "-1" => DataField::Auto,
            "first" | "0" => DataField::First,
            "second" | "1" => DataField::Second,
            other => return Err(format!("cc: data_field {other}").into()),
        };
    }
    Ok(options)
}

fn scc_track(bytes: &[u8]) -> Result<Track> {
    let pool = Budget::new(1 << 30)?;
    let scc = SccPackets::read(bytes, &pool, &mut WorkBudget::new(u64::MAX))?;
    let packets = scc
        .iter()
        .map(|p| Packet {
            pts: Some(p.pts),
            dts: Some(p.pts),
            duration: Some(p.duration),
            data: p.data.to_vec(),
        })
        .collect();
    Ok(Track {
        configuration: Vec::new(),
        time_base: TimeBase::new(1, 1000)?,
        packets,
    })
}

/// The caption packets of the `ordinal`th video stream.
fn video_track(file: &Path, ordinal: usize) -> Result<Track> {
    let head = container::head(file)?;
    if head.starts_with(&[0, 0, 1, 0xb3]) {
        // The extractor takes any run of elementary stream bytes; reading the
        // file in chunks keeps preparation from touching the whole file.
        return extract(VideoCoding::Mpeg2Raw, (1, 1), |push| {
            let mut input = File::open(file)?;
            let mut chunk = vec![0; 32 * 1024];
            loop {
                let got = input.read(&mut chunk)?;
                if got == 0 {
                    return Ok(());
                }
                push(VideoPacket {
                    data: &chunk[..got],
                    pts: None,
                    dts: None,
                    duration: None,
                })?;
            }
        });
    }
    let (mut track, codec) = container::read_file_video_track(file, ordinal)?;
    let mp4 = !head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) && PacketFormat::detect(&head).is_none();
    if mp4 {
        let shift = mp4_edit_shift(&std::fs::read(file)?, ordinal).unwrap_or(0);
        for packet in &mut track.packets {
            packet.pts = packet.pts.map(|pts| pts.saturating_sub(shift));
        }
    }
    let coding = match codec {
        Some(CodecId::Mpeg2Video) => VideoCoding::Mpeg2Pes,
        Some(CodecId::H264) => VideoCoding::Nal(
            Codec::H264,
            length_framing(Codec::H264, &track.configuration),
        ),
        Some(CodecId::Hevc) => VideoCoding::Nal(
            Codec::Hevc,
            length_framing(Codec::Hevc, &track.configuration),
        ),
        _ => return Err("cc: captions are read from MPEG-2, H.264 or HEVC video".into()),
    };
    let time_base = (
        u32::try_from(track.time_base.numerator())?,
        u32::try_from(track.time_base.denominator())?,
    );
    extract(coding, time_base, |push| {
        track.packets.iter().try_for_each(|p| {
            push(VideoPacket {
                data: &p.data,
                pts: p.pts,
                dts: p.dts,
                duration: p.duration,
            })
        })
    })
}

/// The caption frames of the video packets `feed` pushes (decode order) as
/// packets of a track.
fn extract(
    coding: VideoCoding,
    time_base: (u32, u32),
    feed: impl FnOnce(&mut dyn FnMut(VideoPacket<'_>) -> Result<()>) -> Result<()>,
) -> Result<Track> {
    let pool = Budget::new(1 << 30)?;
    let mut work = WorkBudget::new(u64::MAX);
    let mut extractor = CaptionExtractor::new(&pool, coding, time_base);
    let mut frames = Vec::new();
    let collect = |extractor: &mut CaptionExtractor, frames: &mut Vec<Packet>| -> Result<()> {
        while let Some(frame) = extractor.pop()? {
            frames.push(Packet {
                pts: frame.pts,
                dts: frame.pts,
                duration: frame.duration,
                data: frame.data.to_vec(),
            });
        }
        Ok(())
    };
    feed(&mut |packet| {
        extractor.push(packet, &mut work)?;
        collect(&mut extractor, &mut frames)
    })?;
    extractor.finish()?;
    collect(&mut extractor, &mut frames)?;
    let (num, den) = extractor.time_base();
    Ok(Track {
        configuration: Vec::new(),
        time_base: TimeBase::new(num.into(), den.into())?,
        packets: frames,
    })
}

/// How far FFmpeg's MP4 demuxer moves the `ordinal`th video track's times by
/// its edit list: the first edit's `media_time`, less the empty edits before
/// it (in track units). The shared edit cursor of `triergon_isobmff` does not
/// take reordered video, so the video track's shift is applied here.
fn mp4_edit_shift(bytes: &[u8], ordinal: usize) -> Option<i64> {
    let moov = mp4_boxes(bytes).find(|(kind, _)| kind == b"moov")?.1;
    let movie_scale = mp4_boxes(moov)
        .find(|(kind, _)| kind == b"mvhd")
        .and_then(|(_, b)| mp4_timescale(b))?;
    let trak = mp4_boxes(moov)
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, trak)| trak)
        .filter(|trak| {
            let mdia = mp4_boxes(trak).find(|(k, _)| k == b"mdia");
            mdia.and_then(|(_, m)| mp4_boxes(m).find(|(k, _)| k == b"hdlr"))
                .is_some_and(|(_, h)| h.get(8..12) == Some(b"vide"))
        })
        .nth(ordinal)?;
    let mdia = mp4_boxes(trak).find(|(k, _)| k == b"mdia")?.1;
    let track_scale = mp4_boxes(mdia)
        .find(|(k, _)| k == b"mdhd")
        .and_then(|(_, b)| mp4_timescale(b))?;
    let edts = mp4_boxes(trak).find(|(k, _)| k == b"edts")?.1;
    let elst = mp4_boxes(edts).find(|(k, _)| k == b"elst")?.1;
    let (&version, rest) = elst.split_first()?;
    let count = u32::from_be_bytes(rest.get(3..7)?.try_into().ok()?) as usize;
    let entry_size = if version == 1 { 20 } else { 12 };
    let mut empty = 0i64;
    for entry in rest.get(7..)?.chunks_exact(entry_size).take(count) {
        let (duration, media_time) = if version == 1 {
            (
                i64::from_be_bytes(entry[..8].try_into().ok()?),
                i64::from_be_bytes(entry[8..16].try_into().ok()?),
            )
        } else {
            (
                i64::from(u32::from_be_bytes(entry[..4].try_into().ok()?)),
                i64::from(i32::from_be_bytes(entry[4..8].try_into().ok()?)),
            )
        };
        if media_time == -1 {
            empty = empty.checked_add(duration.checked_mul(track_scale)? / movie_scale)?;
        } else {
            return media_time.checked_sub(empty);
        }
    }
    None
}

/// The `timescale` of an `mvhd` or `mdhd` payload.
fn mp4_timescale(full_box: &[u8]) -> Option<i64> {
    let at = if full_box.first()? == &1 { 20 } else { 12 };
    let scale = u32::from_be_bytes(full_box.get(at..at + 4)?.try_into().ok()?);
    (scale > 0).then_some(i64::from(scale))
}

/// The boxes of an ISO BMFF payload: type and payload.
fn mp4_boxes(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len()),
            1 => (
                16,
                usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
            ),
            n => (8, n),
        };
        let payload = data.get(header..size)?;
        data = &data[size..];
        Some((kind, payload))
    })
}

/// The NAL unit length size of an `avcC` or `hvcC` configuration; start codes
/// without one.
fn length_framing(codec: Codec, configuration: &[u8]) -> Framing {
    let at = match codec {
        Codec::H264 => 4,
        Codec::Hevc => 21,
    };
    configuration
        .get(at)
        .map_or(Framing::StartCodes, |b| Framing::Lengths((b & 3) + 1))
}

/// The decoder's storage limit.
const POOL_BYTES: usize = 1 << 26;

struct Captions {
    track: Track,
    options: Options,
    pool: Budget,
}

impl Prepared for Captions {
    fn run(&self, sink: &mut Sink<'_>) -> Result<()> {
        let mut decoder = Decoder::new(&self.pool, self.options);
        let mut subtitle = Subtitle::new(&self.pool);
        let packets = &self.track.packets;
        sink.header(packets.len(), Some(decoder.header().as_bytes()))?;
        let base = self.track.time_base;
        for (index, packet) in packets.iter().enumerate() {
            let clock = Clock {
                pts: packet
                    .pts
                    .and_then(|pts| container::microseconds(pts, base)),
                duration_ms: packet.duration.map_or(0, |d| milliseconds(d, base)),
            };
            let mut work = WorkBudget::new(Decoder::packet_work(packet.data.len()));
            if decoder
                .decode(&packet.data, clock, &mut subtitle, &mut work)
                .at(index)?
            {
                emit(sink, index, &subtitle)?;
            }
        }
        for _ in 0..MAX_DRAIN {
            if !decoder.drain(&mut subtitle).at(packets.len())? {
                break;
            }
            emit(sink, packets.len(), &subtitle)?;
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

/// `value` in `base` units as milliseconds, rounded to nearest with ties away
/// from zero and kept to 32 bits, as the reference's display time is.
fn milliseconds(value: i64, base: TimeBase) -> u32 {
    container::rounded(value, base, 1000) as u32
}

fn emit(sink: &mut Sink<'_>, packet: usize, subtitle: &Subtitle) -> Result<()> {
    sink.begin(EventHead {
        packet: packet as u64,
        pts: subtitle.pts,
        start_ms: 0,
        end_ms: subtitle.end_ms,
        format: 1,
        rects: subtitle.lines().len() as u32,
    })?;
    for line in subtitle.lines() {
        let head = RectHead {
            kind: RectKind::Ass,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            colors: 0,
            flags: 0,
        };
        sink.rect(head, None, None, Some(line))?;
    }
    sink.end()?;
    Ok(())
}
