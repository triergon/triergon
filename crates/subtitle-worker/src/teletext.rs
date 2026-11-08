//! The `teletext` operation: preparation and the timed decode.
//!
//! Preparation reads the Nth DVB teletext stream of an MPEG-TS file (stream
//! type 0x06 with a teletext descriptor, 0x56) through
//! [`container::read_file_track`], each PES payload as the reference's `mpegts`
//! demuxer hands it to its decoder, timed by the program clock. The
//! configuration is the descriptor's type/magazine and page byte pairs.
//!
//! The operation runs [`Decoder`] over every packet, drains it with empty
//! packets and reports each subtitle. Decoder options: `txt_format` (`text`,
//! `ass`; the default `bitmap` is refused), `txt_page`, `txt_default_region`,
//! `txt_chop_top`, `txt_chop_spaces`, `txt_duration`, and the bitmap-only
//! `txt_left`, `txt_top`, `txt_transparent` and `txt_opacity`, which do not
//! change text output.

use crate::container::{self, Result, Track};
use crate::sink::{EventHead, RectHead, RectKind, Sink};
use crate::{Args, Prepared};
use std::path::Path;
use triergon::CodecId;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::teletext::{
    Decoded, Decoder, Format, Options, PageSelection, Refusal,
};

/// The candidate's pool: room for the page memory and the output queue.
const POOL_BYTES: usize = 64 << 20;
/// Drain calls after the last packet, as the reference makes at most.
const DRAIN: usize = 64;

struct Teletext {
    track: Track,
    /// Packet times in microseconds.
    pts: Vec<Option<i64>>,
    /// `None` for the `bitmap` format, which the operation refuses.
    options: Option<Options>,
    pool: Budget,
}

pub fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    args.only(&[
        "txt_format",
        "txt_page",
        "txt_default_region",
        "txt_chop_top",
        "txt_chop_spaces",
        "txt_duration",
        "txt_left",
        "txt_top",
        "txt_transparent",
        "txt_opacity",
    ])?;
    let options = options(args)?;
    let track = container::read_file_track(file, CodecId::DvbTeletext, args.stream)?;
    let pts = track
        .packets
        .iter()
        .map(|p| {
            p.pts
                .and_then(|pts| container::microseconds(pts, track.time_base))
        })
        .collect();
    Ok(Box::new(Teletext {
        track,
        pts,
        options,
        pool: Budget::new(POOL_BYTES)?,
    }))
}

/// The decoder options; `None` for the `bitmap` format.
fn options(args: &Args) -> Result<Option<Options>> {
    let integer = |name: &str, range: std::ops::RangeInclusive<i64>| -> Result<Option<i64>> {
        args.option(name)
            .map(|value| match value.parse::<i64>() {
                Ok(n) if range.contains(&n) => Ok(n),
                _ => Err(format!("{name}={value}").into()),
            })
            .transpose()
    };
    let format = match args.option("txt_format").unwrap_or("bitmap") {
        "text" | "1" => Format::Text,
        "ass" | "2" => Format::Ass,
        "bitmap" | "0" => return Ok(None),
        other => return Err(format!("txt_format={other}").into()),
    };
    for name in ["txt_left", "txt_top"] {
        integer(name, 0..=65535)?;
    }
    integer("txt_transparent", 0..=1)?;
    integer("txt_opacity", -1..=255)?;
    Ok(Some(Options {
        format,
        pages: args
            .option("txt_page")
            .map_or(PageSelection::All, PageSelection::parse),
        default_region: integer("txt_default_region", -1..=87)?.and_then(|r| u8::try_from(r).ok()),
        chop_top: integer("txt_chop_top", 0..=1)? != Some(0),
        chop_spaces: integer("txt_chop_spaces", 0..=1)? != Some(0),
        duration: integer("txt_duration", -1..=86_400_000)?.and_then(|d| u32::try_from(d).ok()),
    }))
}

impl Prepared for Teletext {
    fn run(&self, sink: &mut Sink<'_>) -> Result<()> {
        let options = self
            .options
            .clone()
            .ok_or("teletext: txt_format=bitmap is not supported")?;
        let mut decoder = Decoder::new(&self.pool, options)?;
        let count = self.track.packets.len();
        sink.header(count, Some(decoder.header().as_bytes()))?;
        for index in 0..=count + DRAIN {
            let (data, pts) = match self.track.packets.get(index) {
                Some(packet) => (packet.data.as_slice(), self.pts[index]),
                None => (&[][..], None),
            };
            let mut work = WorkBudget::new(Decoder::MAX_PACKET_WORK);
            let subtitle = match decoder.decode(data, pts, &mut work)? {
                Decoded::Subtitle(subtitle) => subtitle,
                Decoded::Refused(refusal) => {
                    sink.refused(index, error_code(refusal))?;
                    continue;
                }
                Decoded::Nothing if index >= count => break,
                Decoded::Nothing => continue,
            };
            sink.begin(EventHead {
                packet: index.min(count) as u64,
                pts: subtitle.pts,
                start_ms: 0,
                end_ms: subtitle.end_display,
                format: 1,
                rects: u32::from(subtitle.ass.is_some()),
            })?;
            if let Some(ass) = subtitle.ass {
                let head = RectHead {
                    kind: RectKind::Ass,
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                    colors: 0,
                    flags: 0,
                };
                sink.rect(head, None, None, Some(ass.as_bytes()))?;
            }
            sink.end()?;
        }
        Ok(())
    }

    fn input(&self) -> Option<&Track> {
        Some(&self.track)
    }
}

/// The reference's return codes (Linux errno values).
fn error_code(refusal: Refusal) -> i32 {
    match refusal {
        Refusal::InvalidData => -1_094_995_529,
        Refusal::QueueFull => -38,
    }
}
