//! Guard coalescing: source failure priority, sticky cancellation, and exact traces.
use super::*;
use triergon_core::Bytes;

fn reader(pool: &Budget, length: u64) -> Reader {
    Reader::new(
        pool,
        SourceId(29001),
        SourceCapabilities {
            length: Some(length),
            random_access: true,
        },
        Limits::default(),
        8192,
    )
    .unwrap()
}
fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

#[test]
fn inner_guard_cancel_is_sticky_releases_input_but_preserves_alias() {
    let pool = Budget::new(4096).unwrap();
    let base = pool.used();
    let mut r = reader(&pool, 32);
    let request = r.request();
    let parent = Bytes::copy_from_slice(&pool, &[23; 160]).unwrap();
    let alias = parent.slice(16..48).unwrap();
    r.provide(&InputWindow::new(request.source, request.token.clone(), 0, alias.clone()).unwrap())
        .unwrap();
    let charged = pool.used();
    let mut work = WorkBudget::new(0);
    r.envelope.healthy_for_job(&mut work, &no_cancel()).unwrap();
    assert!(matches!(
        r.envelope
            .check_inner_cancel(&mut work, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!((r.position(), work.spent(), pool.used()), (0, 0, charged));
    assert!(matches!(
        r.step(&mut work, &no_cancel()),
        Err(Error::Cancelled)
    ));
    assert_eq!(alias.as_ref(), &[23; 32]);
    let next = r.reset().unwrap();
    assert_ne!(next.token.generation(), request.token.generation());
    assert!(matches!(
        r.provide(&InputWindow::new(request.source, request.token, 0, alias).unwrap()),
        Err(triergon_core::Error::StaleGeneration { .. })
    ));
    drop((r, parent));
    assert_eq!(pool.used(), base);
}

#[test]
fn inner_guard_prior_transport_and_short_finish_win_over_cancel_and_metadata() {
    for short_finish in [false, true] {
        let pool = Budget::new(4096).unwrap();
        let mut r = reader(&pool, 32);
        let req = r.request();
        if short_finish {
            assert!(matches!(
                r.finish(req.source, req.token),
                Err(Error::Truncated)
            ));
        } else {
            r.fail(req.source, req.token, SourceFailure::Transport)
                .unwrap();
        }
        r.metadata_used = r.limits.metadata_bytes + 1;
        for flag in [true, false] {
            let mut work = WorkBudget::new(0);
            let result = r.step_demux_mode(&mut work, &AtomicBool::new(flag), None, false);
            if short_finish {
                assert!(matches!(result, Err(Error::Truncated)));
            } else {
                assert!(matches!(result, Err(triergon_core::Error::Transport)));
            }
            assert_eq!((r.position(), work.spent()), (0, 0));
        }
    }
}

#[test]
fn inner_guard_entry_cancel_precedes_metadata_failure_and_survives_flag_clear() {
    let pool = Budget::new(4096).unwrap();
    let mut r = reader(&pool, 1);
    r.metadata_used = r.limits.metadata_bytes + 1;
    let mut work = WorkBudget::new(7);
    assert!(matches!(
        r.step_demux_mode(&mut work, &AtomicBool::new(true), None, false),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        r.step_demux_mode(&mut work, &no_cancel(), None, false),
        Err(Error::Cancelled)
    ));
    assert_eq!((r.position(), work.spent()), (0, 0));
    r.reset().unwrap();
    r.metadata_used = r.limits.metadata_bytes + 1;
    assert!(matches!(
        r.step_demux_mode(&mut work, &no_cancel(), None, false),
        Err(Error::Policy("metadata"))
    ));
}

#[test]
fn inner_guard_healthy_empty_check_needs_no_work_or_storage() {
    let pool = Budget::new(1024).unwrap();
    let mut r = reader(&pool, 32);
    let held = pool.reserve(pool.available()).unwrap();
    let charged = pool.used();
    let req = r.request();
    let mut work = WorkBudget::new(0);
    r.envelope.healthy_for_job(&mut work, &no_cancel()).unwrap();
    for _ in 0..7 {
        r.envelope
            .check_inner_cancel(&mut work, &no_cancel())
            .unwrap();
    }
    assert_eq!((r.position(), work.spent(), pool.used()), (0, 0, charged));
    assert_eq!(r.request().token, req.token);
    drop(held);
}

fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|width| (data.len() as u64) < (1_u64 << (7 * width)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn fixture() -> Vec<u8> {
    let entry = element(
        0xae,
        &[
            element(0xd7, &[1]),
            element(0x73c5, &[1]),
            element(0x83, &[1]),
            element(0x86, b"V_AUTHORED/OPAQUE"),
            element(0x63a2, b"config"),
        ]
        .concat(),
    );
    let blocks: Vec<_> = (0..3)
        .map(|at| element(0xa3, &[0x81, 0, at, 0x80, b'a' + at]))
        .collect();
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(
            SEGMENT,
            &[
                element(INFO, &element(0x2ad7b1, &[0x0f, 0x42, 0x40])),
                element(TRACKS, &entry),
                element(CLUSTER, &[element(0xe7, &[0]), blocks.concat()].concat()),
            ]
            .concat(),
        ),
    ]
    .concat()
}

fn event(step: &triergon_core::Step<Event, triergon_core::Backpressure>) -> String {
    match step {
        triergon_core::Step::NeedInput(req) => format!(
            "need:{}/{}/{}",
            req.offset,
            req.maximum,
            req.token.request()
        ),
        triergon_core::Step::Yield => "yield".into(),
        triergon_core::Step::Backpressure(triergon_core::Backpressure {
            requested,
            available,
        }) => format!("pressure:{requested}/{available}"),
        triergon_core::Step::End(_) => "end".into(),
        triergon_core::Step::Ready(Event::Envelope(ElementEvent::Start(h))) => {
            format!("start:{}/{}/{}", h.id, h.start, h.data)
        }
        triergon_core::Step::Ready(Event::Envelope(ElementEvent::End { id, offset, .. })) => {
            format!("close:{id}/{offset}")
        }
        triergon_core::Step::Ready(Event::Envelope(ElementEvent::Skipped(h))) => {
            format!("skip:{}/{}", h.id, h.start)
        }
        triergon_core::Step::Ready(Event::Envelope(ElementEvent::Leaf { header, data })) => {
            format!(
                "leaf:{}/{}/{:?}",
                header.id,
                data.retained_capacity(),
                data.as_ref()
            )
        }
        triergon_core::Step::Ready(Event::Configuration(c)) => {
            format!("configuration:{}/{}", c.tracks().len(), c.revision())
        }
        triergon_core::Step::Ready(Event::Document(_)) => "document".into(),
        triergon_core::Step::Ready(Event::Packet(p)) => format!(
            "packet:{:?}/{}/{:?}",
            p.packet.pts_ns,
            p.packet.data.retained_capacity(),
            p.packet.data.as_ref()
        ),
        triergon_core::Step::Ready(Event::Seek(_)) => panic!("sequential fixture must not seek"),
    }
}
fn trace(quantum: u64, chunk: usize, shared: bool, tail: Option<bool>) {
    let raw = fixture();
    let pool = Budget::new(256 * 1024).unwrap();
    let base = pool.used();
    let mut r = Reader::new_with_payload_ownership(
        &pool,
        SourceId(29001),
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
    .unwrap();
    let stop = if tail.is_some() {
        raw.len() - 1
    } else {
        raw.len()
    };
    let mut aliases = Vec::new();
    let mut rows = Vec::new();
    let mut packets = Vec::new();
    let mut done = false;
    for index in 0..100_000 {
        let allowance = if index % 2 == 0 { 0 } else { quantum };
        let mut work = WorkBudget::new(allowance);
        let before = r.position();
        let step = r.step(&mut work, &no_cancel());
        assert!(work.spent() <= allowance);
        if allowance == 0 {
            assert_eq!(r.position(), before);
        }
        let text = match &step {
            Ok(step) => event(step),
            Err(error) => format!("error:{error:?}"),
        };
        rows.push(format!(
            "{}/{}/{}/{}/{}",
            r.position(),
            work.spent(),
            r.metadata_used,
            pool.used(),
            text
        ));
        match step {
            Ok(Step::NeedInput(req)) => {
                let at = req.offset as usize;
                if at == stop {
                    if tail == Some(true) {
                        r.fail(req.source, req.token, SourceFailure::Transport)
                            .unwrap();
                    } else {
                        let result = r.finish(req.source, req.token);
                        if tail == Some(false) {
                            assert!(matches!(result, Err(Error::Truncated)));
                        } else {
                            result.unwrap();
                        }
                    }
                } else {
                    assert!(at < stop);
                    let end = (at + chunk.min(req.maximum)).min(stop);
                    let mut hidden = vec![0; end - at + 128];
                    hidden[16..16 + end - at].copy_from_slice(&raw[at..end]);
                    let owner = Bytes::copy_from_slice(&pool, &hidden).unwrap();
                    let bytes = owner.slice(16..16 + end - at).unwrap();
                    aliases.push(owner);
                    r.provide(&InputWindow::new(req.source, req.token, req.offset, bytes).unwrap())
                        .unwrap();
                }
            }
            Ok(Step::Ready(Event::Packet(packet))) => packets.push(packet),
            Ok(Step::End(_)) => {
                assert!(tail.is_none());
                done = true;
                break;
            }
            Err(error) => {
                match tail {
                    Some(true) => assert!(matches!(error, triergon_core::Error::Transport)),
                    Some(false) => assert!(matches!(error, Error::Truncated)),
                    None => panic!("unexpected: {error:?}"),
                }
                done = true;
                break;
            }
            Ok(Step::Backpressure(_)) => panic!("bounded small fixture has ample pool"),
            Ok(_) => (),
        }
    }
    assert!(done, "bounded trace must finish");
    assert_eq!(packets.len(), if tail.is_none() { 3 } else { 2 });
    if tail.is_none() {
        for (index, p) in packets.iter().enumerate() {
            assert_eq!(p.packet.data.as_ref(), &[b'a' + index as u8]);
            assert_eq!(p.packet.pts_ns, Some(index as i64 * 1_000_000));
        }
    }
    eprintln!(
        "INNER_GUARD_TRACE quantum={quantum} chunk={chunk} shared={shared} tail={tail:?} rows={rows:?}"
    );
    drop(r);
    assert!(pool.used() >= base);
    drop((packets, aliases));
    assert_eq!(pool.used(), base);
}
#[test]
fn inner_guard_metadata_commits_zero_work_fragmented_hidden_owners_trace() {
    for shared in [false, true] {
        for quantum in [1, 7, 8192] {
            for chunk in [1, 7, 64, 8192] {
                trace(quantum, chunk, shared, None);
            }
        }
    }
}
#[test]
fn inner_guard_late_transport_and_eof_trace_remain_distinct() {
    for tail in [false, true] {
        for quantum in [1, 7, 8192] {
            trace(quantum, 7, true, Some(tail));
        }
    }
}
