use super::model::{ClockContext, PacketData, Shared};
use super::{AccessUnit, Packet, PacketClock, PayloadSource, ReaderLimits, SourceEvent, Track};
use crate::clock::{SourceTimePolicy, source_timestamp};
use crate::compat::pes::{PrivateAudioCarriage, PrivateAudioDescriptors};
use crate::psi::DescriptorNamespace;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputRequest, InputWindow, Result, SourceCapabilities,
    SourceId, Step, WorkBudget,
};
use triergon_framing::{
    AacAdtsStreamLimits, AacAdtsStreamReader, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection,
    Codec, RawAnnexBReader,
};

// Keep concrete readers inline; their containing BudgetVec charges the whole state.
#[allow(clippy::large_enum_variant)]
enum Framer {
    Video(RawAnnexBReader),
    Adts(AacAdtsStreamReader),
    Ac3(Ac3FrameReader),
}

impl Framer {
    fn new(pool: &Budget, track: &Track, limits: ReaderLimits) -> Result<Self> {
        // Private concatenated elementary view, never an external source request.
        let source = SourceId(0);
        let caps = SourceCapabilities {
            length: None,
            random_access: false,
        };
        match track.stream().stream_type {
            kind @ (0x1b | 0x24) => Ok(Self::Video(RawAnnexBReader::new(
                pool,
                if kind == 0x1b {
                    Codec::Avc
                } else {
                    Codec::Hevc
                },
                source,
                caps,
                limits.video,
            )?)),
            0x0f => Ok(Self::Adts(AacAdtsStreamReader::new(
                pool,
                source,
                caps,
                AacAdtsStreamLimits {
                    maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                    maximum_frames: limits.maximum_access_units,
                    maximum_input_window: limits.elementary_input_window,
                },
            )?)),
            0x06 if matches!(
                track.stream().descriptor_namespace(),
                DescriptorNamespace::Dvb(_)
            ) =>
            {
                let carriage = PrivateAudioCarriage::from_observed_pmt(
                    6,
                    PrivateAudioDescriptors {
                        program: track.program_map().program_descriptors.as_ref(),
                        elementary: track.stream().descriptor_bytes.as_ref(),
                    },
                    limits.transport,
                )?;
                if carriage != PrivateAudioCarriage::DvbAc3Tag {
                    return Err(Error::Unsupported(
                        "MPEG-TS E-AC-3 access-unit grouping unavailable",
                    ));
                }
                Ok(Self::Ac3(Ac3FrameReader::new(
                    pool,
                    source,
                    caps,
                    Ac3StreamSelection::Ac3Only,
                    Ac3FrameLimits {
                        maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                        maximum_frames: limits.maximum_access_units,
                        maximum_input_window: limits.elementary_input_window,
                    },
                )?))
            }
            _ => Err(Error::Unsupported(
                "MPEG-TS carriage has no admitted access-unit framer",
            )),
        }
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<AccessUnit>> {
        fn map<T>(step: Step<T>, wrap: impl FnOnce(T) -> AccessUnit) -> Step<AccessUnit> {
            match step {
                Step::Ready(unit) => Step::Ready(wrap(unit)),
                Step::Yield => Step::Yield,
                Step::NeedInput(request) => Step::NeedInput(request),
                Step::End(location) => Step::End(location),
                Step::Backpressure(never) => match never {},
            }
        }
        Ok(match self {
            Self::Video(reader) => map(reader.step(work, cancel)?, AccessUnit::Video),
            Self::Adts(reader) => map(reader.step(work, cancel)?, AccessUnit::Adts),
            Self::Ac3(reader) => map(reader.step(work, cancel)?, AccessUnit::Ac3),
        })
    }
    fn provide(&mut self, window: InputWindow) -> Result<()> {
        match self {
            Self::Video(reader) => reader.provide_owned(window),
            Self::Adts(reader) => reader.provide_owned(window),
            Self::Ac3(reader) => reader.provide_owned(window),
        }
    }
    fn finish(&mut self, request: InputRequest) -> Result<()> {
        match self {
            Self::Video(reader) => reader.finish(request.source, request.token),
            Self::Adts(reader) => reader.finish(request.source, request.token),
            Self::Ac3(reader) => reader.finish(request.source, request.token),
        }
    }
}

struct Piece {
    range: std::ops::Range<u64>,
    source: SourceEvent,
    clock: ClockContext,
    serial: u64,
}

struct Input {
    source: SourceEvent,
    span: usize,
    offset: usize,
}

struct Build {
    unit: Option<AccessUnit>,
    buffer: Option<BudgetVec<u8>>,
    bytes: Option<Bytes>,
    nal: usize,
    part: u8,
    copied: usize,
    origins: BudgetVec<PayloadSource>,
    origin: usize,
    clock: Option<PacketClock>,
    last_anchor: u64,
    retire: usize,
}

impl Build {
    fn new(unit: AccessUnit, pool: &Budget, last_anchor: u64) -> Self {
        let bytes = match &unit {
            AccessUnit::Adts(frame) => Some(frame.source_bytes().clone()),
            AccessUnit::Ac3(frame) => Some(frame.bytes().clone()),
            AccessUnit::Video(_) => None,
        };
        Self {
            unit: Some(unit),
            buffer: Some(BudgetVec::new(pool)),
            bytes,
            nal: 0,
            part: 0,
            copied: 0,
            origins: BudgetVec::new(pool),
            origin: 0,
            clock: None,
            last_anchor,
            retire: 0,
        }
    }
    fn unit(&self) -> &AccessUnit {
        self.unit.as_ref().expect("pending access unit")
    }
    fn copy(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        let AccessUnit::Video(unit) = self.unit.as_ref().expect("pending video") else {
            unreachable!()
        };
        let size =
            usize::try_from(unit.range.end - unit.range.start).map_err(|_| Error::Overflow)?;
        let buffer = self.buffer.as_mut().expect("pending coalescing storage");
        buffer.try_reserve_exact(size.saturating_sub(buffer.len()))?;
        const ZEROS: [u8; 256] = [0; 256];
        while let Some(nal) = unit.nals().get(self.nal) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let facts = nal.facts();
            let triergon_framing::NalFraming::AnnexB {
                leading_zero_bytes,
                start_code_bytes,
                ..
            } = facts.framing
            else {
                unreachable!()
            };
            let (bytes, total, zeros): (&[u8], usize, bool) = match self.part {
                0 => (&ZEROS, leading_zero_bytes, true),
                1 if start_code_bytes == 4 => (&[0, 0, 0, 1], 4, false),
                1 => (&[0, 0, 1], 3, false),
                2 => (nal.bytes().as_ref(), nal.bytes().len(), false),
                3 => (&ZEROS, facts.trailing_zero_bytes, true),
                _ => unreachable!(),
            };
            if self.copied == total {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.copied = 0;
                self.part += 1;
                if self.part == 4 {
                    self.part = 0;
                    self.nal += 1;
                }
                continue;
            }
            let count = (total - self.copied).min(if zeros { ZEROS.len() } else { total });
            let count = usize::try_from(work.take(count as u64)).map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(false);
            }
            let start = if zeros { 0 } else { self.copied };
            buffer.try_extend_from_slice(&bytes[start..start + count])?;
            self.copied += count;
        }
        if buffer.len() != size {
            return Err(Error::Invalid("MPEG-TS AU source extent"));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match self.buffer.take().expect("coalescing storage").freeze() {
            Ok(bytes) => self.bytes = Some(bytes),
            Err(error) => {
                self.buffer = Some(error.buffer);
                return Err(error.error);
            }
        }
        Ok(true)
    }
}

pub(super) enum LaneStep {
    Packet(Packet),
    NeedPayload,
    Yield,
    End,
}

pub(super) struct Lane {
    pub track: Track,
    pub unsupported: Option<Error>,
    framer: Option<Framer>,
    pool: Budget,
    limits: ReaderLimits,
    input: Option<Input>,
    pieces: BudgetVec<Piece>,
    build: Option<Build>,
    supplied: u64,
    serial: u64,
    last_anchor: u64,
    retire: usize,
    retire_credit: usize,
    eof: bool,
    completed: usize,
    published: u64,
    pub run: Option<(u64, u64, u64)>,
}

impl Lane {
    pub(super) fn new(pool: &Budget, track: Track, limits: ReaderLimits) -> Result<Self> {
        let (framer, unsupported) = match Framer::new(pool, &track, limits) {
            Ok(framer) => (Some(framer), None),
            Err(error @ Error::Unsupported(_)) => (None, Some(error)),
            Err(error) => return Err(error),
        };
        Ok(Self {
            track,
            unsupported,
            framer,
            pool: pool.clone(),
            limits,
            input: None,
            pieces: BudgetVec::new(pool),
            build: None,
            supplied: 0,
            serial: 0,
            last_anchor: 0,
            retire: 0,
            retire_credit: 0,
            eof: false,
            completed: 0,
            published: 0,
            run: None,
        })
    }
    pub(super) fn unpublished(&self) -> Option<std::ops::Range<u64>> {
        (self.published < self.supplied).then_some(self.published..self.supplied)
    }
    pub(super) fn push(
        &mut self,
        source: &SourceEvent,
        clock: ClockContext,
        work: &mut WorkBudget,
    ) -> Result<bool> {
        if self.framer.is_none() {
            return Ok(true);
        }
        if self.input.is_some() {
            return Err(Error::Invalid("MPEG-TS PES mailbox occupied"));
        }
        if self.pieces.len() == self.limits.maximum_pes_per_access_unit {
            return Err(Error::Limit {
                requested: self.pieces.len().saturating_add(1),
                maximum: self.limits.maximum_pes_per_access_unit,
            });
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        self.pieces.try_reserve_exact(1)?;
        let pes = source.pes();
        let end = self
            .supplied
            .checked_add(u64::try_from(pes.payload_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let serial = self.serial.checked_add(1).ok_or(Error::Overflow)?;
        self.pieces
            .try_push(Piece {
                range: self.supplied..end,
                source: source.clone(),
                clock,
                serial,
            })
            .map_err(|e| e.error)?;
        self.serial = serial;
        self.run = Some((
            pes.epochs.configuration,
            pes.epochs.clock,
            pes.byte_run_epoch,
        ));
        self.input = Some(Input {
            source: source.clone(),
            span: 0,
            offset: 0,
        });
        Ok(true)
    }
    pub(super) fn step(
        &mut self,
        draining: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<LaneStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(LaneStep::Yield);
            }
            if self.retire != 0 {
                let cost = self.pieces.len() - self.retire_credit;
                self.retire_credit +=
                    usize::try_from(work.take(cost as u64)).map_err(|_| Error::Overflow)?;
                if self.retire_credit != self.pieces.len() {
                    return Ok(LaneStep::Yield);
                }
                self.pieces.as_mut_slice().rotate_left(self.retire);
                self.pieces.truncate(self.pieces.len() - self.retire);
                self.retire = 0;
                self.retire_credit = 0;
                continue;
            }
            if self.build.is_some() {
                return self.publish(work, cancel);
            }
            let Some(framer) = &mut self.framer else {
                return Ok(LaneStep::End);
            };
            match framer.step(work, cancel)? {
                Step::Ready(unit) => {
                    if self.completed == self.limits.maximum_access_units {
                        return Err(Error::Limit {
                            requested: self.completed.saturating_add(1),
                            maximum: self.limits.maximum_access_units,
                        });
                    }
                    self.build = Some(Build::new(unit, &self.pool, self.last_anchor));
                }
                Step::Yield => return Ok(LaneStep::Yield),
                Step::End(_) => return Ok(LaneStep::End),
                Step::NeedInput(request) => {
                    if request.offset != self.supplied {
                        return Err(Error::Invalid("MPEG-TS elementary input offset"));
                    }
                    if let Some(input) = &mut self.input {
                        let pes = input.source.pes();
                        let Some(span) = pes.spans().get(input.span) else {
                            self.input = None;
                            continue;
                        };
                        if !work.try_charge(1) {
                            return Ok(LaneStep::Yield);
                        }
                        let bytes = span.payload_bytes()?;
                        let count = (bytes.len() - input.offset).min(request.maximum);
                        if count == 0 {
                            return Err(Error::Invalid("empty MPEG-TS payload span"));
                        }
                        let end = self
                            .supplied
                            .checked_add(count as u64)
                            .ok_or(Error::Overflow)?;
                        framer.provide(InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            bytes.slice(input.offset..input.offset + count)?,
                        )?)?;
                        self.supplied = end;
                        input.offset += count;
                        if input.offset == bytes.len() {
                            input.span += 1;
                            input.offset = 0;
                        }
                    } else if draining {
                        if self.eof {
                            return Err(Error::Invalid("MPEG-TS framer requested input after EOF"));
                        }
                        if !work.try_charge(1) {
                            return Ok(LaneStep::Yield);
                        }
                        framer.finish(request)?;
                        self.eof = true;
                    } else {
                        return Ok(LaneStep::NeedPayload);
                    }
                }
            }
        }
    }
    fn publish(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<LaneStep> {
        let build = self.build.as_mut().expect("pending publication");
        if !build.copy(work, cancel)? {
            return Ok(LaneStep::Yield);
        }
        let range = build.unit().range();
        let anchor_offset = build.unit().anchor_offset();
        while let Some(piece) = self.pieces.as_slice().get(build.origin) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            let start = range.start.max(piece.range.start);
            let end = range.end.min(piece.range.end);
            if start < end {
                build.origins.try_reserve_exact(1)?;
                build
                    .origins
                    .try_push(PayloadSource {
                        elementary_range: start..end,
                        payload_range: usize::try_from(start - piece.range.start)
                            .map_err(|_| Error::Overflow)?
                            ..usize::try_from(end - piece.range.start)
                                .map_err(|_| Error::Overflow)?,
                        pes: piece.source.clone(),
                    })
                    .map_err(|e| e.error)?;
            }
            if piece.range.start <= anchor_offset && piece.serial > build.last_anchor {
                let pes = piece.source.pes();
                let policy = if piece.clock.quarantined {
                    SourceTimePolicy::Unanchored
                } else {
                    self.limits.time_policy
                };
                build.clock = Some(PacketClock {
                    key: piece.clock.key,
                    quarantined: piece.clock.quarantined,
                    pts: source_timestamp(pes.header.pts_raw33, piece.clock.key, policy)?,
                    dts: source_timestamp(pes.header.dts_raw33, piece.clock.key, policy)?,
                    pes: piece.source.clone(),
                });
                build.last_anchor = piece.serial;
            }
            build.origin += 1;
        }
        while let Some(piece) = self.pieces.as_slice().get(build.retire) {
            if piece.range.end > range.end || piece.serial > build.last_anchor {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            build.retire += 1;
        }
        if !work.try_charge(1) {
            return Ok(LaneStep::Yield);
        }
        let charge = Shared::<PacketData>::reserve(&self.pool)?;
        let duration = build.unit().duration()?;
        let duration90 = duration
            .map(|time| -> Result<Option<i64>> {
                let n = time
                    .numerator()
                    .checked_mul(90_000)
                    .ok_or(Error::Overflow)?;
                let d = i128::try_from(time.denominator()).map_err(|_| Error::Overflow)?;
                if n % d == 0 {
                    Ok(Some(i64::try_from(n / d).map_err(|_| Error::Overflow)?))
                } else {
                    Ok(None)
                }
            })
            .transpose()?
            .flatten();
        let pts = build
            .clock
            .as_ref()
            .and_then(|clock| clock.pts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let dts = build
            .clock
            .as_ref()
            .and_then(|clock| clock.dts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let epochs = build
            .origins
            .as_slice()
            .first()
            .ok_or(Error::Invalid("MPEG-TS AU has no source extent"))?
            .pes
            .pes()
            .epochs;
        let build = self.build.take().expect("prepared publication");
        self.retire = build.retire;
        self.last_anchor = build.last_anchor;
        self.completed += 1;
        self.published = range.end;
        Ok(LaneStep::Packet(Packet(Shared::new(
            PacketData {
                track: self.track.clone(),
                unit: build.unit.expect("prepared unit"),
                bytes: build.bytes.expect("prepared bytes"),
                sources: build.origins,
                clock: build.clock,
                epochs,
                duration,
                pts,
                dts,
                duration90,
            },
            charge,
        ))))
    }
}

#[cfg(test)]
mod tests;
