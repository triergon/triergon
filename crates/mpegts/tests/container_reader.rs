//! AU composition against frozen raw-AU boundaries and literal source clocks.
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    CodecId, ContainerReader, Event, MediaKind, PacketInfo, TrackId, TrackInfo,
};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step, Time,
    WorkBudget,
};
use triergon_mpegts::{
    AccessUnit, Packet, PacketFormat, Reader, ReaderEvent, ReaderLimits, TraceEvent,
};

const ID: SourceId = SourceId(77);
const PROGRAM: u16 = 7;
const PID: u16 = 256;
static OFF: AtomicBool = AtomicBool::new(false);

struct Transport {
    bytes: Vec<u8>,
    counters: [u8; 8192],
}
impl Transport {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            counters: [0; 8192],
        }
    }
    fn payload(&mut self, pid: u16, raw: &[u8]) {
        for (index, chunk) in raw.chunks(184).enumerate() {
            let mut packet = [0xff; 188];
            let cc = &mut self.counters[usize::from(pid)];
            packet[..4].copy_from_slice(&[
                0x47,
                ((pid >> 8) as u8) | if index == 0 { 0x40 } else { 0 },
                pid as u8,
                0x10 | *cc,
            ]);
            *cc = (*cc + 1) & 15;
            let at = if chunk.len() == 184 {
                4
            } else {
                packet[3] |= 0x20;
                packet[4] = (183 - chunk.len()) as u8;
                if packet[4] != 0 {
                    packet[5] = 0;
                }
                188 - chunk.len()
            };
            packet[at..].copy_from_slice(chunk);
            self.bytes.extend_from_slice(&packet);
        }
    }
    fn section(&mut self, pid: u16, mut section: Vec<u8>) {
        // Polynomial division independent of the production incremental CRC.
        let mut crc = u32::MAX;
        for byte in &section {
            crc ^= u32::from(*byte) << 24;
            for _ in 0..8 {
                crc = (crc << 1)
                    ^ if crc & 0x8000_0000 != 0 {
                        0x04c1_1db7
                    } else {
                        0
                    };
            }
        }
        section.extend_from_slice(&crc.to_be_bytes());
        section.insert(0, 0);
        self.payload(pid, &section);
    }
    fn tables(&mut self, streams: &[(u8, u16, &[u8])], version: u8) {
        if self.bytes.is_empty() {
            self.section(
                0,
                vec![0, 0xb0, 13, 0, 9, 0xc1, 0, 0, 0, PROGRAM as u8, 0xf0, 0],
            );
        }
        let length = 13 + streams.iter().map(|(_, _, d)| 5 + d.len()).sum::<usize>();
        let mut pmt = vec![
            2,
            0xb0 | (length >> 8) as u8,
            length as u8,
            0,
            PROGRAM as u8,
            0xc1 | (version << 1),
            0,
            0,
            0xe1,
            0,
            0xf0,
            0,
        ];
        for (kind, pid, descriptors) in streams {
            pmt.extend_from_slice(&[
                *kind,
                0xe0 | (pid >> 8) as u8,
                *pid as u8,
                0xf0 | (descriptors.len() >> 8) as u8,
                descriptors.len() as u8,
            ]);
            pmt.extend_from_slice(descriptors);
        }
        self.section(4096, pmt);
    }
    fn pes(&mut self, pid: u16, stream_id: u8, body: &[u8], pts: Option<u64>, dts: Option<u64>) {
        let mut optional = Vec::new();
        if let Some(pts) = pts {
            timestamp(&mut optional, if dts.is_some() { 3 } else { 2 }, pts);
        }
        if let Some(dts) = dts {
            timestamp(&mut optional, 1, dts);
        }
        let length = u16::try_from(body.len() + optional.len() + 3).unwrap();
        let mut pes = vec![
            0,
            0,
            1,
            stream_id,
            (length >> 8) as u8,
            length as u8,
            0x80,
            if dts.is_some() {
                0xc0
            } else if pts.is_some() {
                0x80
            } else {
                0
            },
            optional.len() as u8,
        ];
        pes.extend(optional);
        pes.extend_from_slice(body);
        self.payload(pid, &pes);
    }
    fn m2ts(&self) -> Vec<u8> {
        self.bytes
            .chunks(188)
            .enumerate()
            .flat_map(|(i, packet)| {
                let mut out = (0x8000_0000 | i as u32).to_be_bytes().to_vec();
                out.extend_from_slice(packet);
                out
            })
            .collect()
    }
    fn pcr(&mut self, base: u64, discontinuity: bool) {
        let mut packet = [0xff; 188];
        let cc = self.counters[usize::from(PID)].wrapping_sub(1) & 15;
        packet[..6].copy_from_slice(&[
            0x47,
            (PID >> 8) as u8,
            PID as u8,
            0x20 | cc,
            183,
            0x10 | if discontinuity { 0x80 } else { 0 },
        ]);
        packet[6..12].copy_from_slice(&[
            (base >> 25) as u8,
            (base >> 17) as u8,
            (base >> 9) as u8,
            (base >> 1) as u8,
            ((base & 1) as u8) << 7 | 0x7e,
            0,
        ]);
        self.bytes.extend_from_slice(&packet);
    }
}
fn timestamp(out: &mut Vec<u8>, prefix: u8, value: u64) {
    out.extend_from_slice(&[
        (prefix << 4) | ((value >> 29) as u8 & 0x0e) | 1,
        (value >> 22) as u8,
        ((value >> 14) as u8 & 0xfe) | 1,
        (value >> 7) as u8,
        ((value << 1) as u8 & 0xfe) | 1,
    ]);
}

struct Drive<'a> {
    reader: Reader,
    pool: Budget,
    raw: &'a [u8],
    chunk: usize,
    work: u64,
    catalogue: Vec<(u16, u64)>,
}
impl<'a> Drive<'a> {
    fn new(raw: &'a [u8], limits: ReaderLimits, chunk: usize, work: u64) -> Self {
        let pool = Budget::new(32 << 20).unwrap();
        let reader = Reader::new(
            &pool,
            ID,
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            limits,
            4096,
        )
        .unwrap();
        Self {
            reader,
            pool,
            raw,
            chunk,
            work,
            catalogue: Vec::new(),
        }
    }
    fn next(&mut self) -> Result<Option<Event<Packet, ReaderEvent>>, Error> {
        for _ in 0..2_000_000 {
            let mut work = WorkBudget::new(self.work);
            let step = self.reader.step(&mut work, &OFF)?;
            assert!(work.spent() <= self.work);
            match step {
                Step::Ready(event) => {
                    if let Event::Other(ReaderEvent::Source(source)) = &event
                        && let TraceEvent::ProgramMap {
                            map,
                            active: true,
                            configuration_epoch,
                        } = source.event()
                    {
                        self.catalogue = map
                            .streams
                            .as_slice()
                            .iter()
                            .map(|stream| (stream.pid, *configuration_epoch))
                            .collect();
                    }
                    self.assert_catalogue();
                    return Ok(Some(event));
                }
                Step::Yield => self.assert_catalogue(),
                Step::Backpressure(p) => {
                    return Err(Error::Backpressure {
                        requested: p.requested,
                        available: p.available,
                    });
                }
                Step::NeedInput(r) => {
                    let at = usize::try_from(r.offset).unwrap();
                    if at == self.raw.len() {
                        self.reader.finish(r.source, r.token)?;
                    } else {
                        let end = self.raw.len().min(at + self.chunk.min(r.maximum));
                        let bytes = Bytes::copy_from_slice(&self.pool, &self.raw[at..end])?;
                        self.reader
                            .provide(&InputWindow::new(r.source, r.token, r.offset, bytes)?)?;
                    }
                }
                Step::End(end) => {
                    assert_eq!(end.source, ID);
                    assert_eq!(end.offset, self.raw.len() as u64);
                    return Ok(None);
                }
            }
        }
        panic!("AU reader made no bounded progress");
    }
    fn assert_catalogue(&self) {
        assert_eq!(
            self.reader
                .tracks()
                .iter()
                .map(|track| (track.stream().pid, track.configuration_epoch()))
                .collect::<Vec<_>>(),
            self.catalogue
        );
    }
}

#[test]
fn avc_and_hevc_are_full_source_aus_across_pes_and_carriers() {
    for (kind, raw, golden) in [
        (
            0x1b,
            include_bytes!("../../framing/fixtures/raw-annex-b/avc-aud0.h264").as_slice(),
            include_str!("../../framing/fixtures/raw-annex-b/avc-aud0.packets"),
        ),
        (
            0x24,
            include_bytes!("../../framing/fixtures/raw-annex-b/hevc-aud0.hevc").as_slice(),
            include_str!("../../framing/fixtures/raw-annex-b/hevc-aud0.packets"),
        ),
    ] {
        let mut ts = Transport::new();
        ts.tables(&[(kind, PID, &[])], 0);
        // Split a start code and a large coded picture between finite PES.
        let cut = 2;
        ts.pes(PID, 0xe0, &raw[..cut], Some(90_000), None);
        ts.pes(PID, 0xe0, &raw[cut..], Some(180_000), Some(170_000));
        for (chunk, work, format) in [
            (1, 1, PacketFormat::Ts188),
            (4096, 8192, PacketFormat::M2ts192),
        ] {
            let m2ts = ts.m2ts();
            let input = if format == PacketFormat::Ts188 {
                &ts.bytes
            } else {
                &m2ts
            };
            let mut drive = Drive::new(
                input,
                ReaderLimits {
                    format,
                    ..ReaderLimits::default()
                },
                chunk,
                work,
            );
            let mut packets = Vec::new();
            let mut transports = 0;
            let mut pes_count = 0;
            let mut finished = false;
            while let Some(event) = drive.next().unwrap() {
                match event {
                    Event::Packet(packet) => {
                        assert!(!finished);
                        packets.push(packet);
                    }
                    Event::Other(ReaderEvent::Source(source)) => match source.event() {
                        TraceEvent::Transport(_) => transports += 1,
                        TraceEvent::Pes(_) => pes_count += 1,
                        TraceEvent::Finished => finished = true,
                        TraceEvent::Diagnostic(fault) => {
                            panic!("unexpected source fault: {fault:?}")
                        }
                        _ => (),
                    },
                    Event::Other(ReaderEvent::Unsupported { error, .. }) => panic!("{error:?}"),
                    Event::Other(ReaderEvent::Interrupted { .. }) => panic!("uninterrupted source"),
                    _ => (),
                }
            }
            assert!(finished);
            assert_eq!(pes_count, 2);
            assert_eq!(transports, input.len() / format.bytes());
            assert_eq!(packets.len(), 12);
            for (packet, line) in packets.iter().zip(golden.lines()) {
                let fields: Vec<u64> = line
                    .split_ascii_whitespace()
                    .map(|s| s.parse().unwrap())
                    .collect();
                let range = fields[0] as usize..(fields[0] + fields[1]) as usize;
                assert_eq!(packet.payload(), &raw[range.clone()]);
                let mut from_pes = Vec::new();
                for source in packet.sources() {
                    let TraceEvent::Pes(pes) = source.pes.event() else {
                        unreachable!()
                    };
                    let bytes: Vec<_> = pes.payload().flatten().copied().collect();
                    from_pes.extend_from_slice(&bytes[source.payload_range.clone()]);
                }
                assert_eq!(from_pes, packet.payload());
                assert_eq!(
                    packet.elementary_range(),
                    range.start as u64..range.end as u64
                );
                assert!(matches!(packet.access_unit(), AccessUnit::Video(unit) if unit.has_vcl));
                assert_eq!(
                    packet
                        .sources()
                        .iter()
                        .map(|source| source.elementary_range.end - source.elementary_range.start)
                        .sum::<u64>(),
                    packet.payload().len() as u64
                );
                assert_eq!(packet.track(), TrackId(u64::from(PID)));
                assert_eq!(packet.track_info().program(), PROGRAM);
                assert_eq!(packet.track_info().stream().stream_type, kind);
            }
            assert_eq!(packets[0].pts(), Some(90_000));
            assert_eq!(packets[0].dts(), None);
            assert_eq!(packets[1].pts(), Some(180_000));
            assert_eq!(packets[1].dts(), Some(170_000));
            assert!(
                packets[2..]
                    .iter()
                    .all(|packet| packet.pts().is_none() && packet.dts().is_none())
            );
            assert_eq!(drive.reader.track_count(), 1);
            let used = drive.pool.used();
            let retained = packets[0].clone();
            assert_eq!(used, drive.pool.used());
            drive.reader.close();
            assert_eq!(retained.payload(), packets[0].payload());
            drop(packets);
            drop(retained);
            assert_eq!(drive.pool.used(), Budget::control_storage_bytes());
        }
    }
}

fn adts(body: &[u8]) -> Vec<u8> {
    let length = body.len() + 7;
    let mut frame = vec![
        0xff,
        0xf1,
        0x4c,
        0x80 | (length >> 11) as u8,
        (length >> 3) as u8,
        ((length & 7) << 5) as u8 | 0x1f,
        0xfc,
    ];
    frame.extend_from_slice(body);
    frame
}

#[test]
fn audio_anchors_wait_for_the_next_frame_start_and_missing_dts_stays_absent() {
    let frames = [adts(b"first"), adts(b"second"), adts(b"third")];
    let body = frames.concat();
    let second_cut = frames[0].len() + frames[1].len() + 2;
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &body[..5], Some(90_000), None);
    ts.pes(PID, 0xc0, &body[5..second_cut], Some(180_000), None);
    ts.pes(PID, 0xc0, &body[second_cut..], Some(270_000), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 1, 1);
    let mut packets = Vec::new();
    let mut pressure = None;
    let mut refused = 0;
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(packet))) => packets.push(packet),
            Ok(Some(Event::Other(ReaderEvent::Source(event))))
                if matches!(event.event(), TraceEvent::Pes(_)) =>
            {
                pressure = Some(drive.pool.reserve(drive.pool.available()).unwrap());
            }
            Err(Error::Backpressure { .. }) if pressure.is_some() => {
                drop(pressure.take());
                refused += 1;
            }
            Ok(Some(_)) => (),
            Ok(None) => break,
            Err(error) => panic!("{error:?}"),
        }
    }
    assert_eq!(refused, 3);
    assert_eq!(packets.len(), frames.len());
    assert_eq!(
        packets.iter().map(PacketInfo::pts).collect::<Vec<_>>(),
        [Some(90_000), Some(180_000), None]
    );
    for (packet, frame) in packets.iter().zip(frames) {
        assert_eq!(packet.payload(), frame);
        assert_eq!(packet.dts(), None);
        assert_eq!(
            packet.sample_duration(),
            Some(Time::new(1024, 48_000).unwrap())
        );
        assert_eq!(packet.duration(), Some(1920));
    }
}

#[test]
fn unknown_and_private_carriages_keep_every_source_fact_and_report_a_gap() {
    let descriptors = [0xee, 1, 7, 0xee, 1, 7];
    let mut ts = Transport::new();
    ts.tables(&[(0x99, PID, &descriptors), (0x81, PID + 1, &[])], 0);
    ts.pes(PID, 0xbd, b"opaque private body", None, None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 3);
    let mut gaps = 0;
    let mut pes = None;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Packet(_) => panic!("PES must not be exposed as an AU"),
            Event::Other(ReaderEvent::Unsupported { track, .. }) => {
                assert!(matches!(track.codec(), CodecId::Other(_)));
                gaps += 1;
            }
            Event::Other(ReaderEvent::Source(source))
                if matches!(source.event(), TraceEvent::Pes(_)) =>
            {
                pes = Some(source);
            }
            _ => (),
        }
    }
    assert_eq!(gaps, 2);
    let track = drive.reader.track(0).unwrap();
    assert_eq!(track.stream().descriptor_bytes.as_ref(), descriptors);
    assert_eq!(track.kind(), MediaKind::Unknown);
    assert_eq!(track.codec(), CodecId::Other(&[0x99]));
    let pes = pes.unwrap();
    let TraceEvent::Pes(body) = pes.event() else {
        unreachable!()
    };
    assert_eq!(
        body.payload().flatten().copied().collect::<Vec<_>>(),
        b"opaque private body"
    );
    assert_eq!(body.pid, PID);
    let used = drive.pool.used();
    let alias = pes.clone();
    assert_eq!(used, drive.pool.used());
    drive.reader.close();
    assert!(matches!(alias.event(), TraceEvent::Pes(_)));
}

#[test]
fn zero_work_cancel_reset_and_old_completions_preserve_input_identity() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = Reader::new(
        &pool,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        ReaderLimits::default(),
        4096,
    )
    .unwrap();
    let old = reader.request().unwrap();
    let baseline = pool.used();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &OFF).unwrap(),
        Step::Yield
    ));
    assert_eq!(pool.used(), baseline);
    let new = reader.reset().unwrap();
    assert_ne!(old.token, new.token);
    assert!(reader.finish(old.source, old.token).is_err());
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(0), &cancelled)
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(reader.request().unwrap_err(), Error::Cancelled);
    let request = reader.reset().unwrap();
    reader
        .fail(request.source, request.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        reader.step(&mut WorkBudget::new(100), &OFF).unwrap_err(),
        Error::Transport
    );
    reader.reset().unwrap();
    reader.close();
    assert!(reader.request().is_err());
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn a_partial_audio_frame_at_eof_is_an_error_and_is_never_a_packet() {
    let frame = adts(b"incomplete");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame[..frame.len() - 1], Some(0), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 1);
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(_)) | None) => panic!("partial AU certified at EOF"),
            Ok(Some(_)) => (),
            Err(error) => {
                assert_eq!(error, Error::Truncated);
                break;
            }
        }
    }
    assert_eq!(drive.next().unwrap_err(), Error::Truncated);
}

#[test]
fn pmt_change_reports_interrupted_bytes_and_publishes_an_atomic_new_catalogue() {
    let frame = adts(b"whole frame");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame[..4], Some(90_000), None);
    ts.tables(&[(0x0f, PID + 1, &[])], 1);
    ts.pes(PID + 1, 0xc0, &frame, Some(180_000), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 1, 1);
    let mut interrupted = false;
    let mut maps = 0;
    let mut packet = None;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source))
                if matches!(source.event(), TraceEvent::ProgramMap { active: true, .. }) =>
            {
                maps += 1;
                assert_eq!(drive.reader.track_count(), 1);
                assert_eq!(
                    drive.reader.track(0).unwrap().stream().pid,
                    PID + (maps - 1)
                );
            }
            Event::Other(ReaderEvent::Interrupted {
                track,
                elementary_range,
                ..
            }) => {
                assert_eq!(track.stream().pid, PID);
                assert_eq!(elementary_range, 0..4);
                interrupted = true;
            }
            Event::Packet(value) => {
                assert!(interrupted);
                assert!(packet.is_none());
                packet = Some(value);
            }
            _ => (),
        }
    }
    let packet = packet.unwrap();
    assert_eq!(maps, 2);
    assert_eq!(packet.track_info().stream().pid, PID + 1);
    assert_eq!(packet.payload(), frame);
    assert_eq!(packet.pts(), Some(180_000));
    assert!(packet.track_info().configuration_epoch() > 0);
}

#[test]
fn retained_pes_and_au_limits_are_enforced_without_pes_fallback() {
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    let frame = adts(b"split");
    ts.pes(PID, 0xc0, &frame[..3], Some(0), None);
    ts.pes(PID, 0xc0, &frame[3..], None, None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            maximum_pes_per_access_unit: 1,
            ..ReaderLimits::default()
        },
        7,
        1,
    );
    loop {
        match drive.next() {
            Ok(Some(Event::Packet(_)) | None) => panic!("limit bypass"),
            Ok(Some(_)) => (),
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Limit {
                        requested: 2,
                        maximum: 1
                    }
                );
                break;
            }
        }
    }
}

#[test]
fn explicit_dvb_ac3_uses_the_syncframe_reader_without_backfilling_private_identity() {
    use triergon_mpegts::psi::{DescriptorNamespace, DvbNamespaceEvidence};
    let mut frame = [0; 192];
    // Authored AC-3 envelope: 48 kHz, stereo, 48 kbit/s; opaque coded body.
    frame[..8].copy_from_slice(&[0x0b, 0x77, 0, 0, 4, 0x40, 0x40, 0x1f]);
    let mut ts = Transport::new();
    ts.tables(&[(6, PID, &[0x6a, 1, 0])], 0);
    ts.pes(PID, 0xbd, &frame[..17], Some(90_000), None);
    ts.pes(PID, 0xbd, &frame[17..], None, None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            descriptor_namespace: DescriptorNamespace::Dvb(
                DvbNamespaceEvidence::CallerDeclaredTransportProfile,
            ),
            ..ReaderLimits::default()
        },
        1,
        1,
    );
    let mut packets = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    }
    assert_eq!(packets.len(), 1);
    let packet = &packets[0];
    assert!(matches!(packet.access_unit(), AccessUnit::Ac3(_)));
    assert_eq!(packet.payload(), frame);
    assert_eq!(packet.pts(), Some(90_000));
    assert_eq!(packet.dts(), None);
    assert_eq!(packet.duration(), Some(2880));
    assert_eq!(packet.track_info().codec_tag(), [6]);
    assert_eq!(packet.track_info().codec(), CodecId::Other(&[6]));
    assert_eq!(packet.sources().len(), 2);

    let mut opaque = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 7);
    let mut refused = false;
    while let Some(event) = opaque.next().unwrap() {
        match event {
            Event::Packet(_) => panic!("private namespace inferred from a tag"),
            Event::Other(ReaderEvent::Unsupported { .. }) => refused = true,
            _ => (),
        }
    }
    assert!(refused);
}

#[test]
fn fractional_audio_duration_and_unanchored_source_time_remain_explicit() {
    use triergon_mpegts::clock::{SourceTimePolicy, SourceTimeStatus};
    let mut frame = adts(b"44100");
    frame[2] = 0x50;
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pes(PID, 0xc0, &frame, Some((1 << 33) - 1), None);
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            time_policy: SourceTimePolicy::Unanchored,
            ..ReaderLimits::default()
        },
        7,
        1,
    );
    let mut packets = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        if let Event::Packet(packet) = event {
            packets.push(packet);
        }
    }
    assert_eq!(packets.len(), 1);
    let packet = &packets[0];
    assert_eq!(packet.pts(), None);
    assert_eq!(packet.dts(), None);
    assert_eq!(packet.duration(), None);
    assert_eq!(
        packet.sample_duration(),
        Some(Time::new(1024, 44_100).unwrap())
    );
    assert_eq!(packet.clock().unwrap().pts.raw33, Some((1 << 33) - 1));
    assert_eq!(
        packet.clock().unwrap().pts.status,
        SourceTimeStatus::AnchorBoundMissing
    );
}

#[test]
fn clock_and_continuity_boundaries_never_flush_an_interrupted_frame_as_complete() {
    let frame = adts(b"new complete frame");
    for clock_change in [true, false] {
        let mut ts = Transport::new();
        ts.tables(&[(0x0f, PID, &[])], 0);
        ts.pes(PID, 0xc0, &frame[..4], Some(90_000), None);
        if clock_change {
            ts.pcr(180_000, true);
        } else {
            ts.counters[usize::from(PID)] += 2;
        }
        ts.pes(PID, 0xc0, &frame, Some(180_000), None);
        let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 1, 1);
        let mut interrupted = 0;
        let mut packet = None;
        let mut timeline = false;
        while let Some(event) = drive.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Interrupted {
                    elementary_range, ..
                }) => {
                    assert_eq!(elementary_range, 0..4);
                    interrupted += 1;
                }
                Event::Other(ReaderEvent::Source(source))
                    if matches!(source.event(), TraceEvent::Timeline { .. }) =>
                {
                    timeline = true;
                }
                Event::Packet(value) => {
                    assert!(packet.is_none());
                    packet = Some(value);
                }
                _ => (),
            }
        }
        assert_eq!(interrupted, 1);
        assert_eq!(timeline, clock_change);
        let packet = packet.unwrap();
        assert_eq!(packet.payload(), frame);
        let clock = packet.clock().unwrap();
        assert_eq!(clock.pts.raw33, Some(180_000));
        assert_eq!(
            packet.pts(),
            if clock.quarantined {
                None
            } else {
                Some(180_000)
            }
        );
        assert_eq!(packet.dts(), None);
        assert_eq!(packet.epochs().clock, u64::from(clock_change));
        assert_eq!(packet.clock().unwrap().key.epoch, packet.epochs().clock);
    }
}

#[test]
fn quarantined_clock_retains_raw_fields_but_never_uses_a_matching_bound() {
    use triergon_mpegts::clock::{
        ClockKey, SourceClockAnchor, SourceClockSample, SourceTimePolicy, SourceTimeStatus,
    };
    let frame = adts(b"clock quarantine");
    let mut ts = Transport::new();
    ts.tables(&[(0x0f, PID, &[])], 0);
    ts.pcr(0, true);
    ts.pes(PID, 0xc0, &frame[..4], Some(90_000), None);
    // A second discontinuity before a second ordinary PCR sample is invalid.
    ts.pcr(9000, true);
    ts.pes(PID, 0xc0, &frame, Some(180_000), None);
    let key = ClockKey {
        tsid: 9,
        program: PROGRAM,
        pcr_pid: PID,
        epoch: 1,
    };
    let mut drive = Drive::new(
        &ts.bytes,
        ReaderLimits {
            time_policy: SourceTimePolicy::Bounded(SourceClockAnchor {
                key,
                sample: SourceClockSample::Pts90(180_000),
                maximum_distance27: 0,
            }),
            ..ReaderLimits::default()
        },
        1,
        1,
    );
    let mut packet = None;
    let mut fault = false;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Other(ReaderEvent::Source(source))
                if matches!(source.event(), TraceEvent::Diagnostic(d)
                if d.error == Error::Invalid("clock discontinuity before two PCR samples")) =>
            {
                fault = true;
            }
            Event::Packet(value) => {
                assert!(packet.is_none());
                packet = Some(value);
            }
            _ => (),
        }
    }
    assert!(fault);
    let packet = packet.unwrap();
    let clock = packet.clock().unwrap();
    assert!(clock.quarantined);
    assert_eq!(clock.key, key);
    assert_eq!(clock.pts.raw33, Some(180_000));
    assert_eq!(clock.pts.status, SourceTimeStatus::AnchorBoundMissing);
    assert_eq!(packet.pts(), None);
    assert_eq!(packet.dts(), None);
}

#[test]
fn header_only_video_is_retained_outside_the_neutral_packet_stream() {
    let headers = [0, 0, 0, 1, 0x46, 1, 0x50];
    let mut ts = Transport::new();
    ts.tables(&[(0x24, PID, &[])], 0);
    ts.pes(PID, 0xe0, &headers, Some(90_000), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 1, 1);
    let mut retained = None;
    while let Some(event) = drive.next().unwrap() {
        match event {
            Event::Packet(_) => panic!("headers are not a coded picture"),
            Event::Other(ReaderEvent::VideoHeaders(unit)) => {
                assert_eq!(unit.payload(), headers);
                retained = Some(unit);
            }
            _ => (),
        }
    }
    drive.reader.close();
    assert_eq!(retained.unwrap().payload(), headers);
}

#[test]
fn in_band_sps_changes_do_not_advance_the_pmt_source_configuration_epoch() {
    let first = include_bytes!("../../playback/tests/fixtures/raw-video/avc-short.h264");
    let second = include_bytes!("../../playback/tests/fixtures/raw-video/avc-80x48.h264");
    let mut ts = Transport::new();
    ts.tables(&[(0x1b, PID, &[])], 0);
    ts.pes(PID, 0xe0, first, Some(90_000), None);
    ts.pes(PID, 0xe0, second, Some(180_000), None);
    let mut drive = Drive::new(&ts.bytes, ReaderLimits::default(), 7, 1);
    let mut sps = std::collections::BTreeSet::new();
    let mut epoch = None;
    let mut bytes = Vec::new();
    while let Some(event) = drive.next().unwrap() {
        let (Event::Packet(packet) | Event::Other(ReaderEvent::VideoHeaders(packet))) = event
        else {
            continue;
        };
        let config = packet.track_info().configuration_epoch();
        assert_eq!(*epoch.get_or_insert(config), config);
        assert_eq!(packet.epochs().configuration, config);
        assert!(packet.track_info().configuration().is_empty());
        let AccessUnit::Video(unit) = packet.access_unit() else {
            unreachable!()
        };
        for nal in unit.nals() {
            if nal.facts().header.nal_unit_type() == 7 {
                sps.insert(nal.bytes().as_ref().to_vec());
            }
        }
        bytes.extend_from_slice(packet.payload());
    }
    assert!(sps.len() >= 2, "fixtures must change the in-band SPS");
    assert_eq!(bytes, [first.as_slice(), second.as_slice()].concat());
}
