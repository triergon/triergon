//! Common SimpleBlock publication and full-batch handoff use identical wire facts.
use super::*;
use crate::matroska::typed::usage::Storage;
use triergon_core::Bytes;

fn e(id: u64, data: &[u8]) -> Vec<u8> {
    let mut v: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let w = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    v.extend(((1_u64 << (7 * w)) | data.len() as u64).to_be_bytes()[8 - w..].iter());
    v.extend(data);
    v
}
fn entry(n: u8, kind: u8, extra: Vec<u8>) -> Vec<u8> {
    e(
        0xae,
        &[
            e(0xd7, &[n]),
            e(0x73c5, &[n]),
            e(0x83, &[kind]),
            e(
                0x86,
                if kind == 17 {
                    b"S_TEXT/UTF8"
                } else {
                    b"V_AUTHORED/OPAQUE"
                },
            ),
            e(0x63a2, b"initial"),
            extra,
        ]
        .concat(),
    )
}
fn block(n: u8, t: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    [vec![0x80 | n, 0, t, flags], payload.to_vec()].concat()
}
fn file(bodies: Vec<Vec<u8>>, prefix: bool) -> Vec<u8> {
    let encoding = if prefix {
        e(
            0x6d80,
            &e(
                0x6240,
                &[
                    e(0x5031, &[0]),
                    e(0x5032, &[1]),
                    e(0x5033, &[0]),
                    e(0x5034, &[e(0x4254, &[3]), e(0x4255, b"pre")].concat()),
                ]
                .concat(),
            ),
        )
    } else {
        Vec::new()
    };
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            SEGMENT,
            &[
                e(
                    TRACKS,
                    &[
                        entry(1, 1, Vec::new()),
                        entry(2, 1, encoding),
                        entry(3, 17, Vec::new()),
                    ]
                    .concat(),
                ),
                e(CLUSTER, &[e(0xe7, &[0]), bodies.concat()].concat()),
            ]
            .concat(),
        ),
    ]
    .concat()
}
fn mixed() -> Vec<u8> {
    file(
        vec![
            e(0xa3, &block(1, 0, 0x80, b"P")),
            e(0xa3, &block(1, 1, 0x84, &[1, b'L', b'R'])),
            e(
                0xa0,
                &[
                    e(0xa1, &block(1, 3, 0, b"G")),
                    e(0xa4, b"next"),
                    e(0x9b, &[2]),
                ]
                .concat(),
            ),
            e(0xa3, &block(1, 4, 0x80, b"Q")),
            e(0xa3, &block(2, 5, 0x80, b"X")),
            e(0xa3, &block(3, 6, 0x80, b"S")),
            e(0xa3, &block(1, 7, 0, b"Z")),
        ],
        true,
    )
}
fn reader(pool: &Budget, raw: &[u8], shared: bool) -> Reader {
    Reader::new_with_payload_ownership(
        pool,
        SourceId(30001),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        Limits::default(),
        8192,
        if shared {
            crate::storage::PayloadOwnership::SharedBounded
        } else {
            crate::storage::PayloadOwnership::OwnedExact
        },
    )
    .unwrap()
}
fn supply(r: &mut Reader, req: InputRequest, raw: &[u8], chunk: usize, aliases: &mut Vec<Bytes>) {
    let at = req.offset as usize;
    if at == raw.len() {
        r.finish(req.source, req.token).unwrap();
        return;
    }
    let end = (at + chunk.min(req.maximum)).min(raw.len());
    assert!(end > at);
    let mut parent = vec![0; end - at + 128];
    parent[16..16 + end - at].copy_from_slice(&raw[at..end]);
    let owner = Bytes::copy_from_slice(r.budget(), &parent).unwrap();
    let bytes = owner.slice(16..16 + end - at).unwrap();
    aliases.push(owner);
    r.provide(&InputWindow::new(req.source, req.token, req.offset, bytes).unwrap())
        .unwrap();
}
fn run(raw: &[u8], quantum: u64, chunk: usize, shared: bool) -> (Vec<Vec<u8>>, Option<String>) {
    let pool = Budget::new(2 * 1024 * 1024).unwrap();
    let base = pool.used();
    let mut r = reader(&pool, raw, shared);
    let mut aliases = Vec::new();
    let mut packets = Vec::new();
    let mut rows = Vec::new();
    let mut error = None;
    let mut done = false;
    for at in 0..300_000 {
        let units = if at % 2 == 0 { 0 } else { quantum };
        let mut work = WorkBudget::new(units);
        let before = r.position();
        let step = r.step(&mut work, &AtomicBool::new(false));
        assert!(work.spent() <= units);
        if units == 0 {
            assert_eq!(r.position(), before);
        }
        let event = match &step {
            Ok(ParseStep::Event(Event::Packet(p))) => format!(
                "packet:{}/{:?}/{:?}/{}/{:?}/{:?}/{:?}/{:?}",
                p.packet.track,
                p.packet.pts_ns,
                p.packet.duration_ns,
                p.packet.keyframe,
                p.packet.data.as_ref(),
                p.configuration.initialization(p.packet.track).unwrap().data,
                p.group.as_ref().map(|g| g.raw_body().to_vec()),
                p.packet.timing
            ),
            Ok(ParseStep::Event(Event::Envelope(ElementEvent::Start(h)))) => {
                format!("start:{}", h.id)
            }
            Ok(ParseStep::Event(Event::Envelope(ElementEvent::End { id, offset, .. }))) => {
                format!("end:{id}/{offset}")
            }
            Ok(ParseStep::Event(Event::Envelope(ElementEvent::Leaf { header, .. }))) => {
                format!("leaf:{}", header.id)
            }
            Ok(ParseStep::Event(Event::Envelope(ElementEvent::Skipped(h)))) => {
                format!("skip:{}", h.id)
            }
            Ok(ParseStep::Event(Event::Configuration(c))) => format!("config:{}", c.revision()),
            Ok(ParseStep::Event(Event::Document(_))) => "document".into(),
            Ok(ParseStep::Event(Event::Seek(_))) => panic!("unexpected seek"),
            Ok(ParseStep::NeedInput(q)) => format!("need:{}/{}", q.offset, q.maximum),
            Ok(ParseStep::Yield) => "yield".into(),
            Ok(ParseStep::Finished) => "finished".into(),
            Ok(ParseStep::Backpressure {
                requested,
                available,
            }) => format!("pressure:{requested}/{available}"),
            Err(error) => format!("error:{error:?}"),
        };
        rows.push(format!(
            "{}/{}/{}/{}/{}",
            r.position(),
            work.spent(),
            pool.used(),
            r.state.storage_slots().unwrap(),
            event
        ));
        match step {
            Ok(ParseStep::NeedInput(q)) => supply(&mut r, q, raw, chunk, &mut aliases),
            Ok(ParseStep::Event(Event::Packet(p))) => packets.push(p),
            Ok(ParseStep::Finished) => {
                done = true;
                break;
            }
            Err(e) => {
                error = Some(e.to_string());
                done = true;
                break;
            }
            Ok(ParseStep::Backpressure { .. }) => panic!("small authored input has ample pool"),
            Ok(_) => (),
        }
    }
    assert!(done);
    let out = packets
        .iter()
        .map(|p| p.packet.data.as_ref().to_vec())
        .collect();
    eprintln!("SIMPLE_DIRECT_TRACE q={quantum} chunk={chunk} shared={shared} rows={rows:?}");
    drop((r, packets, aliases));
    assert_eq!(pool.used(), base);
    (out, error)
}
#[test]
fn simple_direct_mixed_plain_lace_group_state_and_prefix_exact_traces() {
    let raw = mixed();
    for shared in [false, true] {
        for q in [1, 7, 8192] {
            for chunk in [1, 7, 8192] {
                let (packets, error) = run(&raw, q, chunk, shared);
                assert!(error.is_none(), "{error:?}");
                assert_eq!(
                    packets,
                    [
                        b"P".to_vec(),
                        b"L".to_vec(),
                        b"R".to_vec(),
                        b"G".to_vec(),
                        b"Q".to_vec(),
                        b"preX".to_vec(),
                        b"S".to_vec(),
                        b"Z".to_vec()
                    ]
                );
            }
        }
    }
}
#[test]
fn simple_direct_header_track_failures_keep_original_charged_boundaries() {
    for body in [
        vec![0, 0, 0, 0],
        block(4, 0, 0x80, b"unknown"),
        block(1, 0, 0x84, &[1, b'x']),
    ] {
        for q in [1, 7, 8192] {
            let raw = file(vec![e(0xa3, &body)], false);
            let (packets, error) = run(&raw, q, 7, false);
            assert!(packets.is_empty());
            assert!(error.is_some());
        }
    }
}
#[test]
fn simple_direct_ready_cancel_transport_reset_and_retained_parent() {
    for transport in [false, true] {
        let raw = file(vec![e(0xa3, &block(1, 0, 0x80, b"payload"))], false);
        let pool = Budget::new(256 * 1024).unwrap();
        let base = pool.used();
        let mut r = reader(&pool, &raw, true);
        let mut aliases = Vec::new();
        let mut entered = false;
        for _ in 0..100_000 {
            if r.simple_direct_test_job_step(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .is_some()
            {
                entered = true;
                break;
            }
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                ParseStep::NeedInput(q) => supply(&mut r, q, &raw, 7, &mut aliases),
                ParseStep::Yield | ParseStep::Event(_) => (),
                other => panic!("{other:?}"),
            }
        }
        assert!(entered);
        let mut total = 0;
        let mut ready = false;
        for _ in 0..1000 {
            let mut work = WorkBudget::new(1);
            match r
                .simple_direct_test_job_step(&mut work, &AtomicBool::new(false))
                .unwrap()
                .unwrap()
            {
                ParseStep::Yield => (),
                ParseStep::Event(()) => {
                    ready = true;
                    total += work.spent();
                    break;
                }
                x => panic!("{x:?}"),
            };
            total += work.spent();
        }
        assert!(ready);
        assert!(total <= 7);
        let request = r.request();
        let stale = request.token.clone();
        if transport {
            r.fail(request.source, request.token, SourceFailure::Transport)
                .unwrap();
        }
        let mut work = WorkBudget::new(0);
        let answer = r.step(&mut work, &AtomicBool::new(true));
        if transport {
            assert!(matches!(answer, Err(triergon_core::Error::Transport)));
        } else {
            assert!(matches!(answer, Err(Error::Cancelled)));
        }
        assert_eq!(work.spent(), 0);
        let next = r.reset().unwrap();
        assert_ne!(next.token, stale);
        assert!(pool.used() > base);
        drop(aliases);
        assert_eq!(pool.used(), base);
    }
}

#[test]
fn simple_direct_empty_payload_owner_survives_reset_and_reader_drop() {
    for shared in [false, true] {
        let raw = file(vec![e(0xa3, &block(1, 0, 0x80, b""))], false);
        let pool = Budget::new(256 * 1024).unwrap();
        let base = pool.used();
        let mut r = reader(&pool, &raw, shared);
        let mut aliases = Vec::new();
        let mut output = None;
        for _ in 0..100_000 {
            match r
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
            {
                ParseStep::NeedInput(q) => supply(&mut r, q, &raw, 1, &mut aliases),
                ParseStep::Event(Event::Packet(p)) => {
                    output = Some(p);
                    break;
                }
                ParseStep::Yield | ParseStep::Event(_) => (),
                other => panic!("{other:?}"),
            }
        }
        let packet = output.unwrap();
        assert!(packet.packet.data.as_ref().is_empty());
        assert_eq!(packet.packet.pts_ns, Some(0));
        assert_eq!(packet.packet.duration_ns, None);
        assert!(packet.group.is_none());
        assert_eq!(packet.packet.data.retained_capacity(), 4);
        r.reset().unwrap();
        drop((r, aliases));
        assert!(pool.used() > base);
        assert!(packet.packet.data.as_ref().is_empty());
        drop(packet);
        assert_eq!(pool.used(), base);
    }
}
