//! Storage proofs of the reader: live charges through its jobs and hidden input
//! parents, caller pressure, workspace sizes, and reset of an unpublished packet.
//! Pool observations are test oracles; the reader itself visits owners.
use crate::ebml_kit::catalogue_file;
use crate::reader::*;
use crate::storage::usage::Pressure;
use std::io::Cursor;
use triergon_core::{Bytes, Reservation};
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
    let raw = catalogue_file();
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
            other @ Step::Backpressure(_) => panic!("ample pool must finish: {other:?}"),
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
    let raw = catalogue_file();
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
                let remaining = 2 * size_of::<Option<crate::packet::Packet>>() - 1;
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
    let raw = catalogue_file();
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
    let mut feeder = input_feeder::InputFeeder::new(Cursor::new(raw), 1).unwrap();
    let cancel = AtomicBool::new(false);
    let request = reader.request();
    let held = pool.reserve(pool.available() - 1).unwrap();
    assert!(matches!(
        feeder
            .feed_sequential_request(reader.envelope.range_input(), &request, &cancel)
            .unwrap(),
        input_feeder::FeedStep::Backpressure { .. }
    ));
    reader.set_adapter_storage(feeder.retained_buffer_bytes());
    assert_eq!(reader.adapter_storage, 1);
    drop(held);
    exact_floor(&reader, 1);
    assert!(matches!(
        feeder
            .feed_sequential_request(reader.envelope.range_input(), &request, &cancel)
            .unwrap(),
        input_feeder::FeedStep::Provided
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
#[cfg(target_pointer_width = "64")]
#[test]
fn performance_lace_workspace_keeps_one_descriptor_arena() {
    assert!(
        size_of::<crate::packet::lace::LaceJob>() <= 4608,
        "frame lengths must not duplicate the descriptor arena: {} bytes",
        size_of::<crate::packet::lace::LaceJob>()
    );
}

#[test]
fn reader_reset_drops_unpublished_ready_packet_and_all_configuration_owners() {
    let raw = catalogue_file();
    let pool = Budget::new(256 * 1024).unwrap();
    let baseline = pool.used();
    let mut reader = Reader::new(
        &pool,
        SourceId(3391),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        Limits::default(),
        8192,
    )
    .unwrap();
    let mut entered = false;
    for _ in 0..100_000 {
        if matches!(reader.state, State::Packet(_)) {
            entered = true;
            break;
        }
        match reader
            .step_parse(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::NeedInput(request) => supply(&mut reader, request, &raw, false),
            ParseStep::Yield | ParseStep::Event(_) => (),
            other => panic!("packet before EOF: {other:?}"),
        }
    }
    assert!(entered);
    let State::Packet(job) = &mut reader.state else {
        panic!("packet job");
    };
    let mut ready = false;
    for _ in 0..20_000 {
        match job
            .step_ready(None, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(()) => {
                ready = true;
                break;
            }
            other => panic!("ready before reset: {other:?}"),
        }
    }
    assert!(ready);
    assert!(pool.used() > baseline);
    let old = reader.request().token;
    let new = reader.reset().unwrap();
    assert_ne!(old, new.token);
    assert_eq!(new.offset, 0);
    assert_eq!(pool.used(), baseline);
    drop(reader);
    assert_eq!(pool.used(), baseline);
}
