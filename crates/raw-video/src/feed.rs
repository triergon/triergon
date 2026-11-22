//! Source-owned raw packets with an active decoder configuration and an
//! explicit sink clock. Annex B carries no container PTS/DTS.

use super::{RawVideoConfigTracker, RawVideoConfiguration, RawVideoSource, State as SourceState};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, Rounding, SourceLocation, Step, Time, WorkBudget};
use triergon_framing::nal::RawAccessUnit;
use triergon_playback::Source;

/// Caller-supplied submission cadence in coded-picture order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RawFeedClock {
    /// No submission clock: packets carry no `submission_time`.
    #[default]
    Unspecified,
    /// The `n`th coded picture is submitted at `start + n * picture_duration`.
    FixedSubmission {
        /// The submission time of the first coded picture.
        start: Time,
        /// The interval between coded pictures; positive.
        picture_duration: Time,
    },
}

#[derive(Debug)]
struct SubmissionClock {
    next: Time,
    picture_duration: Time,
}

/// A raw Annex B access unit with its decoder configuration, retained across
/// later updates. Borrow its NAL slices through `RawAccessUnit::segments` and
/// join them for decoders that take contiguous input.
#[derive(Debug)]
pub struct RawVideoFeedPacket {
    /// The access unit, aliasing the reader's NAL owners.
    pub unit: RawAccessUnit,
    /// The active configuration after this unit, once one is known.
    pub configuration: Option<RawVideoConfiguration>,
    /// The number of configuration changes so far, from 1; present with `configuration`.
    pub configuration_revision: Option<u64>,
    /// Whether this unit changed the configuration.
    pub configuration_changed: bool,
    /// Present only for a coded picture under an explicit submission policy.
    pub submission_time: Option<Time>,
    queue_timestamp_ns: i64,
}

impl RawVideoFeedPacket {
    /// Existing video queues that use zero for an unstamped packet can take
    /// this boundary value without turning it into a source timestamp.
    pub const fn queue_timestamp_ns(&self) -> i64 {
        self.queue_timestamp_ns
    }
}

#[derive(Debug)]
struct Active<S: Source> {
    source: RawVideoSource<S>,
    tracker: RawVideoConfigTracker,
    pending: Option<RawAccessUnit>,
    configuration: Option<RawVideoConfiguration>,
    revision: u64,
    clock: Option<SubmissionClock>,
}

#[allow(
    clippy::large_enum_variant,
    reason = "one inline decoder configuration tracker avoids an uncharged allocation"
)]
#[derive(Debug)]
enum State<S: Source> {
    Active(Active<S>),
    End(SourceLocation),
    Failed(Error),
}

/// Raw access units with their active decoder configuration and optional submission
/// clock, from [`RawVideoSource::into_feed`].
#[derive(Debug)]
pub struct RawVideoFeed<S: Source> {
    state: State<S>,
}

impl<S: Source> RawVideoSource<S> {
    /// Attaches configuration tracking and an optional clock before the first unit.
    pub fn into_feed(
        self,
        maximum_parameter_set_bytes: usize,
        clock: RawFeedClock,
    ) -> Result<RawVideoFeed<S>> {
        let SourceState::Active { reader, .. } = &self.state else {
            return Err(Error::Closed);
        };
        if reader.published_bytes() != 0 {
            return Err(Error::Invalid(
                "raw feed must attach before first access unit",
            ));
        }
        let tracker = RawVideoConfigTracker::new(
            reader.budget(),
            reader.codec(),
            maximum_parameter_set_bytes,
        )?;
        let clock = match clock {
            RawFeedClock::Unspecified => None,
            RawFeedClock::FixedSubmission {
                start,
                picture_duration,
            } => {
                if picture_duration.numerator() <= 0 {
                    return Err(Error::Invalid("nonpositive raw submission duration"));
                }
                start.to_nanoseconds(Rounding::NearestAway)?;
                Some(SubmissionClock {
                    next: start,
                    picture_duration,
                })
            }
        };
        Ok(RawVideoFeed {
            state: State::Active(Active {
                source: self,
                tracker,
                pending: None,
                configuration: None,
                revision: 0,
                clock,
            }),
        })
    }
}

impl<S: Source> RawVideoFeed<S> {
    /// The next access unit within `work`. A failure other than backpressure or
    /// allocation is final: later polls return it again.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawVideoFeedPacket>> {
        let result = match &mut self.state {
            State::Active(active) => active.poll(work, cancel),
            State::End(at) => return Ok(Step::End(*at)),
            State::Failed(error) => return Err(*error),
        };
        match &result {
            Ok(Step::End(at)) => self.state = State::End(*at),
            Err(error) if !matches!(error, Error::Backpressure { .. } | Error::Allocation) => {
                self.state = State::Failed(*error);
            }
            _ => {}
        }
        result
    }
}

impl<S: Source> Active<S> {
    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawVideoFeedPacket>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.pending.is_none() {
            let unit = match self.source.poll(work, cancel)? {
                Step::Ready(unit) => unit,
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::Yield => return Ok(Step::Yield),
                Step::End(at) => return Ok(Step::End(at)),
            };
            self.pending = Some(unit);
        }
        if !work.try_charge(1) {
            return Ok(Step::Yield);
        }
        // One bounded configuration pass over the reader's admitted NALs.
        // On pressure, the tracker and the pending packet are retryable.
        let unit = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("raw feed unit missing"))?;
        let schedule = if unit.has_vcl {
            self.clock
                .as_ref()
                .map(|clock| {
                    let nanos = clock.next.to_nanoseconds(Rounding::NearestAway)?;
                    let next = clock.next.checked_add(clock.picture_duration)?;
                    Ok::<_, Error>((clock.next, nanos, next))
                })
                .transpose()?
        } else {
            None
        };
        let configuration = self.tracker.observe(unit)?;
        let mut changed = false;
        if let Some(configuration) = configuration
            && self.configuration.as_ref() != Some(&configuration)
        {
            self.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
            self.configuration = Some(configuration);
            changed = true;
        }
        if let (Some(clock), Some((_, _, next))) = (&mut self.clock, schedule) {
            clock.next = next;
        }
        let unit = self
            .pending
            .take()
            .ok_or(Error::Invalid("raw feed unit missing"))?;
        Ok(Step::Ready(RawVideoFeedPacket {
            unit,
            configuration: self.configuration.clone(),
            configuration_revision: self.configuration.as_ref().map(|_| self.revision),
            configuration_changed: changed,
            submission_time: schedule.map(|(at, _, _)| at),
            queue_timestamp_ns: schedule.map_or(0, |(_, nanos, _)| nanos),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId};
    use triergon_framing::nal::{Codec, RawAnnexBLimits, RawAnnexBReader};
    use triergon_playback::ByteSource;

    #[test]
    fn configuration_pressure_retains_pending_packet_and_submission_clock() {
        let pool = Budget::new(1 << 20).unwrap();
        let bytes = include_bytes!("../../playback/tests/fixtures/raw-video/avc-short.h264");
        let input = || {
            let id = SourceId(22);
            let reader = RawAnnexBReader::new(
                &pool,
                Codec::Avc,
                id,
                SourceCapabilities {
                    length: Some(bytes.len() as u64),
                    random_access: false,
                },
                RawAnnexBLimits::default(),
            )
            .unwrap();
            let source =
                ByteSource::new(id, Bytes::copy_from_slice(&pool, bytes).unwrap(), 512).unwrap();
            RawVideoSource::new(reader, source)
        };
        let cancel = AtomicBool::new(false);
        let mut raw = input();
        let unit = loop {
            if let Step::Ready(unit) = raw.poll(&mut WorkBudget::new(512), &cancel).unwrap() {
                break unit;
            }
        };
        assert!(matches!(
            raw.into_feed(4096, RawFeedClock::Unspecified),
            Err(Error::Invalid(
                "raw feed must attach before first access unit"
            ))
        ));

        let start = Time::new(3, 2).unwrap();
        let mut feed = input()
            .into_feed(
                4096,
                RawFeedClock::FixedSubmission {
                    start,
                    picture_duration: Time::new(1001, 24000).unwrap(),
                },
            )
            .unwrap();
        let State::Active(active) = &mut feed.state else {
            panic!("raw feed should be active")
        };
        active.pending = Some(unit);
        let held = pool.reserve(pool.limit() - pool.used()).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                feed.poll(&mut WorkBudget::new(1), &cancel),
                Err(Error::Backpressure { .. })
            ));
        }
        drop(held);
        let Step::Ready(packet) = feed.poll(&mut WorkBudget::new(1), &cancel).unwrap() else {
            panic!("configuration retry did not publish the waiting packet");
        };
        assert_eq!(packet.submission_time, Some(start));
        assert_eq!(packet.queue_timestamp_ns(), 1_500_000_000);
        assert_eq!(packet.configuration_revision, Some(1));
        assert!(packet.configuration_changed);
        assert!(
            packet
                .unit
                .segments()
                .flatten()
                .copied()
                .eq(bytes.iter().copied())
        );
    }
}
