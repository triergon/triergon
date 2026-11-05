//! Exact public/shared progress, source, work and owner lifetime observations.
use super::*;
use triergon_core::Bytes;

fn element(id: u64, body: &[u8]) -> Vec<u8> {
    let mut bytes: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|n| (body.len() as u64) < (1_u64 << (7 * n)) - 1)
        .unwrap();
    bytes.extend_from_slice(
        &((1_u64 << (7 * width)) | body.len() as u64).to_be_bytes()[8 - width..],
    );
    bytes.extend_from_slice(body);
    bytes
}
fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}
fn track(number: u64, kind: u64, codec: &[u8], extra: &[u8]) -> Vec<u8> {
    element(
        0xae,
        &[
            uint(0xd7, number),
            uint(0x73c5, number),
            uint(0x83, kind),
            element(0x86, codec),
            element(0x63a2, b"raw-config"),
            extra.to_vec(),
        ]
        .concat(),
    )
}
fn fixture(kind: u8) -> Vec<u8> {
    let tracks = element(
        TRACKS,
        &if kind == 3 {
            [
                track(1, 1, b"V_AUTHORED/OPAQUE", &[]),
                track(
                    2,
                    2,
                    b"A_AUTHORED/OPAQUE",
                    &[
                        uint(0x56aa, 40_000_000),
                        uint(0x56bb, 40_000_000),
                        element(0xe1, &element(0xb5, &48000_f64.to_be_bytes())),
                    ]
                    .concat(),
                ),
                track(3, 17, b"S_TEXT/UTF8", &[]),
            ]
            .concat()
        } else {
            track(1, 1, b"V_AUTHORED/OPAQUE", &[])
        },
    );
    let simple = |ticks: u8, data: u8| element(0xa3, &[0x81, 0, ticks, 0x80, data]);
    let cluster = if kind == 2 {
        element(
            CLUSTER,
            &[
                element(0xe7, &[0]),
                element(0xa3, &[0x81, 0, 0, 0x82, 1, 1, b'a', b'b']),
                element(
                    0xa0,
                    &[element(0xa1, &[0x81, 0, 40, 0, b'c']), uint(0x9b, 40)].concat(),
                ),
            ]
            .concat(),
        )
    } else if kind == 3 {
        [
            element(
                CLUSTER,
                &[
                    element(0xe7, &[0]),
                    simple(0, b'a'),
                    element(0xa3, &[0x83, 0, 0, 0x80, b's']),
                ]
                .concat(),
            ),
            element(CLUSTER, &[element(0xe7, &[40]), simple(0, b'b')].concat()),
            element(CLUSTER, &[element(0xe7, &[80]), simple(0, b'c')].concat()),
        ]
        .concat()
    } else {
        element(
            CLUSTER,
            &[element(0xe7, &[0]), simple(0, b'a'), simple(40, b'b')].concat(),
        )
    };
    let tags = element(
        crate::metadata::TAGS,
        &element(
            0x7373,
            &element(
                0x67c8,
                &[element(0x45a3, b"TITLE"), element(0x4487, b"kept-owned")].concat(),
            ),
        ),
    );
    let prefix = if kind == 1 { vec![] } else { tracks.clone() };
    let id = if kind == 1 {
        TRACKS
    } else {
        crate::metadata::TAGS
    };
    let head = |at| {
        element(
            SEEK_HEAD,
            &element(
                0x4dbb,
                &[element(0x53ab, &id.to_be_bytes()[4..]), uint(0x53ac, at)].concat(),
            ),
        )
    };
    let placeholder = head(0);
    let head = head((prefix.len() + placeholder.len() + cluster.len()) as u64);
    assert_eq!(head.len(), placeholder.len());
    let tail = if kind == 1 {
        [tracks, tags].concat()
    } else {
        tags
    };
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(SEGMENT, &[prefix, head, cluster, tail].concat()),
    ]
    .concat()
}
fn make(kind: u8) -> (Budget, usize, Bytes, Reader) {
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let raw = fixture(kind);
    let root = Bytes::copy_from_slice(&pool, &[&[0xaa][..], raw.as_slice(), &[0xbb][..]].concat())
        .unwrap();
    let bytes = root.slice(1..1 + raw.len()).unwrap();
    drop(root);
    let reader = Reader::new(
        &pool,
        SourceId(9193),
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    (pool, baseline, bytes, reader)
}
fn supply(reader: &mut Reader, request: InputRequest, bytes: &Bytes) {
    let at = usize::try_from(request.offset).unwrap();
    if at == bytes.len() {
        reader.finish(request.source, request.token).unwrap();
    } else {
        let end = at + request.maximum.min(17).min(bytes.len() - at);
        reader
            .provide(
                &InputWindow::new(
                    request.source,
                    request.token,
                    request.offset,
                    bytes.slice(at..end).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
    }
}
fn event_facts(event: &Event) -> String {
    match event {
        Event::Envelope(ElementEvent::Leaf { header, data }) => {
            format!("leaf:{header:?}:{:?}", data.as_ref())
        }
        Event::Envelope(other) => format!("envelope:{other:?}"),
        Event::Configuration(config) => format!(
            "configuration:{}:{}:{:?}",
            config.generation(),
            config.revision(),
            config
                .tracks()
                .iter()
                .map(|t| (t.number, t.kind, t.codec_private.as_ref(), t.name.as_str()))
                .collect::<Vec<_>>()
        ),
        Event::Document(document) => format!("document:{}:{:?}", document.kind(), document.raw()),
        Event::Packet(packet) => format!(
            "packet:{:?}:{}:{:?}:{:?}:{:?}:{}:{}:{:?}:{}:{}:{:?}",
            packet.packet.timing,
            packet.packet.track,
            packet.packet.pts_ns,
            packet.packet.dts_ns,
            packet.packet.duration_ns,
            packet.packet.keyframe,
            packet.packet.discard_padding_ns,
            packet.packet.data.as_ref(),
            packet.configuration.generation(),
            packet.configuration.revision(),
            packet.group.as_ref().map(|g| g.raw_body())
        ),
        Event::Seek(receipt) => format!("seek:{receipt:?}"),
    }
}
fn trace(kind: u8, quantum: u64, seek: bool) -> Vec<String> {
    let (pool, baseline, bytes, mut reader) = make(kind);
    let input_charge = pool.used();
    let mut trace = Vec::new();
    let mut sought = false;
    let mut restarted = false;
    let mut held = None;
    let mut configurations = Vec::new();
    let mut packets = 0;
    let mut finished = false;
    for turn in 0..100_000 {
        if seek && !sought && reader.is_ready() {
            let r = reader
                .seek_for_playback(0, if kind == 3 { 80_000_000 } else { 40_000_000 })
                .unwrap();
            trace.push(format!(
                "begin:{}:{}:{}:{}",
                r.source.0,
                r.token.generation(),
                r.token.request(),
                r.offset
            ));
            sought = true;
        }
        let q = if turn % 2 == 0 { 0 } else { quantum };
        let mut work = WorkBudget::new(q);
        let result = reader.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= q);
        let facts = match &result {
            Ok(ParseStep::Event(e)) => event_facts(e),
            Ok(ParseStep::NeedInput(r)) => format!(
                "need:{}:{}:{}:{}:{}",
                r.source.0,
                r.token.generation(),
                r.token.request(),
                r.offset,
                r.maximum
            ),
            other => format!("{other:?}"),
        };
        trace.push(format!(
            "q={q};spent={};pos={};used={};ready={};recovering={};prefix={};{facts}",
            work.spent(),
            reader.position(),
            pool.used(),
            reader.is_ready(),
            reader.subtitle_recovery.is_some(),
            reader.replay_prefix.is_some()
        ));
        match result.unwrap() {
            ParseStep::NeedInput(request) => {
                if sought && !restarted && kind != 3 {
                    let stale = request.clone();
                    let new = reader.seek_for_playback(0, 0).unwrap();
                    assert_ne!(new.token, stale.token);
                    assert!(reader.finish(stale.source, stale.token).is_err());
                    restarted = true;
                    trace.push(format!(
                        "restart:{}:{}:{}",
                        new.token.generation(),
                        new.token.request(),
                        new.offset
                    ));
                } else {
                    supply(&mut reader, request, &bytes);
                }
            }
            ParseStep::Event(Event::Configuration(config)) => configurations.push(config),
            ParseStep::Event(Event::Packet(packet)) => {
                packets += 1;
                if held.is_none() {
                    held = Some(packet);
                }
            }
            ParseStep::Finished => {
                finished = true;
                break;
            }
            _ => (),
        }
    }
    assert!(finished && packets > 0);
    assert_eq!(
        packets,
        if kind == 2 {
            3
        } else if kind == 3 {
            4
        } else {
            2
        }
    );
    if kind == 3 && seek {
        assert!(trace.iter().any(|line| line.contains("recovering=true")));
    }
    reader.reset().unwrap();
    assert!(pool.used() > input_charge);
    if let Some(packet) = &held {
        assert!(!packet.packet.data.is_empty());
        assert_eq!(
            packet.configuration.tracks()[0].codec_private.as_ref(),
            b"raw-config"
        );
    }
    drop((held, configurations));
    assert_eq!(pool.used(), input_charge);
    drop((reader, bytes));
    assert_eq!(pool.used(), baseline);
    trace
}
#[test]
fn demux_slot_plain_exact_source_work_read_and_alias_traces() {
    for kind in [0, 1, 2, 3] {
        for quantum in [1, 7, 8192] {
            let public = trace(kind, quantum, false);
            println!("DEMUX_SLOT_TRACE plain-{kind}-{quantum} {public:?}");
        }
    }
}
#[test]
fn demux_slot_reentrant_seek_and_recovery_traces() {
    for kind in [0, 1, 3] {
        for quantum in [1, 7, 8192] {
            let public = trace(kind, quantum, true);
            assert!(public.iter().any(|line| line.contains("seek:")));
            println!("DEMUX_SLOT_TRACE seek-{kind}-{quantum} {public:?}");
        }
    }
}
// Test-only baseline adapter. On old Reader, method lookup uses this trait;
// on the candidate it uses the real inherent thin boundary. Assertions and
// fixture bytes stay identical, and public traces are frozen before the fix.
trait WideBaselineSlot {
    fn step_demux_projected_into<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
        ready: &mut Option<D::Event>,
    ) -> Result<ParseStep<()>>;
}
impl WideBaselineSlot for Reader {
    fn step_demux_projected_into<D: PacketProjection>(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
        stop: Option<u64>,
        scan_prefix: bool,
        ready: &mut Option<D::Event>,
    ) -> Result<ParseStep<()>> {
        match self.step_demux_projected::<D>(work, cancel, stop, scan_prefix)? {
            ParseStep::Event(event) => {
                *ready = Some(event);
                Ok(ParseStep::Event(()))
            }
            other => crate::matroska::typed::pending(other),
        }
    }
}
#[test]
fn demux_slot_pending_transport_cancel_and_eof_owners_are_empty() {
    for baseline in [true, false] {
        let (pool, base, bytes, mut reader) = make(0);
        let call = |reader: &mut Reader, work: &mut WorkBudget, cancel: &AtomicBool| {
            let mut ready = None;
            let progress = if baseline {
                WideBaselineSlot::step_demux_projected_into::<TypedProjection>(
                    reader, work, cancel, None, false, &mut ready,
                )
            } else {
                reader.step_demux_projected_into::<TypedProjection>(
                    work, cancel, None, false, &mut ready,
                )
            };
            assert!(ready.is_none(), "pending/error may not publish an owner");
            progress
        };
        let mut zero = WorkBudget::new(0);
        let first = call(&mut reader, &mut zero, &AtomicBool::new(false)).unwrap();
        assert!(
            matches!(first,ParseStep::NeedInput(request) if request.offset==0 && request.maximum==1)
        );
        assert_eq!(zero.spent(), 0);
        let stale = reader.request();
        reader
            .fail(stale.source, stale.token.clone(), SourceFailure::Transport)
            .unwrap();
        assert!(matches!(
            call(&mut reader, &mut zero, &AtomicBool::new(true)),
            Err(Error::Core(triergon_core::Error::Transport))
        ));
        reader.reset().unwrap();
        assert!(matches!(
            call(&mut reader, &mut zero, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        reader.reset().unwrap();
        let current = reader.request();
        assert!(matches!(
            reader.finish(current.source, current.token),
            Err(Error::Truncated)
        ));
        let mut work = WorkBudget::new(8192);
        assert!(matches!(
            call(&mut reader, &mut work, &AtomicBool::new(false)),
            Err(Error::Truncated)
        ));
        assert!(reader.finish(stale.source, stale.token).is_err());
        drop((reader, bytes));
        assert_eq!(pool.used(), base);
    }
}
#[test]
fn demux_slot_blocking_packet_and_receipt_boundaries_remain_complete() {
    for kind in [0, 1, 2, 3] {
        let raw = fixture(kind);
        let mut reader =
            BlockingReader::open_seekable(std::io::Cursor::new(raw), Limits::default()).unwrap();
        if kind == 3 {
            let receipt = reader
                .seek_for_playback(0, 80_000_000, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(receipt.actual_ns, 40_000_000);
        }
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_typed_packet(&AtomicBool::new(false)).unwrap() {
            packets.push(packet.packet.data.as_ref().to_vec());
        }
        assert_eq!(
            packets,
            if kind == 2 {
                vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
            } else if kind == 3 {
                vec![b"a".to_vec(), b"s".to_vec(), b"b".to_vec(), b"c".to_vec()]
            } else {
                vec![b"a".to_vec(), b"b".to_vec()]
            }
        );
    }
}

#[test]
fn demux_slot_real_full_pool_rejection_publishes_no_owner() {
    for baseline in [true, false] {
        let (pool, base, bytes, mut reader) = make(0);
        let blocker = pool.reserve(pool.available()).unwrap();
        let mut blocked = false;
        for _ in 0..10000 {
            let mut ready = None;
            let mut work = WorkBudget::new(1);
            let status = if baseline {
                WideBaselineSlot::step_demux_projected_into::<TypedProjection>(
                    &mut reader,
                    &mut work,
                    &AtomicBool::new(false),
                    None,
                    false,
                    &mut ready,
                )
            } else {
                reader.step_demux_projected_into::<TypedProjection>(
                    &mut work,
                    &AtomicBool::new(false),
                    None,
                    false,
                    &mut ready,
                )
            };
            match status {
                Ok(ParseStep::Event(())) => {
                    assert!(ready.take().is_some());
                }
                Ok(ParseStep::NeedInput(request)) => {
                    assert!(ready.is_none());
                    supply(&mut reader, request, &bytes);
                }
                Ok(ParseStep::Backpressure { .. }) | Err(Error::Backpressure { .. }) => {
                    assert!(ready.is_none());
                    blocked = true;
                    break;
                }
                Ok(ParseStep::Yield) => assert!(ready.is_none()),
                other => panic!("unexpected full-pool result {other:?}"),
            }
        }
        assert!(blocked);
        drop(blocker);
        reader.reset().unwrap();
        drop((reader, bytes));
        assert_eq!(pool.used(), base);
    }
}
