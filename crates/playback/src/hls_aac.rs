//! Access units for explicitly selected TS AAC ADTS carriage (PMT stream type
//! 0x0f). PES boundaries are transport boundaries, even inside an ADTS header.
use crate::{
    TrackKey, TsNativeCarriage, TsNativeConfiguration,
    hls_avc::{clock_describes, lane_track, pes_time},
    ts_payload::PesInput,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, BytesView, Error, Result, Time, WorkBudget};
use triergon_framing::{AacAdtsHeader, inspect_aac_adts_header};
use triergon_mpegts::{
    AssemblyStep,
    compat::pes::{
        CompactDemuxPacket, DemuxPacket, DemuxPayloadKind, DemuxTimeView, PayloadCursor,
        RegisteredCarriage,
    },
};

const PREFIX_BYTES: usize = 7;
const MAXIMUM_ADTS_BYTES: usize = 0x1fff;
const PES_CORRUPT: u32 = 2;

#[derive(Debug)]
pub struct HlsAacFrame {
    /// Complete ADTS frame, including the original header and any CRC.
    pub bytes: Bytes,
    pub header: AacAdtsHeader,
    pub pts: Option<Time>,
    pub dts: Option<Time>,
    pub duration: Time,
}

#[derive(Clone, Copy, Default)]
struct Clock {
    pts: Option<Time>,
    dts: Option<Time>,
}

struct PendingPes {
    spans: PayloadCursor,
    // A joined tail replaces the current logical span only after its complete
    // allocation, copy and freeze succeed. Ordinary spans borrow the cursor.
    current: Option<Bytes>,
    offset: usize,
    remaining: usize,
    coalescing_attempted: bool,
}

impl PendingPes {
    fn current(&self) -> Option<BytesView<'_>> {
        self.current
            .as_ref()
            .map(|bytes| bytes.view(0..bytes.len()).expect("complete joined tail"))
            .or_else(|| self.spans.current().map(|span| span.bytes))
    }

    fn try_coalesce(
        &mut self,
        pool: &Budget,
        maximum: usize,
        frame_length: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        if self.coalescing_attempted || self.offset != PREFIX_BYTES || self.remaining == 0 {
            return Ok(false);
        }
        let Some(current_len) = self.current().map(BytesView::len) else {
            return Ok(false);
        };
        if frame_length - PREFIX_BYTES <= current_len - self.offset {
            return Ok(false);
        }
        let Some(total) = current_len
            .checked_add(self.remaining)
            .filter(|&n| n <= maximum)
        else {
            return Ok(false);
        };
        // Bound the metadata scan, full copy, allocation/freeze and retirement.
        let Some(required) = u64::try_from(self.spans.remaining())
            .ok()
            .and_then(|count| count.checked_mul(2))
            .and_then(|scan_and_drop| scan_and_drop.checked_add(total as u64 + 2))
        else {
            return Ok(false);
        };
        if work.remaining() < required {
            return Ok(false);
        }
        self.coalescing_attempted = true;
        let mut remaining = self.remaining;
        for span in self.spans.remaining_spans() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            work.take(1);
            if !span.bytes.belongs_to(pool) {
                return Ok(false);
            }
            let Some(next) = remaining.checked_sub(span.as_ref().len()) else {
                return Ok(false);
            };
            remaining = next;
        }
        if remaining != 0 {
            return Ok(false);
        }

        // Keep every original span until allocation, copying and freezing all
        // succeed. An optional failure resumes the ordered span path.
        work.take(1);
        let mut joined = BudgetVec::new(pool);
        if joined.try_reserve_exact(total).is_err() || joined.capacity() > maximum {
            return Ok(false);
        }
        let current = self.current().expect("admitted coalescing span");
        work.take(current.len() as u64);
        if joined.try_extend_from_slice(current.as_ref()).is_err() {
            return Ok(false);
        }
        for span in self.spans.remaining_spans() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            work.take(span.as_ref().len() as u64);
            if joined.try_extend_from_slice(span.as_ref()).is_err() {
                return Ok(false);
            }
        }
        work.take(1);
        let Ok(joined) = joined.freeze() else {
            return Ok(false);
        };
        self.spans.drain_remaining(|| {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            work.take(1);
            Ok(())
        })?;
        self.spans.retire_current();
        self.current = Some(joined);
        self.remaining = 0;
        Ok(true)
    }
}

/// One logical AAC byte stream. Create a new reader at a configuration change
/// or a discontinuity. Retention is one PES span list and one charged frame
/// buffer. Consumed spans are released as the cursor advances, and delivered
/// frames own their charge independently of the reader.
pub struct HlsAac {
    pool: Budget,
    config: TsNativeConfiguration,
    maximum_frame_bytes: usize,
    pending: Option<PendingPes>,
    clock_epoch: Option<u64>,
    next_clock: Clock,
    frame_clock: Clock,
    prefix: [u8; PREFIX_BYTES],
    prefix_used: usize,
    header: Option<AacAdtsHeader>,
    duration: Option<(u32, Time)>,
    frame: BudgetVec<u8>,
    eof: bool,
    failure: Option<Error>,
}

impl HlsAac {
    pub fn new(
        pool: &Budget,
        config: TsNativeConfiguration,
        maximum_frame_bytes: usize,
    ) -> Result<Self> {
        if config.carriage != TsNativeCarriage::Registered(RegisteredCarriage::AacAdts) {
            return Err(Error::Unsupported("HLS AAC needs selected ADTS carriage"));
        }
        if !matches!(config.track, TrackKey::Ts { program, pid } if program != 0 && pid <= 0x1ffe)
            || !(PREFIX_BYTES + 1..=MAXIMUM_ADTS_BYTES).contains(&maximum_frame_bytes)
        {
            return Err(Error::Invalid("HLS AAC track or frame limit"));
        }
        Ok(Self {
            pool: pool.clone(),
            config,
            maximum_frame_bytes,
            pending: None,
            clock_epoch: None,
            next_clock: Clock::default(),
            frame_clock: Clock::default(),
            prefix: [0; PREFIX_BYTES],
            prefix_used: 0,
            header: None,
            duration: None,
            frame: BudgetVec::new(pool),
            eof: false,
            failure: None,
        })
    }

    /// Moves a PES only on acceptance. Drain `poll` to `NeedPayload` before
    /// offering another chunk. False leaves the mailbox unchanged.
    pub fn push(
        &mut self,
        mailbox: &mut Option<DemuxPacket>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        self.push_packet(mailbox, clock)
    }

    /// Accept the same PES facts through borrowed logical carrier ranges.
    /// The mailbox is moved only after the ordinary admission checks succeed.
    pub fn push_compact(
        &mut self,
        mailbox: &mut Option<CompactDemuxPacket>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        self.push_packet(mailbox, clock)
    }

    pub(crate) fn push_packet<P: PesInput>(
        &mut self,
        mailbox: &mut Option<P>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.eof {
            return Err(Error::Invalid("HLS AAC input after EOF"));
        }
        if self.pending.is_some() {
            return Ok(false);
        }
        let Some(packet) = mailbox.as_ref() else {
            return Ok(false);
        };
        let packet = packet.facts();
        if !lane_track(self.config.track, self.config.configuration_epoch, &packet)
            || packet.stream_id != 0xc0
            || packet.source_header.stream_id != packet.stream_id
        {
            return Err(Error::Invalid("HLS AAC PES track or configuration"));
        }
        if packet.flags & PES_CORRUPT != 0
            || packet.payload_kind == DemuxPayloadKind::DeclaredIncomplete
            || packet.payload_len == 0
            || packet.empty
        {
            return Err(Error::Invalid("HLS AAC corrupt or empty PES"));
        }
        if !packet.payload_budget.same_pool(&self.pool)
            || !packet.header_budget.same_pool(&self.pool)
        {
            return Err(Error::ForeignPool);
        }
        let clock_epoch = packet.source_epochs.clock;
        let changed_epoch = self.clock_epoch.is_some_and(|epoch| epoch != clock_epoch);
        if changed_epoch && self.prefix_used != 0 {
            return Err(Error::Invalid("HLS AAC frame crosses clock discontinuity"));
        }
        let anchor = packet.time_header();
        let raw_pts = anchor.and_then(|header| header.pts_raw33);
        let raw_dts = anchor.and_then(|header| header.dts_raw33);
        if !clock_describes(&clock, &packet, raw_pts, raw_dts) {
            return Err(Error::Invalid("HLS AAC clock does not describe this PES"));
        }
        if anchor.is_some() {
            // A newer applicable PES supersedes an unused anchor. An
            // applicable header with no timestamp carries explicit absence.
            self.next_clock = Clock {
                pts: pes_time(raw_pts, clock.pts90, TIMESTAMP)?,
                dts: pes_time(raw_dts, clock.dts90, TIMESTAMP)?,
            };
        } else if changed_epoch {
            self.next_clock = Clock::default();
        }
        self.clock_epoch = Some(clock_epoch);
        let remaining = packet.payload_len;
        self.pending = mailbox.take().map(|packet| PendingPes {
            spans: packet.into_payload().into_cursor(),
            current: None,
            offset: 0,
            remaining,
            coalescing_attempted: false,
        });
        Ok(true)
    }

    /// Signal logical EOF, after the last PES was accepted. Poll to drain it.
    pub fn finish(&mut self) {
        self.eof = true;
    }

    /// At most one output. Backpressure/allocation errors keep the staged
    /// prefix, anchor, body and PES cursor for retry. Cancellation and invalid
    /// stream data retire the reader and release its internal owners.
    pub fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsAacFrame>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Backpressure { .. } | Error::Allocation)
        {
            self.failure = Some(error);
            self.pending = None;
            self.frame = BudgetVec::new(&self.pool);
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsAacFrame>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(header) = self.header {
                if self.frame.len() == usize::from(header.frame_length()) {
                    if !work.try_charge(1) {
                        return Ok(AssemblyStep::Yield);
                    }
                    return self.publish(header, None);
                }
            } else if self.prefix_used == PREFIX_BYTES {
                if !work.try_charge(1) {
                    return Ok(AssemblyStep::Yield);
                }
                self.header = Some(self.inspect_prefix()?);
            }

            let Some(input) = &mut self.pending else {
                return match (self.eof, self.prefix_used) {
                    (false, _) => Ok(AssemblyStep::NeedPayload),
                    (true, 0) => Ok(AssemblyStep::Finished),
                    (true, _) => Err(Error::Truncated),
                };
            };
            if self.frame.is_empty()
                && let Some(header) = self.header
                && input.try_coalesce(
                    &self.pool,
                    self.maximum_frame_bytes,
                    usize::from(header.frame_length()),
                    work,
                    cancel,
                )?
            {
                continue;
            }
            if input.current().is_none() {
                let Some(span) = input.spans.next_span() else {
                    if input.remaining != 0 {
                        return Err(Error::Invalid("HLS AAC PES payload extent"));
                    }
                    self.pending = None;
                    continue;
                };
                let bytes = span.bytes;
                if !bytes.belongs_to(&self.pool) {
                    return Err(Error::ForeignPool);
                }
                // Validate a span's full extent before reading any of it.
                input.remaining = input
                    .remaining
                    .checked_sub(bytes.len())
                    .ok_or(Error::Invalid("HLS AAC PES payload extent"))?;
            }
            let span = input
                .current()
                .ok_or(Error::Invalid("HLS AAC missing PES span"))?;
            let bytes = &span.as_ref()[input.offset..];
            if bytes.is_empty() {
                if !work.try_charge(1) {
                    return Ok(AssemblyStep::Yield);
                }
                input.current = None;
                input.spans.retire_current();
                input.offset = 0;
                continue;
            }

            let needed = if let Some(header) = self.header {
                let length = usize::from(header.frame_length());
                if self.frame.is_empty() {
                    let body = length - PREFIX_BYTES;
                    // A split prefix consumes at most six bytes of its last
                    // span. Otherwise retain bounded backing storage and charge the
                    // same staging, body and publication work as a copy.
                    if input.offset >= PREFIX_BYTES
                        && span.retained_capacity() <= self.maximum_frame_bytes
                        && body <= bytes.len()
                        && work.try_charge(body as u64 + 2)
                    {
                        let bytes = span
                            .slice(input.offset - PREFIX_BYTES..input.offset + body)?
                            .into_bytes()?;
                        input.offset += body;
                        return self.publish(header, Some(bytes));
                    }
                    if !work.try_charge(1) {
                        return Ok(AssemblyStep::Yield);
                    }
                    self.frame.try_reserve_exact(length)?;
                    self.frame.try_extend_from_slice(&self.prefix)?;
                }
                length - self.frame.len()
            } else {
                PREFIX_BYTES - self.prefix_used
            };
            let count = work.take(bytes.len().min(needed) as u64) as usize;
            if count == 0 {
                return Ok(AssemblyStep::Yield);
            }
            if self.header.is_some() {
                self.frame.try_extend_from_slice(&bytes[..count])?;
            } else {
                if self.prefix_used == 0 {
                    // An applicable PES may begin during a prior frame. Its
                    // anchor waits until a new frame actually starts.
                    self.frame_clock = std::mem::take(&mut self.next_clock);
                }
                let end = self.prefix_used + count;
                self.prefix[self.prefix_used..end].copy_from_slice(&bytes[..count]);
                self.prefix_used = end;
            }
            input.offset += count;
        }
    }

    fn inspect_prefix(&self) -> Result<AacAdtsHeader> {
        let header = inspect_aac_adts_header(&self.prefix)?;
        if header.syncword() != 0xfff || header.layer() != 0 {
            return Err(Error::Invalid("ADTS sync/layer"));
        }
        if header.coded_frequency_hz().is_none() {
            return Err(Error::Invalid("ADTS sampling index"));
        }
        if header.profile_code() != 1 {
            return Err(Error::Unsupported("ADTS non-LC profile"));
        }
        let header_bytes = header
            .single_block_header_bytes()
            .ok_or(Error::Unsupported("ADTS multiple raw-data-block layout"))?;
        let length = usize::from(header.frame_length());
        if length <= header_bytes {
            return Err(Error::Invalid("ADTS frame has no payload"));
        }
        if length > self.maximum_frame_bytes {
            return Err(Error::Limit {
                requested: length,
                maximum: self.maximum_frame_bytes,
            });
        }
        Ok(header)
    }

    fn publish(
        &mut self,
        header: AacAdtsHeader,
        shared: Option<Bytes>,
    ) -> Result<AssemblyStep<HlsAacFrame>> {
        let rate = header
            .coded_frequency_hz()
            .ok_or(Error::Invalid("ADTS sampling index"))?;
        let duration = match self.duration {
            Some((cached_rate, duration)) if cached_rate == rate => duration,
            _ => {
                let duration = Time::new(1024, u128::from(rate))?;
                self.duration = Some((rate, duration));
                duration
            }
        };
        let bytes = if let Some(bytes) = shared {
            bytes
        } else {
            let frame = std::mem::replace(&mut self.frame, BudgetVec::new(&self.pool));
            match frame.freeze() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.frame = error.buffer;
                    return Err(error.error);
                }
            }
        };
        self.header = None;
        self.prefix_used = 0;
        Ok(AssemblyStep::Output(HlsAacFrame {
            bytes,
            header,
            pts: self.frame_clock.pts,
            dts: self.frame_clock.dts,
            duration,
        }))
    }
}

const TIMESTAMP: &str = "HLS AAC PES timestamp representative";

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::SourceId;
    use triergon_mpegts::{
        PacketFormat, ParseLimits, SourceEpochs,
        compat::pes::{DemuxClock, DemuxObserver, DemuxProfile, DemuxScope, SourceContext},
        parse_transport,
    };

    const TRACK: TrackKey = TrackKey::Ts {
        program: 1,
        pid: 257,
    };
    const CONFIG: TsNativeConfiguration = TsNativeConfiguration {
        track: TRACK,
        configuration_epoch: 2,
        carriage: TsNativeCarriage::Registered(RegisteredCarriage::AacAdts),
    };
    const EPOCHS: SourceEpochs = SourceEpochs {
        clock: 0,
        configuration: 2,
    };
    const SOURCE: SourceId = SourceId(5);

    struct PesFixture {
        pool: Budget,
        pes: DemuxObserver,
        clock: DemuxClock,
        carrier: u8,
    }

    impl PesFixture {
        fn new() -> Self {
            let pool = Budget::new(16 * 1024).unwrap();
            let pes = DemuxObserver::new_registered(
                DemuxProfile::PesPackets,
                DemuxScope {
                    program: 1,
                    pid: 257,
                    stream_type: 0x0f,
                    stream_id: 0xc0,
                    entity: SOURCE,
                    epochs: EPOCHS,
                },
                RegisteredCarriage::AacAdts,
                pool.clone(),
                ParseLimits::default(),
            )
            .unwrap();
            Self {
                pool,
                pes,
                clock: DemuxClock::new(1).unwrap(),
                carrier: 0,
            }
        }

        fn chunk(
            &mut self,
            bytes: &[u8],
            pts: u32,
            dts: Option<u32>,
        ) -> (Option<DemuxPacket>, DemuxTimeView) {
            // Authored finite PES, carried in one TS packet with adaptation
            // padding. The real native observer supplies the charged spans.
            let optional = if dts.is_some() { 10 } else { 5 };
            let pes_length = (3 + optional + bytes.len()) as u16;
            let pes_length = pes_length.to_be_bytes();
            let mut pes = vec![
                0,
                0,
                1,
                0xc0,
                pes_length[0],
                pes_length[1],
                0x80,
                if dts.is_some() { 0xc0 } else { 0x80 },
                optional as u8,
            ];
            pes.extend_from_slice(&timestamp(if dts.is_some() { 3 } else { 2 }, pts));
            if let Some(dts) = dts {
                pes.extend_from_slice(&timestamp(1, dts));
            }
            pes.extend_from_slice(bytes);
            let mut raw = [0xff; 188];
            let adaptation = 183 - pes.len();
            raw[..6].copy_from_slice(&[0x47, 0x41, 1, 0x30 | self.carrier, adaptation as u8, 0]);
            raw[5 + adaptation..].copy_from_slice(&pes);
            let mut carrier = parse_transport(
                Bytes::copy_from_slice(&self.pool, &raw).unwrap(),
                PacketFormat::Ts188,
                u64::from(self.carrier) * 188,
            )
            .unwrap();
            carrier.source_id = Some(SOURCE);
            self.pes
                .push(
                    carrier,
                    SourceContext {
                        source_event_ordinal: u64::from(self.carrier),
                        epochs: EPOCHS,
                        byte_run_epoch: None,
                    },
                )
                .unwrap();
            self.carrier += 1;
            let cancel = AtomicBool::new(false);
            let mut work = WorkBudget::new(4096);
            let AssemblyStep::Output(packet) = self.pes.poll(&mut work, &cancel).unwrap() else {
                panic!("fixture PES must emit")
            };
            let clock = self
                .clock
                .project_header(257, EPOCHS, packet.time_header(), &mut work, &cancel)
                .unwrap()
                .unwrap();
            (Some(packet), clock)
        }

        fn reader(&self) -> HlsAac {
            HlsAac::new(&self.pool, CONFIG, MAXIMUM_ADTS_BYTES).unwrap()
        }
    }

    fn timestamp(tag: u8, ticks: u32) -> [u8; 5] {
        [
            (tag << 4) | (((ticks >> 30) as u8) << 1) | 1,
            (ticks >> 22) as u8,
            (((ticks >> 15) as u8) << 1) | 1,
            (ticks >> 7) as u8,
            ((ticks as u8) << 1) | 1,
        ]
    }

    // Indexed AAC LC frames with one raw data block. The framing operation
    // retains the body verbatim and does not interpret compressed AAC.
    const F48: [u8; 10] = [0xff, 0xf1, 0x4c, 0x40, 1, 0x5f, 0xfc, 0x31, 0x32, 0x33];
    const F44_CRC: [u8; 11] = [
        0xff, 0xf0, 0x50, 0x40, 1, 0x7f, 0xfc, 0x12, 0x34, 0x41, 0x42,
    ];

    fn drain(reader: &mut HlsAac, out: &mut Vec<HlsAacFrame>) -> AssemblyStep<()> {
        let cancel = AtomicBool::new(false);
        for _ in 0..128 {
            match reader.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                AssemblyStep::Output(frame) => out.push(frame),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return AssemblyStep::NeedPayload,
                AssemblyStep::Finished => return AssemblyStep::Finished,
            }
        }
        panic!("AAC reader stalled at work quantum 1");
    }

    fn fragmented_packet(
        fixture: &mut PesFixture,
        parts: &[&[u8]],
    ) -> (Option<DemuxPacket>, DemuxTimeView) {
        let (mut packet, clock) = fixture.chunk(parts[0], 90_000, Some(89_000));
        for part in &parts[1..] {
            let (tail, _) = fixture.chunk(part, 180_000, None);
            let tail = tail.unwrap();
            let packet = packet.as_mut().unwrap();
            packet.payload_len += tail.payload_len;
            for span in tail.spans {
                packet.spans.try_push(span).unwrap();
            }
        }
        (packet, clock)
    }

    fn long_frame() -> Vec<u8> {
        let mut frame = vec![0x31; 240];
        frame[..PREFIX_BYTES].copy_from_slice(&[0xff, 0xf1, 0x4c, 0x40, 30, 0x1f, 0xfc]);
        frame
    }

    fn collect(reader: &mut HlsAac, quantum: u64) -> (Vec<HlsAacFrame>, u64) {
        let mut frames = Vec::new();
        let mut spent = 0;
        for _ in 0..4096 {
            let mut work = WorkBudget::new(quantum);
            let step = reader.poll(&mut work, &AtomicBool::new(false)).unwrap();
            spent += work.spent();
            match step {
                AssemblyStep::Output(frame) => frames.push(frame),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return (frames, spent),
                AssemblyStep::Finished => {
                    panic!("expected frames followed by a request for another PES");
                }
            }
        }
        panic!("fragmented AAC reader stalled");
    }

    #[test]
    fn fragmented_frames_share_one_bounded_pes_copy_with_exact_work_and_clocks() {
        let mut fixture = PesFixture::new();
        let frame = long_frame();
        let wire = frame.repeat(3);
        let parts: Vec<_> = wire.chunks(120).collect();
        let (mut packet, clock) = fragmented_packet(&mut fixture, &parts);
        let mut reader = fixture.reader();
        assert!(reader.push(&mut packet, clock).unwrap());
        let (frames, spent) = collect(&mut reader, 4096);
        assert_eq!(frames.len(), 3);
        // Copy the whole PES, inspect and retire its five remaining spans,
        // allocate/freeze once, retain ordinary frame work and one boundary.
        assert_eq!(spent, wire.len() as u64 + 2 * 5 + 2 + 3 * 243 + 1);
        for (index, output) in frames.iter().enumerate() {
            assert_eq!(output.bytes.as_ref(), frame);
            assert!(output.bytes.shares_storage_with(&frames[0].bytes));
            assert_eq!(output.bytes.retained_capacity(), wire.len());
            assert!(output.bytes.belongs_to(&fixture.pool));
            assert_eq!(output.duration, Time::new(1024, 48_000).unwrap());
            assert_eq!(output.pts, (index == 0).then(|| Time::new(1, 1).unwrap()));
            assert_eq!(
                output.dts,
                (index == 0).then(|| Time::new(89_000, 90_000).unwrap())
            );
        }
        let pool = fixture.pool.clone();
        drop(reader);
        drop(fixture);
        assert_eq!(
            pool.used(),
            Budget::control_storage_bytes() + frames[0].bytes.retained_storage_bytes()
        );
        drop(frames);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn coalescing_waits_for_complete_work_and_retains_publication_under_pressure() {
        let mut fixture = PesFixture::new();
        let frame = long_frame();
        let wire = frame.repeat(3);
        let parts: Vec<_> = wire.chunks(120).collect();
        let (mut packet, clock) = fragmented_packet(&mut fixture, &parts);
        let mut reader = fixture.reader();
        assert!(reader.push(&mut packet, clock).unwrap());
        let cancel = AtomicBool::new(false);
        let required = wire.len() as u64 + 2 * 5 + 2;
        let mut work = WorkBudget::new(PREFIX_BYTES as u64 + 1 + required);
        assert!(matches!(
            reader.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::Yield
        ));
        assert_eq!(work.remaining(), 0);
        let pending = reader.pending.as_ref().unwrap();
        assert_eq!(pending.offset, PREFIX_BYTES);
        assert_eq!(pending.remaining, 0);
        assert_eq!(pending.spans.remaining(), 0);
        assert_eq!(pending.current.as_ref().unwrap().as_ref(), wire);
        assert!(reader.frame.is_empty());
        let pressure = fixture.pool.reserve(fixture.pool.available()).unwrap();
        let mut work = WorkBudget::new(frame.len() as u64 - PREFIX_BYTES as u64 + 2);
        let AssemblyStep::Output(output) = reader.poll(&mut work, &cancel).unwrap() else {
            panic!("coalesced bytes must publish without another allocation");
        };
        assert_eq!(work.remaining(), 0);
        assert_eq!(output.bytes.as_ref(), frame);
        assert_eq!(output.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(output.dts, Some(Time::new(89_000, 90_000).unwrap()));
        drop(pressure);
    }

    #[test]
    fn optional_allocation_and_freeze_failures_keep_every_original_span() {
        for available in [0, 720] {
            let mut fixture = PesFixture::new();
            let frame = long_frame();
            let wire = frame.repeat(3);
            let parts: Vec<_> = wire.chunks(120).collect();
            let (mut packet, clock) = fragmented_packet(&mut fixture, &parts);
            let mut reader = fixture.reader();
            assert!(reader.push(&mut packet, clock).unwrap());
            let cancel = AtomicBool::new(false);
            assert!(matches!(
                reader.poll(&mut WorkBudget::new(8), &cancel).unwrap(),
                AssemblyStep::Yield
            ));
            let pending = reader.pending.as_mut().unwrap();
            let original = pending.current().unwrap().backing_bytes().storage_address();
            let tail: Vec<_> = pending
                .spans
                .remaining_spans()
                .map(|span| span.bytes.backing_bytes().storage_address())
                .collect();
            let mut short = WorkBudget::new(wire.len() as u64 + 2 * 5 + 1);
            assert!(
                !pending
                    .try_coalesce(
                        &fixture.pool,
                        MAXIMUM_ADTS_BYTES,
                        frame.len(),
                        &mut short,
                        &cancel
                    )
                    .unwrap()
            );
            assert_eq!(short.spent(), 0);
            assert!(!pending.coalescing_attempted);
            let pressure = fixture
                .pool
                .reserve(fixture.pool.available() - available)
                .unwrap();
            let used = fixture.pool.used();
            let mut work = WorkBudget::new(4096);
            assert!(
                !pending
                    .try_coalesce(
                        &fixture.pool,
                        MAXIMUM_ADTS_BYTES,
                        frame.len(),
                        &mut work,
                        &cancel
                    )
                    .unwrap()
            );
            assert_eq!(work.spent(), if available == 0 { 6 } else { 727 });
            assert_eq!(fixture.pool.used(), used);
            assert_eq!(
                pending.current().unwrap().backing_bytes().storage_address(),
                original
            );
            assert_eq!(pending.offset, PREFIX_BYTES);
            assert_eq!(pending.remaining, wire.len() - 120);
            assert_eq!(
                pending
                    .spans
                    .remaining_spans()
                    .map(|span| span.bytes.backing_bytes().storage_address())
                    .collect::<Vec<_>>(),
                tail
            );
            assert!(pending.coalescing_attempted);
            let mut retry = WorkBudget::new(4096);
            assert!(
                !pending
                    .try_coalesce(
                        &fixture.pool,
                        MAXIMUM_ADTS_BYTES,
                        frame.len(),
                        &mut retry,
                        &cancel
                    )
                    .unwrap()
            );
            assert_eq!(retry.spent(), 0);
            drop(pressure);
            let (frames, _) = collect(&mut reader, 4096);
            assert_eq!(frames.len(), 3);
            for output in frames {
                assert_eq!(output.bytes.as_ref(), frame);
                assert_eq!(output.bytes.retained_capacity(), frame.len());
            }
        }
    }

    #[test]
    fn bounded_coalescing_preserves_delayed_span_errors() {
        for (foreign, missing) in [(true, false), (false, false), (false, true)] {
            let mut fixture = PesFixture::new();
            let (mut packet, clock) = fragmented_packet(&mut fixture, &[&F48[..8], &F48[8..]]);
            if missing {
                packet.as_mut().unwrap().payload_len += 1;
            } else {
                let (tail, _) = if foreign {
                    PesFixture::new().chunk(&F48, 180_000, None)
                } else {
                    fixture.chunk(&F48, 180_000, None)
                };
                let mut tail = tail.unwrap();
                let packet = packet.as_mut().unwrap();
                if foreign {
                    packet.payload_len += tail.payload_len;
                }
                packet.spans.try_push(tail.spans.pop().unwrap()).unwrap();
            }
            let mut reader = fixture.reader();
            assert!(reader.push(&mut packet, clock).unwrap());
            let cancel = AtomicBool::new(false);
            let mut work = WorkBudget::new(128);
            let AssemblyStep::Output(output) = reader.poll(&mut work, &cancel).unwrap() else {
                panic!("the earlier valid fragmented frame must be delivered");
            };
            assert_eq!(output.bytes.as_ref(), F48);
            assert!(reader.pending.as_ref().unwrap().coalescing_attempted);
            assert_eq!(
                reader.poll(&mut work, &cancel).unwrap_err(),
                if foreign {
                    Error::ForeignPool
                } else {
                    Error::Invalid("HLS AAC PES payload extent")
                }
            );
        }
    }

    #[test]
    fn tiny_work_keeps_copy_accounting_and_joined_tails_respect_the_frame_bound() {
        for (maximum, quantum) in [(MAXIMUM_ADTS_BYTES, 1), (240, 4096)] {
            let mut fixture = PesFixture::new();
            let frame = long_frame();
            let wire = frame.repeat(3);
            let parts: Vec<_> = wire.chunks(120).collect();
            let (mut packet, clock) = fragmented_packet(&mut fixture, &parts);
            let mut reader = HlsAac::new(&fixture.pool, CONFIG, maximum).unwrap();
            assert!(reader.push(&mut packet, clock).unwrap());
            let (frames, spent) = collect(&mut reader, quantum);
            // With a 240-byte bound only the last frame's tail can join: its
            // copy and four setup/metadata units replace one span boundary.
            assert_eq!(
                spent,
                3 * 243 + 6 + if quantum == 1 { 0 } else { 240 + 4 - 1 }
            );
            assert_eq!(frames.len(), 3);
            for (index, output) in frames.iter().enumerate() {
                assert_eq!(output.bytes.as_ref(), frame);
                assert_eq!(output.bytes.retained_capacity(), frame.len());
                if index != 0 {
                    assert!(!output.bytes.shares_storage_with(&frames[0].bytes));
                }
            }
        }
    }

    #[test]
    fn coalesced_tail_keeps_a_split_prefix_and_the_next_pes_anchor() {
        let mut fixture = PesFixture::new();
        let wire = [&F48[..], &F48[..], &F48[..5]].concat();
        let (mut packet, clock) = fragmented_packet(&mut fixture, &[&wire[..8], &wire[8..]]);
        let mut reader = fixture.reader();
        assert!(reader.push(&mut packet, clock).unwrap());
        let (mut frames, _) = collect(&mut reader, 4096);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].bytes.shares_storage_with(&frames[1].bytes));
        assert_eq!(reader.prefix_used, 5);
        let (mut packet, clock) =
            fixture.chunk(&[&F48[5..], &F48[..]].concat(), 180_000, Some(179_000));
        assert!(reader.push(&mut packet, clock).unwrap());
        frames.extend(collect(&mut reader, 4096).0);
        assert_eq!(frames.len(), 4);
        for output in &frames {
            assert_eq!(output.bytes.as_ref(), F48);
        }
        assert_eq!(frames[0].pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(frames[0].dts, Some(Time::new(89_000, 90_000).unwrap()));
        assert_eq!((frames[1].pts, frames[1].dts), (None, None));
        assert_eq!((frames[2].pts, frames[2].dts), (None, None));
        assert_eq!(frames[3].pts, Some(Time::new(2, 1).unwrap()));
        assert_eq!(frames[3].dts, Some(Time::new(179_000, 90_000).unwrap()));
    }

    #[test]
    fn complete_frames_share_bounded_storage_without_new_capacity() {
        let mut fixture = PesFixture::new();
        let (mut packet, clock) = fixture.chunk(&[&F48[..], &F44_CRC[..]].concat(), 90_000, None);
        let backing = packet.as_ref().unwrap().spans.as_slice()[0]
            .backing_bytes()
            .clone();
        let mut reader = HlsAac::new(&fixture.pool, CONFIG, backing.retained_capacity()).unwrap();
        assert!(reader.push(&mut packet, clock).unwrap());
        let pressure = fixture.pool.reserve(fixture.pool.available()).unwrap();
        let cancel = AtomicBool::new(false);
        let mut frames = Vec::new();
        for expected in [&F48[..], &F44_CRC[..]] {
            let mut work = WorkBudget::new(expected.len() as u64 + 3);
            let AssemblyStep::Output(frame) = reader.poll(&mut work, &cancel).unwrap() else {
                panic!("complete frame must fit its full work budget");
            };
            assert_eq!(work.spent(), expected.len() as u64 + 3);
            assert_eq!(frame.bytes.as_ref(), expected);
            assert_eq!(frame.header, inspect_aac_adts_header(expected).unwrap());
            assert!(frame.bytes.shares_storage_with(&backing));
            assert!(frame.bytes.belongs_to(&fixture.pool));
            frames.push(frame);
        }
        assert_eq!(frames[0].pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(frames[1].pts, None);
        assert_eq!((frames[0].dts, frames[1].dts), (None, None));
        assert_eq!(frames[0].duration, Time::new(1024, 48_000).unwrap());
        assert_eq!(frames[1].duration, Time::new(1024, 44_100).unwrap());
        let pool = fixture.pool.clone();
        drop(pressure);
        drop(reader);
        drop(fixture);
        drop(backing);
        assert_eq!(
            pool.used(),
            Budget::control_storage_bytes() + frames[0].bytes.retained_storage_bytes()
        );
        drop(frames);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    #[test]
    fn complete_frame_uses_buffer_when_storage_or_work_exceeds_the_fast_path() {
        for (maximum, units) in [(F48.len(), 13), (MAXIMUM_ADTS_BYTES, 12)] {
            let mut fixture = PesFixture::new();
            let (mut packet, clock) = fixture.chunk(&F48, 90_000, None);
            let backing = packet.as_ref().unwrap().spans.as_slice()[0]
                .backing_bytes()
                .clone();
            let mut reader = HlsAac::new(&fixture.pool, CONFIG, maximum).unwrap();
            assert!(reader.push(&mut packet, clock).unwrap());
            let cancel = AtomicBool::new(false);
            let mut work = WorkBudget::new(units);
            let step = reader.poll(&mut work, &cancel).unwrap();
            assert_eq!(work.spent(), units);
            let frame = match step {
                AssemblyStep::Output(frame) => frame,
                AssemblyStep::Yield => {
                    assert_eq!(reader.frame.as_slice(), F48);
                    let AssemblyStep::Output(frame) =
                        reader.poll(&mut WorkBudget::new(1), &cancel).unwrap()
                    else {
                        panic!("buffered frame must resume publication");
                    };
                    frame
                }
                _ => panic!("complete frame must be buffered or published"),
            };
            assert_eq!(frame.bytes.as_ref(), F48);
            assert!(!frame.bytes.shares_storage_with(&backing));
            assert!(frame.bytes.retained_capacity() <= maximum);
            assert_eq!(frame.pts, Some(Time::new(1, 1).unwrap()));
        }
    }

    #[test]
    fn a_valid_header_at_a_split_prefix_continuation_still_uses_the_buffer() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        // The staged header describes 15 bytes; the next span coincidentally
        // begins with a different complete ADTS frame.
        let prefix = [0xff, 0xf1, 0x4c, 0x40, 1];
        let (mut first, clock) = fixture.chunk(&prefix, 90_000, None);
        assert!(reader.push(&mut first, clock).unwrap());
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(64), &cancel).unwrap(),
            AssemblyStep::NeedPayload
        ));
        let (mut second, clock) = fixture.chunk(&F44_CRC, 180_000, None);
        let backing = second.as_ref().unwrap().spans.as_slice()[0]
            .backing_bytes()
            .clone();
        assert!(reader.push(&mut second, clock).unwrap());
        let AssemblyStep::Output(frame) = reader.poll(&mut WorkBudget::new(64), &cancel).unwrap()
        else {
            panic!("split prefix must complete its original frame");
        };
        assert_eq!(frame.bytes.as_ref(), [&prefix[..], &F44_CRC[..10]].concat());
        assert!(!frame.bytes.shares_storage_with(&backing));
        assert_eq!(frame.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(frame.duration, Time::new(1024, 48_000).unwrap());
    }

    #[test]
    fn complete_frame_limit_is_checked_before_aliasing() {
        let mut fixture = PesFixture::new();
        let mut reader = HlsAac::new(&fixture.pool, CONFIG, F44_CRC.len() - 1).unwrap();
        let (mut packet, clock) = fixture.chunk(&F44_CRC, 90_000, None);
        assert!(reader.push(&mut packet, clock).unwrap());
        let mut work = WorkBudget::new(64);
        assert_eq!(
            reader.poll(&mut work, &AtomicBool::new(false)).unwrap_err(),
            Error::Limit {
                requested: F44_CRC.len(),
                maximum: F44_CRC.len() - 1,
            }
        );
        assert_eq!(work.spent(), PREFIX_BYTES as u64 + 1);
    }

    #[test]
    fn pes_anchors_belong_to_first_frame_start_and_bytes_survive_reader() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut first, clock_a) = fixture.chunk(&[&F48[..], &F44_CRC[..3]].concat(), 90_000, None);
        let (mut second, clock_b) = fixture.chunk(
            &[&F44_CRC[3..], &F48, &F48].concat(),
            180_000,
            Some(179_000),
        );
        assert!(reader.push(&mut first, clock_a).unwrap());
        assert!(first.is_none());
        assert!(!reader.push(&mut second, clock_b).unwrap());
        assert!(second.is_some());
        let mut out = Vec::new();
        assert!(matches!(
            drain(&mut reader, &mut out),
            AssemblyStep::NeedPayload
        ));
        assert_eq!(out.len(), 1);
        assert!(reader.push(&mut second, clock_b).unwrap());
        reader.finish();
        assert!(matches!(
            drain(&mut reader, &mut out),
            AssemblyStep::Finished
        ));
        drop(reader);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].bytes.as_ref(), F48);
        assert_eq!(out[1].bytes.as_ref(), F44_CRC);
        assert_eq!(out[2].bytes.as_ref(), F48);
        assert_eq!(out[3].bytes.as_ref(), F48);
        assert_eq!(out[0].duration, Time::new(1024, 48_000).unwrap());
        assert_eq!(out[1].duration, Time::new(1024, 44_100).unwrap());
        assert_eq!(out[2].duration, out[0].duration);
        assert_eq!(out[3].duration, out[2].duration);
        assert_eq!(out[0].pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(out[0].dts, None);
        assert_eq!((out[1].pts, out[1].dts), (None, None));
        assert_eq!(out[2].pts, Some(Time::new(2, 1).unwrap()));
        assert_eq!(out[2].dts, Some(Time::new(179_000, 90_000).unwrap()));
        assert_eq!((out[3].pts, out[3].dts), (None, None));
    }

    #[test]
    fn headerless_continuation_keeps_unused_anchor() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let mut out = Vec::new();
        for (part, pts) in [(&F48[..8], 90_000), (&F48[8..], 180_000)] {
            let (mut packet, clock) = fixture.chunk(part, pts, None);
            assert!(reader.push(&mut packet, clock).unwrap());
            assert!(matches!(
                drain(&mut reader, &mut out),
                AssemblyStep::NeedPayload
            ));
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts, Some(Time::new(1, 1).unwrap()));
        let (mut packet, _) = fixture.chunk(&F48, 270_000, None);
        let headerless = packet.as_mut().unwrap();
        headerless.time_header_applies = false;
        headerless.payload_kind = DemuxPayloadKind::HeaderlessContinuation;
        let absent = fixture
            .clock
            .project_header(
                257,
                EPOCHS,
                None,
                &mut WorkBudget::new(1),
                &AtomicBool::new(false),
            )
            .unwrap()
            .unwrap();
        assert!(reader.push(&mut packet, absent).unwrap());
        reader.finish();
        assert!(matches!(
            drain(&mut reader, &mut out),
            AssemblyStep::Finished
        ));
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].bytes.as_ref(), F48);
        assert_eq!(out[1].pts, Some(Time::new(2, 1).unwrap()));
        assert_eq!(out[1].dts, None);
    }

    #[test]
    fn exhausted_span_releases_its_owner_after_the_boundary_charge() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut packet, clock) = fixture.chunk(&F48[..8], 90_000, Some(89_000));
        let (tail, _) = fixture.chunk(&F48[8..], 180_000, None);
        let mut tail = tail.unwrap();
        let head = packet.as_mut().unwrap();
        let first_charge = head.spans.as_slice()[0]
            .backing_bytes()
            .retained_storage_bytes();
        head.payload_len += tail.payload_len;
        head.spans.try_push(tail.spans.pop().unwrap()).unwrap();
        drop(tail);
        assert!(reader.push(&mut packet, clock).unwrap());

        let cancel = AtomicBool::new(false);
        let mut work = WorkBudget::new(10);
        assert!(matches!(
            reader.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::Yield
        ));
        assert_eq!(work.spent(), 10);
        let before = fixture.pool.used();
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            reader.poll(&mut work, &cancel).unwrap(),
            AssemblyStep::Yield
        ));
        assert_eq!(work.spent(), 1);
        assert_eq!(fixture.pool.used(), before - first_charge);

        let mut work = WorkBudget::new(3);
        let AssemblyStep::Output(frame) = reader.poll(&mut work, &cancel).unwrap() else {
            panic!("expected the frame spanning both carriers");
        };
        assert_eq!(work.spent(), 3);
        assert_eq!(frame.bytes.as_ref(), F48);
        assert_eq!(frame.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(frame.dts, Some(Time::new(89_000, 90_000).unwrap()));
    }

    #[test]
    fn an_invalid_trailing_span_is_checked_after_the_ready_frame() {
        for (foreign, expected) in [
            (true, Error::ForeignPool),
            (false, Error::Invalid("HLS AAC PES payload extent")),
        ] {
            let mut fixture = PesFixture::new();
            let mut reader = fixture.reader();
            let (mut packet, clock) = fixture.chunk(&F48, 90_000, None);
            let (tail, _) = if foreign {
                PesFixture::new().chunk(&F48, 180_000, None)
            } else {
                fixture.chunk(&F48, 180_000, None)
            };
            let mut tail = tail.unwrap();
            let head = packet.as_mut().unwrap();
            if foreign {
                head.payload_len += tail.payload_len;
            }
            head.spans.try_push(tail.spans.pop().unwrap()).unwrap();
            assert!(reader.push(&mut packet, clock).unwrap());

            let cancel = AtomicBool::new(false);
            let mut work = WorkBudget::new(64);
            let AssemblyStep::Output(frame) = reader.poll(&mut work, &cancel).unwrap() else {
                panic!("expected the frame before the invalid span");
            };
            assert_eq!(frame.bytes.as_ref(), F48);
            assert_eq!(reader.poll(&mut work, &cancel).unwrap_err(), expected);
        }
    }

    #[test]
    fn retained_frame_retries_publication_after_pool_pressure() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut packet, clock) = fixture.chunk(&F48, 90_000, None);
        assert!(reader.push(&mut packet, clock).unwrap());
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(0), &cancel).unwrap(),
            AssemblyStep::Yield
        ));
        // Charge all input bytes, header parsing and allocation, leaving the
        // completed frame waiting for one publication unit.
        assert!(matches!(
            reader
                .poll(&mut WorkBudget::new(F48.len() as u64 + 2), &cancel)
                .unwrap(),
            AssemblyStep::Yield
        ));
        let pressure = fixture.pool.reserve(fixture.pool.available()).unwrap();
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(1), &cancel),
            Err(Error::Backpressure { .. })
        ));
        drop(pressure);
        let AssemblyStep::Output(frame) = reader.poll(&mut WorkBudget::new(1), &cancel).unwrap()
        else {
            panic!("one publication after pressure")
        };
        assert_eq!(frame.bytes.as_ref(), F48);
        assert_eq!(frame.pts, Some(Time::new(1, 1).unwrap()));
        assert!(frame.bytes.belongs_to(&fixture.pool));
        let mut out = Vec::new();
        assert!(matches!(
            drain(&mut reader, &mut out),
            AssemblyStep::NeedPayload
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn clean_eof_cancel_and_invalid_layout_are_distinct() {
        let mut invalid = F48;
        invalid[6] |= 1; // Multiple raw data blocks.
        for (bytes, error) in [
            (&F48[..3], Error::Truncated),
            (&F44_CRC[..9], Error::Truncated),
            (
                &invalid[..],
                Error::Unsupported("ADTS multiple raw-data-block layout"),
            ),
        ] {
            let mut fixture = PesFixture::new();
            let mut reader = fixture.reader();
            let (mut packet, clock) = fixture.chunk(bytes, 90_000, None);
            assert!(reader.push(&mut packet, clock).unwrap());
            reader.finish();
            let cancel = AtomicBool::new(false);
            assert_eq!(
                reader.poll(&mut WorkBudget::new(64), &cancel).unwrap_err(),
                error
            );
            assert_eq!(
                reader.poll(&mut WorkBudget::new(64), &cancel).unwrap_err(),
                error
            );
        }
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut packet, clock) = fixture.chunk(&F48, 90_000, None);
        assert!(reader.push(&mut packet, clock).unwrap());
        assert!(matches!(
            reader
                .poll(&mut WorkBudget::new(3), &AtomicBool::new(false))
                .unwrap(),
            AssemblyStep::Yield
        ));
        let used = fixture.pool.used();
        assert_eq!(
            reader
                .poll(&mut WorkBudget::new(64), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
        assert!(fixture.pool.used() < used);
    }
}
