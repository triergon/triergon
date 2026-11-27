use super::document::{Document, Parts};
use super::{
    CallerClock, Discovery, DocumentInput, Limits, Operation, Packet, Snapshot, Source, Status,
    Stream,
};
use crate::text::{Format, Input};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Refused, Result, Time, TimeBase, WorkBudget};

/// One input at a time, published as a [`Snapshot`].
///
/// [`provide`](Self::provide) takes an input and its operation, `poll` reads
/// it until a snapshot is [`Status::Ready`], and [`take_output`](Self::take_output)
/// hands it over; no input is taken while a snapshot waits.
#[derive(Debug)]
pub struct Session {
    pool: Budget,
    limits: Limits,
    job: Option<Job>,
    output: Option<Snapshot>,
    ended: bool,
}

/// The input being read, with its operation.
#[derive(Debug)]
enum Job {
    Sample {
        input: Input,
        clock: CallerClock,
    },
    Document {
        input: DocumentInput,
        discovery: Discovery,
        file_name: Bytes,
        reading: Document,
    },
}

impl Job {
    /// The job for `source` and `operation`, or `source` back with the
    /// reason it is refused.
    #[expect(
        clippy::result_large_err,
        reason = "A refused input goes back to the caller inline, without an allocation."
    )]
    fn new(
        pool: &Budget,
        limits: Limits,
        source: Source,
        operation: Operation,
    ) -> std::result::Result<Self, Refused<Source>> {
        match (source, operation) {
            (Source::Sample(input), Operation::Sample(clock)) => {
                match check_sample(&input, clock, limits) {
                    Ok(()) => Ok(Self::Sample { input, clock }),
                    Err(error) => Err(Refused {
                        error,
                        value: Source::Sample(input),
                    }),
                }
            }
            (
                Source::Document(input),
                Operation::Document {
                    discovery,
                    file_name,
                },
            ) => {
                let reading = bound(input.payload.len(), limits.payload_bytes)
                    .and_then(|()| bound(file_name.len(), limits.label_bytes))
                    .and_then(|()| {
                        Document::new(pool, limits, discovery, file_name.as_ref(), &input.payload)
                    });
                match reading {
                    Ok(reading) => Ok(Self::Document {
                        input,
                        discovery,
                        file_name,
                        reading,
                    }),
                    Err(error) => Err(Refused {
                        error,
                        value: Source::Document(input),
                    }),
                }
            }
            (source @ Source::Sample(_), Operation::Document { .. }) => Err(Refused {
                error: Error::Unsupported("document carriage requires a text document"),
                value: source,
            }),
            (source @ Source::Document(_), Operation::Sample(_)) => Err(Refused {
                error: Error::Unsupported("sample carriage requires an explicit packet format"),
                value: source,
            }),
        }
    }

    /// The input and operation, as provided.
    fn into_parts(self) -> (Source, Operation) {
        match self {
            Self::Sample { input, clock } => (Source::Sample(input), Operation::Sample(clock)),
            Self::Document {
                input,
                discovery,
                file_name,
                ..
            } => (
                Source::Document(input),
                Operation::Document {
                    discovery,
                    file_name,
                },
            ),
        }
    }
}

fn bound(actual: usize, maximum: usize) -> Result<()> {
    if actual > maximum {
        Err(Error::Limit {
            requested: actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

/// Whether a sample fits the limits and its clock states its timing.
fn check_sample(input: &Input, clock: CallerClock, limits: Limits) -> Result<()> {
    let length = |bytes: Option<&Bytes>| bytes.map_or(0, Bytes::len);
    bound(input.payload.len(), limits.payload_bytes)?;
    bound(length(input.codec_private.as_ref()), limits.config_bytes)?;
    bound(length(input.codec_id.as_ref()), limits.tag_bytes)?;
    bound(length(input.charset_label.as_ref()), limits.label_bytes)?;
    bound(
        length(input.addition.as_ref().map(|addition| &addition.bytes)),
        limits.addition_bytes,
    )?;
    validate_clock(input, clock)?;
    bound(1, limits.packets)?;
    bound(input.payload.len(), limits.total_packet_bytes)
}

fn validate_clock(input: &Input, clock: CallerClock) -> Result<()> {
    let numerator =
        u64::try_from(clock.numerator).map_err(|_| Error::Invalid("caller timebase"))?;
    let denominator =
        u64::try_from(clock.denominator).map_err(|_| Error::Invalid("caller timebase"))?;
    let base = TimeBase::new(numerator, denominator)?;
    let exact = |value: Option<i64>| {
        value
            .map(|value| Time::from_ticks(i128::from(value), base))
            .transpose()
    };
    if exact(clock.pts)? != input.timing.start()
        || exact(clock.dts)? != input.timing.dts()
        || exact(clock.duration)? != input.timing.duration()
    {
        return Err(Error::Invalid("caller/source clock disagreement"));
    }
    if let Some(raw) = input.timing.raw_clock()
        && (raw.numerator != numerator
            || raw.denominator != denominator
            || raw.pts_ticks != clock.pts.map(i128::from)
            || raw.dts_ticks != clock.dts.map(i128::from)
            || raw.duration_ticks != clock.duration.map(i128::from))
    {
        return Err(Error::Invalid("caller changed the original raw clock"));
    }
    Ok(())
}

/// FFmpeg's codec identifier number and name for a sample's format, as its
/// API reports them.
fn sample_codec(format: Format) -> (i32, &'static str) {
    match format {
        Format::Tx3g => (94213, "mov_text"),
        Format::Utf8Packet => (94210, "text"),
        Format::MatroskaWebVtt => (94226, "webvtt"),
        Format::OpaqueAss => (94230, "ass"),
    }
}

impl Session {
    /// An idle session whose snapshots are charged to `pool`.
    pub fn new(pool: &Budget, limits: Limits) -> Self {
        Self {
            pool: pool.clone(),
            limits,
            job: None,
            output: None,
            ended: false,
        }
    }

    /// Takes the input in `offered` for `operation`. It is refused, and left
    /// in `offered`, while another input or a snapshot is pending, after
    /// [`finish`](Self::finish), when it exceeds the limits, when a sample's
    /// clock disagrees with its timing, or when the operation does not fit
    /// the input.
    pub fn provide(&mut self, offered: &mut Option<Source>, operation: Operation) -> Result<()> {
        if self.job.is_some() || self.output.is_some() || self.ended {
            return Err(Error::Invalid("carriage input unavailable"));
        }
        let source = offered
            .take()
            .ok_or(Error::Invalid("empty carriage input"))?;
        match Job::new(&self.pool, self.limits, source, operation) {
            Ok(job) => {
                self.job = Some(job);
                Ok(())
            }
            Err(refused) => {
                *offered = Some(refused.value);
                Err(refused.error)
            }
        }
    }

    /// Reads the pending input by at most `work` units: one for a sample,
    /// and for a document what its reader spends. A failure keeps the input
    /// for [`recover_pending`](Self::recover_pending); a document's reading
    /// cannot resume after one.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.output.is_some() {
            return Ok(Status::NeedOutput);
        }
        let parts = match &mut self.job {
            None => {
                return Ok(if self.ended {
                    Status::End
                } else {
                    Status::NeedInput
                });
            }
            Some(Job::Sample { input, clock }) => {
                if !work.try_charge(1) {
                    return Ok(Status::Yield);
                }
                sample(&self.pool, input, *clock)?
            }
            Some(Job::Document { input, reading, .. }) => {
                if !reading.advance(input.payload.as_ref(), work, cancel)? {
                    return Ok(Status::Yield);
                }
                reading.finish()?
            }
        };
        if let Some(job) = self.job.take() {
            let (input, operation) = job.into_parts();
            self.output = Some(Snapshot {
                input,
                operation,
                format: parts.format,
                stream: parts.stream,
                packets: parts.packets,
                progress: parts.progress,
                error_code: parts.error_code,
            });
        }
        Ok(Status::Ready)
    }

    /// The ready snapshot, if there is one.
    pub fn take_output(&mut self) -> Option<Snapshot> {
        self.output.take()
    }

    /// Ends input: once the pending input's snapshot is taken, `poll`
    /// returns [`Status::End`].
    pub fn finish(&mut self) {
        self.ended = true;
    }

    /// Takes back the pending input, dropping what was read of it.
    pub fn recover_pending(&mut self) -> Option<Source> {
        self.job.take().map(|job| job.into_parts().0)
    }
}

/// A sample as one packet with the caller's clock.
fn sample(pool: &Budget, input: &Input, clock: CallerClock) -> Result<Parts> {
    let (codec_id, codec_name) = sample_codec(input.format);
    let time_base = [clock.numerator, clock.denominator];
    let mut packets = BudgetVec::new(pool);
    packets.try_reserve_exact(1)?;
    packets
        .try_push(Packet {
            ordinal: 0,
            stream: 0,
            data: input.payload.clone(),
            position_raw: -1,
            flags: 0,
            buffer_is_null: false,
            pts_raw: clock.pts.unwrap_or(i64::MIN),
            dts_raw: clock.dts.unwrap_or(i64::MIN),
            duration_raw: clock.duration.unwrap_or(0),
            packet_time_base: time_base,
            stream_time_base: time_base,
            side_data: BudgetVec::new(pool),
        })
        .map_err(|error| error.error)?;
    Ok(Parts {
        format: None,
        stream: Some(Stream {
            index: 0,
            codec_type: 3,
            codec_id,
            codec_name,
            codec_tag: 0,
            time_base,
            start_raw: i64::MIN,
            duration_raw: i64::MIN,
            disposition: 0,
            extradata: input.codec_private.clone(),
            coded_side_data: BudgetVec::new(pool),
            metadata: BudgetVec::new(pool),
        }),
        packets,
        progress: BudgetVec::new(pool),
        error_code: None,
    })
}

const _: () = assert!(size_of::<Session>() <= 4096);
