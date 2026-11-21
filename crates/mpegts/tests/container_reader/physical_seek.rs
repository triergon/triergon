//! Authored physical offsets are independent of the reader's elementary view.
use super::*;
use std::ops::Range;
use triergon_core::container::{SeekMethod, SeekPolicy, SeekRequest};
use triergon_mpegts::clock::{SourceClockAnchor, SourceClockSample, SourceTimePolicy};

#[path = "physical_seek_video.rs"]
mod video;

const FRAMES: usize = 10;
const FRAME_BYTES: usize = 96;

fn pat(ts: &mut Transport) {
    ts.section(
        0,
        vec![0, 0xb0, 13, 0, 9, 0xc1, 0, 0, 0, PROGRAM as u8, 0xf0, 0],
    );
}

fn input(repeat: bool) -> (Transport, Vec<u64>) {
    let mut frame = [0; FRAME_BYTES];
    frame[..4].copy_from_slice(&[0xff, 0xfd, 0x14, 0xc0]); // MPEG-1 Layer II, 32 kb/s, 48 kHz.
    input_frames(repeat, &frame)
}

fn input_frames(repeat: bool, frame: &[u8]) -> (Transport, Vec<u64>) {
    input_timed_frames(repeat, frame, |index| Some(90_000 + index as u64 * 2160))
}

fn input_timed_frames(
    repeat: bool,
    frame: &[u8],
    pts: impl Fn(usize) -> Option<u64>,
) -> (Transport, Vec<u64>) {
    let mut ts = Transport::new();
    // Physical prefix has no elementary coordinates. It must not be read on
    // the indexed direct path, even with a single-byte input window.
    for _ in 0..17 {
        ts.payload(0x1fff, &[0xff; 184]);
    }
    let mut offsets = Vec::new();
    for index in 0..FRAMES {
        offsets.push(ts.bytes.len() as u64);
        if index == 0 || repeat {
            pat(&mut ts);
            ts.tables(&[(0x03, PID, &[])], 0);
        }
        ts.pes(
            PID,
            0xc0,
            frame,
            pts(index),
            pts(index).filter(|_| index % 2 != 0),
        );
    }
    (ts, offsets)
}

fn request(index: usize) -> SeekRequest {
    SeekRequest {
        track: TrackId(PID.into()),
        target: Time::new(90_001 + index as i128 * 2160, 90_000).unwrap(),
        policy: SeekPolicy::PlaybackPreroll,
    }
}

struct Source<'a> {
    reader: Reader,
    pool: Budget,
    bytes: &'a [u8],
    chunk: usize,
    grant: u64,
    floor: u64,
    reads: Vec<Range<u64>>,
}

struct ObservedSeek {
    method: SeekMethod,
    scanned_bytes: u64,
    replays_from_start: bool,
}

impl<'a> Source<'a> {
    fn new(bytes: &'a [u8], chunk: usize, grant: u64, limits: ReaderLimits) -> Self {
        let pool = Budget::new(2 << 20).unwrap();
        let reader = Reader::new(
            &pool,
            ID,
            SourceCapabilities {
                length: Some(bytes.len() as u64),
                random_access: true,
            },
            limits,
            4096,
        )
        .unwrap();
        Self {
            reader,
            pool,
            bytes,
            chunk,
            grant,
            floor: 0,
            reads: Vec::new(),
        }
    }

    fn next(&mut self) -> Result<Option<Event<Packet, ReaderEvent>>, Error> {
        for _ in 0..1_000_000 {
            let mut work = WorkBudget::new(self.grant);
            let step = self.reader.step(&mut work, &OFF)?;
            assert!(work.spent() <= self.grant);
            match step {
                Step::Ready(event) => return Ok(Some(event)),
                Step::End(end) => {
                    assert_eq!(end.offset, self.bytes.len() as u64);
                    return Ok(None);
                }
                Step::Yield => (),
                Step::NeedInput(r) => {
                    assert!(
                        r.offset >= self.floor,
                        "read prefix at {} below {}",
                        r.offset,
                        self.floor
                    );
                    let at = usize::try_from(r.offset).unwrap();
                    if at == self.bytes.len() {
                        self.reader.finish(ID, r.token)?;
                    } else {
                        let end = self.bytes.len().min(at + r.maximum.min(self.chunk));
                        self.reads.push(r.offset..end as u64);
                        self.reader.provide(&InputWindow::new(
                            ID,
                            r.token,
                            r.offset,
                            Bytes::copy_from_slice(&self.pool, &self.bytes[at..end])?,
                        )?)?;
                    }
                }
                Step::Backpressure(value) => panic!("unexpected pressure: {value:?}"),
            }
        }
        panic!("physical seek did not progress");
    }

    fn suffix(&mut self) -> Result<(Option<ObservedSeek>, Vec<String>), Error> {
        let mut result = None;
        let mut packets = Vec::new();
        while let Some(event) = self.next()? {
            match event {
                Event::Seek(value) => assert!(
                    result
                        .replace(ObservedSeek {
                            method: value.method,
                            scanned_bytes: value.scanned_bytes,
                            replays_from_start: value.replays_from_start,
                        })
                        .is_none()
                ),
                Event::Packet(packet) => packets.push(facts(&packet)),
                Event::Other(_) => (),
            }
        }
        Ok((result, packets))
    }
}

// Compare public source facts, including the exact rationals, the raw/missing
// clocks, original table locations, PES headers and all physical payload ranges.
fn facts(packet: &Packet) -> String {
    let clock = packet.clock().unwrap();
    let map = packet.track_info().program_map();
    let origins: Vec<_> = packet
        .sources()
        .iter()
        .map(|origin| {
            let TraceEvent::Pes(pes) = origin.pes.event() else {
                panic!("source PES")
            };
            (
                origin.elementary_range.clone(),
                origin.payload_range.clone(),
                pes.elementary_offset,
                pes.byte_run_epoch,
                pes.epochs,
                pes.header.as_bytes().to_vec(),
                pes.header_source_ranges.as_slice().to_vec(),
                pes.source_ranges().collect::<Vec<_>>(),
            )
        })
        .collect();
    let video = match packet.access_unit() {
        AccessUnit::Video(unit) => Some((
            unit.source,
            unit.codec,
            unit.range.clone(),
            unit.has_vcl,
            unit.random_access,
            unit.nals()
                .iter()
                .map(triergon_framing::nal::NalUnit::facts)
                .collect::<Vec<_>>(),
        )),
        _ => None,
    };
    format!(
        "{:?}",
        (
            packet.payload(),
            packet.elementary_range(),
            packet.pts(),
            packet.dts(),
            packet.sample_duration(),
            packet.epochs(),
            clock.key,
            clock.pts,
            clock.dts,
            (
                map.source.as_ref(),
                map.source_ranges.as_slice(),
                packet.track_info().configuration_epoch()
            ),
            origins,
            (
                packet.random_access(),
                packet.track_info().configuration(),
                clock.quarantined,
                video
            ),
        )
    )
}

#[test]
fn indexed_physical_seek_reads_no_prefix_and_matches_every_packet_fact() {
    let (ts, offsets) = input(true);
    for (chunk, grant) in [(1, 1), (7, 43), (4096, 8192)] {
        let mut sequential = Source::new(&ts.bytes, chunk, grant, ReaderLimits::default());
        let (_, expected) = sequential.suffix().unwrap();
        assert_eq!(expected.len(), FRAMES);
        let mut source = Source::new(&ts.bytes, chunk, grant, ReaderLimits::default());
        source
            .reader
            .request_seek_and_index(request(2), FRAMES)
            .unwrap();
        let (result, packets) = source.suffix().unwrap();
        assert_eq!(result.unwrap().method, SeekMethod::BoundedScan);
        assert_eq!(packets, expected[2..]);
        for index in [7, 1, 4, 4] {
            source.reads.clear();
            source.floor = offsets[index];
            let stale = source.reader.request().unwrap();
            let start = source.reader.request_seek(request(index)).unwrap();
            assert_eq!(start.offset, offsets[index]);
            assert_ne!(start.token.generation(), stale.token.generation());
            let stale_window = InputWindow::new(
                ID,
                stale.token,
                stale.offset,
                Bytes::copy_from_slice(&source.pool, &ts.bytes[..1]).unwrap(),
            )
            .unwrap();
            assert!(matches!(
                source.reader.provide(&stale_window),
                Err(Error::StaleGeneration { .. })
            ));
            drop(stale_window);
            let (result, packets) = source.suffix().unwrap();
            let result = result.unwrap();
            assert_eq!(result.method, SeekMethod::Index);
            assert_eq!(result.scanned_bytes, 0);
            assert!(!result.replays_from_start);
            assert_eq!(packets, expected[index..]);
            assert_eq!(source.reads.first().unwrap().start, offsets[index]);
            assert_eq!(
                source
                    .reads
                    .iter()
                    .map(|range| range.end - range.start)
                    .sum::<u64>(),
                ts.bytes.len() as u64 - offsets[index]
            );
        }
        source.reader.close();
        assert_eq!(source.pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn offsets_without_a_local_table_proof_keep_prefix_replay() {
    let (ts, _) = input(false);
    let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(2), FRAMES)
        .unwrap();
    source.suffix().unwrap();
    let start = source.reader.request_seek(request(7)).unwrap();
    assert_eq!(start.offset, 0);
    source.reads.clear();
    let (result, packets) = source.suffix().unwrap();
    assert_eq!(result.unwrap().method, SeekMethod::Index);
    assert_eq!(packets.len(), FRAMES - 7);
    assert_eq!(source.reads.first().unwrap().start, 0);
}

#[test]
fn changed_checkpoint_bytes_are_refused_before_landing() {
    let (ts, offsets) = input(true);
    for byte in [3, 188 + 3, 376 + 3, 376 + 187] {
        let mut changed = ts.bytes.clone();
        let mut source = Source::new(&ts.bytes, 7, 43, ReaderLimits::default());
        source
            .reader
            .request_seek_and_index(request(2), FRAMES)
            .unwrap();
        source.suffix().unwrap();
        source.reader.request_seek(request(7)).unwrap();
        changed[offsets[7] as usize + byte] ^= 1;
        source.bytes = &changed;
        source.floor = offsets[7];
        assert!(matches!(source.suffix(), Err(Error::SourceMismatch)));
        assert!(matches!(
            source.reader.request(),
            Err(Error::SourceMismatch)
        ));
    }
}

#[test]
fn exact_anchored_clocks_survive_direct_restore() {
    let (ts, offsets) = input(true);
    let limits = ReaderLimits {
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
    let (_, expected) = sequential.suffix().unwrap();
    let mut source = Source::new(&ts.bytes, 4096, 8192, limits);
    let mut seek = request(3);
    seek.target = Time::new((1_i128 << 33) + 90_001 + 3 * 2160, 90_000).unwrap();
    source.reader.request_seek_and_index(seek, FRAMES).unwrap();
    source.suffix().unwrap();
    assert_eq!(source.reader.request_seek(seek).unwrap().offset, offsets[3]);
    source.floor = offsets[3];
    assert_eq!(source.suffix().unwrap().1, expected[3..]);
}

#[test]
fn independent_layers_and_fractional_sample_durations_keep_exact_facts() {
    for (header, size) in [
        ([0xff, 0xff, 0x14, 0xc0], 32),  // MPEG-1 Layer I, 48 kHz.
        ([0xff, 0xfd, 0x10, 0xc0], 104), // MPEG-1 Layer II, 44.1 kHz.
        ([0xff, 0xf5, 0x14, 0xc0], 48),  // MPEG-2 Layer II, 24 kHz.
    ] {
        let mut frame = vec![0; size];
        frame[..4].copy_from_slice(&header);
        let (ts, offsets) = input_frames(true, &frame);
        let mut sequential = Source::new(&ts.bytes, 4096, 8192, ReaderLimits::default());
        let (_, expected) = sequential.suffix().unwrap();
        let mut source = Source::new(&ts.bytes, 7, 1, ReaderLimits::default());
        source
            .reader
            .request_seek_and_index(request(4), FRAMES)
            .unwrap();
        source.suffix().unwrap();
        assert_eq!(
            source.reader.request_seek(request(4)).unwrap().offset,
            offsets[4]
        );
        source.floor = offsets[4];
        assert_eq!(source.suffix().unwrap().1, expected[4..]);
    }
}

#[test]
fn layer_three_and_multi_carrier_pes_keep_indexed_replay() {
    for (header, size) in [
        ([0xff, 0xfb, 0x14, 0xc0], 96),
        ([0xff, 0xfd, 0x44, 0xc0], 192),
    ] {
        let mut frame = vec![0; size];
        frame[..4].copy_from_slice(&header);
        let (ts, offsets) = input_frames(true, &frame);
        let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
        source
            .reader
            .request_seek_and_index(request(4), FRAMES)
            .unwrap();
        source.suffix().unwrap();
        assert_eq!(
            source.reader.request_seek(request(4)).unwrap().offset,
            if size == 192 { offsets[4] } else { 0 }
        );
        source.reads.clear();
        let (result, packets) = source.suffix().unwrap();
        assert_eq!(result.unwrap().method, SeekMethod::Index);
        assert_eq!(packets.len(), FRAMES - 4);
        assert_eq!(
            source.reads.first().unwrap().start,
            if size == 192 { offsets[4] } else { 0 }
        );
    }
}

#[test]
fn optional_checkpoint_pressure_releases_it_and_restarts_replay() {
    for format in [PacketFormat::Ts188, PacketFormat::M2ts192] {
        let (mut ts, mut offsets) = input(true);
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
        let mut source = Source::new(&ts.bytes, 4096, 8192, limits);
        source
            .reader
            .request_seek_and_index(request(4), FRAMES)
            .unwrap();
        source.suffix().unwrap();
        let start = source.reader.request_seek(request(6)).unwrap();
        assert_eq!(start.offset, offsets[6]);
        let window = InputWindow::new(
            ID,
            start.token.clone(),
            start.offset,
            Bytes::copy_from_slice(&source.pool, &ts.bytes[start.offset as usize..]).unwrap(),
        )
        .unwrap();
        source.reader.provide(&window).unwrap();
        drop(window);
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
                Step::NeedInput(request) => break request,
                step => panic!("expected fallback input: {step:?}"),
            }
        };
        assert_eq!(replay.offset, 0);
        assert_ne!(replay.token.generation(), start.token.generation());
        drop(occupied);
        let (result, packets) = source.suffix().unwrap();
        assert_eq!(result.unwrap().method, SeekMethod::Index);
        assert_eq!(packets.len(), FRAMES - 6);
        source.reader.close();
        assert_eq!(source.pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn cancellation_during_direct_bootstrap_releases_the_checkpoint() {
    let (ts, _) = input(true);
    let mut source = Source::new(&ts.bytes, 7, 1, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(3), FRAMES)
        .unwrap();
    source.suffix().unwrap();
    let start = source.reader.request_seek(request(6)).unwrap();
    let window = InputWindow::new(
        ID,
        start.token,
        start.offset,
        Bytes::copy_from_slice(
            &source.pool,
            &ts.bytes[start.offset as usize..start.offset as usize + 1],
        )
        .unwrap(),
    )
    .unwrap();
    source.reader.provide(&window).unwrap();
    drop(window);
    assert!(matches!(
        source.reader.step(&mut WorkBudget::new(1), &OFF).unwrap(),
        Step::Yield | Step::NeedInput(_)
    ));
    assert!(matches!(
        source
            .reader
            .step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(source.pool.used(), Budget::control_storage_bytes());
}

#[test]
fn tied_landings_and_untimed_frames_preserve_one_use_clocks() {
    let mut frame = [0; FRAME_BYTES];
    frame[..4].copy_from_slice(&[0xff, 0xfd, 0x14, 0xc0]);
    let (ts, offsets) = input_timed_frames(true, &frame, |index| match index {
        4 => Some(90_000 + 3 * 2160),
        5 | 8 => None,
        _ => Some(90_000 + index as u64 * 2160),
    });
    let mut sequential = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
    let (_, expected) = sequential.suffix().unwrap();
    let mut source = Source::new(&ts.bytes, 7, 1, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(3), FRAMES)
        .unwrap();
    assert_eq!(source.suffix().unwrap().1, expected[4..]);
    assert_eq!(
        source.reader.request_seek(request(3)).unwrap().offset,
        offsets[4]
    );
    source.floor = offsets[4];
    assert_eq!(source.suffix().unwrap().1, expected[4..]);
}

#[test]
fn checkpoint_capacity_refusal_preserves_the_last_tied_timestamp_index() {
    let mut frame = [0; FRAME_BYTES];
    frame[..4].copy_from_slice(&[0xff, 0xfd, 0x14, 0xc0]);
    let (ts, _) = input_timed_frames(true, &frame, |_| Some(90_000));
    let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
    source.reader.request_seek_and_index(request(0), 1).unwrap();
    let first = source.suffix().unwrap();
    assert_eq!(first.1.len(), 1);
    // One timestamp slot still fits. Its optional physical proof table filled
    // before the last tied frame, so the index remains usable through replay.
    assert_eq!(source.reader.request_seek(request(0)).unwrap().offset, 0);
    let repeated = source.suffix().unwrap();
    assert_eq!(repeated.0.unwrap().method, SeekMethod::Index);
    assert_eq!(repeated.1, first.1);
}

#[test]
fn checkpoint_allocation_is_also_bounded_by_the_source_extent() {
    let (ts, offsets) = input(true);
    let mut source = Source::new(&ts.bytes, 127, 43, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(3), usize::MAX)
        .unwrap();
    let first = source.suffix().unwrap();
    assert_eq!(
        source.reader.request_seek(request(3)).unwrap().offset,
        offsets[3]
    );
    source.floor = offsets[3];
    assert_eq!(source.suffix().unwrap().1, first.1);
}

#[test]
#[ignore = "set TRIERGON_FFMPEG to a reference binary; performs no timing"]
fn black_box_ffmpeg_packet_copy_matches_the_direct_suffix() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let binary = std::env::var_os("TRIERGON_FFMPEG").expect("TRIERGON_FFMPEG reference binary");
    let copy = |input: &[u8]| {
        let mut child = Command::new(&binary)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "mpegts",
                "-i",
                "pipe:0",
                "-map",
                "0:a:0",
                "-c:a",
                "copy",
                "-f",
                "mp2",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let (ts, offsets) = input(true);
    let reference = copy(&ts.bytes);
    assert_eq!(reference.len(), FRAMES * FRAME_BYTES);
    let mut source = Source::new(&ts.bytes, 7, 43, ReaderLimits::default());
    source
        .reader
        .request_seek_and_index(request(3), FRAMES)
        .unwrap();
    source.suffix().unwrap();
    assert_eq!(
        source.reader.request_seek(request(6)).unwrap().offset,
        offsets[6]
    );
    source.floor = offsets[6];
    let mut actual = Vec::new();
    while let Some(event) = source.next().unwrap() {
        if let Event::Packet(packet) = event {
            actual.extend_from_slice(packet.payload());
        }
    }
    assert_eq!(actual, reference[6 * FRAME_BYTES..]);
    assert_eq!(actual, copy(&ts.bytes[offsets[6] as usize..]));
}

#[test]
fn m2ts_multi_carrier_restart_preserves_prefixes_and_all_coordinates() {
    for (header, size) in [
        ([0xff, 0xff, 0x94, 0xc0], 288),
        ([0xff, 0xfd, 0x44, 0xc0], 192),
        ([0xff, 0xfd, 0xe4, 0xc0], 1152),
    ] {
        let mut frame = vec![0; size];
        frame[..4].copy_from_slice(&header);
        let (ts, offsets) = input_frames(true, &frame);
        let bytes = ts.m2ts();
        let limits = ReaderLimits {
            format: PacketFormat::M2ts192,
            ..ReaderLimits::default()
        };
        let mut sequential = Source::new(&bytes, 4096, 8192, limits);
        let (_, expected) = sequential.suffix().unwrap();
        assert_eq!(expected.len(), FRAMES);
        for (chunk, grant) in [(1, 1), (7, 43), (4096, 8192)] {
            let mut source = Source::new(&bytes, chunk, grant, limits);
            source
                .reader
                .request_seek_and_index(request(2), FRAMES)
                .unwrap();
            assert_eq!(source.suffix().unwrap().1, expected[2..]);
            for index in [7, 1, 4] {
                let offset = offsets[index] / 188 * 192;
                assert_eq!(
                    source.reader.request_seek(request(index)).unwrap().offset,
                    offset
                );
                source.floor = offset;
                source.reads.clear();
                let (seek, packets) = source.suffix().unwrap();
                assert_eq!(packets, expected[index..]);
                assert!(!seek.unwrap().replays_from_start);
                assert_eq!(source.reads.first().unwrap().start, offset);
            }
            source.reader.close();
            assert_eq!(source.pool.used(), Budget::control_storage_bytes());
        }
        // Prefix bytes are opaque but are part of the physical checkpoint proof.
        for byte in [0, 192, 384, 576, 767] {
            let mut changed = bytes.clone();
            let mut source = Source::new(&bytes, 7, 1, limits);
            source
                .reader
                .request_seek_and_index(request(2), FRAMES)
                .unwrap();
            source.suffix().unwrap();
            let offset = offsets[7] / 188 * 192;
            assert_eq!(
                source.reader.request_seek(request(7)).unwrap().offset,
                offset
            );
            changed[offset as usize + byte] ^= 1;
            source.bytes = &changed;
            source.floor = offset;
            assert!(matches!(source.suffix(), Err(Error::SourceMismatch)));
        }
    }
}

#[test]
fn generated_m2ts_audio_comparisons_read_directly_after_indexing() {
    for (bytes, step, first, repeated, carriers) in [
        (
            include_bytes!("../../../../harness/features/mpegts-previous-audio/inputs/mpeg1-layer1-m2ts-multi-carrier-repeated-preroll.m2ts").as_slice(),
            720,
            7,
            1,
            2,
        ),
        (
            include_bytes!("../../../../harness/features/mpegts-previous-audio/inputs/mpeg1-layer2-m2ts-multi-carrier-repeated-preroll.m2ts").as_slice(),
            2160,
            1,
            7,
            7,
        ),
    ] {
        let limits = ReaderLimits {
            format: PacketFormat::M2ts192,
            ..ReaderLimits::default()
        };
        let mut sequential = Source::new(bytes, 4096, 8192, limits);
        let (_, expected) = sequential.suffix().unwrap();
        assert_eq!(expected.len(), FRAMES);
        let seek = |index| SeekRequest {
            target: Time::new(90_001 + index as i128 * step, 90_000).unwrap(),
            ..request(index)
        };
        for (chunk, grant) in [(1, 1), (7, 43), (4096, 8192)] {
            let mut source = Source::new(bytes, chunk, grant, limits);
            source
                .reader
                .request_seek_and_index(seek(first), FRAMES)
                .unwrap();
            assert_eq!(source.suffix().unwrap().1, expected[first..]);
            let offset = ((17 + repeated * (2 + carriers)) * 192) as u64;
            assert_eq!(
                source.reader.request_seek(seek(repeated)).unwrap().offset,
                offset
            );
            source.floor = offset;
            source.reads.clear();
            let (seek, packets) = source.suffix().unwrap();
            let seek = seek.unwrap();
            assert_eq!(seek.method, SeekMethod::Index);
            assert!(!seek.replays_from_start);
            assert_eq!(packets, expected[repeated..]);
            assert_eq!(source.reads.first().unwrap().start, offset);
            source.reader.close();
            assert_eq!(source.pool.used(), Budget::control_storage_bytes());
        }
    }
}
