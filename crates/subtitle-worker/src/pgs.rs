//! The `pgs` operation: preparation and the timed decode.
//!
//! Preparation reads one PGS stream as the reference's demuxers present it: a
//! `.sup` file as one packet per `PG` unit, or through
//! [`container::read_file_track`] a Matroska `S_HDMV/PGS` track or an MPEG-TS /
//! M2TS stream of type 0x90 as one packet per PES. The readers stream the file,
//! so only the packets stay resident. The operation runs the receiver
//! ([`ReceiverDecoder`]) over every packet and reports each subtitle it outputs.

use crate::container::{self, Packet, Result, Track};
use crate::sink::{Bitmap, EventHead, RectHead, RectKind, Sink};
use crate::{Args, Prepared};
use std::cell::OnceCell;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use triergon::{CodecId, TimeBase};
use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_mpegts::PacketFormat;
use triergon_subtitles::compat::pgs::{ReceiverDecoder, Refusal};
use triergon_subtitles::pgs::{
    DisplayConfig, DisplayLimits, PacketClock, RecoveredRect, RecoveryStatus,
};

/// The candidate's pool limit: the prepared packets and one display set.
const POOL_BYTES: usize = 1 << 30;

struct Pgs {
    pool: Budget,
    configuration: Vec<u8>,
    time_base: TimeBase,
    /// The prepared packets, moved into the pool; each operation clones them.
    packets: Vec<(Bytes, PacketClock)>,
    forced_only: bool,
    /// The `sub-input` view, built from `packets` only when asked for.
    input: OnceCell<Track>,
}

pub fn prepare(file: &Path, args: &Args) -> Result<Box<dyn Prepared>> {
    args.only(&["forced_subs_only"])?;
    let forced_only = match args.option("forced_subs_only") {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => return Err(format!("forced_subs_only={other}").into()),
    };
    let head = container::head(file)?;
    let matroska = head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]);
    let track = if matroska || PacketFormat::detect(&head).is_some() {
        container::read_file_track(file, CodecId::Pgs, args.stream)?
    } else {
        sup::read(BufReader::new(File::open(file)?))?
    };
    let pool = Budget::new(POOL_BYTES)?;
    let packets = track
        .packets
        .into_iter()
        .map(|packet| {
            let clock = PacketClock {
                pts: packet.pts,
                dts: packet.dts,
                timebase: track.time_base,
            };
            Ok((
                Bytes::try_from_vec(&pool, packet.data).map_err(|e| e.error())?,
                clock,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(Box::new(Pgs {
        pool,
        configuration: track.configuration,
        time_base: track.time_base,
        packets,
        forced_only,
        input: OnceCell::new(),
    }))
}

impl Prepared for Pgs {
    fn run(&self, sink: &mut Sink<'_>) -> Result<()> {
        let cancel = AtomicBool::new(false);
        // The reference's own ceilings: objects no larger than a canvas it
        // accepts, 24-bit object lengths, no fragment, packet or work bound.
        let limits = DisplayLimits {
            max_object_pixels: 1 << 28,
            max_display_pixels: 1 << 29,
            max_encoded: 1 << 24,
            max_fragments: usize::MAX,
            max_packet_bytes: usize::MAX,
            max_work: usize::MAX,
            ..DisplayLimits::default()
        };
        let config = DisplayConfig {
            receiver_forced_only: self.forced_only,
        };
        let mut decoder = ReceiverDecoder::new(&self.pool, limits, config)?;
        sink.header(self.packets.len(), None)?;
        for (index, (payload, clock)) in self.packets.iter().enumerate() {
            if payload.is_empty() {
                continue;
            }
            decoder.push(&mut Some(payload.clone()), *clock, &cancel)?;
            let out = loop {
                match decoder.poll(&mut WorkBudget::new(u64::MAX), &cancel)? {
                    RecoveryStatus::OutputReady => break decoder.take_output(&cancel)?,
                    RecoveryStatus::Yield => {}
                    status => return Err(format!("pgs: unexpected {status:?}").into()),
                }
            };
            if let Some(refusal) = out.refusal() {
                // The reference's return codes: AVERROR(EPERM), AVERROR(ENOMEM).
                let code = match refusal {
                    Refusal::ShortPacket => -1,
                    Refusal::ObjectLength => -12,
                };
                sink.refused(index, code)?;
            }
            if let Some(event) = out.receiver_output() {
                let rects = event.rectangles();
                sink.begin(EventHead {
                    packet: index as u64,
                    pts: event.pts_microseconds,
                    start_ms: 0,
                    end_ms: u32::MAX,
                    format: 0,
                    rects: u32::try_from(rects.len())?,
                })?;
                for rect in rects {
                    report(sink, rect)?;
                }
                sink.end()?;
            }
        }
        decoder.finish_input(&cancel)?;
        while decoder.poll(&mut WorkBudget::new(u64::MAX), &cancel)? != RecoveryStatus::Ended {}
        Ok(())
    }

    fn input(&self) -> Option<&Track> {
        Some(self.input.get_or_init(|| {
            Track {
                configuration: self.configuration.clone(),
                time_base: self.time_base,
                packets: self
                    .packets
                    .iter()
                    .map(|(data, clock)| Packet {
                        pts: clock.pts,
                        dts: clock.dts,
                        duration: None,
                        data: data.to_vec(),
                    })
                    .collect(),
            }
        }))
    }
}

/// One rectangle as the reference's API presents it: 256 palette words and
/// the forced flag (`AV_SUBTITLE_FLAG_FORCED`).
fn report(sink: &mut Sink<'_>, rect: &RecoveredRect) -> Result<()> {
    let bitmap = Bitmap {
        palette: rect.palette(),
        pixels: rect.pixels().as_ref(),
        stride: usize::from(rect.stride),
    };
    let head = RectHead {
        kind: RectKind::Bitmap,
        x: i32::from(rect.x),
        y: i32::from(rect.y),
        w: i32::from(rect.width),
        h: i32::from(rect.height),
        colors: 256,
        flags: i32::from(rect.forced),
    };
    Ok(sink.rect(head, Some(&bitmap), None, None)?)
}

/// `.sup` files as the reference's sup demuxer reads them.
mod sup {
    use crate::container::{Packet, Result, Track};
    use std::io::Read;
    use triergon::TimeBase;

    /// The payload bytes the reference reads while probing a stream before it
    /// stops on its own (its default probe size).
    const PROBE_BYTES: usize = 5_000_000;

    /// One packet per `PG` unit.
    ///
    /// A unit is the `PG` magic, a 32-bit PTS and DTS on a 90 kHz clock and the
    /// segment (type, 16-bit length, body), which is the packet's payload. A
    /// DTS of zero or after the PTS reads as the PTS. A segment cut short by the
    /// end of the file is passed as far as it goes, unless nothing of a
    /// declared body is left; a unit with no segment byte ends the stream.
    ///
    /// A unit without the magic is an error, except once while the reference
    /// is still probing the stream: it then skips those two bytes and reads on.
    /// Probing ends after a PCS that declares a valid canvas, after
    /// [`PROBE_BYTES`] payload bytes, or at that first skipped magic.
    pub fn read(mut file: impl Read) -> Result<Track> {
        let mut packets = Vec::new();
        let mut probing = true;
        let mut probed = 0;
        loop {
            let mut magic = [0; 2];
            if fill(&mut file, &mut magic)? < 2 {
                break;
            }
            if magic != *b"PG" {
                if !(probing && probed < PROBE_BYTES) {
                    return Err("pgs: invalid sup unit".into());
                }
                probing = false;
                continue;
            }
            let mut clocks = [0; 8];
            let mut header = [0; 3];
            if fill(&mut file, &mut clocks)? < 8 {
                break;
            }
            let read = fill(&mut file, &mut header)?;
            if read == 0 {
                break;
            }
            let length = match header[..read] {
                [_, high, low] => u16::from_be_bytes([high, low]),
                _ => 0,
            };
            let mut data = Vec::with_capacity(read + usize::from(length));
            data.extend_from_slice(&header[..read]);
            if length != 0 && (&mut file).take(u64::from(length)).read_to_end(&mut data)? == 0 {
                break;
            }
            if probing {
                probed += data.len();
                probing = !(data[0] == 0x16 && declares_canvas(data.get(3..).unwrap_or_default()));
            }
            let word = |at: usize| {
                i64::from(u32::from_be_bytes([
                    clocks[at],
                    clocks[at + 1],
                    clocks[at + 2],
                    clocks[at + 3],
                ]))
            };
            let (pts, dts) = (word(0), word(4));
            packets.push(Packet {
                pts: Some(pts),
                dts: Some(if dts == 0 || dts > pts { pts } else { dts }),
                duration: None,
                data,
            });
        }
        Ok(Track {
            configuration: Vec::new(),
            time_base: TimeBase::new(1, 90_000)?,
            packets,
        })
    }

    /// Reads into `buffer` until it is full or the input ends.
    fn fill(file: &mut impl Read, buffer: &mut [u8]) -> Result<usize> {
        let mut read = 0;
        while read < buffer.len() {
            match file.read(&mut buffer[read..])? {
                0 => break,
                n => read += n,
            }
        }
        Ok(read)
    }

    /// Whether a PCS body starts with a canvas size the reference accepts.
    fn declares_canvas(body: &[u8]) -> bool {
        let &[w0, w1, h0, h1, ..] = body else {
            return false;
        };
        let w = u64::from(u16::from_be_bytes([w0, w1]));
        let h = u64::from(u16::from_be_bytes([h0, h1]));
        w > 0 && h > 0 && (w + 128) * (h + 128) < u64::from(i32::MAX.unsigned_abs()) / 8
    }
}
