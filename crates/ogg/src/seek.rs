//! Seeking: a bisection over pages by granule position, and the landing on the packet
//! delivery restarts from.
//!
//! A seek to `target` on a track lands on the packet whose output is needed first:
//!
//! - with [`SeekPolicy::PreviousRandomAccess`], the packet containing the target, or for
//!   Vorbis the packet before it, whose block the target packet overlaps, or for Theora
//!   the keyframe the target frame is decoded from;
//! - with [`SeekPolicy::PlaybackPreroll`], the same for `target` less the mapping's
//!   preroll (Opus 80 ms, RFC 7845 section 4.6), with output discarded up to the target.
//!
//! The target is on the physical stream's timeline. In a chained source the seek lands
//! in the link that holds it, on the track at the requested track's position among its
//! link's tracks: a known link that starts at or before the target, or, past the last
//! known link, the links that follow it as reading reaches them.
//!
//! On a random-access source of known length the reader bisects the link for the last
//! page of the track whose granule position is early enough, probing a bounded number
//! of bytes at each step, and reads forward from it; a Theora target that is not a
//! keyframe bisects again for the keyframe its page's granule positions name (Theora I
//! appendix A.2.3). Otherwise it reads the source again from its start.
use crate::mapping::Timeline;
use crate::page::{MAX_PAGE, Page};
use crate::queue::Queue;
use crate::track::{Packet, Track};
use triergon_core::container::{Event, RandomAccess, SeekMethod, SeekPolicy, SeekReceipt};
use triergon_core::{Result, Rounding, Time, TimeBase};

/// Search spans at most this long are read forward instead of probed.
const LINEAR: u64 = MAX_PAGE as u64;

/// A bisection over one link's pages for the last page of a serial whose packets end
/// at or before a tick.
#[derive(Debug)]
pub(crate) struct Bisection {
    serial: u32,
    timeline: Timeline,
    /// In link-local ticks of the track time base.
    goal: i64,
    /// Pages before this offset hold headers.
    data: u64,
    /// A page start delivery may resume from: the link start, or a page whose granule
    /// position is at most the goal.
    lo: u64,
    /// No page at or after this offset is early enough.
    hi: u64,
    /// The offset being probed, and the bytes read since.
    probe: Option<(u64, u64)>,
    probe_limit: u64,
    pub scanned: u64,
}

/// What a probe concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// Keep reading pages.
    Continue,
    /// The probe is settled; choose the next.
    Settled,
}

impl Bisection {
    pub(crate) fn new(
        serial: u32,
        timeline: Timeline,
        goal: i64,
        start: u64,
        data: u64,
        end: u64,
        probe_limit: u64,
    ) -> Bisection {
        Bisection {
            serial,
            timeline,
            goal,
            data,
            lo: start,
            hi: end,
            probe: None,
            probe_limit,
            scanned: 0,
        }
    }

    /// The next offset to probe, or `None` when delivery should resume at
    /// [`Bisection::resume`].
    pub(crate) fn next_probe(&mut self) -> Option<u64> {
        let floor = self.lo.max(self.data);
        if self.hi <= floor || self.hi - floor <= LINEAR {
            return None;
        }
        let probe = floor + (self.hi - floor) / 2;
        self.probe = Some((probe, 0));
        Some(probe)
    }

    pub(crate) fn resume(&self) -> u64 {
        self.lo
    }

    /// Whether a probe is being read.
    pub(crate) fn probing(&self) -> bool {
        self.probe.is_some()
    }

    /// A page read while probing; `known` says whether its serial belongs to the link.
    pub(crate) fn page(&mut self, page: &Page, known: bool) -> Probe {
        let Some((start, read)) = self.probe.as_mut() else {
            return Probe::Settled;
        };
        let bytes = page.skipped + page.bytes.len() as u64;
        *read += bytes;
        self.scanned += bytes;
        let start = *start;
        let header = &page.header;
        if page.offset >= self.hi || (header.begin_of_stream() && !known) {
            return self.exhausted();
        }
        if header.serial == self.serial
            && page.offset >= self.data
            && let Some(granule) = header.granule_position()
        {
            if self.timeline.ticks(granule) <= self.goal {
                self.lo = page.offset;
            } else {
                self.hi = start;
            }
            self.probe = None;
            return Probe::Settled;
        }
        if self.probe.is_some_and(|(_, read)| read > self.probe_limit) {
            return self.exhausted();
        }
        Probe::Continue
    }

    /// The probe found no page of the serial before the end, the next link or its limit.
    pub(crate) fn exhausted(&mut self) -> Probe {
        if let Some((start, _)) = self.probe.take() {
            self.hi = start;
        }
        Probe::Settled
    }
}

/// Delivery output.
pub(crate) type Output = Event<Packet, crate::ReaderEvent>;

/// Skips packets until the landing of a seek and then announces it.
#[derive(Debug)]
pub(crate) struct Landing {
    pub track: usize,
    pub requested: Time,
    pub policy: SeekPolicy,
    /// Packets ending at or before this tick are not needed.
    pub threshold: i64,
    primes: bool,
    /// The track's timeline and link offset, for the keyframes its granule positions
    /// name.
    timeline: Timeline,
    offset: i64,
    /// Where the last keyframe of the track seen starts, for mappings with keyframes.
    keyframe: Option<i64>,
    /// The threshold was moved back to the keyframe the target needs.
    rewound: bool,
    pub method: SeekMethod,
    pub scanned: u64,
    pub replays_from_start: bool,
    time_base: TimeBase,
    held: Option<Packet>,
    last_end: Option<i64>,
}

/// What offering a packet did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// The packet is not needed.
    Skipped,
    /// The seek has landed: the receipt and the packets from the landing are queued.
    Landed,
    /// The target is not a keyframe: delivery must restart before the keyframe it
    /// needs, which the threshold now names.
    Rewind,
}

impl Landing {
    /// A landing on track `index` for `requested`.
    pub(crate) fn new(
        index: usize,
        track: &Track,
        requested: Time,
        policy: SeekPolicy,
        method: SeekMethod,
    ) -> Result<Landing> {
        let mut landing = Landing {
            track: index,
            requested,
            policy,
            threshold: 0,
            primes: false,
            timeline: *track.timeline(),
            offset: 0,
            keyframe: None,
            rewound: false,
            method,
            scanned: 0,
            replays_from_start: method == SeekMethod::Restart,
            time_base: track.time_base,
            held: None,
            last_end: None,
        };
        landing.retarget(index, track)?;
        Ok(landing)
    }

    /// Land on another track instead: the one at the same position of the next chain
    /// link, when the seek did not land in the previous one.
    pub(crate) fn retarget(&mut self, index: usize, track: &Track) -> Result<()> {
        let timeline = *track.timeline();
        let preroll = match self.policy {
            SeekPolicy::PlaybackPreroll => timeline.preroll(),
            SeekPolicy::PreviousRandomAccess => 0,
        };
        let target = self
            .requested
            .to_i64_ticks(track.time_base, Rounding::Floor)?;
        self.track = index;
        self.threshold = target.saturating_sub(preroll);
        self.primes = timeline.primes();
        self.timeline = timeline;
        self.offset = track.offset;
        self.keyframe = None;
        self.rewound = false;
        self.time_base = track.time_base;
        self.held = None;
        self.last_end = None;
        Ok(())
    }

    /// Offer a packet of any track.
    pub(crate) fn offer(&mut self, packet: Packet, out: &mut Queue<Output>) -> Result<Offer> {
        if packet.index != self.track {
            return Ok(Offer::Skipped);
        }
        let end = packet.end();
        // The keyframe this packet is decoded from: the last keyframe packet, or the
        // one its page's granule position names when it ends the page.
        let keyframes = self.timeline.keyframes();
        if keyframes {
            if packet.random_access == RandomAccess::Sync {
                self.keyframe = packet.pts;
            }
            // A keyframe tick out of range names none.
            if let Some(named) = packet
                .granule
                .and_then(|g| self.timeline.keyframe(g))
                .and_then(|tick| tick.checked_add(self.offset))
            {
                self.keyframe = Some(named);
            }
        }
        if end.is_none_or(|end| end <= self.threshold) {
            self.last_end = end.or(self.last_end);
            if self.primes {
                self.held = Some(packet);
            }
            return Ok(Offer::Skipped);
        }
        if keyframes
            && !self.rewound
            && packet.random_access != RandomAccess::Sync
            && let Some(keyframe) = self.keyframe
        {
            // Packets ending at or before the keyframe's start are not needed.
            self.rewound = true;
            self.threshold = keyframe;
            self.last_end = None;
            return Ok(Offer::Rewind);
        }
        let first = self.held.take();
        self.announce(first.as_ref().unwrap_or(&packet), out)?;
        if let Some(first) = first {
            out.push(Event::Packet(first))?;
        }
        out.push(Event::Packet(packet))?;
        Ok(Offer::Landed)
    }

    /// The input ended before the landing: land on the last packet seen, or where the
    /// track's packets ended.
    pub(crate) fn finish(mut self, out: &mut Queue<Output>) -> Result<()> {
        if let Some(packet) = self.held.take() {
            self.announce(&packet, out)?;
            out.push(Event::Packet(packet))?;
            return Ok(());
        }
        let landing = match self.last_end {
            Some(end) => Time::from_ticks(i128::from(end), self.time_base)?,
            None => self.requested,
        };
        out.push(Event::Seek(
            self.receipt(landing, landing.max(self.requested)),
        ))
    }

    fn announce(&self, landing: &Packet, out: &mut Queue<Output>) -> Result<()> {
        // A Vorbis packet decoded first completes no samples; its output starts at its end.
        let ticks = landing.pts.or(landing.end()).unwrap_or(self.threshold);
        let time = Time::from_ticks(i128::from(ticks), self.time_base)?;
        let discard_until = match self.policy {
            SeekPolicy::PreviousRandomAccess => time,
            SeekPolicy::PlaybackPreroll => time.max(self.requested),
        };
        out.push(Event::Seek(self.receipt(time, discard_until)))
    }

    fn receipt(&self, landing: Time, discard_until: Time) -> SeekReceipt {
        SeekReceipt {
            requested: self.requested,
            landing,
            discard_until,
            method: self.method,
            scanned_bytes: self.scanned,
            replays_from_start: self.replays_from_start,
        }
    }
}
