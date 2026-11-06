//! Source-preserving runtime RED/resource guards and compatibility controls.
//! No new production symbol or hypothetical Unsupported behavior is required.
use super::*;
use crate::timing::BlockTiming;
use crate::typed::{InitializationOrigin, Packet as TypedPacket, SourcePosition};
use triergon_core::Bytes;

fn e(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn u(id: u64, value: u64) -> Vec<u8> {
    e(id, &value.to_be_bytes())
}
fn entry(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    e(
        0xae,
        &[
            u(0xd7, number),
            u(0x73c5, number),
            u(0x83, kind),
            e(
                0x86,
                if kind == 17 {
                    b"S_TEXT/UTF8"
                } else {
                    b"V_AUTHORED/OPAQUE"
                },
            ),
            e(0x63a2, b"initial"),
            extra.to_vec(),
        ]
        .concat(),
    )
}
fn simple(number: u64, width: usize, offset: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    assert!((1..=8).contains(&width) && number < (1_u64 << (7 * width)) - 1);
    let mut body = ((1_u64 << (7 * width)) | number).to_be_bytes()[8 - width..].to_vec();
    body.extend(offset.to_be_bytes());
    body.push(flags);
    body.extend(payload);
    e(0xa3, &body)
}
fn cluster(ticks: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    e(0x1f43b675, &[u(0xe7, ticks), blocks.concat()].concat())
}
struct Fixture {
    raw: Vec<u8>,
    clusters: u64,
}
fn file(entries: &[u8], info: &[u8], clusters: &[Vec<u8>]) -> Fixture {
    let body = [info.to_vec(), e(0x1654ae6b, entries), clusters.concat()].concat();
    Fixture {
        raw: [e(0x1a45dfa3, &e(0x4282, b"matroska")), e(0x18538067, &body)].concat(),
        // Independent concatenated wire lengths, not a parser/seek helper.
        clusters: clusters.iter().map(|bytes| bytes.len() as u64).sum(),
    }
}
fn repeated(count: usize, payload: usize) -> Fixture {
    let blocks: Vec<_> = (0..count)
        .map(|index| simple(1, 1, index as i16, 0x80, &vec![b'x'; payload]))
        .collect();
    file(&entry(1, 1, &[]), &[], &[cluster(0, &blocks)])
}

#[derive(Debug, PartialEq, Eq)]
struct Receipt {
    requested: i64,
    actual: i64,
    scanned: u64,
    indexed: bool,
    discard_until: i64,
    preroll: u64,
    from_start: bool,
}
impl From<crate::matroska::SeekReceipt> for Receipt {
    fn from(value: crate::matroska::SeekReceipt) -> Self {
        Self {
            requested: value.requested_ns,
            actual: value.actual_ns,
            scanned: value.scanned_bytes,
            indexed: value.used_index,
            discard_until: value.discard_until_ns,
            preroll: value.preroll_ns,
            from_start: value.replays_from_start,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
struct TrackView {
    source: Vec<u8>,
    private: Vec<u8>,
    initialization: Vec<u8>,
    origin: InitializationOrigin,
    revision: u64,
}
#[derive(Debug, PartialEq, Eq)]
struct PacketView {
    track: usize,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<u64>,
    key: bool,
    padding: i64,
    timing: BlockTiming,
    payload: Vec<u8>,
    capacity: usize,
    generation: u64,
    revision: u64,
    tracks: Vec<TrackView>,
    group: Option<(Vec<u8>, Option<SourcePosition>)>,
}
fn view(value: TypedPacket) -> PacketView {
    let packet = &value.packet;
    let tracks = value
        .configuration
        .tracks()
        .iter()
        .enumerate()
        .map(|(at, track)| {
            let initialization = value.configuration.initialization(at).unwrap();
            TrackView {
                source: track.source_metadata.to_vec(),
                private: track.codec_private.to_vec(),
                initialization: initialization.data.to_vec(),
                origin: initialization.origin,
                revision: initialization.revision,
            }
        })
        .collect();
    PacketView {
        track: packet.track,
        pts: packet.pts_ns,
        dts: packet.dts_ns,
        duration: packet.duration_ns,
        key: packet.keyframe,
        padding: packet.discard_padding_ns,
        timing: packet.timing,
        payload: packet.data.as_ref().to_vec(),
        capacity: packet.data.retained_capacity(),
        generation: value.configuration.generation(),
        revision: value.configuration.revision(),
        tracks,
        group: value
            .group
            .as_ref()
            .map(|group| (group.raw_body().to_vec(), group.position().unwrap())),
    }
}

struct Feed<'a> {
    raw: &'a [u8],
    chunk: usize,
    hidden: bool,
    hold: bool,
    alias: Option<Bytes>,
    fault: Option<(usize, bool)>,
    immutable: Option<Bytes>,
    requests: usize,
}
impl<'a> Feed<'a> {
    fn new(raw: &'a [u8], chunk: usize) -> Self {
        Self {
            raw,
            chunk,
            hidden: false,
            hold: false,
            alias: None,
            fault: None,
            immutable: None,
            requests: 0,
        }
    }
    fn supply(&mut self, reader: &mut Reader, request: InputRequest) -> Result<()> {
        self.requests += 1;
        let at = usize::try_from(request.offset).unwrap();
        assert!(at <= self.raw.len() && (request.maximum != 0 || at == self.raw.len()));
        if let Some((end, transport)) = self.fault
            && at == end
        {
            return if transport {
                reader.fail(request.source, request.token, SourceFailure::Transport)
            } else {
                reader.finish(request.source, request.token)
            };
        }
        if at == self.raw.len() {
            return reader.finish(request.source, request.token);
        }
        let stop = self.fault.map_or(self.raw.len(), |(end, _)| end);
        let end = (at + request.maximum.min(self.chunk)).min(stop);
        assert!(end > at);
        let bytes = if let Some(root) = &self.immutable {
            root.slice(at..end)?
        } else if self.hidden {
            let mut parent = vec![0_u8; 512 + end - at];
            parent[16..16 + end - at].copy_from_slice(&self.raw[at..end]);
            Bytes::copy_from_slice(reader.budget(), &parent)?.slice(16..16 + end - at)?
        } else {
            Bytes::copy_from_slice(reader.budget(), &self.raw[at..end])?
        };
        if self.hold {
            self.alias = Some(bytes.clone());
        }
        reader.provide(&InputWindow::new(
            request.source,
            request.token,
            request.offset,
            bytes,
        )?)
    }
}
fn new_reader(pool: &Budget, raw: &[u8], limits: Limits) -> Reader {
    Reader::new(
        pool,
        SourceId(28001),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        limits,
        8192,
    )
    .unwrap()
}
fn open(reader: &mut Reader, feed: &mut Feed<'_>) -> Result<()> {
    for _ in 0..200_000 {
        if reader.is_ready() {
            return Ok(());
        }
        match reader.step(&mut WorkBudget::new(8192), &AtomicBool::new(false))? {
            ParseStep::NeedInput(request) => feed.supply(reader, request)?,
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("opening must be bounded: {other:?}"),
        }
    }
    panic!("opening stalled")
}
fn drain(reader: &mut Reader, feed: &mut Feed<'_>, quantum: u64) -> Result<Vec<PacketView>> {
    let mut packets = Vec::new();
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= quantum);
        match step {
            ParseStep::NeedInput(request) => feed.supply(reader, request)?,
            ParseStep::Event(Event::Packet(value)) => packets.push(view(value)),
            ParseStep::Yield | ParseStep::Event(_) => (),
            ParseStep::Finished => return Ok(packets),
            other => panic!("ample-pool replay stalled: {other:?}"),
        }
    }
    panic!("replay stalled")
}
fn seek(reader: &mut Reader, feed: &mut Feed<'_>, quantum: u64) -> Result<(Receipt, u64)> {
    let mut spent = 0;
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        let step = reader.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= quantum);
        spent += work.spent();
        match step? {
            ParseStep::NeedInput(request) => feed.supply(reader, request)?,
            ParseStep::Event(Event::Seek(receipt)) => return Ok((receipt.into(), spent)),
            ParseStep::Event(Event::Packet(_)) => {
                panic!("no packet before a completed seek receipt")
            }
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("seek must finish or report its real error: {other:?}"),
        }
    }
    panic!("seek stalled")
}
struct Run {
    receipt: Receipt,
    packets: Vec<PacketView>,
    work: u64,
    initial_peak: usize,
    scan_peak: usize,
}
fn run(raw: &[u8], chunk: usize, quantum: u64, after_eof: bool, target: i64) -> Run {
    run_mode(raw, chunk, quantum, after_eof, target, true)
}
fn run_mode(
    raw: &[u8],
    chunk: usize,
    quantum: u64,
    after_eof: bool,
    target: i64,
    playback: bool,
) -> Run {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, raw, Limits::default());
    let mut feed = Feed::new(raw, chunk);
    open(&mut reader, &mut feed).unwrap();
    if after_eof {
        drop(drain(&mut reader, &mut feed, quantum).unwrap());
    }
    let initial_peak = pool.peak();
    if playback {
        reader.seek_for_playback(0, target).unwrap();
    } else {
        reader.seek_previous(0, target).unwrap();
    }
    let (receipt, work) = seek(&mut reader, &mut feed, quantum).unwrap();
    let scan_peak = pool.peak();
    let packets = drain(&mut reader, &mut feed, quantum).unwrap();
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
    Run {
        receipt,
        packets,
        work,
        initial_peak,
        scan_peak,
    }
}
fn assert_origin(run: &Run, fixture: &Fixture, count: usize) {
    assert_eq!(
        run.receipt,
        Receipt {
            requested: 0,
            actual: 0,
            scanned: fixture.clusters,
            indexed: false,
            discard_until: 0,
            preroll: 0,
            from_start: true,
        }
    );
    assert_eq!(run.packets.len(), count);
    for (index, packet) in run.packets.iter().enumerate() {
        assert_eq!(packet.track, 0);
        assert_eq!(packet.pts, Some(index as i64 * 1_000_000));
        assert_eq!(packet.dts, None);
        assert_eq!(packet.duration, None);
        assert!(packet.key);
        assert_eq!(packet.padding, 0);
        assert_eq!(packet.tracks[0].initialization, b"initial");
    }
}

#[test]
fn scan_prefix_resource_scan_does_not_materialize_a_fragmented_32k_opaque_frame() {
    let fixture = repeated(1, 32 * 1024);
    let eligible = run(&fixture.raw, 64, 8192, false, 0);
    let fallback = run(&fixture.raw, 1, 1, false, 0);
    assert_origin(&eligible, &fixture, 1);
    assert_eq!(eligible.receipt, fallback.receipt);
    assert_eq!(eligible.packets, fallback.packets);
    assert_eq!(eligible.packets[0].payload, vec![b'x'; 32 * 1024]);
    // Scan-only high water, sampled before normal replay constructs its output.
    // One64-byte source window plus existing metadata fits comfortably in8KiB;
    // no scan output needs an additional32KiB owner.
    let growth = eligible.scan_peak.saturating_sub(eligible.initial_peak);
    eprintln!("SCAN_PREFIX_MATERIALIZATION scan_growth={growth} limit=8192");
    assert!(growth <= 8192, "unused scan payload materialized: {growth}");
}

#[test]
fn scan_prefix_resource_linear_work_excludes_frame_staging_and_publication() {
    let small_fixture = repeated(64, 1);
    let large_fixture = repeated(128, 1);
    let small = run(&small_fixture.raw, 64, 8192, false, 0);
    let large = run(&large_fixture.raw, 64, 8192, false, 0);
    assert_origin(&small, &small_fixture, 64);
    assert_origin(&large, &large_fixture, 128);
    assert!(
        small
            .packets
            .iter()
            .chain(&large.packets)
            .all(|packet| packet.payload == b"x")
    );
    let growth = large.work.checked_sub(small.work).unwrap();
    // Each extra block has seven wire bytes. Nine further units per block
    // generously cover bounded envelope/prefix/track/timing checks;32 units
    // cover differing file/Cluster boundaries. No emitted scan packet exists.
    let bound = 64 * 16 + 32;
    eprintln!(
        "SCAN_PREFIX_WORK small={} large={} growth={growth} bound={bound}",
        small.work, large.work
    );
    assert!(
        growth <= bound,
        "scan performs unused frame work: {growth} > {bound}"
    );
}

#[test]
fn scan_prefix_control_first_span_widths_empty_payload_and_source_clock() {
    for width in 1..=8 {
        for payload in [&b""[..], &b"body"[..]] {
            let fixture = file(
                &entry(1, 1, &u(0x56aa, 333_333)),
                &[],
                &[cluster(10, &[simple(1, width, -2, 0x89, payload)])],
            );
            for after_eof in [false, true] {
                let eligible = run(&fixture.raw, 64, 8192, after_eof, 9_000_000);
                let fallback = run(&fixture.raw, 1, 1, after_eof, 9_000_000);
                assert_eq!(eligible.receipt, fallback.receipt);
                assert_eq!(eligible.packets, fallback.packets);
                assert_eq!(eligible.receipt.actual, 7_666_667);
                assert_eq!(eligible.receipt.scanned, fixture.clusters);
                assert_eq!(eligible.packets.len(), 1);
                assert_eq!(eligible.packets[0].payload, payload);
                assert_eq!(eligible.packets[0].pts, Some(7_666_667));
            }
        }
    }
}

#[test]
fn scan_prefix_control_physical_equal_time_and_other_tracks_use_identical_receipts() {
    let entries = [entry(1, 1, &[]), entry(2, 17, &[])].concat();
    let fixture = file(
        &entries,
        &[],
        &[
            cluster(
                0,
                &[simple(2, 1, 0, 0x80, b"s0"), simple(1, 1, 0, 0x80, b"v0")],
            ),
            cluster(
                10,
                &[
                    simple(2, 1, 0, 0x80, b"s10"),
                    simple(1, 1, 0, 0x80, b"first"),
                    simple(1, 1, 0, 0x80, b"tie"),
                    simple(2, 1, 1, 0x80, b"s11"),
                ],
            ),
            cluster(20, &[simple(1, 1, 0, 0x80, b"v20")]),
        ],
    );
    for after_eof in [false, true] {
        let a = run(&fixture.raw, 64, 8192, after_eof, 15_000_000);
        let b = run(&fixture.raw, 1, 1, after_eof, 15_000_000);
        assert_eq!(a.receipt, b.receipt);
        assert_eq!(a.packets, b.packets);
        assert_eq!(a.receipt.actual, 10_000_000);
        let payloads: Vec<_> = a
            .packets
            .iter()
            .map(|packet| packet.payload.as_slice())
            .collect();
        assert_eq!(payloads, [&b"s10"[..], b"first", b"tie", b"s11", b"v20"]);
    }
}

#[test]
fn scan_prefix_control_plain_previous_uses_the_real_block_and_unchanged_epoch_receipt() {
    let entries = [entry(1, 1, &[]), entry(2, 17, &[])].concat();
    let fixture = file(
        &entries,
        &[],
        &[
            cluster(0, &[simple(1, 1, 0, 0x80, b"v0")]),
            cluster(
                10,
                &[
                    simple(2, 1, 0, 0x80, b"before"),
                    simple(1, 1, 0, 0x80, b"first"),
                    simple(1, 1, 0, 0x80, b"tie"),
                ],
            ),
            cluster(20, &[simple(1, 1, 0, 0x80, b"v20")]),
        ],
    );
    for after_eof in [false, true] {
        let a = run_mode(&fixture.raw, 64, 8192, after_eof, 15_000_000, false);
        let b = run_mode(&fixture.raw, 1, 1, after_eof, 15_000_000, false);
        assert_eq!(a.receipt, b.receipt);
        assert_eq!(a.packets, b.packets);
        assert_eq!(a.receipt.actual, 10_000_000);
        assert!(!a.receipt.from_start && !a.receipt.indexed);
        let payloads: Vec<_> = a
            .packets
            .iter()
            .map(|packet| packet.payload.as_slice())
            .collect();
        assert_eq!(payloads, [&b"first"[..], b"tie", b"v20"]);
    }
}

#[test]
fn scan_prefix_control_all_track_preroll_keeps_original_source_packets() {
    let entries = [entry(1, 1, &[]), entry(2, 2, &u(0x56bb, 12_000_000))].concat();
    let fixture = file(
        &entries,
        &[],
        &[
            cluster(
                0,
                &[simple(2, 1, 0, 0x80, b"a0"), simple(1, 1, 0, 0x80, b"v0")],
            ),
            cluster(
                10,
                &[simple(2, 1, 0, 0x80, b"a10"), simple(1, 1, 0, 0x80, b"v10")],
            ),
            cluster(20, &[simple(1, 1, 0, 0x80, b"v20")]),
        ],
    );
    for after_eof in [false, true] {
        let a = run(&fixture.raw, 64, 8192, after_eof, 15_000_000);
        let b = run(&fixture.raw, 1, 1, after_eof, 15_000_000);
        assert_eq!(a.receipt, b.receipt);
        assert_eq!(a.packets, b.packets);
        assert_eq!(
            a.receipt,
            Receipt {
                requested: 15_000_000,
                actual: 0,
                scanned: fixture.clusters,
                indexed: false,
                discard_until: 15_000_000,
                preroll: 12_000_000,
                from_start: true,
            }
        );
        let payloads: Vec<_> = a
            .packets
            .iter()
            .map(|packet| packet.payload.as_slice())
            .collect();
        assert_eq!(payloads, [&b"a0"[..], b"v0", b"a10", b"v10", b"v20"]);
    }
}

fn seek_error(fixture: &Fixture, fault: Option<(usize, bool)>, chunk: usize) -> Error {
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, chunk);
    open(&mut reader, &mut feed).unwrap();
    feed.fault = fault;
    reader.seek_for_playback(0, 0).unwrap();
    let error =
        seek(&mut reader, &mut feed, 8192).expect_err("invalid source must not emit a receipt");
    assert!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .is_err()
    );
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
    error
}
#[test]
fn scan_prefix_control_invalid_or_valid_prefix_keeps_late_source_error_priority() {
    for prefix in [
        vec![0x81, 0, 0, 0x80],
        vec![0, 0, 0, 0x80],
        vec![0x83, 0, 0, 0x80],
    ] {
        let block = e(0xa3, &[prefix, vec![b'x'; 128]].concat());
        let fixture = file(&entry(1, 1, &[]), &[], &[cluster(0, &[block])]);
        let cut = fixture.raw.len() - 8;
        assert!(matches!(
            seek_error(&fixture, Some((cut, false)), 64),
            Error::Truncated
        ));
        assert!(matches!(
            seek_error(&fixture, Some((cut, true)), 64),
            Error::Core(triergon_core::Error::Transport)
        ));
    }
}
#[test]
fn scan_prefix_control_bad_prefix_and_clock_errors_are_not_silently_accepted() {
    let invalid = file(
        &entry(1, 1, &[]),
        &[],
        &[cluster(0, &[e(0xa3, &[0, 0, 0, 0x80, b'x'])])],
    );
    assert!(matches!(
        seek_error(&invalid, None, 64),
        Error::Invalid("zero VINT marker")
    ));
    let unknown = file(
        &entry(1, 1, &[]),
        &[],
        &[cluster(0, &[simple(3, 1, 0, 0x80, b"x")])],
    );
    assert!(matches!(
        seek_error(&unknown, None, 64),
        Error::Invalid("unregistered block track")
    ));
    let overflow = file(
        &entry(1, 1, &[]),
        &e(0x1549a966, &u(0x2ad7b1, u64::MAX)),
        &[cluster(u64::MAX, &[simple(1, 1, 0, 0x80, &[b'x'; 128])])],
    );
    assert!(matches!(seek_error(&overflow, None, 64), Error::Overflow));
    let cut = overflow.raw.len() - 8;
    assert!(matches!(
        seek_error(&overflow, Some((cut, false)), 64),
        Error::Truncated
    ));
    assert!(matches!(
        seek_error(&overflow, Some((cut, true)), 64),
        Error::Core(triergon_core::Error::Transport)
    ));
}

const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
fn encoding(algorithm: u64, settings: &[u8]) -> Vec<u8> {
    e(
        0x6d80,
        &e(
            0x6240,
            &[
                u(0x5032, 1),
                e(
                    0x5034,
                    &[u(0x4254, algorithm), e(0x4255, settings)].concat(),
                ),
            ]
            .concat(),
        ),
    )
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for &byte in bytes {
        value ^= u32::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(value & 1));
        }
    }
    !value
}
fn group(corrupt: bool) -> Vec<u8> {
    let body = [
        e(0xa1, &[0x81, 0, 0, 0, b'g']),
        e(0xa4, b"new-state"),
        e(
            0x75a1,
            &e(0xa6, &[u(0xee, 1), e(0xa5, b"opaque-addition")].concat()),
        ),
    ]
    .concat();
    let checksum = crc32(&body) ^ u32::from(corrupt);
    e(0xa0, &[e(0xbf, &checksum.to_le_bytes()), body].concat())
}
#[test]
fn scan_prefix_control_lacing_compression_restoration_and_codec_state_use_full_validation() {
    let mut cases = Vec::new();
    for (mode, body) in [
        (1, vec![1, 1, b'a', b'b']),
        (2, vec![1, b'a', b'b']),
        (3, vec![1, 0x81, b'a', b'b']),
    ] {
        cases.push((
            Vec::new(),
            simple(1, 1, 0, 0x80 | (mode << 1), &body),
            vec![b"a".to_vec(), b"b".to_vec()],
        ));
    }
    cases.push((
        encoding(0, &[]),
        simple(1, 1, 0, 0x80, &ABC),
        vec![b"abc".to_vec()],
    ));
    cases.push((
        encoding(3, b"P"),
        simple(1, 1, 0, 0x80, b"x"),
        vec![b"Px".to_vec()],
    ));
    cases.push((u(0x55ee, 1), group(false), vec![b"g".to_vec()]));
    for (extra, block, expected) in cases {
        let fixture = file(&entry(1, 1, &extra), &[], &[cluster(0, &[block])]);
        for after_eof in [false, true] {
            let a = run(&fixture.raw, 64, 8192, after_eof, 0);
            let b = run(&fixture.raw, 1, 1, after_eof, 0);
            assert_eq!(a.receipt, b.receipt);
            assert_eq!(a.packets, b.packets);
            assert_eq!(
                a.packets
                    .iter()
                    .map(|packet| &packet.payload)
                    .collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            if let Some(group) = &a.packets[0].group {
                assert!(group.1.is_some());
                assert_eq!(a.packets[0].tracks[0].initialization, b"new-state");
                assert!(matches!(
                    a.packets[0].tracks[0].origin,
                    InitializationOrigin::CodecState(Some(_))
                ));
            }
        }
    }
}
#[test]
fn scan_prefix_control_group_crc_and_last_compressed_lace_error_precede_seek_receipt() {
    let fixture = file(
        &entry(1, 1, &u(0x55ee, 1)),
        &[],
        &[cluster(0, &[group(true)])],
    );
    assert!(
        matches!(seek_error(&fixture, None, 64), Error::Invalid(reason) if reason.contains("CRC"))
    );
    let mut laces = vec![1];
    laces.extend(ABC);
    laces.extend(ABC);
    *laces.last_mut().unwrap() ^= 1;
    let fixture = file(
        &entry(1, 1, &encoding(0, &[])),
        &[],
        &[cluster(0, &[simple(1, 1, 0, 0x84, &laces)])],
    );
    assert!(matches!(
        seek_error(&fixture, None, 64),
        Error::Invalid("Adler-32 mismatch")
    ));
}

#[test]
fn scan_prefix_control_zero_work_cancel_reset_and_hidden_owner_lifetime() {
    let fixture = repeated(1, 4096);
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 64);
    open(&mut reader, &mut feed).unwrap();
    reader.seek_for_playback(0, 0).unwrap();
    feed.hidden = true;
    feed.hold = true;
    let body_start = fixture.raw.len() - 4096 - 4;
    let mut reached = false;
    for _ in 0..100_000 {
        let step = reader
            .step(&mut WorkBudget::new(32), &AtomicBool::new(false))
            .unwrap();
        if let ParseStep::NeedInput(request) = step {
            feed.supply(&mut reader, request).unwrap();
        }
        if reader.position() >= body_start as u64 + 16
            && reader.position() < fixture.raw.len() as u64
        {
            reached = true;
            break;
        }
    }
    assert!(reached, "reach real partial body input before cancel");
    let position = reader.position();
    let used = pool.used();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(
        (reader.position(), pool.used(), zero.spent()),
        (position, used, 0)
    );
    assert!(matches!(
        reader.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(
        reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .is_err()
    );
    let alias = feed.alias.take().unwrap();
    let charge = alias.retained_storage_bytes();
    assert!(alias.retained_capacity() > alias.len());
    let generation = reader.generation();
    reader.reset().unwrap();
    assert_ne!(reader.generation(), generation);
    drop((reader, feed));
    assert_eq!(pool.used(), baseline + charge);
    drop(alias);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn scan_prefix_control_complete_first_span_keeps_zero_and_one_work_boundaries() {
    let fixture = repeated(1, 128);
    let expected = run(&fixture.raw, 64, 8192, false, 0);
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 64);
    open(&mut reader, &mut feed).unwrap();
    reader.seek_for_playback(0, 0).unwrap();
    let body_start = fixture.raw.len() - 128 - 4;
    let mut supplied_body = false;
    for _ in 0..100_000 {
        match reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let is_body = request.offset == body_start as u64;
                feed.supply(&mut reader, request).unwrap();
                if is_body {
                    supplied_body = true;
                    break;
                }
            }
            ParseStep::Yield | ParseStep::Event(Event::Envelope(_)) => (),
            other => panic!("reach first actual source span before receipt: {other:?}"),
        }
    }
    assert!(supplied_body);
    let mut first_span_work = WorkBudget::new(11);
    assert!(matches!(
        reader
            .step(&mut first_span_work, &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(first_span_work.spent(), 11);
    assert_eq!(reader.position(), body_start as u64 + 11);
    let position = reader.position();
    let used = pool.used();
    let mut zero = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
        ParseStep::Yield
    ));
    assert_eq!(
        (reader.position(), pool.used(), zero.spent()),
        (position, used, 0)
    );
    let (receipt, _) = seek(&mut reader, &mut feed, 1).unwrap();
    assert_eq!(receipt, expected.receipt);
    assert_eq!(drain(&mut reader, &mut feed, 1).unwrap(), expected.packets);
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn scan_prefix_control_caller_pressure_can_be_released_without_losing_seek_progress() {
    let fixture = repeated(1, 4096);
    let expected = run(&fixture.raw, 64, 8192, false, 0);
    let pool = Budget::new(16 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 64);
    open(&mut reader, &mut feed).unwrap();
    let mut caller = Some(
        pool.reserve(pool.available().checked_sub(1024).unwrap())
            .unwrap(),
    );
    reader.seek_for_playback(0, 0).unwrap();
    let mut receipt = None;
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(8192);
        match reader.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::NeedInput(request) => feed.supply(&mut reader, request).unwrap(),
            ParseStep::Backpressure { .. } => {
                assert!(
                    caller.is_some(),
                    "caller-retained pressure must be sufficient to explain rejection"
                );
                let position = reader.position();
                let mut zero = WorkBudget::new(0);
                assert!(matches!(
                    reader.step(&mut zero, &AtomicBool::new(false)).unwrap(),
                    ParseStep::Yield | ParseStep::Backpressure { .. }
                ));
                assert_eq!((reader.position(), zero.spent()), (position, 0));
                drop(caller.take());
            }
            ParseStep::Event(Event::Seek(value)) => {
                receipt = Some(Receipt::from(value));
                break;
            }
            ParseStep::Event(Event::Packet(_)) => panic!("no packet before seek completion"),
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("transient pressure cannot become permanent exhaustion: {other:?}"),
        }
        assert!(work.spent() <= 8192);
    }
    // If the optimized scan needs no allocation, it need not invent pressure.
    // In either path the normal output replay retains its original requirements.
    drop(caller);
    assert_eq!(receipt.unwrap(), expected.receipt);
    assert_eq!(
        drain(&mut reader, &mut feed, 8192).unwrap(),
        expected.packets
    );
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn scan_prefix_control_short_prefix_and_missing_timestamp_keep_body_error_order() {
    for body in [vec![0x81], vec![0x81, 0], vec![1, 0, 0, 0, 0, 0, 1]] {
        let fixture = file(&entry(1, 1, &[]), &[], &[cluster(0, &[e(0xa3, &body)])]);
        assert!(matches!(seek_error(&fixture, None, 64), Error::Truncated));
    }
    let block = simple(1, 1, 0, 0x80, &[b'x'; 128]);
    let fixture = file(&entry(1, 1, &[]), &[], &[e(0x1f43b675, &block)]);
    assert!(matches!(
        seek_error(&fixture, None, 64),
        Error::Invalid("Block before Cluster Timestamp")
    ));
    let cut = fixture.raw.len() - 8;
    assert!(matches!(
        seek_error(&fixture, Some((cut, false)), 64),
        Error::Truncated
    ));
    assert!(matches!(
        seek_error(&fixture, Some((cut, true)), 64),
        Error::Core(triergon_core::Error::Transport)
    ));
}

#[test]
fn scan_prefix_control_declared_block_and_overall_scan_limits_stay_permanent() {
    let fixture = repeated(2, 128);
    for limits in [
        Limits {
            block_bytes: 64,
            ..Limits::default()
        },
        Limits {
            scan_bytes: fixture.clusters - 1,
            ..Limits::default()
        },
    ] {
        let pool = Budget::new(128 * 1024).unwrap();
        let mut reader = new_reader(&pool, &fixture.raw, limits);
        let mut feed = Feed::new(&fixture.raw, 64);
        open(&mut reader, &mut feed).unwrap();
        reader.seek_for_playback(0, 0).unwrap();
        assert!(matches!(
            seek(&mut reader, &mut feed, 8192),
            Err(Error::Limit(_))
        ));
    }
}

#[test]
fn scan_prefix_control_real_subtitle_recovery_prefix_preserves_all_native_suffix_rows() {
    const RAW: &[u8] =
        include_bytes!("../../tests/fixtures/seek-subtitle-recovery/observed-11.mkv");
    const NATIVE: &str = include_str!("../../tests/fixtures/seek-subtitle-recovery/native-11.tsv");
    let a = run(RAW, 512, 8192, false, 8_000_000_000);
    let b = run(RAW, 1, 1, false, 8_000_000_000);
    assert_eq!(a.receipt, b.receipt);
    assert_eq!(a.packets, b.packets);
    assert_eq!(a.packets.len(), 78);
    assert_eq!(
        a.packets
            .iter()
            .map(|packet| packet.payload.len())
            .sum::<usize>(),
        76_247
    );
    for (packet, line) in a.packets.iter().zip(NATIVE.lines()) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 8);
        let signed = |text: &str| {
            if text == "null" {
                None
            } else {
                Some(text.parse::<i64>().unwrap())
            }
        };
        let unsigned = |text: &str| {
            if text == "null" {
                None
            } else {
                Some(text.parse::<u64>().unwrap())
            }
        };
        assert_eq!(packet.track, fields[0].parse::<usize>().unwrap());
        assert_eq!(packet.pts, signed(fields[1]));
        assert_eq!(packet.dts, signed(fields[2]));
        assert_eq!(packet.duration, unsigned(fields[3]));
        assert_eq!(packet.key, fields[4] == "true");
        assert_eq!(packet.padding, fields[6].parse::<i64>().unwrap());
        let hex: String = packet
            .payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(hex, fields[7]);
    }
    assert_eq!(
        (a.packets[0].track, a.packets[0].pts, a.packets[0].duration),
        (2, Some(7_200_000_000), None)
    );
}

// Additional semantic guards for keeping the same seek job in its Reader slot.
// Original guards above remain byte-for-byte unchanged.
fn inplace_trace_step(step: &ParseStep<Event>) -> String {
    match step {
        ParseStep::NeedInput(r) => format!(
            "input:{}:{}:{}:{}:{}",
            r.source.0,
            r.token.generation(),
            r.token.request(),
            r.offset,
            r.maximum
        ),
        ParseStep::Yield => "yield".to_owned(),
        ParseStep::Finished => "finished".to_owned(),
        ParseStep::Backpressure {
            requested,
            available,
        } => format!("pressure:{requested}:{available}"),
        ParseStep::Event(Event::Packet(p)) => format!("packet:{:?}", view(p.clone())),
        ParseStep::Event(Event::Configuration(c)) => format!(
            "configuration:{}:{}:{:?}",
            c.generation(),
            c.revision(),
            c.tracks()
                .iter()
                .map(|t| (t.number, t.kind, t.codec_private.to_vec()))
                .collect::<Vec<_>>()
        ),
        ParseStep::Event(Event::Seek(r)) => format!("seek:{:?}", Receipt::from(*r)),
        ParseStep::Event(Event::Document(d)) => format!("document:{}:{:?}", d.kind(), d.raw()),
        ParseStep::Event(Event::Envelope(ElementEvent::Leaf { header, data })) => {
            format!("leaf:{header:?}:{:?}", data.as_ref())
        }
        ParseStep::Event(Event::Envelope(e)) => format!("envelope:{e:?}"),
    }
}
fn inplace_trace_seek(
    reader: &mut Reader,
    feed: &mut Feed<'_>,
    quanta: &[u64],
) -> (Receipt, Vec<String>) {
    let mut trace = Vec::new();
    for index in 0..200_000 {
        let mut work = WorkBudget::new(quanta[index % quanta.len()]);
        let before = reader.position();
        let step = reader.step(&mut work, &AtomicBool::new(false)).unwrap();
        trace.push(format!(
            "{before}:{}:{}:{}:{}",
            reader.position(),
            work.spent(),
            reader.budget().used(),
            inplace_trace_step(&step)
        ));
        match step {
            ParseStep::NeedInput(request) => feed.supply(reader, request).unwrap(),
            ParseStep::Event(Event::Seek(receipt)) => return (receipt.into(), trace),
            ParseStep::Event(Event::Packet(_)) => panic!("packet before seek receipt"),
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("seek trace did not finish: {other:?}"),
        }
    }
    panic!("trace stalled")
}
#[test]
fn scan_inplace_guard_exact_request_work_and_owner_trace() {
    let fixture = file(
        &[entry(1, 1, &[]), entry(17, 17, &[])].concat(),
        &[],
        &[
            cluster(
                0,
                &[
                    simple(1, 1, 10, 0x80, b"late"),
                    simple(1, 1, 5, 0x80, b"first"),
                    simple(17, 1, 6, 0, b"sub"),
                ],
            ),
            cluster(
                0,
                &[
                    simple(1, 1, 7, 0x80, b"best"),
                    simple(1, 1, 5, 0x80, b"last"),
                ],
            ),
        ],
    );
    for (chunk, quanta) in [
        (1, vec![0, 1, 0, 7]),
        (7, vec![1]),
        (64, vec![7]),
        (8192, vec![8192]),
    ] {
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let baseline = pool.used();
        let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
        let mut feed = Feed::new(&fixture.raw, chunk);
        feed.hidden = true;
        feed.hold = true;
        open(&mut reader, &mut feed).unwrap();
        reader.seek_previous(0, 8_000_000).unwrap();
        let (receipt, trace) = inplace_trace_seek(&mut reader, &mut feed, &quanta);
        assert_eq!(receipt.actual, 7_000_000);
        assert!(!receipt.indexed);
        let packets = drain(&mut reader, &mut feed, 7).unwrap();
        println!("SCAN_INPLACE_TRACE|requests-{chunk}|{receipt:?}|{trace:?}|{packets:?}");
        drop((reader, feed));
        assert_eq!(pool.used(), baseline);
    }
}
#[test]
fn scan_inplace_guard_codec_epoch_and_reentrant_seek_trace() {
    let fixture = file(
        &entry(1, 1, &u(0x55ee, 1)),
        &[],
        &[
            cluster(0, &[group(false)]),
            cluster(1, &[simple(1, 1, 0, 0x80, b"best")]),
            cluster(2, &[group(false), simple(1, 1, 1, 0, b"tail")]),
        ],
    );
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 7);
    feed.hidden = true;
    open(&mut reader, &mut feed).unwrap();
    for target in [1_000_000, 0, 2_000_000, 1_000_000] {
        reader.seek_for_playback(0, target).unwrap();
        let (receipt, trace) = inplace_trace_seek(&mut reader, &mut feed, &[0, 1, 7]);
        let packets = drain(&mut reader, &mut feed, 1).unwrap();
        assert!(!packets.is_empty());
        println!("SCAN_INPLACE_TRACE|epoch-{target}|{receipt:?}|{trace:?}|{packets:?}");
    }
    drop((reader, feed));
    assert_eq!(pool.used(), baseline);
}
#[test]
fn scan_inplace_guard_pending_best_cancel_reset_retained_alias() {
    let fixture = file(
        &entry(1, 1, &u(0x55ee, 1)),
        &[],
        &[
            cluster(0, &[group(false)]),
            cluster(1, &[simple(1, 1, 0, 0x80, b"best")]),
            cluster(2, &[simple(1, 1, 0, 0, &[b'x'; 256])]),
        ],
    );
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = new_reader(&pool, &fixture.raw, Limits::default());
    let mut feed = Feed::new(&fixture.raw, 7);
    feed.hidden = true;
    feed.hold = true;
    open(&mut reader, &mut feed).unwrap();
    reader.seek_for_playback(0, 1_000_000).unwrap();
    let mut reached = false;
    for _ in 0..100_000 {
        let step = reader
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
        if let ParseStep::NeedInput(r) = step {
            feed.supply(&mut reader, r).unwrap();
        }
        if reader
            .scan
            .as_ref()
            .is_some_and(|s| s.best.is_some() && s.best_configuration.is_some())
            && reader.position() < fixture.raw.len() as u64
        {
            reached = true;
            break;
        }
    }
    assert!(reached);
    let configuration = reader.configuration.clone().unwrap();
    let alias = feed.alias.take().unwrap();
    let position = reader.position();
    let used = pool.used();
    let old_generation = reader.generation();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.step(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        (reader.position(), work.spent(), pool.used()),
        (position, 0, used)
    );
    reader.reset().unwrap();
    assert_ne!(reader.generation(), old_generation);
    drop((reader, feed));
    assert!(pool.used() > baseline);
    println!(
        "SCAN_INPLACE_TRACE|cancel|{position}|{used}|{}|{}|{}",
        pool.used(),
        alias.retained_capacity(),
        configuration.revision()
    );
    drop((configuration, alias));
    assert_eq!(pool.used(), baseline);
}
#[test]
fn scan_inplace_guard_layout_and_late_fault_priority() {
    assert!(std::mem::size_of::<Reader>() <= 10048);
    assert!(std::mem::size_of::<PacketBatchJob>() <= 3408);
    println!(
        "SCAN_INPLACE_LAYOUT|reader={}|batch={}|scan={}|scan_offset={}",
        std::mem::size_of::<Reader>(),
        std::mem::size_of::<PacketBatchJob>(),
        std::mem::size_of::<ScanState>(),
        std::mem::offset_of!(Reader, scan)
    );
    let fixture = repeated(3, 256);
    let end = fixture.raw.len() - 1;
    for chunk in [1, 64] {
        assert!(matches!(
            seek_error(&fixture, Some((end, true)), chunk),
            Error::Core(triergon_core::Error::Transport)
        ));
        assert!(matches!(
            seek_error(&fixture, Some((end, false)), chunk),
            Error::Truncated
        ));
    }
}

fn immutable_fixture(raw: &[u8], limits: Limits) -> (Reader, Feed<'_>) {
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let root = Bytes::copy_from_slice(&pool, raw).unwrap();
    let reader =
        Reader::new_with_immutable_seek_index(&pool, SourceId(28001), root.clone(), limits, 8192)
            .unwrap();
    let mut feed = Feed::new(raw, 7);
    feed.immutable = Some(root);
    (reader, feed)
}

#[test]
fn immutable_playback_seek_reuses_complete_index_and_earliest_equal_timestamp() {
    let clusters = [(0, b"a"), (100, b"b"), (100, b"c"), (200, b"d")]
        .map(|(time, payload)| cluster(time, &[simple(1, 1, 0, 0x80, payload)]));
    let fixture = file(&entry(1, 1, &[]), &[], &clusters);
    let (mut reader, mut feed) = immutable_fixture(&fixture.raw, Limits::default());
    open(&mut reader, &mut feed).unwrap();
    drain(&mut reader, &mut feed, 8192).unwrap();
    for target in [200_000_000, 100_000_000, 300_000_000, 0] {
        let baseline = run(&fixture.raw, 7, 8192, true, target);
        reader.seek_for_playback(0, target).unwrap();
        let (receipt, work) = seek(&mut reader, &mut feed, 1).unwrap();
        assert!(receipt.indexed);
        assert_eq!(receipt.scanned, 0);
        assert_eq!(receipt.actual, baseline.receipt.actual);
        assert_eq!(receipt.from_start, baseline.receipt.from_start);
        assert_eq!(receipt.discard_until, baseline.receipt.discard_until);
        assert_eq!(receipt.preroll, baseline.receipt.preroll);
        assert!(work < baseline.work);
        let packets = drain(&mut reader, &mut feed, 128).unwrap();
        let payloads: Vec<_> = packets.iter().map(|packet| &packet.payload).collect();
        let expected: Vec<_> = baseline
            .packets
            .iter()
            .map(|packet| &packet.payload)
            .collect();
        assert_eq!(payloads, expected);
    }
}

#[test]
fn immutable_seek_falls_back_for_incomplete_unordered_bounded_and_stateful_sources() {
    let first = cluster(0, &[simple(1, 1, 0, 0x80, b"a")]);
    let second = cluster(100, &[simple(1, 1, 0, 0x80, b"b")]);
    let plain = file(&entry(1, 1, &[]), &[], &[first.clone(), second.clone()]);
    let unordered = file(&entry(1, 1, &[]), &[], &[second, first.clone()]);
    let state = cluster(100, &[group(false)]);
    let stateful = file(&entry(1, 1, &u(0x55ee, 1)), &[], &[first, state]);
    for (fixture, complete, index_entries) in [
        (&plain, false, 8),
        (&unordered, true, 8),
        (&plain, true, 1),
        (&stateful, true, 8),
    ] {
        let limits = Limits {
            index_entries,
            ..Limits::default()
        };
        let (mut reader, mut feed) = immutable_fixture(&fixture.raw, limits);
        open(&mut reader, &mut feed).unwrap();
        if complete {
            drain(&mut reader, &mut feed, 8192).unwrap();
        }
        reader.seek_for_playback(0, 100_000_000).unwrap();
        let (receipt, _) = seek(&mut reader, &mut feed, 8192).unwrap();
        assert!(!receipt.indexed);
        assert_eq!(receipt.actual, 100_000_000);
        assert!(receipt.scanned > 0);
    }
}

#[test]
fn immutable_seek_index_rejects_foreign_owner_and_wrong_offset_views() {
    let fixture = repeated(2, 16);
    let (mut reader, mut feed) = immutable_fixture(&fixture.raw, Limits::default());
    let request = reader.request();
    let incorrect_views = [
        Bytes::copy_from_slice(reader.budget(), &fixture.raw[..1]).unwrap(),
        feed.immutable.as_ref().unwrap().slice(1..2).unwrap(),
    ];
    for bytes in incorrect_views {
        let window =
            InputWindow::new(request.source, request.token.clone(), request.offset, bytes).unwrap();
        assert!(matches!(
            reader.provide(&window),
            Err(Error::Core(triergon_core::Error::SourceMismatch))
        ));
    }
    open(&mut reader, &mut feed).unwrap();
    drain(&mut reader, &mut feed, 8192).unwrap();
    assert!(reader.seek_index.as_ref().unwrap().is_complete());
    reader.reset().unwrap();
    assert!(!reader.seek_index.as_ref().unwrap().is_complete());
}

#[test]
fn immutable_cold_seek_coalesces_refills_and_keeps_scan_faults() {
    let fixture = repeated(48, 200);
    let target = 16_000_000;
    let limits = Limits::default();
    for fault in [
        None,
        Some((fixture.raw.len() - 1, false)),
        Some((fixture.raw.len() - 1, true)),
    ] {
        let (mut fast, mut fast_feed) = immutable_fixture(&fixture.raw, limits);
        let pool = Budget::new(4 * 1024 * 1024).unwrap();
        let mut ordinary = new_reader(&pool, &fixture.raw, limits);
        let mut ordinary_feed = Feed::new(&fixture.raw, 4096);
        open(&mut ordinary, &mut ordinary_feed).unwrap();
        open(&mut fast, &mut fast_feed).unwrap();

        fast_feed.chunk = 4096;
        ordinary_feed.fault = fault;
        fast_feed.fault = fault;
        ordinary_feed.requests = 0;
        fast_feed.requests = 0;
        ordinary.seek_for_playback(0, target).unwrap();
        fast.seek_for_playback(0, target).unwrap();
        let ordinary_result = seek(&mut ordinary, &mut ordinary_feed, 8192);
        let fast_result = seek(&mut fast, &mut fast_feed, 8192);
        assert!(fast_feed.requests < ordinary_feed.requests);
        match (ordinary_result, fast_result) {
            (Ok((expected, _)), Ok((receipt, _))) => assert_eq!(receipt, expected),
            (Err(expected), Err(error)) => {
                assert_eq!(format!("{error:?}"), format!("{expected:?}"))
            }
            (ordinary, fast) => panic!("ordinary {ordinary:?}, immutable {fast:?}"),
        }
    }
}
