//! Aligned Dolby syncframes for one selected independent HLS TS segment.
use crate::{
    TrackKey, TsNativeCarriage, TsNativeConfiguration,
    hls_pes::{clock_describes, lane_track, pes_time},
    hls_segment::HlsMediaAudioUnit,
    ts_payload::PesInput,
};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVecIntoIter, Error, InputWindow, Result, SourceCapabilities, SourceId, Step,
    Time, WorkBudget,
};
use triergon_framing::{Ac3FrameDetails, Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection};
use triergon_mpegts::{
    AssemblyStep,
    compat::pes::{
        DemuxBoundary, DemuxPacket, DemuxPayloadKind, DemuxSpan, DemuxTimeView,
        PrivateAudioCarriage,
    },
};

const PES_CORRUPT: u32 = 2;
const MAXIMUM_SYNCFRAME_BYTES: usize = 4096;
const TS_PAYLOAD_BYTES: usize = 184;

#[derive(Default)]
struct PesAnchor {
    pts: Option<Time>,
    dts: Option<Time>,
}

struct PendingPes {
    spans: BudgetVecIntoIter<DemuxSpan>,
    remaining: usize,
}

/// One charged PES and one frame under construction. Published frames own
/// their bytes, and their timestamps describe only applicable PES facts.
pub(crate) struct HlsDolby {
    pool: Budget,
    config: TsNativeConfiguration,
    reader: Result<Ac3FrameReader>,
    pending: Option<PendingPes>,
    source_clock_epoch: Option<(SourceId, u64)>,
    frame_start: u64,
    frame_anchor: PesAnchor,
    next_anchor: PesAnchor,
    last_duration: Option<(u32, u16, Time)>,
    eof: bool,
}

impl HlsDolby {
    pub(crate) fn new(pool: &Budget, config: TsNativeConfiguration) -> Result<Self> {
        let selection = match config.carriage {
            TsNativeCarriage::PrivateAudio(
                PrivateAudioCarriage::DvbAc3Tag | PrivateAudioCarriage::PrivateType81Ac3,
            ) => Ac3StreamSelection::Ac3Only,
            TsNativeCarriage::PrivateAudio(
                PrivateAudioCarriage::DvbEac3Tag | PrivateAudioCarriage::PrivateType87Eac3,
            ) => Ac3StreamSelection::Eac3Only,
            _ => {
                return Err(Error::Unsupported(
                    "HLS Dolby needs selected AC-3 or E-AC-3 carriage",
                ));
            }
        };
        if !matches!(config.track, TrackKey::Ts { program, pid } if program != 0 && pid <= 0x1ffe) {
            return Err(Error::Invalid("HLS Dolby track"));
        }
        let reader = Ac3FrameReader::new(
            pool,
            // This cursor indexes the concatenated elementary payload only.
            SourceId(0),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            selection,
            Ac3FrameLimits {
                maximum_frame_bytes: MAXIMUM_SYNCFRAME_BYTES,
                // The segment reader enforces its configured access-unit limit.
                maximum_frames: usize::MAX,
                maximum_input_window: TS_PAYLOAD_BYTES,
            },
        )?;
        Ok(Self {
            pool: pool.clone(),
            config,
            reader: Ok(reader),
            pending: None,
            source_clock_epoch: None,
            frame_start: 0,
            frame_anchor: PesAnchor::default(),
            next_anchor: PesAnchor::default(),
            last_duration: None,
            eof: false,
        })
    }

    /// Drain to `NeedPayload` before offering another PES. False and errors
    /// leave the mailbox with its caller.
    pub(crate) fn push(
        &mut self,
        mailbox: &mut Option<DemuxPacket>,
        clock: DemuxTimeView,
    ) -> Result<bool> {
        let reader = self.reader.as_ref().map_err(|error| *error)?;
        if self.eof {
            return Err(Error::Invalid("HLS Dolby input after EOF"));
        }
        if self.pending.is_some() {
            return Ok(false);
        }
        let Some(packet) = mailbox.as_ref() else {
            return Ok(false);
        };
        let facts = packet.facts();
        if !lane_track(self.config.track, self.config.configuration_epoch, &facts)
            || packet.stream_id != 0xbd
            || packet.source_header.stream_id != packet.stream_id
        {
            return Err(Error::Invalid("HLS Dolby PES track or configuration"));
        }
        if packet.flags & PES_CORRUPT != 0
            || packet.payload_kind != DemuxPayloadKind::DeclaredComplete
            || packet.boundary != DemuxBoundary::FiniteLength
            || packet.payload_len == 0
            || packet.spans.is_empty()
        {
            return Err(Error::Invalid(
                "HLS Dolby needs a complete clean finite PES",
            ));
        }
        if !packet.spans.budget().same_pool(&self.pool)
            || !packet.header_source_ranges.budget().same_pool(&self.pool)
        {
            return Err(Error::ForeignPool);
        }
        let origin = (packet.source_origin.source, packet.source_epochs.clock);
        if self
            .source_clock_epoch
            .is_some_and(|current| current != origin)
        {
            return Err(Error::Invalid(
                "HLS Dolby PES crosses source or clock epoch",
            ));
        }
        let header = packet
            .time_header()
            .ok_or(Error::Invalid("HLS Dolby PES missing applicable header"))?;
        if !clock_describes(&clock, &facts, header.pts_raw33, header.dts_raw33) {
            return Err(Error::Invalid("HLS Dolby clock does not describe this PES"));
        }
        let anchor = PesAnchor {
            pts: pes_time(header.pts_raw33, clock.pts90, TIMESTAMP)?,
            dts: pes_time(header.dts_raw33, clock.dts90, TIMESTAMP)?,
        };
        if reader.position() == self.frame_start {
            self.frame_anchor = anchor;
        } else {
            // A PES beginning in a frame cannot replace that frame's anchor.
            // Its header applies to the first new frame beginning in this PES.
            self.next_anchor = anchor;
        }
        self.source_clock_epoch = Some(origin);
        self.pending = mailbox.take().map(|packet| PendingPes {
            remaining: packet.payload_len,
            spans: packet.spans.into_iter(),
        });
        Ok(true)
    }

    pub(crate) fn finish(&mut self) {
        self.eof = true;
    }

    pub(crate) fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsMediaAudioUnit>> {
        let result = self.advance(work, cancel);
        if let Err(error) = result
            && !matches!(error, Error::Allocation | Error::Backpressure { .. })
        {
            self.reader = Err(error);
            self.pending = None;
        }
        result
    }

    fn advance(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<AssemblyStep<HlsMediaAudioUnit>> {
        let reader = self.reader.as_mut().map_err(|error| *error)?;
        loop {
            match reader.step(work, cancel)? {
                Step::Ready(frame) => {
                    let header = frame.facts().header;
                    match header.details {
                        Ac3FrameDetails::Eac3 { stream_type: 1, .. } => {
                            return Err(Error::Unsupported("HLS E-AC-3 dependent substream"));
                        }
                        Ac3FrameDetails::Eac3 { stream_type: 3, .. } => {
                            return Err(Error::Unsupported("HLS E-AC-3 reserved stream type"));
                        }
                        Ac3FrameDetails::Eac3 {
                            substream_id: 1.., ..
                        } => {
                            return Err(Error::Unsupported("HLS E-AC-3 additional substream"));
                        }
                        _ => {}
                    }
                    let rate = header.sample_rate_hz;
                    let samples = header.coded_samples();
                    let duration = match self.last_duration {
                        Some((last_rate, last_samples, duration))
                            if (rate, samples) == (last_rate, last_samples) =>
                        {
                            duration
                        }
                        _ => {
                            let duration = header.sample_span()?;
                            self.last_duration = Some((rate, samples, duration));
                            duration
                        }
                    };
                    let anchor = std::mem::replace(
                        &mut self.frame_anchor,
                        std::mem::take(&mut self.next_anchor),
                    );
                    self.frame_start = reader.position();
                    return Ok(AssemblyStep::Output(HlsMediaAudioUnit {
                        bytes: frame.into_bytes(),
                        pts: anchor.pts,
                        dts: anchor.dts,
                        duration,
                    }));
                }
                Step::NeedInput(request) => {
                    if let Some(input) = &mut self.pending {
                        if input.spans.len() == 0 {
                            if input.remaining != 0 {
                                return Err(Error::Invalid("HLS Dolby PES payload extent"));
                            }
                            self.pending = None;
                            continue;
                        }
                        if !work.try_charge(1) {
                            return Ok(AssemblyStep::Yield);
                        }
                        let bytes = input
                            .spans
                            .next()
                            .ok_or(Error::Invalid("HLS Dolby missing PES span"))?
                            .into_bytes();
                        let remaining = input
                            .remaining
                            .checked_sub(bytes.len())
                            .ok_or(Error::Invalid("HLS Dolby PES payload extent"))?;
                        let window =
                            InputWindow::new(request.source, request.token, request.offset, bytes)?;
                        reader.provide_owned(window)?;
                        input.remaining = remaining;
                    } else if self.eof {
                        reader.finish(request.source, request.token)?;
                    } else {
                        return Ok(AssemblyStep::NeedPayload);
                    }
                }
                Step::Yield => return Ok(AssemblyStep::Yield),
                Step::End(_) => return Ok(AssemblyStep::Finished),
            }
        }
    }
}

const TIMESTAMP: &str = "HLS Dolby PES timestamp representative";

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_core::Bytes;
    use triergon_mpegts::{
        PacketFormat, ParseLimits, SourceEpochs,
        compat::pes::{
            DemuxClock, DemuxObserver, DemuxProfile, DemuxScope, PrivateAudioDescriptors,
            SourceContext,
        },
        parse_transport,
    };

    const SOURCE: SourceId = SourceId(5);
    const EPOCHS: SourceEpochs = SourceEpochs {
        clock: 0,
        configuration: 2,
    };

    struct PesFixture {
        pool: Budget,
        reader: HlsDolby,
        observer: DemuxObserver,
        clock: DemuxClock,
        carrier: u8,
    }

    impl PesFixture {
        fn new(carriage: PrivateAudioCarriage, stream_type: u8) -> Self {
            let pool = Budget::new(16 * 1024).unwrap();
            let observer = DemuxObserver::new_private_audio(
                DemuxProfile::PesPackets,
                DemuxScope {
                    program: 1,
                    pid: 257,
                    stream_type,
                    stream_id: 0xbd,
                    entity: SOURCE,
                    epochs: EPOCHS,
                },
                carriage,
                PrivateAudioDescriptors::default(),
                pool.clone(),
                ParseLimits::default(),
            )
            .unwrap();
            let reader = HlsDolby::new(
                &pool,
                TsNativeConfiguration {
                    track: TrackKey::Ts {
                        program: 1,
                        pid: 257,
                    },
                    configuration_epoch: EPOCHS.configuration,
                    carriage: TsNativeCarriage::PrivateAudio(carriage),
                },
            )
            .unwrap();
            Self {
                pool,
                reader,
                observer,
                clock: DemuxClock::new(1).unwrap(),
                carrier: 0,
            }
        }

        fn packet(
            &mut self,
            bytes: &[u8],
            pts: u32,
            dts: Option<u32>,
        ) -> (Option<DemuxPacket>, DemuxTimeView) {
            let optional = if dts.is_some() { 10 } else { 5 };
            let length = u16::try_from(3 + optional + bytes.len())
                .unwrap()
                .to_be_bytes();
            let mut pes = vec![
                0,
                0,
                1,
                0xbd,
                length[0],
                length[1],
                0x80,
                if dts.is_some() { 0xc0 } else { 0x80 },
                optional as u8,
            ];
            pes.extend_from_slice(&timestamp(if dts.is_some() { 3 } else { 2 }, pts));
            if let Some(dts) = dts {
                pes.extend_from_slice(&timestamp(1, dts));
            }
            pes.extend_from_slice(bytes);
            let cancel = AtomicBool::new(false);
            let mut work = WorkBudget::new(4096);
            for (i, payload) in pes.chunks(TS_PAYLOAD_BYTES).enumerate() {
                let mut raw = [0xff; 188];
                raw[..4].copy_from_slice(&[
                    0x47,
                    if i == 0 { 0x41 } else { 1 },
                    1,
                    0x10 | (self.carrier & 15),
                ]);
                if payload.len() < TS_PAYLOAD_BYTES {
                    raw[3] |= 0x20;
                    raw[4] = (183 - payload.len()) as u8;
                    raw[5] = 0;
                }
                raw[188 - payload.len()..].copy_from_slice(payload);
                let ordinal = u64::from(self.carrier);
                self.carrier += 1;
                let mut packet = parse_transport(
                    Bytes::copy_from_slice(&self.pool, &raw).unwrap(),
                    PacketFormat::Ts188,
                    ordinal * 188,
                )
                .unwrap();
                packet.source_id = Some(SOURCE);
                self.observer
                    .push(
                        packet,
                        SourceContext {
                            source_event_ordinal: ordinal,
                            epochs: EPOCHS,
                            byte_run_epoch: None,
                        },
                    )
                    .unwrap();
                match self.observer.poll(&mut work, &cancel).unwrap() {
                    AssemblyStep::Output(packet) => {
                        let clock = self
                            .clock
                            .project_header(257, EPOCHS, packet.time_header(), &mut work, &cancel)
                            .unwrap()
                            .unwrap();
                        return (Some(packet), clock);
                    }
                    AssemblyStep::NeedPayload => {}
                    other => panic!("unexpected PES fixture step {other:?}"),
                }
            }
            panic!("fixture PES must emit")
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

    fn eac3(stream_type: u8, substream: u8, rate_blocks: u8) -> [u8; 16] {
        let mut frame = [0; 16];
        // Stereo, bsid 16, 8 words, dialnorm 31 and no compression.
        frame[..7].copy_from_slice(&[
            0x0b,
            0x77,
            (stream_type << 6) | (substream << 3),
            7,
            rate_blocks | 4,
            0x87,
            0xc0,
        ]);
        frame
    }

    fn drain(reader: &mut HlsDolby, frames: &mut Vec<HlsMediaAudioUnit>) -> AssemblyStep<()> {
        let cancel = AtomicBool::new(false);
        for _ in 0..128 {
            match reader.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
                AssemblyStep::Output(frame) => frames.push(frame),
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => return AssemblyStep::NeedPayload,
                AssemblyStep::Finished => return AssemblyStep::Finished,
            }
        }
        panic!("Dolby reader stalled at work quantum 1")
    }

    #[test]
    fn split_syncframe_keeps_its_anchor_and_next_start_uses_current_pes() {
        let mut fixture = PesFixture::new(PrivateAudioCarriage::PrivateType87Eac3, 0x87);
        let first = eac3(0, 0, 0x40);
        let second = eac3(0, 0, 0x20);
        let third = eac3(2, 0, 0xb0);
        let tail = [&first[7..], &second, &third].concat();
        let mut frames = Vec::new();
        for (bytes, pts, dts) in [
            (&first[..2], 90_000, None),
            (&first[2..7], 135_000, None),
            (&tail[..], 180_000, Some(179_000)),
        ] {
            let (mut packet, clock) = fixture.packet(bytes, pts, dts);
            assert!(fixture.reader.push(&mut packet, clock).unwrap());
            assert!(packet.is_none());
            assert!(matches!(
                drain(&mut fixture.reader, &mut frames),
                AssemblyStep::NeedPayload
            ));
        }
        fixture.reader.finish();
        assert!(matches!(
            drain(&mut fixture.reader, &mut frames),
            AssemblyStep::Finished
        ));
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].bytes.as_ref(), first);
        assert_eq!(frames[1].bytes.as_ref(), second);
        assert_eq!(frames[2].bytes.as_ref(), third);
        assert_eq!(frames[0].duration, Time::new(256, 44_100).unwrap());
        assert_eq!(frames[1].duration, Time::new(768, 48_000).unwrap());
        assert_eq!(frames[2].duration, Time::new(1536, 32_000).unwrap());
        assert_eq!(frames[0].pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(frames[0].dts, None);
        assert_eq!(frames[1].pts, Some(Time::new(2, 1).unwrap()));
        assert_eq!(frames[1].dts, Some(Time::new(179_000, 90_000).unwrap()));
        assert_eq!((frames[2].pts, frames[2].dts), (None, None));
    }

    #[test]
    fn ac3_across_ts_spans_retries_after_pool_pressure() {
        let mut fixture = PesFixture::new(PrivateAudioCarriage::PrivateType81Ac3, 0x81);
        let mut frame = [0; 192];
        frame[..8].copy_from_slice(&[0x0b, 0x77, 0, 0, 4, 0x40, 0x40, 0x1f]);
        let (mut packet, clock) = fixture.packet(&frame, 90_000, None);
        assert_eq!(packet.as_ref().unwrap().spans.len(), 2);
        assert!(fixture.reader.push(&mut packet, clock).unwrap());
        fixture.reader.finish();
        let pressure = fixture.pool.reserve(fixture.pool.available()).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            fixture.reader.poll(&mut WorkBudget::new(512), &cancel),
            Err(Error::Backpressure { .. })
        ));
        drop(pressure);
        let AssemblyStep::Output(output) = fixture
            .reader
            .poll(&mut WorkBudget::new(512), &cancel)
            .unwrap()
        else {
            panic!("AC-3 frame after retry")
        };
        assert!(matches!(
            fixture
                .reader
                .poll(&mut WorkBudget::new(1), &cancel)
                .unwrap(),
            AssemblyStep::Finished
        ));
        drop(fixture);
        assert_eq!(output.bytes.as_ref(), frame);
        assert_eq!(output.pts, Some(Time::new(1, 1).unwrap()));
        assert_eq!(output.duration, Time::new(1536, 48_000).unwrap());
    }

    #[test]
    fn truncated_and_non_independent_frames_fail_at_the_segment_boundary() {
        let independent = eac3(0, 0, 0);
        for (bytes, expected) in [
            (&independent[..15], Error::Truncated),
            (
                &eac3(1, 0, 0)[..],
                Error::Unsupported("HLS E-AC-3 dependent substream"),
            ),
            (
                &eac3(0, 1, 0)[..],
                Error::Unsupported("HLS E-AC-3 additional substream"),
            ),
        ] {
            let mut fixture = PesFixture::new(PrivateAudioCarriage::PrivateType87Eac3, 0x87);
            let (mut packet, clock) = fixture.packet(bytes, 90_000, None);
            assert!(fixture.reader.push(&mut packet, clock).unwrap());
            fixture.reader.finish();
            let cancel = AtomicBool::new(false);
            for _ in 0..2 {
                assert_eq!(
                    fixture
                        .reader
                        .poll(&mut WorkBudget::new(512), &cancel)
                        .unwrap_err(),
                    expected
                );
            }
        }
    }

    #[test]
    fn rejects_damaged_and_stale_pes_without_taking_the_mailbox() {
        let mut fixture = PesFixture::new(PrivateAudioCarriage::PrivateType87Eac3, 0x87);
        let bytes = eac3(0, 0, 0);
        let (mut packet, clock) = fixture.packet(&bytes[..7], 90_000, None);
        assert!(fixture.reader.push(&mut packet, clock).unwrap());
        assert!(matches!(
            drain(&mut fixture.reader, &mut Vec::new()),
            AssemblyStep::NeedPayload
        ));
        let (mut packet, mut clock) = fixture.packet(&bytes[7..], 180_000, None);
        packet.as_mut().unwrap().payload_kind = DemuxPayloadKind::DeclaredIncomplete;
        assert!(matches!(
            fixture.reader.push(&mut packet, clock),
            Err(Error::Invalid(
                "HLS Dolby needs a complete clean finite PES"
            ))
        ));
        let source = packet.as_mut().unwrap();
        source.payload_kind = DemuxPayloadKind::DeclaredComplete;
        source.source_epochs.clock += 1;
        clock.source_epochs = source.source_epochs;
        assert!(matches!(
            fixture.reader.push(&mut packet, clock),
            Err(Error::Invalid(
                "HLS Dolby PES crosses source or clock epoch"
            ))
        ));
        assert!(packet.is_some());
        assert_eq!(
            fixture
                .reader
                .poll(&mut WorkBudget::new(1), &AtomicBool::new(true))
                .unwrap_err(),
            Error::Cancelled
        );
    }
}
