//! Access units for explicitly selected TS AAC ADTS carriage (PMT stream type
//! 0x0f). PES boundaries are transport boundaries, even inside an ADTS header.
use crate::{
    TrackKey, TsNativeCarriage, TsNativeConfiguration,
    hls_avc::{clock_describes, lane_track, pes_time},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, BudgetVecIntoIter, Bytes, Error, Result, Time, WorkBudget};
use triergon_framing::{AacAdtsHeader, inspect_aac_adts_header};
use triergon_mpegts::{
    AssemblyStep,
    compat::pes::{DemuxPacket, DemuxPayloadKind, DemuxSpan, DemuxTimeView, RegisteredCarriage},
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
    spans: BudgetVecIntoIter<DemuxSpan>,
    current: Option<Bytes>,
    offset: usize,
    remaining: usize,
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
        if !lane_track(self.config.track, self.config.configuration_epoch, packet)
            || packet.stream_id != 0xc0
            || packet.source_header.stream_id != packet.stream_id
        {
            return Err(Error::Invalid("HLS AAC PES track or configuration"));
        }
        if packet.flags & PES_CORRUPT != 0
            || packet.payload_kind == DemuxPayloadKind::DeclaredIncomplete
            || packet.payload_len == 0
            || packet.spans.is_empty()
        {
            return Err(Error::Invalid("HLS AAC corrupt or empty PES"));
        }
        if !packet.spans.budget().same_pool(&self.pool)
            || !packet.header_source_ranges.budget().same_pool(&self.pool)
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
        if !clock_describes(&clock, packet, raw_pts, raw_dts) {
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
        self.pending = mailbox.take().map(|packet| PendingPes {
            spans: packet.spans.into_iter(),
            current: None,
            offset: 0,
            remaining: packet.payload_len,
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
                    return self.publish(header);
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
            let span = match &mut input.current {
                Some(bytes) => bytes,
                slot @ None => {
                    let Some(span) = input.spans.next() else {
                        if input.remaining != 0 {
                            return Err(Error::Invalid("HLS AAC PES payload extent"));
                        }
                        self.pending = None;
                        continue;
                    };
                    let bytes = span.into_bytes();
                    if !bytes.belongs_to(&self.pool) {
                        return Err(Error::ForeignPool);
                    }
                    // Validate a span's full extent before reading any of it.
                    input.remaining = input
                        .remaining
                        .checked_sub(bytes.len())
                        .ok_or(Error::Invalid("HLS AAC PES payload extent"))?;
                    slot.insert(bytes)
                }
            };
            let bytes = &span.as_ref()[input.offset..];
            if bytes.is_empty() {
                if !work.try_charge(1) {
                    return Ok(AssemblyStep::Yield);
                }
                input.current = None;
                input.offset = 0;
                continue;
            }

            let needed = if let Some(header) = self.header {
                let length = usize::from(header.frame_length());
                if self.frame.is_empty() {
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

    fn publish(&mut self, header: AacAdtsHeader) -> Result<AssemblyStep<HlsAacFrame>> {
        let rate = header
            .coded_frequency_hz()
            .ok_or(Error::Invalid("ADTS sampling index"))?;
        let duration = Time::new(1024, u128::from(rate))?;
        let frame = std::mem::replace(&mut self.frame, BudgetVec::new(&self.pool));
        let bytes = match frame.freeze() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.frame = error.buffer;
                return Err(error.error);
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
