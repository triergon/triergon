//! Structural recovery controls; the external FATE test also checks decoded PCM.
use super::*;
#[derive(Debug)]
struct SeekResult {
    requested: Time,
    landing: Time,
    discard_until: Time,
    method: SeekMethod,
}

#[path = "../../../../xtask/src/sha256.rs"]
mod sha256;

// One 24-bit stereo XLL asset without a core (construction 17, Table E-2).
const DESCRIPTOR: &[u8] = &[0x7f, 8, 0x0e, 0x40, 5, 2, 0x64, 0x8c, 0, 0];

struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn new(bytes: usize) -> Self {
        Self {
            bytes: vec![0; bytes],
            at: 0,
        }
    }
    fn put(&mut self, width: usize, value: u32) {
        for bit in (0..width).rev() {
            self.bytes[self.at / 8] |= ((value >> bit) as u8 & 1) << (7 - self.at % 8);
            self.at += 1;
        }
    }
}

fn crc(bytes: &mut [u8], start: usize, end: usize) {
    let mut value = u16::MAX;
    for &byte in &bytes[start..end - 2] {
        value ^= u16::from(byte) << 8;
        for _ in 0..8 {
            value = (value << 1) ^ if value & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    bytes[end - 2..end].copy_from_slice(&value.to_be_bytes());
}

fn set(bytes: &mut [u8], start: usize, width: usize, value: u32) {
    for (at, bit) in (start..start + width).zip((0..width).rev()) {
        let mask = 1 << (7 - at % 8);
        bytes[at / 8] = (bytes[at / 8] & !mask) | (((value >> bit) as u8 & 1) * mask);
    }
}

// This payload tests framing/recovery metadata, not XLL sample decoding.
// Tables 7-2/5/6/7 and 8-1 independently author its headers.
fn frame(sync: bool, delay: u32, offset: u32, tag: u8) -> Vec<u8> {
    let mut bits = Bits::new(44);
    for (n, value) in [
        (32, 0x6458_2025),
        (8, 0),
        (2, 0),
        (1, 0),
        (8, 27),
        (16, 43),
        (1, 1),
        (2, 2),
        (3, 0),
        (1, 0),
        (3, 0),
        (3, 0),
        (1, 1),
        (8, 1),
        (1, 0),
        (16, 15),
        (9, 11),
        (3, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (5, 23),
        (4, 12),
        (8, 1),
        (1, 1),
        (1, 0),
        (3, 0),
        (1, 0),
        (1, 0),
        (2, 1),
        (16, 15),
        (1, u32::from(sync)),
    ] {
        bits.put(n, value);
    }
    if sync {
        bits.put(4, 0); // No smoothing buffer.
        bits.put(5, 3); // Four bits of initial decoding delay.
        bits.put(4, delay);
        bits.put(16, offset);
    }
    bits.put(3, 0); // DTS-HD stream ID.
    bits.put(2, 0); // Primary decoder; no Rev2 DRC.
    assert!(bits.at <= 202);
    bits.at = 202;
    bits.put(1, 0); // No backward-compatible core.
    crc(&mut bits.bytes, 5, 28);
    bits.at = 28 * 8;
    for (n, value) in [
        (32, 0x41a2_9547),
        (4, 0),
        (8, 11),
        (5, 5),
        (6, 15),
        (4, 0),
        (4, 0),
        (4, 9),
        (5, 7),
        (2, 0),
        (1, 0),
        (5, 1),
    ] {
        bits.put(n, value);
    }
    assert_eq!(bits.at, 38 * 8);
    crc(&mut bits.bytes, 32, 40);
    bits.bytes[40..].fill(tag);
    bits.bytes
}

fn transport(frames: &[(&[u8], Option<u64>, Option<u64>)]) -> Transport {
    transport_with_descriptor(frames, DESCRIPTOR)
}

fn transport_with_descriptor(
    frames: &[(&[u8], Option<u64>, Option<u64>)],
    descriptor: &[u8],
) -> Transport {
    let mut ts = Transport::new();
    ts.tables(&[(0x06, PID, descriptor), (0x1b, PID + 1, &[])], 0);
    for &(frame, pts, dts) in frames {
        let start = ts.bytes.len();
        ts.pes(PID, 0xbd, frame, pts, dts);
        let at = ts.bytes[start..start + 188]
            .windows(4)
            .position(|p| p == [0, 0, 1, 0xbd])
            .unwrap();
        ts.bytes[start + at + 6] |= 4;
    }
    ts
}

fn new_reader(pool: &Budget, bytes: &[u8], format: PacketFormat, selected: bool) -> Reader {
    let caps = SourceCapabilities {
        length: Some(bytes.len() as u64),
        random_access: true,
    };
    let limits = ReaderLimits {
        format,
        descriptor_namespace: audio_eof_namespace(),
        ..ReaderLimits::default()
    };
    if selected {
        Reader::new_with_packet_tracks(pool, ID, caps, limits, 4096, &[TrackId(PID.into())])
            .unwrap()
    } else {
        Reader::new(pool, ID, caps, limits, 4096).unwrap()
    }
}

fn drain(
    reader: &mut Reader,
    pool: &Budget,
    bytes: &[u8],
    chunk: usize,
    grant: u64,
) -> Result<(Option<SeekResult>, Vec<Packet>), Error> {
    let mut result = None;
    let mut packets = Vec::new();
    while let Some(event) = next(reader, pool, bytes, chunk, grant)? {
        match event {
            Event::Seek(value) => {
                assert!(packets.is_empty());
                assert!(
                    result
                        .replace(SeekResult {
                            requested: value.requested,
                            landing: value.landing,
                            discard_until: value.discard_until,
                            method: value.method,
                        })
                        .is_none()
                );
            }
            Event::Packet(packet) => packets.push(packet),
            Event::Other(_) => (),
        }
    }
    Ok((result, packets))
}

fn same_packets(actual: &[Packet], expected: &[Packet]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.payload(), expected.payload());
        assert_eq!(actual.track(), expected.track());
        assert_eq!(actual.elementary_range(), expected.elementary_range());
        assert_eq!(actual.epochs(), expected.epochs());
        assert_eq!(
            (actual.pts(), actual.dts(), actual.duration()),
            (expected.pts(), expected.dts(), expected.duration())
        );
        assert_eq!(actual.sample_duration(), expected.sample_duration());
        match (actual.clock(), expected.clock()) {
            (Some(actual), Some(expected)) => {
                assert_eq!(actual.key, expected.key);
                assert_eq!(actual.quarantined, expected.quarantined);
                assert_eq!(actual.pts, expected.pts);
                assert_eq!(actual.dts, expected.dts);
                same_pes(&actual.pes, &expected.pes);
            }
            (None, None) => (),
            _ => panic!("source clock presence changed"),
        }
        assert_eq!(actual.sources().len(), expected.sources().len());
        for (actual, expected) in actual.sources().iter().zip(expected.sources()) {
            assert_eq!(actual.elementary_range, expected.elementary_range);
            assert_eq!(actual.payload_range, expected.payload_range);
            same_pes(&actual.pes, &expected.pes);
        }
    }
}

fn same_pes(actual: &triergon_mpegts::SourceEvent, expected: &triergon_mpegts::SourceEvent) {
    let (TraceEvent::Pes(actual), TraceEvent::Pes(expected)) = (actual.event(), expected.event())
    else {
        panic!("expected original PES");
    };
    assert_eq!(actual.pid, expected.pid);
    assert_eq!(actual.header.as_bytes(), expected.header.as_bytes());
    assert_eq!(actual.header.stream_id, expected.header.stream_id);
    assert_eq!(
        actual.header.declared_packet_length,
        expected.header.declared_packet_length
    );
    assert_eq!(actual.header.pts_raw33, expected.header.pts_raw33);
    assert_eq!(actual.header.dts_raw33, expected.header.dts_raw33);
    assert_eq!(
        actual.header.data_alignment_indicator,
        expected.header.data_alignment_indicator
    );
    assert_eq!(actual.header.priority, expected.header.priority);
    assert_eq!(actual.epochs, expected.epochs);
    assert_eq!(actual.elementary_offset, expected.elementary_offset);
    assert_eq!(actual.byte_run_epoch, expected.byte_run_epoch);
    assert_eq!(actual.payload_len, expected.payload_len);
    assert_eq!(
        actual.header_source_ranges.as_slice(),
        expected.header_source_ranges.as_slice()
    );
    assert_eq!(actual.spans().len(), expected.spans().len());
    for (actual, expected) in actual.spans().iter().zip(expected.spans()) {
        assert_eq!(actual.source_range(), expected.source_range());
        assert_eq!(actual.as_ref(), expected.as_ref());
    }
}

#[test]
fn previous_xll_entry_preserves_original_suffix_and_preroll() {
    let first = frame(true, 0, 0, 1);
    let dependent = frame(false, 0, 0, 2);
    let last = frame(true, 0, 0, 3);
    let untimed = frame(true, 0, 0, 4);
    let ts = transport(&[
        (&first, Some(90_000), None),
        (&dependent, Some(90_960), Some(90_800)),
        (&last, Some(91_920), None),
        (&untimed, None, None),
    ]);
    for (format, selected, chunk, grant) in [
        (PacketFormat::Ts188, true, 7, 1),
        (PacketFormat::M2ts192, false, 4096, 8192),
    ] {
        let m2ts = ts.m2ts();
        let bytes = if format == PacketFormat::Ts188 {
            &ts.bytes
        } else {
            &m2ts
        };
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = new_reader(&pool, bytes, format, selected);
        let (_, sequential) = drain(&mut reader, &pool, bytes, chunk, grant).unwrap();
        assert_eq!(sequential.len(), 4);
        reader.close();
        drop(reader);
        let mut reader = new_reader(&pool, bytes, format, selected);
        for (target, policy, start) in [
            (91_500, SeekPolicy::PlaybackPreroll, 0),
            (100_000, SeekPolicy::PreviousRandomAccess, 2),
            (91_000, SeekPolicy::PreviousRandomAccess, 0),
        ] {
            let request = SeekRequest {
                track: TrackId(PID.into()),
                target: clock(target),
                policy,
            };
            reader.request_seek(request).unwrap();
            let (result, actual) = drain(&mut reader, &pool, bytes, chunk, grant).unwrap();
            let result = result.unwrap();
            assert_eq!(
                result.landing,
                clock(if start == 0 { 90_000 } else { 91_920 })
            );
            assert_eq!(
                result.discard_until,
                if policy == SeekPolicy::PlaybackPreroll {
                    request.target
                } else {
                    result.landing
                }
            );
            same_packets(&actual, &sequential[start..]);
        }
        reader.close();
        drop(sequential);
        drop(reader);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn tracked_xll_inputs_preserve_both_landings_policies_and_indexed_suffixes() {
    for (bytes, format, frame_bytes) in [
        (
            include_bytes!("../../../../harness/features/mpegts-dts-seek/inputs/dts-xll-short.ts")
                .as_slice(),
            PacketFormat::Ts188,
            428,
        ),
        (
            include_bytes!("../../../../harness/features/mpegts-dts-seek/inputs/dts-xll-long.m2ts")
                .as_slice(),
            PacketFormat::M2ts192,
            432,
        ),
    ] {
        let pool = Budget::new(32 << 20).unwrap();
        let mut reader = new_reader(&pool, bytes, format, true);
        let (_, sequential) = drain(&mut reader, &pool, bytes, 7, 1).unwrap();
        assert_eq!(sequential.len(), 4);
        for (index, packet) in sequential.iter().enumerate() {
            let pts = 90_000 + index as i64 * 960;
            assert_eq!(packet.pts(), Some(pts));
            assert_eq!(packet.dts(), (index % 2 == 0).then_some(pts - 120));
            assert_eq!(packet.payload().len(), frame_bytes);
            assert!(matches!(packet.access_unit(), AccessUnit::DtsExtension(_)));
            let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
                panic!("expected source PES");
            };
            assert!(pes.header.data_alignment_indicator);
            assert_eq!(pes.payload_len, frame_bytes);
        }
        reader.close();
        drop(reader);
        for policy in [
            SeekPolicy::PreviousRandomAccess,
            SeekPolicy::PlaybackPreroll,
        ] {
            let mut reader = new_reader(&pool, bytes, format, true);
            for (ordinal, (target, start)) in [(92_500, 2), (90_500, 0), (92_500, 2)]
                .into_iter()
                .enumerate()
            {
                let request = SeekRequest {
                    track: TrackId(PID.into()),
                    target: clock(target),
                    policy,
                };
                if ordinal == 0 {
                    reader.request_seek_and_index(request, 8).unwrap();
                } else {
                    reader.request_seek(request).unwrap();
                }
                let (result, actual) = drain(&mut reader, &pool, bytes, 7, 1).unwrap();
                let result = result.unwrap();
                assert_eq!(result.requested, request.target);
                assert_eq!(result.landing, clock(90_000 + start as u64 * 960));
                assert_eq!(
                    result.discard_until,
                    if policy == SeekPolicy::PlaybackPreroll {
                        request.target
                    } else {
                        result.landing
                    }
                );
                assert_eq!(
                    result.method,
                    if ordinal == 0 {
                        SeekMethod::BoundedScan
                    } else {
                        SeekMethod::Index
                    }
                );
                same_packets(&actual, &sequential[start..]);
            }
            reader.close();
        }
        drop(sequential);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn xll_needs_sync_without_delay_or_partial_frame_and_a_source_pts() {
    for (sync, delay, offset, pts) in [
        (false, 0, 0, Some(90_000)),
        (true, 3, 0, Some(90_000)),
        (true, 0, 1, Some(90_000)),
        (true, 0, 0, None),
        (true, 0, 0, Some(91_000)),
    ] {
        let frame = frame(sync, delay, offset, 0);
        let ts = transport(&[(&frame, pts, None)]);
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(90_500),
                policy: SeekPolicy::PlaybackPreroll,
            })
            .unwrap();
        assert!(matches!(
            drain(&mut reader, &pool, &ts.bytes, 7, 1),
            Err(Error::Unsupported(
                "MPEG-TS has no timestamped audio frame at or before the target"
            ))
        ));
        reader.close();
        drop(reader);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn xll_header_crc_is_required_before_the_seek_result() {
    let mut frame = frame(true, 0, 0, 0);
    frame[38] ^= 1;
    let ts = transport(&[(&frame, Some(90_000), None)]);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(90_000),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    assert!(matches!(
        drain(&mut reader, &pool, &ts.bytes, 7, 1),
        Err(Error::Invalid("DTS-HD seek XLL header CRC"))
    ));
}

#[test]
fn inherited_setup_and_smoothing_do_not_establish_an_xll_entry() {
    for (start, width, value) in [(67, 1, 0), (164, 4, 1)] {
        let mut frame = frame(true, 0, 0, 0);
        set(&mut frame, start, width, value);
        crc(&mut frame, 5, 28);
        let ts = transport(&[(&frame, Some(90_000), None)]);
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(90_000),
                policy: SeekPolicy::PreviousRandomAccess,
            })
            .unwrap();
        assert!(matches!(
            drain(&mut reader, &pool, &ts.bytes, 7, 1),
            Err(Error::Unsupported(
                "MPEG-TS has no timestamped audio frame at or before the target"
            ))
        ));
    }
}

#[test]
fn xll_cannot_reinterpret_another_advertised_construction() {
    let frame = frame(true, 0, 0, 0);
    for construction in [0, 14, 18] {
        let mut descriptor = DESCRIPTOR.to_vec();
        descriptor[7] = (construction << 3) | 4;
        let ts = transport_with_descriptor(&[(&frame, Some(90_000), None)], &descriptor);
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(90_000),
                policy: SeekPolicy::PreviousRandomAccess,
            })
            .unwrap();
        assert!(matches!(
            drain(&mut reader, &pool, &ts.bytes, 7, 1),
            Err(Error::Unsupported("MPEG-TS DTS-HD seek recovery layout"))
        ));
    }
}

#[test]
fn valid_checksums_do_not_make_misaligned_xll_sync_an_entry() {
    let mut frame = frame(true, 0, 0, 0);
    // Extend the extension header by one byte, preserving both complete
    // checksummed headers and all sixteen XLL bytes at offset 29.
    frame.insert(28, 0);
    set(&mut frame, 43, 8, 28);
    set(&mut frame, 51, 16, 44);
    crc(&mut frame, 5, 29);
    assert_eq!(&frame[29..33], &[0x41, 0xa2, 0x95, 0x47]);
    let ts = transport(&[(&frame, Some(90_000), None)]);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(90_000),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    assert!(matches!(
        drain(&mut reader, &pool, &ts.bytes, 7, 1),
        Err(Error::Unsupported("MPEG-TS DTS-HD seek recovery layout"))
    ));
}

#[test]
fn xll_refuses_unsupported_asset_navigation_and_invalid_common_headers() {
    for (start, width, value, expected) in [
        (74, 3, 1, "layout"),      // More than one presentation.
        (77, 3, 1, "layout"),      // More than one asset.
        (81, 8, 2, "layout"),      // The only asset is not active.
        (89, 1, 1, "layout"),      // Mixer dependencies.
        (90, 16, 14, "layout"),    // Asset extent excludes payload bytes.
        (106, 9, 0, "descriptor"), // Descriptor does not contain its navigation.
        (115, 3, 1, "layout"),     // Wrong asset index.
        (138, 1, 0, "layout"),     // Non-speaker representation.
        (140, 3, 1, "layout"),     // Speaker remapping tables.
        (145, 2, 0, "layout"),     // A CBR component may be required.
        (147, 16, 14, "layout"),   // XLL does not cover the whole asset.
        (196, 1, 1, "layout"),     // Secondary decoder.
        (197, 1, 1, "layout"),     // Rev2 DRC metadata.
        (202, 1, 1, "layout"),     // Backward-compatible core.
        (224, 32, 0, "sync"),
        (256, 4, 1, "layout"), // Unsupported XLL version.
        (260, 8, 0, "header"), // Common header shorter than its fields and CRC.
        (273, 6, 14, "frame"), // Partial or multiple lossless frames.
    ] {
        let mut frame = frame(true, 0, 0, 0);
        set(&mut frame, start, width, value);
        crc(&mut frame, 5, 28);
        let ts = transport(&[(&frame, Some(90_000), None)]);
        let pool = Budget::new(1 << 20).unwrap();
        let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
        reader
            .request_seek(SeekRequest {
                track: TrackId(PID.into()),
                target: clock(90_000),
                policy: SeekPolicy::PreviousRandomAccess,
            })
            .unwrap();
        let error = drain(&mut reader, &pool, &ts.bytes, 7, 1).unwrap_err();
        let expected = match expected {
            "layout" => Error::Unsupported("MPEG-TS DTS-HD seek recovery layout"),
            "descriptor" => Error::Invalid("DTS-HD seek asset descriptor extent"),
            "sync" => Error::Invalid("DTS-HD seek XLL sync"),
            "header" => Error::Invalid("DTS-HD seek XLL header extent"),
            "frame" => Error::Invalid("DTS-HD seek XLL frame extent"),
            _ => unreachable!(),
        };
        assert_eq!(error, expected, "field at bit {start}");
    }
}

#[test]
fn core_history_alone_cannot_establish_dts_hd_recovery() {
    let mut core =
        include_bytes!("../../../../harness/fixtures/mpegts/dts-core-mono.dts")[..512].to_vec();
    // TS 102 114 Table 5-1 places HFLAG at bit 87 of the core header.
    set(&mut core, 87, 1, 0);
    assert_eq!(
        triergon_framing::probe_dts_core_header(&core)
            .unwrap()
            .history,
        0
    );
    let extension = frame(true, 0, 0, 0);
    let payload = [core.as_slice(), &extension].concat();
    let descriptor = &[
        0x7f, 14, 0x0e, 0xc0, 5, 1, 0x60, 0x08, 0, 0, 5, 2, 0x64, 0x74, 0, 0,
    ];
    let mut ts = transport(&[(&payload, Some(90_000), None)]);
    let mut tables = Transport::new();
    tables.tables(&[(0x06, PID, descriptor)], 0);
    ts.bytes.splice(..376, tables.bytes);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    reader
        .request_seek(SeekRequest {
            track: TrackId(PID.into()),
            target: clock(90_000),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    assert!(matches!(
        drain(&mut reader, &pool, &ts.bytes, 7, 1),
        Err(Error::Unsupported("MPEG-TS DTS-HD seek"))
    ));
}

#[test]
fn xll_index_keeps_the_last_entry_on_a_tied_timestamp() {
    let first = frame(true, 0, 0, 1);
    let tied = frame(true, 0, 0, 2);
    let last = frame(true, 0, 0, 3);
    let ts = transport(&[
        (&first, Some(90_000), None),
        (&tied, Some(90_000), None),
        (&last, Some(91_920), None),
    ]);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    let request = SeekRequest {
        track: TrackId(PID.into()),
        target: clock(90_500),
        policy: SeekPolicy::PlaybackPreroll,
    };
    reader.request_seek_and_index(request, 8).unwrap();
    for indexed in [false, true] {
        if indexed {
            reader.request_seek(request).unwrap();
        }
        let (result, packets) = drain(&mut reader, &pool, &ts.bytes, 7, 1).unwrap();
        assert_eq!(
            result.unwrap().method,
            if indexed {
                SeekMethod::Index
            } else {
                SeekMethod::BoundedScan
            }
        );
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].payload(), tied);
    }
}

#[test]
#[ignore = "requires the hash-pinned FATE dts/dcadec-suite/xll_51_24_48_none.dtshd and ffmpeg"]
fn fate_lossless_only_entries_decode_the_exact_pcm_suffix() {
    let path = std::env::var_os("TRIERGON_DTS_HD_XLL").expect("TRIERGON_DTS_HD_XLL");
    assert_eq!(
        sha256::file_hex(std::path::Path::new(&path)).unwrap(),
        "c445bb746c6afc920761b55b00932053d64a96d3d08cd9cad4c759a892fae4cd"
    );
    let raw = std::fs::read(path).unwrap();
    assert_eq!(
        triergon_harness_protocol::md5::digest(&raw),
        [
            0xd1, 0xe1, 0x79, 0xb7, 0xd0, 0x68, 0x51, 0xe8, 0x8e, 0xc6, 0x3f, 0x81, 0xe0, 0xd9,
            0x21, 0x13,
        ]
    );
    // The pinned DTS-HD file's STRMDATA contains three complete extension frames.
    assert_eq!(&raw[96..104], b"STRMDATA");
    let payload = &raw[112..34_276];
    let frames = [&payload[..68], &payload[68..17_108], &payload[17_108..]];
    let ts = transport_with_descriptor(
        &[
            (frames[0], Some(90_000), None),
            (frames[1], Some(91_920), Some(91_900)),
            (frames[2], Some(93_840), None),
        ],
        &[0x7f, 8, 0x0e, 0x40, 5, 5, 0xe4, 0x8c, 0, 0],
    );
    let pool = Budget::new(32 << 20).unwrap();
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    let (_, sequential) = drain(&mut reader, &pool, &ts.bytes, 4096, 8192).unwrap();
    assert_eq!(sequential.len(), 3);
    reader.close();
    drop(reader);
    let mut reader = new_reader(&pool, &ts.bytes, PacketFormat::Ts188, true);
    let decode = |packets: &[Packet]| {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let ffmpeg = std::env::var_os("TRIERGON_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
        let mut child = Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-xerror",
                "-f",
                "dts",
                "-i",
                "pipe:0",
                "-map",
                "0:a:0",
                "-c:a",
                "pcm_s32le",
                "-f",
                "s32le",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Write input and drain output concurrently so neither pipe can block the other.
        let mut stdin = child.stdin.take().unwrap();
        let bytes: Vec<u8> = packets
            .iter()
            .flat_map(|p| p.payload().iter().copied())
            .collect();
        let writer = std::thread::spawn(move || {
            stdin.write_all(&bytes).unwrap();
        });
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let all_pcm = decode(&sequential);
    assert_eq!(all_pcm.len(), 3 * 1024 * 6 * 4);
    for (ordinal, index) in [1, 2, 0].into_iter().enumerate() {
        let request = SeekRequest {
            track: TrackId(PID.into()),
            target: clock(90_000 + index as u64 * 1920 + 100),
            policy: SeekPolicy::PlaybackPreroll,
        };
        if ordinal == 0 {
            reader.request_seek_and_index(request, 8).unwrap();
        } else {
            reader.request_seek(request).unwrap();
        }
        let (result, packets) = drain(&mut reader, &pool, &ts.bytes, 7, 1).unwrap();
        assert_eq!(
            result.unwrap().method,
            if ordinal == 0 {
                SeekMethod::BoundedScan
            } else {
                SeekMethod::Index
            }
        );
        same_packets(&packets, &sequential[index..]);
        assert_eq!(decode(&packets), all_pcm[index * 1024 * 6 * 4..]);
    }
}
