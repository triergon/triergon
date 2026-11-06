//! Original bounded carriage candidate. The runtime-red src/session.rs remains
//! active until the parent records its failures against the frozen tests.
use super::document::Document;
use super::{CallerClock, Limits, Operation, Packet, Snapshot, Status, Stream};
use crate::text::{Format, Input};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Error, Result, Time, TimeBase, WorkBudget};

pub struct Session {
    pool: Budget,
    limits: Limits,
    pending: Option<Input>,
    operation: Option<Operation>,
    document: Option<Document>,
    output: Option<Snapshot>,
    ended: bool,
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

fn sample_codec(format: Format) -> Result<(i32, &'static str)> {
    // Pinned public enum values/descriptor names from the observed d32 ABI.
    // No decoder registry is consulted and no source codec-tag is rewritten.
    match format {
        Format::Tx3g => Ok((94213, "mov_text")),
        Format::Utf8Packet => Ok((94210, "text")),
        Format::MatroskaWebVtt => Ok((94226, "webvtt")),
        Format::OpaqueAss => Ok((94230, "ass")),
        _ => Err(Error::Unsupported(
            "sample carriage requires an explicit packet format",
        )),
    }
}

impl Session {
    pub fn new(pool: &Budget, limits: Limits) -> Self {
        Self {
            pool: pool.clone(),
            limits,
            pending: None,
            operation: None,
            document: None,
            output: None,
            ended: false,
        }
    }

    pub fn provide(&mut self, offered: &mut Option<Input>, operation: Operation) -> Result<()> {
        if self.pending.is_some() || self.output.is_some() || self.ended {
            return Err(Error::Invalid("carriage input unavailable"));
        }
        let input = offered
            .as_ref()
            .ok_or(Error::Invalid("empty carriage input"))?;
        bound(input.payload.len(), self.limits.payload_bytes)?;
        bound(
            input.codec_private.as_ref().map_or(0, |value| value.len()),
            self.limits.config_bytes,
        )?;
        bound(
            input.codec_id.as_ref().map_or(0, |value| value.len()),
            self.limits.tag_bytes,
        )?;
        bound(
            input.charset_label.as_ref().map_or(0, |value| value.len()),
            self.limits.label_bytes,
        )?;
        bound(
            input.addition.as_ref().map_or(0, |value| value.bytes.len()),
            self.limits.addition_bytes,
        )?;
        match &operation {
            Operation::Sample(clock) => {
                sample_codec(input.format)?;
                validate_clock(input, *clock)?;
                bound(1, self.limits.packets)?;
                bound(input.payload.len(), self.limits.total_packet_bytes)?;
            }
            Operation::Document { file_name, .. } => {
                if !matches!(input.format, Format::SubRip | Format::WebVtt) {
                    return Err(Error::Unsupported(
                        "document carriage requires a text document",
                    ));
                }
                bound(file_name.len(), self.limits.label_bytes)?;
            }
        }
        self.pending = offered.take();
        self.operation = Some(operation);
        Ok(())
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Status> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.output.is_some() {
            return Ok(Status::NeedOutput);
        }
        let Some(input) = self.pending.as_ref() else {
            return Ok(if self.ended {
                Status::End
            } else {
                Status::NeedInput
            });
        };
        let operation = self
            .operation
            .as_ref()
            .ok_or(Error::Invalid("missing carriage operation"))?;
        let parts = match operation {
            Operation::Sample(clock) => {
                if !work.try_charge(1) {
                    return Ok(Status::Yield);
                }
                let (codec_id, codec_name) = sample_codec(input.format)?;
                let clock_base = [clock.numerator, clock.denominator];
                let stream = Stream {
                    index: 0,
                    codec_type: 3,
                    codec_id,
                    codec_name,
                    codec_tag: 0,
                    time_base: clock_base,
                    start_raw: i64::MIN,
                    duration_raw: i64::MIN,
                    disposition: 0,
                    extradata: input.codec_private.clone(),
                    coded_side_data: BudgetVec::new(&self.pool),
                    metadata: BudgetVec::new(&self.pool),
                };
                let mut packets = BudgetVec::new(&self.pool);
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
                        packet_time_base: clock_base,
                        stream_time_base: clock_base,
                        side_data: BudgetVec::new(&self.pool),
                    })
                    .map_err(|error| error.error)?;
                super::document::Parts {
                    format: None,
                    stream: Some(stream),
                    packets,
                    progress: BudgetVec::new(&self.pool),
                    native_error: None,
                }
            }
            Operation::Document {
                discovery,
                file_name,
            } => {
                if self.document.is_none() {
                    self.document = Some(Document::new(
                        &self.pool,
                        self.limits,
                        *discovery,
                        file_name,
                    ));
                }
                let document = self
                    .document
                    .as_mut()
                    .ok_or(Error::Invalid("missing document state"))?;
                if !document.advance(input, work, cancel)? {
                    return Ok(Status::Yield);
                }
                self.document
                    .take()
                    .ok_or(Error::Invalid("missing completed document"))?
                    .finish()?
            }
        };
        let input = self
            .pending
            .take()
            .ok_or(Error::Invalid("lost carriage input"))?;
        let operation = self
            .operation
            .take()
            .ok_or(Error::Invalid("lost carriage operation"))?;
        self.output = Some(Snapshot {
            input,
            operation,
            format: parts.format,
            stream: parts.stream,
            packets: parts.packets,
            progress: parts.progress,
            native_error: parts.native_error,
        });
        Ok(Status::Ready)
    }

    pub fn take_output(&mut self) -> Option<Snapshot> {
        self.output.take()
    }
    pub fn finish(&mut self) {
        self.ended = true;
    }
    pub fn recover_pending(&mut self) -> Option<Input> {
        self.document = None;
        self.operation = None;
        self.pending.take()
    }
}

const _: () = assert!(std::mem::size_of::<Session>() <= 4096);
