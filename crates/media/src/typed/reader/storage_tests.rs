//! Independent budget-observation guards for the format's permanent-pressure
//! proof. Pool observations are test oracles only; production visits owners.
use super::*;
use crate::matroska::typed::usage::Pressure;
use std::io::Cursor;
use triergon_core::{Bytes, Reservation};

fn e(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|byte| *byte == 0)
        .collect();
    let width = (1..=8)
        .find(|width| (data.len() as u64) < (1_u64 << (7 * width)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}
fn u(id: u64, value: u64) -> Vec<u8> {
    e(id, &value.to_be_bytes())
}
const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
fn file() -> Vec<u8> {
    let encoding = |order, algorithm, settings: &[u8]| {
        e(
            0x6240,
            &[
                u(0x5031, order),
                u(0x5032, 3),
                e(
                    0x5034,
                    &[u(0x4254, algorithm), e(0x4255, settings)].concat(),
                ),
            ]
            .concat(),
        )
    };
    let track = e(
        0xae,
        &[
            u(0xd7, 1),
            u(0x73c5, 1),
            u(0x83, 1),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x536e, "épisode".as_bytes()),
            e(0x22b59d, b"fr-CA"),
            e(0x63a2, &ABC),
            e(
                0x6d80,
                &[encoding(1, 0, &[]), encoding(0, 3, b"P")].concat(),
            ),
        ]
        .concat(),
    );
    let tracks = e(TRACKS, &track);
    let chapter = e(
        crate::metadata::CHAPTERS,
        &e(
            0x45b9,
            &e(
                0xb6,
                &[
                    u(0x73c4, 1),
                    u(0x91, 0),
                    e(0x80, &e(0x85, "épisode".as_bytes())),
                ]
                .concat(),
            ),
        ),
    );
    let tags = e(
        crate::metadata::TAGS,
        &e(
            0x7373,
            &e(
                0x67c8,
                &[e(0x45a3, b"TITLE"), e(0x4487, b"source")].concat(),
            ),
        ),
    );
    let mut block = vec![0x81, 0, 0, 0x84, 1];
    block.extend(ABC);
    block.extend(ABC);
    let cluster = e(CLUSTER, &[u(0xe7, 0), e(0xa3, &block)].concat());
    let cue = e(
        CUES,
        &e(
            0xbb,
            &[
                u(0xb3, 0),
                e(
                    0xb7,
                    &[
                        u(0xf7, 1),
                        u(0xf1, (tracks.len() + chapter.len() + tags.len()) as u64),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    );
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(SEGMENT, &[tracks, chapter, tags, cluster, cue].concat()),
    ]
    .concat()
}
fn supply(reader: &mut Reader, request: InputRequest, raw: &[u8], hidden_parent: bool) {
    let at = request.offset as usize;
    if at == raw.len() {
        reader.finish(request.source, request.token).unwrap();
        return;
    }
    let bytes = if hidden_parent {
        Bytes::copy_from_slice(reader.budget(), raw)
            .unwrap()
            .slice(at..at + 1)
            .unwrap()
    } else {
        Bytes::copy_from_slice(reader.budget(), &raw[at..at + 1]).unwrap()
    };
    reader
        .provide(&InputWindow::new(request.source, request.token, request.offset, bytes).unwrap())
        .unwrap();
}
fn exact_floor(reader: &Reader, adapter: usize) {
    let available = reader.budget().available();
    // No caller retains any pool allocation here. One byte beyond the observed
    // exact remaining capacity must be permanent; the exact boundary must fit.
    for (requested, permanent) in [(available, false), (available + 1, true)] {
        let mut proof = Pressure::new(requested);
        let mut done = false;
        for _ in 0..100_000 {
            let mut work = WorkBudget::new(7);
            let result = proof.step(reader, reader.budget(), &mut work, &AtomicBool::new(false));
            assert!(work.spent() <= 7);
            match result {
                Ok(ParseStep::Yield) => (),
                Ok(ParseStep::Event(())) => {
                    assert!(
                        !permanent,
                        "missing operation owner (adapter capacity {adapter})"
                    );
                    done = true;
                    break;
                }
                Err(Error::Policy("operation retained storage")) => {
                    assert!(
                        permanent,
                        "owner counted more than once (adapter capacity {adapter})"
                    );
                    done = true;
                    break;
                }
                other => panic!("unexpected exact-floor outcome: {other:?}"),
            }
        }
        assert!(done, "bounded owner comparison did not finish");
    }
}
#[test]
fn own_storage_proof_matches_live_charges_through_typed_jobs_and_hidden_input_parents() {
    let raw = file();
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = Reader::new(
        &pool,
        SourceId(401),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let (mut packets, mut documents) = (0, 0);
    let mut finished = false;
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let result = reader.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        match result {
            Step::NeedInput(request) => supply(&mut reader, request, &raw, true),
            Step::Ready(Event::Packet(packet)) => {
                assert_eq!(packet.packet.data.as_ref(), b"Pabc");
                packets += 1;
            }
            Step::Ready(Event::Document(document)) => {
                documents += 1;
                drop(document);
            }
            Step::Ready(event) => drop(event),
            Step::Yield => (),
            Step::End(_) => finished = true,
            other => panic!("ample pool must finish: {other:?}"),
        }
        exact_floor(&reader, 0);
        if finished {
            break;
        }
    }
    assert!(finished);
    assert_eq!((packets, documents), (2, 2));
    drop(reader);
    assert_eq!(pool.used(), baseline);
}
#[test]
fn caller_storage_pressure_preserves_the_pending_batch_and_resumes_after_release() {
    let raw = file();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = Reader::new(
        &pool,
        SourceId(402),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut held: Option<Reservation> = None;
    let (mut armed, mut blocked, mut packets, mut finished) = (false, false, 0, false);
    for _ in 0..100_000 {
        let mut work = WorkBudget::new(1);
        let result = reader.step(&mut work, &cancel).unwrap();
        assert!(work.spent() <= 1);
        match result {
            Step::NeedInput(request) => supply(&mut reader, request, &raw, false),
            Step::Ready(Event::Envelope(ElementEvent::Start(header)))
                if header.id == CLUSTER && !armed =>
            {
                let remaining = 2 * size_of::<Option<crate::matroska::Packet>>() - 1;
                held = Some(pool.reserve(pool.available() - remaining).unwrap());
                armed = true;
            }
            Step::Backpressure(_) => {
                assert!(!blocked, "release must permit progress");
                assert!(held.is_some());
                blocked = true;
                held = None;
            }
            Step::Ready(Event::Packet(packet)) => {
                assert!(blocked);
                assert_eq!(packet.packet.data.as_ref(), b"Pabc");
                packets += 1;
            }
            Step::Ready(event) => drop(event),
            Step::Yield => (),
            Step::End(_) => {
                finished = true;
                break;
            }
        }
    }
    assert!(armed && blocked && finished);
    assert_eq!(packets, 2);
    drop((reader, held));
    assert_eq!(pool.used(), baseline);
}
#[test]
fn pending_blocking_read_and_frozen_source_owners_have_the_same_exact_floor() {
    let raw = file();
    let pool = Budget::new(128 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = Reader::new(
        &pool,
        SourceId(403),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let mut feeder =
        crate::matroska::typed::blocking::InputFeeder::new(Cursor::new(raw), 1).unwrap();
    let cancel = AtomicBool::new(false);
    let request = reader.request();
    let held = pool.reserve(pool.available() - 1).unwrap();
    assert!(matches!(
        feeder
            .feed_sequential_request(reader.envelope.range_input(), &request, &cancel)
            .unwrap(),
        crate::matroska::typed::blocking::FeedStep::Backpressure { .. }
    ));
    reader.set_adapter_storage(feeder.retained_buffer_bytes());
    assert_eq!(reader.adapter_storage, 1);
    drop(held);
    exact_floor(&reader, 1);
    assert!(matches!(
        feeder
            .feed_sequential_request(reader.envelope.range_input(), &request, &cancel)
            .unwrap(),
        crate::matroska::typed::blocking::FeedStep::Provided
    ));
    reader.set_adapter_storage(feeder.retained_buffer_bytes());
    exact_floor(&reader, 0);
    drop((reader, feeder));
    assert_eq!(pool.used(), baseline);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn inline_transaction_workspace_stays_within_the_observed_host_ceiling() {
    let bytes = size_of::<State>();
    println!(
        "State={bytes} PacketBatch={} Tracks={} Catalogue={}",
        size_of::<PacketBatchJob>(),
        size_of::<TracksJob>(),
        size_of::<CatalogueJob>()
    );
    assert!(
        bytes <= 14256,
        "the observed transaction workspace ceiling must not grow"
    );
}

// Fixed workspace guards target the observed Mac host RSS/large-move failure.
// They are not heap policy exceptions or a proof of physical stack-frame size.
#[cfg(target_pointer_width = "64")]
#[test]
fn performance_reader_workspace_fits_one_16_kib_object_without_heap_indirection() {
    assert!(
        size_of::<Reader>() <= 16 * 1024,
        "the reader must not reserve two exclusive packet workspaces: {} bytes",
        size_of::<Reader>()
    );
}
#[cfg(target_pointer_width = "64")]
#[test]
fn performance_lace_workspace_keeps_one_descriptor_arena() {
    assert!(
        size_of::<crate::matroska::typed::lace::LaceJob>() <= 4608,
        "frame lengths must not duplicate the descriptor arena: {} bytes",
        size_of::<crate::matroska::typed::lace::LaceJob>()
    );
}
#[test]
fn performance_blocking_supply_preserves_canonical_exact_read_requests() {
    use std::{cell::RefCell, rc::Rc};
    let raw = file();
    let pool = Budget::new(4 * 1024 * 1024).unwrap();
    let mut canonical = Reader::new(
        &pool,
        SourceId(701),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut expected = Vec::new();
    let mut canonical_packets = 0;
    let mut finished = false;
    for _ in 0..100_000 {
        match canonical
            .step_parse(&mut WorkBudget::new(8192), &cancel)
            .unwrap()
        {
            ParseStep::NeedInput(request) => {
                let maximum = request.maximum.min(8192);
                expected.push((request.offset, maximum.max(1)));
                let at = request.offset as usize;
                if at == raw.len() {
                    canonical.finish(request.source, request.token).unwrap();
                } else {
                    let end = (at + maximum).min(raw.len());
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..end]).unwrap();
                    canonical
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            ParseStep::Event(Event::Packet(_)) => canonical_packets += 1,
            ParseStep::Event(_) | ParseStep::Yield => (),
            ParseStep::Finished => {
                finished = true;
                break;
            }
            other => panic!("canonical supply: {other:?}"),
        }
    }
    assert!(finished);
    struct Recording {
        cursor: Cursor<Vec<u8>>,
        reads: Rc<RefCell<Vec<(u64, usize)>>>,
    }
    impl std::io::Read for Recording {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            self.reads
                .borrow_mut()
                .push((self.cursor.position(), output.len()));
            std::io::Read::read(&mut self.cursor, output)
        }
    }
    let reads = Rc::new(RefCell::new(Vec::new()));
    let source = Recording {
        cursor: Cursor::new(raw),
        reads: reads.clone(),
    };
    let mut blocking = BlockingReader::open(source, Limits::default()).unwrap();
    let mut packets = 0;
    while blocking.next_packet(&cancel).unwrap().is_some() {
        packets += 1;
    }
    assert_eq!(packets, canonical_packets);
    assert_eq!(
        *reads.borrow(),
        expected,
        "do not widen hints/read buffers or move I/O across a source boundary"
    );
}

mod reader_ready_reset_tests;
