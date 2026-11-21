use super::*;
use triergon_framing::nal::{AnnexBUnits, Codec, NalHeader};

// Real coded first pictures, with SEI omitted explicitly. The direct profile
// carries only its local parameter chain and one independent coded picture.
fn picture(kind: u8, aud: bool) -> Vec<u8> {
    let (codec, raw, count) = if kind == 0x1b {
        (
            Codec::Avc,
            &include_bytes!("../../../framing/fixtures/raw-annex-b/avc-aud0.h264")[..2045],
            4,
        )
    } else {
        (
            Codec::Hevc,
            &include_bytes!("../../../framing/fixtures/raw-annex-b/hevc-aud0.hevc")[..4295],
            4,
        )
    };
    let mut bytes = Vec::new();
    if aud {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(if kind == 0x1b {
            &[9, 0xf0]
        } else {
            &[70, 1, 0x10]
        });
    }
    let mut copied = 0;
    for nal in AnnexBUnits::new(raw) {
        let tag = NalHeader::parse(codec, nal).unwrap().nal_unit_type();
        if matches!((kind, tag), (0x1b, 5 | 7 | 8) | (0x24, 20 | 32..=34)) {
            bytes.extend_from_slice(if copied % 2 == 0 {
                &[0, 0, 0, 1]
            } else {
                &[0, 0, 1]
            });
            bytes.extend_from_slice(nal);
            copied += 1;
        }
    }
    assert_eq!(copied, count);
    bytes
}

fn pes_header(bytes: &[u8], carrier: usize) -> usize {
    carrier
        + if bytes[carrier + 3] & 0x20 == 0 {
            4
        } else {
            5 + usize::from(bytes[carrier + 4])
        }
}

fn input_video(kind: u8, repeat: bool, body: impl Fn(usize) -> Vec<u8>) -> (Transport, Vec<u64>) {
    let mut ts = Transport::new();
    for _ in 0..17 {
        ts.payload(0x1fff, &[0xff; 184]);
    }
    let mut offsets = Vec::new();
    for index in 0..FRAMES {
        offsets.push(ts.bytes.len() as u64);
        if index == 0 || repeat {
            pat(&mut ts);
            ts.tables(&[(kind, PID, &[])], 0);
        }
        let carrier = ts.bytes.len();
        ts.pes(
            PID,
            0xe0,
            &body(index),
            Some(90_000 + index as u64 * 2160),
            (index % 2 != 0).then_some(85_000 + index as u64 * 2160),
        );
        let header = pes_header(&ts.bytes, carrier);
        ts.bytes[header + 6] |= 4; // PES data_alignment_indicator, verified against the AU too.
    }
    (ts, offsets)
}

#[test]
fn independent_video_reads_no_prefix_and_preserves_the_complete_suffix() {
    for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
        for kind in [0x1b, 0x24] {
            for (chunk, grant, aud) in [(1, 1, false), (7, 43, true), (4096, 8192, false)] {
                let (mut ts, mut offsets) = input_video(kind, true, |_| picture(kind, aud));
                assert!(offsets[1] - offsets[0] > 3 * 188, "multi-carrier PES");
                if format == PacketFormat::M2ts192 {
                    ts.bytes = ts.m2ts();
                    for offset in &mut offsets {
                        *offset = *offset / 188 * 192;
                    }
                }
                let limits = ReaderLimits {
                    format,
                    ..ReaderLimits::default()
                };
                let mut sequential = Source::new(&ts.bytes, chunk, grant, limits);
                let (_, expected) = sequential.suffix().unwrap();
                assert_eq!(expected.len(), FRAMES);
                let mut source = Source::new(&ts.bytes, chunk, grant, limits);
                source
                    .reader
                    .request_seek_and_index(request(3), FRAMES)
                    .unwrap();
                assert_eq!(source.suffix().unwrap().1, expected[3..]);
                for index in [7, 1, 4, 4, 9] {
                    source.reads.clear();
                    source.floor = offsets[index];
                    let start = source.reader.request_seek(request(index)).unwrap();
                    assert_eq!(start.offset, offsets[index], "kind {kind:x}");
                    let (result, actual) = source.suffix().unwrap();
                    let result = result.unwrap();
                    assert_eq!(result.method, SeekMethod::Index);
                    assert_eq!(result.scanned_bytes, 0);
                    assert!(!result.replays_from_start);
                    assert_eq!(actual, expected[index..]);
                    assert_eq!(source.reads.first().unwrap().start, offsets[index]);
                    assert_eq!(
                        source.reads.iter().map(|r| r.end - r.start).sum::<u64>(),
                        ts.bytes.len() as u64 - offsets[index]
                    );
                }
                source.reader.close();
                assert_eq!(source.pool.used(), Budget::control_storage_bytes());
            }
        }
    }
}

#[test]
fn every_video_checkpoint_carrier_is_verified_before_any_landing() {
    for kind in [0x1b, 0x24] {
        let (ts, offsets) = input_video(kind, true, |_| picture(kind, false));
        let carriers = (offsets[8] - offsets[7]) as usize / 188;
        for index in 0..carriers {
            let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
            source
                .reader
                .request_seek_and_index(request(3), FRAMES)
                .unwrap();
            source.suffix().unwrap();
            let start = source.reader.request_seek(request(7)).unwrap();
            assert_eq!(start.offset, offsets[7]);
            let mut changed = ts.bytes.clone();
            changed[start.offset as usize + index * 188 + 3] ^= 1;
            source.bytes = &changed;
            source.floor = start.offset;
            loop {
                match source.next() {
                    Ok(Some(Event::Other(_))) => (),
                    Err(Error::SourceMismatch) => break,
                    event => panic!("changed carrier {index} published {event:?}"),
                }
            }
            assert!(matches!(
                source.reader.request(),
                Err(Error::SourceMismatch)
            ));
        }
    }
}

fn replay_only(ts: &Transport) {
    let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(3), FRAMES)
        .unwrap();
    let expected = source.suffix().unwrap().1;
    assert_eq!(source.reader.request_seek(request(3)).unwrap().offset, 0);
    source.reads.clear();
    let (result, actual) = source.suffix().unwrap();
    assert_eq!(result.unwrap().method, SeekMethod::Index);
    assert_eq!(source.reads.first().unwrap().start, 0);
    assert_eq!(actual, expected);
}

#[test]
fn missing_local_tables_or_setup_and_unaligned_video_keep_replay() {
    for kind in [0x1b, 0x24] {
        let (ts, _) = input_video(kind, false, |_| picture(kind, false));
        replay_only(&ts);
        let (ts, _) = input_video(kind, true, |index| {
            let bytes = picture(kind, false);
            if index == 0 {
                return bytes;
            }
            let mut slices = Vec::new();
            for nal in AnnexBUnits::new(&bytes) {
                if matches!((kind, nal[0]), (0x1b, 0x65) | (0x24, 0x28)) {
                    slices.extend_from_slice(&[0, 0, 0, 1]);
                    slices.extend_from_slice(nal);
                }
            }
            assert!(!slices.is_empty());
            slices
        });
        replay_only(&ts);
        let (mut ts, offsets) = input_video(kind, true, |_| picture(kind, false));
        let header = pes_header(&ts.bytes, offsets[6] as usize + 376);
        ts.bytes[header + 6] &= !4;
        replay_only(&ts);
    }
}

#[test]
fn hevc_irap_flag_alone_does_not_supply_independence() {
    for tag in [19, 21] {
        // IDR_W_RADL and CRA keep their ordinary coded key fact.
        let (ts, _) = input_video(0x24, true, |_| {
            let mut bytes = picture(0x24, false);
            let at = bytes.len() - AnnexBUnits::new(&bytes).last().unwrap().len();
            bytes[at] = tag << 1;
            bytes
        });
        replay_only(&ts);
    }
}

#[test]
fn video_with_extra_state_or_unproved_extents_keeps_replay() {
    for kind in [0x1b, 0x24] {
        let (ts, _) = input_video(kind, true, |_| {
            let mut bytes = vec![0, 0, 0, 1];
            bytes.extend_from_slice(if kind == 0x1b {
                &[6, 0x80]
            } else {
                &[78, 1, 0x80]
            });
            bytes.extend_from_slice(&picture(kind, false));
            bytes
        });
        replay_only(&ts);
        let (ts, offsets) = input_video(kind, true, |_| {
            let mut bytes = picture(kind, false);
            // Opaque slice data stays frameable but exceeds the restart proof's
            // fixed carrier history. It must retain ordinary prefix replay.
            bytes.extend_from_slice(&[0x55; 6000]);
            bytes
        });
        assert!(offsets[1] - offsets[0] > 34 * 188);
        replay_only(&ts);
    }
    let (ts, _) = input_video(0x24, true, |_| {
        let mut bytes = picture(0x24, false);
        bytes[6] ^= 0x10; // Local VPS ID no longer matches the SPS reference.
        bytes
    });
    replay_only(&ts);
}

#[test]
fn direct_video_preserves_bounded_anchored_clocks_and_output_limits() {
    for kind in [0x1b, 0x24] {
        let (ts, offsets) = input_video(kind, true, |_| picture(kind, true));
        let limits = ReaderLimits {
            maximum_access_units: FRAMES,
            video: triergon_framing::nal::RawAnnexBLimits {
                nal: triergon_framing::nal::ReaderLimits {
                    maximum_nal_units: FRAMES * 5,
                    ..triergon_framing::nal::RawAnnexBLimits::default().nal
                },
                ..triergon_framing::nal::RawAnnexBLimits::default()
            },
            time_policy: SourceTimePolicy::Bounded(SourceClockAnchor {
                key: triergon_mpegts::clock::ClockKey {
                    tsid: 9,
                    program: PROGRAM,
                    pcr_pid: PID,
                    epoch: 0,
                },
                sample: SourceClockSample::Pts90((1_i128 << 33) + 90_000),
                maximum_distance27: 20_000_000,
            }),
            ..ReaderLimits::default()
        };
        let mut sequential = Source::new(&ts.bytes, 4096, 8192, limits);
        let expected = sequential.suffix().unwrap().1;
        let mut source = Source::new(&ts.bytes, 7, 1, limits);
        let mut seek = request(5);
        seek.target = Time::new((1_i128 << 33) + 90_001 + 5 * 2160, 90_000).unwrap();
        source.reader.request_seek_and_index(seek, FRAMES).unwrap();
        source.suffix().unwrap();
        assert_eq!(source.reader.request_seek(seek).unwrap().offset, offsets[5]);
        source.floor = offsets[5];
        assert_eq!(source.suffix().unwrap().1, expected[5..]);
    }
}

#[test]
fn video_checkpoint_pressure_restarts_safe_replay_with_original_facts() {
    for kind in [0x1b, 0x24] {
        let (ts, offsets) = input_video(kind, true, |_| picture(kind, false));
        let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
        source
            .reader
            .request_seek_and_index(request(6), FRAMES)
            .unwrap();
        let expected = source.suffix().unwrap().1;
        let start = source.reader.request_seek(request(6)).unwrap();
        assert_eq!(start.offset, offsets[6]);
        let end = ts.bytes.len().min(start.offset as usize + start.maximum);
        source
            .reader
            .provide(
                &InputWindow::new(
                    ID,
                    start.token.clone(),
                    start.offset,
                    Bytes::copy_from_slice(&source.pool, &ts.bytes[start.offset as usize..end])
                        .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let occupied = source
            .pool
            .reserve(source.pool.limit() - source.pool.used())
            .unwrap();
        let replay = loop {
            match source
                .reader
                .step(&mut WorkBudget::new(8192), &OFF)
                .unwrap()
            {
                Step::Yield | Step::Ready(Event::Other(_)) => (),
                Step::NeedInput(input) => break input,
                event => panic!("expected replay input: {event:?}"),
            }
        };
        assert_eq!(replay.offset, 0);
        assert_ne!(replay.token.generation(), start.token.generation());
        drop(occupied);
        source.reads.clear();
        assert_eq!(source.suffix().unwrap().1, expected);
        assert_eq!(source.reads.first().unwrap().start, 0);
        source.reader.close();
        assert_eq!(source.pool.used(), Budget::control_storage_bytes());
    }
}
