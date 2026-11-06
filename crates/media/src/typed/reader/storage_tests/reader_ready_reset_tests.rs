//! Reset while an internally validated packet remains unpublished.
use super::*;

#[test]
fn reader_reset_drops_unpublished_ready_packet_and_all_configuration_owners() {
    let raw = file();
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
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
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
