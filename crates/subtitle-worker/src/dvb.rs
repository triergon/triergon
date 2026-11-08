//! The `dvb` operation: preparation and the timed decode.
//!
//! Preparation reads one DVB subtitle stream as the reference's demuxers
//! present it, through [`container::read_file_track`]: from an MPEG-TS / M2TS
//! stream of type 0x06 whose PMT entry carries a subtitling descriptor (0x59),
//! each PES data field cut into codec packets; or from a Matroska `S_DVBSUB`
//! track. The configuration is five bytes per service: composition page,
//! ancillary page, subtitling type. The operation runs [`ReceiverDecoder`] over
//! every packet and reports each subtitle it outputs.
//!
//! Decoder options: `compute_edt` (0, 1), `compute_clut` (-2, -1, 0, 1) and
//! `dvb_substream` (-1 or a service index).

use crate::container::{self, Result, Track};
use crate::sink::{Bitmap, EventHead, RectHead, RectKind, Sink};
use crate::{Args, Prepared};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon::CodecId;
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::dvb::{AutomaticPalette, Options, ReceiverDecoder};

/// The candidate's pool: room for the decoder's regions and output.
const POOL_BYTES: usize = 1 << 30;

struct Dvb {
    track: Track,
    /// Packet times in microseconds.
    pts: Vec<Option<i64>>,
    options: Options,
    pool: Budget,
}

pub fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    args.only(&["compute_edt", "compute_clut", "dvb_substream"])?;
    let options = options(args)?;
    let track = container::read_file_track(file, CodecId::DvbSubtitle, args.stream)?;
    let pts = track
        .packets
        .iter()
        .map(|p| {
            p.pts
                .and_then(|pts| container::microseconds(pts, track.time_base))
        })
        .collect();
    Ok(Box::new(Dvb {
        track,
        pts,
        options,
        pool: Budget::new(POOL_BYTES)?,
    }))
}

fn options(args: &Args) -> Result<Options> {
    let refuse = |name: &str, value: &str| format!("dvb: {name}={value}");
    let end_from_next = match args.option("compute_edt") {
        None | Some("0") => false,
        Some("1") => true,
        Some(value) => return Err(refuse("compute_edt", value).into()),
    };
    let automatic_palette = match args.option("compute_clut") {
        None | Some("-1" | "-2") => AutomaticPalette::WithoutClut,
        Some("0") => AutomaticPalette::Never,
        Some("1") => AutomaticPalette::Always,
        Some(value) => return Err(refuse("compute_clut", value).into()),
    };
    let service = match args.option("dvb_substream") {
        None | Some("-1") => None,
        Some(value) => match value.parse::<usize>() {
            Ok(n @ 0..=63) => Some(n),
            _ => return Err(refuse("dvb_substream", value).into()),
        },
    };
    Ok(Options {
        end_from_next,
        automatic_palette,
        service,
    })
}

/// The code the reference reports for a refused packet (`AVERROR_INVALIDDATA`);
/// traced only.
const INVALID_DATA: i32 = -1_094_995_529;

impl Prepared for Dvb {
    fn run(&self, sink: &mut Sink<'_>) -> Result<()> {
        let cancel = AtomicBool::new(false);
        let mut decoder = ReceiverDecoder::new(&self.pool, &self.track.configuration, self.options);
        let count = self.track.packets.len();
        sink.header(count, None)?;
        for (index, (packet, &pts)) in self.track.packets.iter().zip(&self.pts).enumerate() {
            let mut work = WorkBudget::new(u64::MAX);
            let subtitle = match decoder.decode(&packet.data, pts, &mut work, &cancel) {
                Ok(Some(subtitle)) => subtitle,
                Ok(None) => continue,
                Err(triergon_core::Error::Invalid(_)) => {
                    sink.refused(index, INVALID_DATA)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            sink.begin(EventHead {
                packet: index as u64,
                pts: subtitle.pts,
                start_ms: subtitle.start_display_ms,
                end_ms: subtitle.end_display_ms,
                format: 0,
                rects: u32::try_from(subtitle.rects().len())?,
            })?;
            for rect in subtitle.rects() {
                let bitmap = Bitmap {
                    palette: rect.palette,
                    pixels: rect.pixels,
                    stride: usize::from(rect.width),
                };
                sink.rect(
                    RectHead {
                        kind: RectKind::Bitmap,
                        x: rect.x,
                        y: rect.y,
                        w: i32::from(rect.width),
                        h: i32::from(rect.height),
                        colors: i32::try_from(rect.palette.len())?,
                        flags: 0,
                    },
                    Some(&bitmap),
                    None,
                    None,
                )?;
            }
            sink.end()?;
        }
        Ok(())
    }

    fn input(&self) -> Option<&Track> {
        Some(&self.track)
    }
}
