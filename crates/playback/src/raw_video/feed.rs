//! Source-owned raw packets with an active decoder configuration and an
//! explicit sink clock. Annex B carries no container PTS/DTS.

use super::{RawVideoConfigTracker, RawVideoConfiguration, RawVideoSource, State as SourceState};
use crate::{Error, Result, input::Source};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error as CoreError, Rounding, SourceLocation, Step, Time, WorkBudget};
use triergon_framing::RawAccessUnit;

/// A caller-supplied cadence is an ordered submission clock for a sink that
/// wants it, such as a local sample player. It is separate from display order
/// and the timing information signalled in codec parameter sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RawFeedClock {
    #[default]
    Unspecified,
    FixedSubmission {
        start: Time,
        picture_duration: Time,
    },
}

struct SubmissionClock {
    next: Time,
    picture_duration: Time,
}

/// Packet bytes still alias the bounded raw NAL owners. The configuration
/// owner belongs to this packet and remains valid across later updates.
#[derive(Debug)]
pub struct RawVideoFeedPacket {
    pub unit: RawAccessUnit,
    pub configuration: Option<RawVideoConfiguration>,
    pub configuration_revision: Option<u64>,
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
enum State<S: Source> {
    Active(Active<S>),
    End(SourceLocation),
    Failed(CoreError),
}

pub struct RawVideoFeed<S: Source> {
    state: State<S>,
}

impl<S: Source> RawVideoSource<S> {
    /// Add the decoder configuration and optional submission clock to this
    /// forward raw source. Attach before polling for the first access unit.
    pub fn into_feed(
        self,
        maximum_parameter_set_bytes: usize,
        clock: RawFeedClock,
    ) -> Result<RawVideoFeed<S>> {
        let SourceState::Active { reader, .. } = &self.state else {
            return Err(Error::Closed);
        };
        if reader.published_bytes() != 0 {
            return Err(CoreError::Invalid("raw feed must attach before first access unit").into());
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
                    return Err(CoreError::Invalid("nonpositive raw submission duration").into());
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
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<RawVideoFeedPacket>> {
        let result = match &mut self.state {
            State::Active(active) => active.poll(work, cancel),
            State::End(at) => return Ok(Step::End(*at)),
            State::Failed(error) => return Err((*error).into()),
        };
        match &result {
            Ok(Step::End(at)) => self.state = State::End(*at),
            Err(Error::Core(error))
                if !matches!(
                    error,
                    CoreError::Backpressure { .. } | CoreError::Allocation
                ) =>
            {
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
            return Err(CoreError::Cancelled.into());
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
            .ok_or(CoreError::Invalid("raw feed unit missing"))?;
        let schedule = if unit.has_vcl {
            self.clock
                .as_ref()
                .map(|clock| {
                    let nanos = clock.next.to_nanoseconds(Rounding::NearestAway)?;
                    let next = clock.next.checked_add(clock.picture_duration)?;
                    Ok::<_, CoreError>((clock.next, nanos, next))
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
            self.revision = self.revision.checked_add(1).ok_or(CoreError::Overflow)?;
            self.configuration = Some(configuration);
            changed = true;
        }
        if let (Some(clock), Some((_, _, next))) = (&mut self.clock, schedule) {
            clock.next = next;
        }
        let unit = self
            .pending
            .take()
            .ok_or(CoreError::Invalid("raw feed unit missing"))?;
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
    use crate::ByteSource;
    use triergon_core::{Budget, Bytes, SourceCapabilities, SourceId};
    use triergon_framing::{Codec, RawAnnexBLimits, RawAnnexBReader};

    #[test]
    fn configuration_pressure_retains_pending_packet_and_submission_clock() {
        let pool = Budget::new(1 << 20).unwrap();
        let bytes = include_bytes!("../../tests/fixtures/raw-video/avc-short.h264");
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
            Err(Error::Core(CoreError::Invalid(
                "raw feed must attach before first access unit"
            )))
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
                Err(Error::Core(CoreError::Backpressure { .. }))
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
