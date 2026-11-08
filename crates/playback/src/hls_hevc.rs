//! HEVC pictures from one independent MPEG-TS segment, in source decoding order.
//! Annex B delimiters and picture roles come from the shared raw reader. The
//! synthetic byte offsets below count only PES payload, in transport order.

use crate::{
    TrackKey, TsNativeCarriage, TsNativeConfiguration,
    hls_avc::{AvcRaster, AvcTsClock, AvcTsLimits, clock_describes, lane_track},
    raw_video::RawVideoConfigTracker,
    ts_payload::PesInput,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, BudgetVec, BudgetVecIntoIter, Bytes, Error, InputWindow, Result, SourceCapabilities,
    SourceId, SourceLocation, Step, WorkBudget,
};
use triergon_framing::{
    Codec, NalFraming, NalHeader, RawAccessUnit, RawAnnexBLimits, RawAnnexBReader, ReaderLimits,
};
use triergon_mpegts::{
    AssemblyStep,
    compat::pes::{DemuxPacket, DemuxPayloadKind, DemuxSpan, DemuxTimeView, RegisteredCarriage},
};

const ELEMENTARY: SourceId = SourceId(0);

#[derive(Debug)]
pub(crate) struct HlsHevcPicture {
    unit: RawAccessUnit,
    size: usize,
    pub(crate) raster: AvcRaster,
    pub(crate) clock: Option<AvcTsClock>,
    pub(crate) random_access: bool,
}

impl HlsHevcPicture {
    pub(crate) fn feed_size(&self) -> usize {
        self.size
    }

    pub(crate) fn feed_spans(&self) -> impl Iterator<Item = &[u8]> {
        self.unit.segments()
    }

    /// Copy the original Annex B extent into a contiguous queue payload.
    pub(crate) fn contiguous_feed(&self, pool: &Budget) -> Result<Bytes> {
        if self
            .unit
            .nals()
            .first()
            .is_none_or(|nal| !nal.bytes().belongs_to(pool))
        {
            return Err(Error::ForeignPool);
        }
        let mut feed = BudgetVec::new(pool);
        feed.try_reserve_exact(self.size)?;
        for span in self.feed_spans() {
            feed.try_extend_from_slice(span)?;
        }
        feed.freeze().map_err(|error| error.error)
    }
}

#[derive(Clone, Copy)]
struct PesAnchor {
    start: u64,
    clock: Option<AvcTsClock>,
}

/// Recycle charged slots as pictures retire their anchors. Growth occurs only
/// when lookahead reaches a new high-water mark.
struct PesAnchors {
    slots: BudgetVec<PesAnchor>,
    head: usize,
    len: usize,
}

impl PesAnchors {
    fn push(&mut self, anchor: PesAnchor) -> Result<()> {
        if self.len == self.slots.len() {
            self.slots.try_push(anchor).map_err(|error| error.error)?;
            self.slots.as_mut_slice()[..self.len].rotate_left(self.head);
            self.head = 0;
            // Use the already charged spare capacity as ring slots so that
            // logical growth and rotation follow the allocation's growth.
            self.slots.try_resize(self.slots.capacity(), anchor)?;
        } else {
            let index = (self.head + self.len) % self.slots.len();
            self.slots.as_mut_slice()[index] = anchor;
        }
        self.len += 1;
        Ok(())
    }

    fn front(&self) -> Option<PesAnchor> {
        (self.len != 0).then(|| self.slots.as_slice()[self.head])
    }

    fn pop(&mut self) {
        self.len -= 1;
        self.head = (self.head + 1) % self.slots.len();
    }
}

struct PendingPes {
    spans: BudgetVecIntoIter<DemuxSpan>,
    end: u64,
    anchor: Option<PesAnchor>,
}

#[derive(Clone, Copy)]
enum Entry {
    First,
    Leading(u8),
    Started,
}

struct Active {
    config: TsNativeConfiguration,
    source_epoch: Option<(SourceId, u64)>,
    last_pes: Option<SourceLocation>,
    reader: RawAnnexBReader,
    tracker: RawVideoConfigTracker,
    supplied: u64,
    input: Option<PendingPes>,
    unit: Option<RawAccessUnit>,
    anchors: PesAnchors,
    clock: Option<AvcTsClock>,
    entry: Entry,
    eof: bool,
}

pub(crate) struct HlsHevc {
    active: Option<Active>,
    failure: Option<Error>,
}

impl HlsHevc {
    pub(crate) fn new(
        pool: &Budget,
        config: TsNativeConfiguration,
        limits: AvcTsLimits,
    ) -> Result<Self> {
        if config.carriage != TsNativeCarriage::Registered(RegisteredCarriage::Hevc) {
            return Err(Error::Unsupported("HLS HEVC needs selected HEVC carriage"));
        }
        if !matches!(config.track, TrackKey::Ts { program, pid } if program != 0 && (1..0x1fff).contains(&pid))
        {
            return Err(Error::Invalid("HLS HEVC track"));
        }
        let defaults = RawAnnexBLimits::default();
        let raw_limits = RawAnnexBLimits {
            nal: ReaderLimits {
                maximum_nal_bytes: limits.maximum_access_unit_bytes,
                ..defaults.nal
            },
            maximum_access_unit_bytes: limits.maximum_access_unit_bytes,
            ..defaults
        };
        let reader = RawAnnexBReader::new(
            pool,
            Codec::Hevc,
            ELEMENTARY,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            raw_limits,
        )?;
        Ok(Self {
            active: Some(Active {
                config,
                source_epoch: None,
                last_pes: None,
                reader,
                tracker: RawVideoConfigTracker::new(
                    pool,
                    Codec::Hevc,
                    limits.maximum_parameter_set_bytes,
                )?,
                supplied: 0,
                input: None,
                unit: None,
                anchors: PesAnchors {
                    slots: BudgetVec::new(pool),
                    head: 0,
                    len: 0,
                },
                clock: None,
                entry: Entry::First,
                eof: false,
            }),
            failure: None,
        })
    }

    /// Drain to NeedPayload before offering another PES. Rejection keeps the mailbox.
    pub(crate) fn push(
        &mut self,
        mailbox: &mut Option<DemuxPacket>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.active
            .as_mut()
            .ok_or(Error::Invalid("HLS HEVC input after EOF"))?
            .push(mailbox, clock)
    }

    pub(crate) fn finish(&mut self) {
        if let Some(active) = &mut self.active {
            active.eof = true;
        }
    }

    pub(crate) fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsHevcPicture>> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let Some(active) = &mut self.active else {
            return Ok(AssemblyStep::Finished);
        };
        let result = active.poll(work, cancel);
        match result {
            Ok(AssemblyStep::Finished) => self.active = None,
            Err(error) if !matches!(error, Error::Backpressure { .. } | Error::Allocation) => {
                self.failure = Some(error);
                self.active = None;
            }
            _ => {}
        }
        result
    }
}

impl Active {
    fn push(&mut self, mailbox: &mut Option<DemuxPacket>, clock: DemuxTimeView) -> Result<bool> {
        if self.eof {
            return Err(Error::Invalid("HLS HEVC input after EOF"));
        }
        if self.input.is_some() {
            return Ok(false);
        }
        let Some(packet) = mailbox.as_ref() else {
            return Ok(false);
        };
        let facts = packet.facts();
        if !lane_track(self.config.track, self.config.configuration_epoch, &facts)
            || packet.stream_id & 0xf0 != 0xe0
            || packet.source_header.stream_id != packet.stream_id
        {
            return Err(Error::Invalid("HLS HEVC PES track or configuration"));
        }
        if packet.flags & 2 != 0
            || packet.payload_kind == DemuxPayloadKind::DeclaredIncomplete
            || packet.payload_len == 0
            || packet.spans.is_empty()
        {
            return Err(Error::Invalid("HLS HEVC corrupt or empty PES"));
        }
        let pool = self.reader.budget();
        if !packet.spans.budget().same_pool(pool)
            || !packet.header_source_ranges.budget().same_pool(pool)
        {
            return Err(Error::ForeignPool);
        }
        let source_epoch = (packet.source_origin.source, packet.source_epochs.clock);
        if self
            .source_epoch
            .is_some_and(|previous| previous != source_epoch)
        {
            return Err(Error::Invalid("HLS HEVC PES crosses source or clock epoch"));
        }

        let header = packet.time_header();
        let continuation = packet.payload_kind == DemuxPayloadKind::HeaderlessContinuation;
        if continuation != header.is_none()
            || (continuation && self.last_pes != Some(packet.source_origin))
        {
            return Err(Error::Invalid(
                "HLS HEVC PES continuation without its causal header",
            ));
        }
        let (pts, dts) = (
            header.and_then(|h| h.pts_raw33),
            header.and_then(|h| h.dts_raw33),
        );
        if !clock_describes(&clock, &facts, pts, dts) {
            return Err(Error::Invalid("HLS HEVC clock does not describe this PES"));
        }
        let end = self
            .supplied
            .checked_add(u64::try_from(packet.payload_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        let anchor = if header.is_some() {
            let clock = AvcTsClock::try_from(clock)?;
            let stamped = clock.source_pts_raw33.is_some() || clock.source_dts_raw33.is_some();
            Some(PesAnchor {
                start: self.supplied,
                clock: stamped.then_some(clock),
            })
        } else {
            None
        };
        self.source_epoch = Some(source_epoch);
        self.last_pes = Some(packet.source_origin);
        self.input = mailbox.take().map(|packet| PendingPes {
            spans: packet.spans.into_iter(),
            end,
            anchor,
        });
        Ok(true)
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsHevcPicture>> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.unit.is_some() {
                return self.picture(work, cancel);
            }
            match self.reader.step(work, cancel)? {
                Step::Ready(unit) => self.unit = Some(unit),
                Step::Yield => return Ok(AssemblyStep::Yield),
                Step::End(_) => return Ok(AssemblyStep::Finished),
                Step::NeedInput(request) => {
                    if request.offset != self.supplied {
                        return Err(Error::Invalid("HLS HEVC elementary offset"));
                    }
                    if self
                        .input
                        .as_ref()
                        .is_some_and(|input| input.spans.len() == 0)
                    {
                        self.input = None;
                    }
                    if let Some(input) = &mut self.input {
                        // Keep the PES and its clock together if reserving an
                        // anchor encounters temporary memory pressure.
                        if let Some(anchor) = input.anchor {
                            if !work.try_charge(1) {
                                return Ok(AssemblyStep::Yield);
                            }
                            self.anchors.push(anchor)?;
                            input.anchor = None;
                        }
                        if !work.try_charge(1) {
                            return Ok(AssemblyStep::Yield);
                        }
                        let span = input
                            .spans
                            .next()
                            .ok_or(Error::Invalid("HLS HEVC missing PES span"))?;
                        if self.source_epoch.is_none_or(|(source, _)| {
                            span.source.entity != source || span.transport.entity != source
                        }) {
                            return Err(Error::SourceMismatch);
                        }
                        let bytes = span.backing_bytes();
                        if bytes.is_empty() || bytes.len() != span.source.bytes {
                            return Err(Error::Invalid("HLS HEVC PES span extent"));
                        }
                        let end = request
                            .offset
                            .checked_add(u64::try_from(bytes.len()).map_err(|_| Error::Overflow)?)
                            .ok_or(Error::Overflow)?;
                        if end > input.end || (input.spans.len() == 0 && end != input.end) {
                            return Err(Error::Invalid("HLS HEVC PES payload extent"));
                        }
                        self.reader.provide_owned(InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            span.into_bytes(),
                        )?)?;
                        self.supplied = end;
                    } else if self.eof {
                        self.reader.finish(request.source, request.token)?;
                    } else {
                        return Ok(AssemblyStep::NeedPayload);
                    }
                }
            }
        }
    }

    fn picture(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsHevcPicture>> {
        let unit = self
            .unit
            .as_ref()
            .ok_or(Error::Invalid("HLS HEVC missing access unit"))?;
        let first = unit.nals().first().ok_or(Error::Truncated)?;
        let NalFraming::AnnexB { prefix_offset, .. } = first.facts().framing else {
            return Err(Error::Invalid("HLS HEVC NAL framing"));
        };
        // A later PES may begin inside this picture or even inside its start
        // code. Select by the prefix's first byte, including across lookahead.
        while let Some(anchor) = self
            .anchors
            .front()
            .filter(|anchor| anchor.start <= prefix_offset)
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(AssemblyStep::Yield);
            }
            self.clock = anchor.clock;
            self.anchors.pop();
        }
        if !work.try_charge(1) {
            return Ok(AssemblyStep::Yield);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let entry = picture_entry(unit, self.entry)?;
        let size =
            usize::try_from(unit.range.end - unit.range.start).map_err(|_| Error::Overflow)?;
        // The shared tracker's bounded pass resolves the first base slice's
        // active chain transactionally. Keep the pending unit on pressure.
        let config = self.tracker.observe(unit)?.ok_or(Error::Truncated)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let raster = AvcRaster {
            width: config.width,
            height: config.height,
        };
        let unit = self
            .unit
            .take()
            .ok_or(Error::Invalid("HLS HEVC missing access unit"))?;
        self.entry = entry;
        Ok(AssemblyStep::Output(HlsHevcPicture {
            size,
            raster,
            clock: self.clock.take(),
            random_access: unit.random_access,
            unit,
        }))
    }
}

fn picture_entry(unit: &RawAccessUnit, entry: Entry) -> Result<Entry> {
    let (kind, temporal, first_slice) = unit
        .nals()
        .iter()
        .find_map(|nal| match nal.facts().header {
            NalHeader::Hevc {
                nal_unit_type: kind @ (0..=9 | 16..=21),
                layer_id: 0,
                temporal_id_plus_one,
            } => Some((
                kind,
                temporal_id_plus_one,
                nal.bytes().get(2).is_some_and(|byte| byte & 0x80 != 0),
            )),
            _ => None,
        })
        .ok_or(Error::Truncated)?;
    if !first_slice || (unit.random_access && temporal != 1) {
        return Err(Error::Invalid(
            "HLS HEVC picture starts outside a valid first slice",
        ));
    }
    match entry {
        Entry::First if !unit.random_access => Err(Error::Unsupported(
            "independent HEVC segment must start with IRAP",
        )),
        Entry::First => Ok(Entry::Leading(kind)),
        Entry::Leading(_) if matches!(kind, 8 | 9) => Err(Error::Unsupported(
            "independent HEVC segment starts with RASL dependencies",
        )),
        Entry::Leading(18 | 20) if matches!(kind, 6 | 7) => {
            Err(Error::Invalid("HEVC N_LP picture has leading pictures"))
        }
        Entry::Leading(_) if matches!(kind, 6 | 7) => Ok(entry),
        _ => Ok(Entry::Started),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_mpegts::{
        PacketFormat, ParseLimits, SourceEpochs,
        compat::pes::{DemuxClock, DemuxObserver, DemuxProfile, DemuxScope, SourceContext},
        parse_transport,
    };

    static NO: AtomicBool = AtomicBool::new(false);
    const SOURCE: SourceId = SourceId(5);
    const EPOCHS: SourceEpochs = SourceEpochs {
        configuration: 2,
        clock: 3,
    };
    const CONFIG: TsNativeConfiguration = TsNativeConfiguration {
        track: TrackKey::Ts {
            program: 1,
            pid: 257,
        },
        configuration_epoch: EPOCHS.configuration,
        carriage: TsNativeCarriage::Registered(RegisteredCarriage::Hevc),
    };

    // The first three NALs of the authored fixture are VPS, SPS and PPS.
    const HEVC: &[u8] = include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud0.hevc");
    const PARAMETER_END: usize = 84;

    struct PesFixture {
        pool: Budget,
        observer: DemuxObserver,
        clock: DemuxClock,
        carrier: u8,
    }

    impl PesFixture {
        fn new() -> Self {
            let pool = Budget::new(1 << 20).unwrap();
            let observer = DemuxObserver::new_registered(
                DemuxProfile::PesPackets,
                DemuxScope {
                    program: 1,
                    pid: 257,
                    stream_type: 0x24,
                    stream_id: 0xe0,
                    entity: SOURCE,
                    epochs: EPOCHS,
                },
                RegisteredCarriage::Hevc,
                pool.clone(),
                ParseLimits::default(),
            )
            .unwrap();
            Self {
                pool,
                observer,
                clock: DemuxClock::new(1).unwrap(),
                carrier: 0,
            }
        }

        fn reader(&self) -> HlsHevc {
            HlsHevc::new(&self.pool, CONFIG, AvcTsLimits::default()).unwrap()
        }

        fn pes(
            &mut self,
            bytes: &[u8],
            pts: u64,
            dts: u64,
        ) -> (Option<DemuxPacket>, DemuxTimeView) {
            let length = u16::try_from(13 + bytes.len()).unwrap().to_be_bytes();
            let mut pes = vec![0, 0, 1, 0xe0, length[0], length[1], 0x80, 0xc0, 10];
            pes.extend_from_slice(&timestamp(3, pts));
            pes.extend_from_slice(&timestamp(1, dts));
            pes.extend_from_slice(bytes);
            let mut ts = [0xff; 188];
            let adaptation = 183 - pes.len();
            ts[..6].copy_from_slice(&[0x47, 0x41, 1, 0x30 | self.carrier, adaptation as u8, 0]);
            ts[5 + adaptation..].copy_from_slice(&pes);
            let mut transport = parse_transport(
                Bytes::copy_from_slice(&self.pool, &ts).unwrap(),
                PacketFormat::Ts188,
                u64::from(self.carrier) * 188,
            )
            .unwrap();
            transport.source_id = Some(SOURCE);
            self.observer
                .push(
                    transport,
                    SourceContext {
                        source_event_ordinal: u64::from(self.carrier),
                        epochs: EPOCHS,
                        byte_run_epoch: None,
                    },
                )
                .unwrap();
            self.carrier += 1;
            let mut work = WorkBudget::new(4096);
            let AssemblyStep::Output(packet) = self.observer.poll(&mut work, &NO).unwrap() else {
                panic!("finite fixture PES did not emit");
            };
            let view = self
                .clock
                .project_header(257, EPOCHS, packet.time_header(), &mut work, &NO)
                .unwrap()
                .unwrap();
            (Some(packet), view)
        }
    }

    fn timestamp(tag: u8, ticks: u64) -> [u8; 5] {
        [
            (tag << 4) | ((ticks >> 30) as u8) << 1 | 1,
            (ticks >> 22) as u8,
            ((ticks >> 15) as u8) << 1 | 1,
            (ticks >> 7) as u8,
            (ticks as u8) << 1 | 1,
        ]
    }

    fn drain(reader: &mut HlsHevc, pictures: &mut Vec<HlsHevcPicture>) -> Result<AssemblyStep<()>> {
        for _ in 0..8192 {
            match reader.poll(&mut WorkBudget::new(1), &NO)? {
                AssemblyStep::Output(picture) => pictures.push(picture),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return Ok(AssemblyStep::NeedPayload),
                AssemblyStep::Finished => return Ok(AssemblyStep::Finished),
            }
        }
        panic!("HEVC lane stalled at a one-unit work budget");
    }

    #[test]
    fn split_pes_preserves_annex_b_decode_order_and_one_clock_per_header() {
        // A first-slice IDR and opaque suffix, with no AUD. The following
        // picture has two slices in a temporal sublayer.
        let first = [
            &HEVC[..PARAMETER_END],
            &[0, 0, 1, 40, 1, 0xa0, 0x55, 0, 0, 1, 124, 1, 0x66],
        ]
        .concat();
        let second = [0, 0, 0, 1, 2, 2, 0xc0, 0x71, 0, 0, 1, 2, 2, 0x40, 0x72];
        let third = [0, 0, 1, 2, 1, 0xc0, 0x73];
        let fourth = [0, 0, 1, 2, 1, 0xc0, 0x74, 0, 0];
        let split = first.len() - 2;
        let middle = [&first[split..], &second[..2]].concat();
        let tail = [&second[2..], third.as_slice(), fourth.as_slice()].concat();

        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut a, clock_a) = fixture.pes(&first[..split], (1 << 33) - 600, (1 << 33) - 900);
        let (mut b, clock_b) = fixture.pes(&middle, 9000, 0);
        let (mut c, clock_c) = fixture.pes(&tail, 6000, 3000);
        let mut pictures = Vec::new();
        assert!(reader.push(&mut a, clock_a).unwrap());
        assert!(!reader.push(&mut b, clock_b).unwrap());
        assert!(b.is_some());
        assert!(matches!(
            drain(&mut reader, &mut pictures).unwrap(),
            AssemblyStep::NeedPayload
        ));
        assert!(reader.push(&mut b, clock_b).unwrap());
        assert!(matches!(
            drain(&mut reader, &mut pictures).unwrap(),
            AssemblyStep::NeedPayload
        ));
        assert!(reader.push(&mut c, clock_c).unwrap());
        reader.finish();
        assert!(matches!(
            drain(&mut reader, &mut pictures).unwrap(),
            AssemblyStep::Finished
        ));

        assert_eq!(pictures.len(), 4);
        assert!(pictures[0].random_access);
        assert!(pictures[1..].iter().all(|picture| !picture.random_access));
        assert!(
            pictures
                .iter()
                .all(|picture| picture.raster == pictures[0].raster)
        );
        assert_eq!(
            pictures[0].clock,
            Some(AvcTsClock::try_from(clock_a).unwrap())
        );
        assert_eq!(pictures[0].clock.unwrap().pts90, Some(-600));
        assert_eq!(pictures[0].clock.unwrap().dts90, Some(-900));
        assert_eq!(pictures[1].clock.unwrap().pts90, Some(9000));
        assert_eq!(pictures[2].clock.unwrap().pts90, Some(6000));
        assert_eq!(pictures[3].clock, None);

        for (picture, bytes) in pictures
            .iter()
            .zip([first.as_slice(), &second, &third, &fourth])
        {
            assert_eq!(picture.feed_size(), bytes.len());
            assert_eq!(
                picture.feed_spans().flatten().copied().collect::<Vec<_>>(),
                bytes
            );
            assert_eq!(
                picture.contiguous_feed(&fixture.pool).unwrap().as_ref(),
                bytes
            );
        }
    }

    #[test]
    fn entry_requires_a_first_slice_irap_with_an_active_chain() {
        for data in [
            [&HEVC[..PARAMETER_END], &[0, 0, 1, 2, 1, 0xc0]].concat(),
            [&HEVC[..PARAMETER_END], &[0, 0, 1, 40, 1, 0x20]].concat(),
            vec![0, 0, 1, 40, 1, 0xa0],
        ] {
            let mut fixture = PesFixture::new();
            let mut reader = fixture.reader();
            let (mut packet, clock) = fixture.pes(&data, 0, 0);
            assert!(reader.push(&mut packet, clock).unwrap());
            reader.finish();
            let mut pictures = Vec::new();
            assert!(matches!(
                drain(&mut reader, &mut pictures),
                Err(Error::Invalid(_) | Error::Unsupported(_))
            ));
            assert!(pictures.is_empty());
        }
    }

    #[test]
    fn cra_entry_refuses_dependent_leading_rasl() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let data = [
            &HEVC[..PARAMETER_END],
            &[0, 0, 1, 42, 1, 0xa0, 0, 0, 1, 16, 1, 0xc0],
        ]
        .concat();
        let (mut packet, clock) = fixture.pes(&data, 0, 0);
        assert!(reader.push(&mut packet, clock).unwrap());
        reader.finish();
        let mut pictures = Vec::new();
        assert!(matches!(
            drain(&mut reader, &mut pictures),
            Err(Error::Unsupported(
                "independent HEVC segment starts with RASL dependencies"
            ))
        ));
        assert_eq!(pictures.len(), 1);
    }

    #[test]
    fn hevc_anchor_backpressure_preserves_the_pes_and_its_clock() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let data = [&HEVC[..PARAMETER_END], &[0, 0, 1, 40, 1, 0xa0]].concat();
        let (mut packet, clock) = fixture.pes(&data, 600, 300);
        assert!(reader.push(&mut packet, clock).unwrap());
        assert!(packet.is_none());
        let pressure = fixture.pool.reserve(fixture.pool.available()).unwrap();
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(32), &NO),
            Err(Error::Backpressure { .. })
        ));
        let active = reader.active.as_ref().unwrap();
        assert!(active.input.as_ref().unwrap().anchor.is_some());
        assert_eq!(active.supplied, 0);
        drop(pressure);

        reader.finish();
        let mut pictures = Vec::new();
        assert!(matches!(
            drain(&mut reader, &mut pictures).unwrap(),
            AssemblyStep::Finished
        ));
        let [picture] = pictures.as_slice() else {
            panic!("expected a single HEVC picture");
        };
        assert_eq!(picture.clock, Some(AvcTsClock::try_from(clock).unwrap()));
        assert_eq!(
            picture.contiguous_feed(&fixture.pool).unwrap().as_ref(),
            data
        );
    }

    #[test]
    fn cancellation_retires_an_accepted_pes() {
        let mut fixture = PesFixture::new();
        let mut reader = fixture.reader();
        let (mut packet, clock) = fixture.pes(&HEVC[..PARAMETER_END], 0, 0);
        assert!(reader.push(&mut packet, clock).unwrap());
        let before = fixture.pool.used();
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(0), &NO).unwrap(),
            AssemblyStep::Yield
        ));
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(1), &cancelled),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            reader.poll(&mut WorkBudget::new(1), &NO),
            Err(Error::Cancelled)
        ));
        assert!(fixture.pool.used() < before);
    }
}
