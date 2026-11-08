//! Reference-compatible policies.
//!
//! The reader follows the specifications and keeps exact source facts. Where FFmpeg
//! reports an input differently and a consumer needs FFmpeg's result, the policies here
//! reproduce FFmpeg's behaviour for that input. Each module is one policy family; its
//! documentation names the FFmpeg build it reproduces.

/// The packet clock FFmpeg's Ogg demuxer reports.
///
/// [`Packet`] times each packet exactly: backwards from its page's
/// granule position with the samples every packet completes, the last page forwards
/// with an end trim. FFmpeg's clock differs in four ways, and
/// [`packet_timing`] reports it:
///
/// - Inside a page, a packet's time is the page's granule position less the durations
///   from it to the page end, where its own duration is the one it states alone. A
///   short Vorbis block after a long one states a quarter of two short blocks, not its
///   overlap with the long block, so that packet starts `(long - short) / 4` late and
///   is as much shorter. The first packet after a page that ends a packet takes that
///   page's granule position, even when pages were lost between them.
/// - The first Vorbis audio packet lasts as long as it states instead of completing no
///   samples, so the stream starts before zero.
/// - The last page is timed forwards from its first packet; the excess over its
///   granule position is reported as discard padding on its last packet, whose duration
///   shrinks accordingly (FFmpeg reports a negative result modulo 2^32; this clock keeps
///   it negative). Opus pre-skip is reported as skip samples on the first packet.
/// - A chained link continues where the previous link's last packet ended, without
///   its own start offset.
///
/// Reference: the `ogg` demuxer of FFmpeg 9.0.2, observed through `ffprobe
/// -show_packets` on the FATE Vorbis samples and on generated Vorbis, Opus and FLAC
/// files (`examples/ogg_compare.rs`).
pub mod clock {
    pub use crate::compat::{ReferenceTiming, packet_timing};
}

use crate::Packet;
use crate::mapping::Timeline;
use crate::timing::Slot;

/// A packet's time as FFmpeg's Ogg demuxer reports it, in the track time base.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ReferenceTiming {
    /// Presentation and decode time; FFmpeg reports the same value for both.
    pub pts: Option<i64>,
    pub duration: Option<i64>,
    /// Samples to skip at the start of the decoded output (`AV_PKT_DATA_SKIP_SAMPLES`).
    pub skip_samples: u32,
    /// Samples to discard at the end (the same side data's discard padding).
    pub discard_padding: u32,
}

/// FFmpeg's time for `packet`.
pub fn packet_timing(packet: &Packet) -> ReferenceTiming {
    packet.reference
}

/// The reference clock of one logical stream.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReferenceClock {
    /// The previous page's granule position, which the next packet takes.
    anchor: Option<i64>,
    /// Where the previous packet ended, for packets the demuxer leaves untimed.
    end: Option<i64>,
    /// The first packet time of the last page and the durations since.
    last_page: Option<(i64, i64)>,
    started: bool,
    /// The previous link's end, which this link's first packet moves to.
    chain_end: Option<i64>,
    shift: i64,
}

impl ReferenceClock {
    pub(crate) fn new(chain_end: Option<i64>) -> ReferenceClock {
        ReferenceClock {
            chain_end,
            ..ReferenceClock::default()
        }
    }

    /// A clock resuming mid-stream with a link shift found earlier.
    pub(crate) fn resumed(shift: i64) -> ReferenceClock {
        ReferenceClock {
            started: true,
            shift,
            ..ReferenceClock::default()
        }
    }

    /// The shift that moves this link after the previous one.
    pub(crate) fn shift(&self) -> i64 {
        self.shift
    }

    /// Where the last packet ended, as the next link continues.
    pub(crate) fn end(&self) -> Option<i64> {
        self.end.map(|end| end + self.shift)
    }

    /// A page with no media packets: its granule position still times the next packet.
    pub(crate) fn header_page(&mut self, granule: Option<i64>) {
        if granule.is_some() {
            self.anchor = granule;
        }
    }

    /// Time one page's packets. `granule` is in ticks; `raw_zero` says the page states
    /// granule position 0.
    pub(crate) fn page<S: AsMut<Slot>>(
        &mut self,
        timeline: &Timeline,
        granule: Option<i64>,
        raw_zero: bool,
        end_of_stream: bool,
        slots: &mut [S],
    ) {
        let backwards = matches!(timeline, Timeline::Vorbis { .. } | Timeline::Opus { .. });
        let trims = timeline.trims();
        let last = slots.len().saturating_sub(1);
        // Durations from each packet to the page end, the packet itself counted alone.
        let mut after = Some(0i64);
        let mut from_here = [None; 255];
        for (index, slot) in slots.iter_mut().map(AsMut::as_mut).enumerate().rev() {
            from_here[index] = after
                .zip(slot.durations.standalone)
                .map(|(rest, own)| rest + i64::from(own));
            after = after
                .zip(slot.durations.carried)
                .map(|(rest, own)| rest + i64::from(own));
        }
        for (index, slot) in slots.iter_mut().map(AsMut::as_mut).enumerate() {
            let mut duration = slot.durations.carried.map(i64::from);
            let mut pts = None;
            if backwards
                && !end_of_stream
                && let Some(granule) = granule
                && self.anchor.is_none_or(|anchor| anchor == 0)
            {
                duration = slot.durations.standalone.map(i64::from);
                pts = from_here[index].map(|rest| granule - rest);
                if raw_zero && from_here[index].is_some_and(|rest| rest != 0) {
                    pts = None;
                }
                self.anchor = None;
            } else if let Some(anchor) = self.anchor.take() {
                pts = Some(anchor);
                if end_of_stream && trims {
                    self.last_page = Some((anchor, 0));
                }
            }
            let mut padding = 0;
            if end_of_stream && trims {
                if let (true, Some((first, sum)), Some(granule), Some(own)) =
                    (index == last, self.last_page, granule, duration)
                {
                    let excess = first + sum + own - granule;
                    padding = u32::try_from(excess.max(0)).unwrap_or(u32::MAX);
                    duration = Some(granule - first - sum);
                }
                if let (Some((first, sum)), Some(own)) = (self.last_page, duration) {
                    self.last_page = Some((first, sum + own));
                }
            }
            let pts = pts.or(self.end);
            self.end = pts.zip(duration).map(|(pts, d)| pts + d).or(self.end);
            let skip = match timeline {
                Timeline::Opus { pre_skip } if !self.started => u32::from(*pre_skip),
                _ => 0,
            };
            if !self.started {
                self.started = true;
                if let (Some(end), Some(pts)) = (self.chain_end, pts) {
                    self.shift = end - pts;
                }
            }
            slot.reference = ReferenceTiming {
                pts: pts.map(|pts| pts + self.shift),
                duration,
                skip_samples: skip,
                discard_padding: padding,
            };
        }
        if granule.is_some() {
            self.anchor = granule;
        }
    }
}
