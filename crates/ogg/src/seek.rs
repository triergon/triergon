//! Seeking: a bisection over pages by granule position, and the landing on the packet
//! delivery restarts from.
//!
//! A seek to `target` on a track lands on the packet whose output is needed first:
//!
//! - with [`SeekPolicy::PreviousRandomAccess`], the packet containing the target, or for
//!   Vorbis the packet before it, whose block the target packet overlaps;
//! - with [`SeekPolicy::PlaybackPreroll`], the same for `target` less the mapping's
//!   preroll (Opus 80 ms, RFC 7845 section 4.6), with output discarded up to the target.
//!
//! On a random-access source of known length the reader bisects the track's link for
//! the last page of the track whose granule position is early enough, probing a bounded
//! number of bytes at each step, and reads forward from it. Otherwise it reads the
//! source again from its start.
use crate::page::{MAX_PAGE, Page};
use crate::track::Packet;
use std::collections::VecDeque;
use triergon_core::container::{Event, SeekMethod, SeekPolicy, SeekReceipt};
use triergon_core::{Result, Time, TimeBase};

/// Search spans at most this long are read forward instead of probed.
const LINEAR: u64 = MAX_PAGE as u64;

/// A bisection over one link's pages for the last page of a serial at or before a
/// granule position.
#[derive(Debug)]
pub(crate) struct Bisection {
    serial: u32,
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
        goal: i64,
        start: u64,
        data: u64,
        end: u64,
        probe_limit: u64,
    ) -> Bisection {
        Bisection {
            serial,
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
            if granule <= self.goal {
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
    pub primes: bool,
    pub method: SeekMethod,
    pub scanned: u64,
    pub replays_from_start: bool,
    time_base: TimeBase,
    held: Option<Packet>,
    last_end: Option<i64>,
}

impl Landing {
    pub(crate) fn new(
        track: usize,
        requested: Time,
        policy: SeekPolicy,
        threshold: i64,
        primes: bool,
        method: SeekMethod,
        time_base: TimeBase,
    ) -> Landing {
        Landing {
            track,
            requested,
            policy,
            threshold,
            primes,
            method,
            scanned: 0,
            replays_from_start: method == SeekMethod::Restart,
            time_base,
            held: None,
            last_end: None,
        }
    }

    /// Offer a packet; `true` when the seek has landed and `out` holds the receipt and
    /// the packets that follow.
    pub(crate) fn offer(&mut self, packet: Packet, out: &mut VecDeque<Output>) -> Result<bool> {
        if packet.index != self.track {
            return Ok(false);
        }
        let end = packet.end();
        if end.is_none_or(|end| end <= self.threshold) {
            self.last_end = end.or(self.last_end);
            if self.primes {
                self.held = Some(packet);
            }
            return Ok(false);
        }
        let first = self.held.take();
        self.announce(first.as_ref().unwrap_or(&packet), out)?;
        out.extend(first.map(Event::Packet));
        out.push_back(Event::Packet(packet));
        Ok(true)
    }

    /// The input ended before the landing: land on the last packet seen, or where the
    /// track's packets ended.
    pub(crate) fn finish(mut self, out: &mut VecDeque<Output>) -> Result<()> {
        if let Some(packet) = self.held.take() {
            self.announce(&packet, out)?;
            out.push_back(Event::Packet(packet));
            return Ok(());
        }
        let landing = match self.last_end {
            Some(end) => Time::from_ticks(i128::from(end), self.time_base)?,
            None => self.requested,
        };
        out.push_back(Event::Seek(
            self.receipt(landing, landing.max(self.requested)),
        ));
        Ok(())
    }

    fn announce(&self, landing: &Packet, out: &mut VecDeque<Output>) -> Result<()> {
        // A Vorbis packet decoded first completes no samples; its output starts at its end.
        let ticks = landing.pts.or(landing.end()).unwrap_or(self.threshold);
        let time = Time::from_ticks(i128::from(ticks), self.time_base)?;
        let discard_until = match self.policy {
            SeekPolicy::PreviousRandomAccess => time,
            SeekPolicy::PlaybackPreroll => time.max(self.requested),
        };
        out.push_back(Event::Seek(self.receipt(time, discard_until)));
        Ok(())
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
