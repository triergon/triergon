mod common;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{BoxHeader, BoxStep, BoxView, HeaderRead, Limits};

#[test]
fn composition_wire_bits_and_versioned_sign_are_both_preserved() {
    use triergon_isobmff::CompositionOffset;
    let unsigned = CompositionOffset::from_wire(0, 0xffff_ffff).unwrap();
    let signed = CompositionOffset::from_wire(1, 0xffff_ffff).unwrap();
    assert_eq!(unsigned.raw_bits(), signed.raw_bits());
    assert_eq!(unsigned.ticks(), 4_294_967_295);
    assert_eq!(signed.ticks(), -1);
    assert_eq!(signed.signed_value(), Some(-1));
    assert_eq!(unsigned.signed_value(), None);
    assert_eq!(signed.version(), 1);
    assert_eq!(
        CompositionOffset::from_wire(1, 0x8000_0000)
            .unwrap()
            .ticks(),
        -2_147_483_648
    );
    assert!(CompositionOffset::from_wire(2, 0).is_err());
}

#[test]
fn headers_are_incremental_and_u64_endpoints_are_checked() {
    for n in 0..8 {
        assert!(matches!(
            BoxHeader::parse(&[0; 8][..n], 0, None, true),
            Ok(HeaderRead::Need(8))
        ));
    }
    let mut data = [0u8; 16];
    data[..4].copy_from_slice(&1u32.to_be_bytes());
    data[4..8].copy_from_slice(b"mdat");
    data[8..].copy_from_slice(&0x1_0000_0040u64.to_be_bytes());
    for n in 8..16 {
        assert!(matches!(
            BoxHeader::parse(&data[..n], 20, None, true),
            Ok(HeaderRead::Need(16))
        ));
    }
    let HeaderRead::Ready(header) = BoxHeader::parse(&data, 20, None, true).unwrap() else {
        panic!()
    };
    assert_eq!(header.end().unwrap(), Some(0x1_0000_0054));
    assert!(matches!(
        BoxHeader::parse(&data, u64::MAX - 8, None, true),
        Err(Error::Overflow)
    ));
    assert!(BoxHeader::parse(&data, 0, Some(12), true).is_err());
    let terminal = [0, 0, 0, 0, b'm', b'd', b'a', b't'];
    let HeaderRead::Ready(open) = BoxHeader::parse(&terminal, 30, None, true).unwrap() else {
        panic!()
    };
    assert_eq!(open.end().unwrap(), None);
    assert!(BoxHeader::parse(&terminal, 0, Some(8), false).is_err());
}

#[test]
fn child_work_yield_does_not_skip_a_box() {
    let budget = Budget::new(4096).unwrap();
    let data = [
        0, 0, 0, 16, b'm', b'o', b'o', b'v', 0, 0, 0, 8, b'f', b'r', b'e', b'e',
    ];
    let parent = BoxView::new(common::bytes(&budget, &data), 100).unwrap();
    let mut children = parent.children(0, Limits::default()).unwrap();
    assert!(matches!(
        children.poll(&mut WorkBudget::new(0)).unwrap(),
        BoxStep::Yield
    ));
    match children.poll(&mut WorkBudget::new(1)).unwrap() {
        BoxStep::Box(b) => assert_eq!(b.header().start, 108),
        _ => panic!(),
    }
    assert!(matches!(
        children.poll(&mut WorkBudget::new(1)).unwrap(),
        BoxStep::End
    ));
}

#[test]
fn retained_sample_entry_keeps_the_shared_core_charge_alive() {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == "r-private-carriage")
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let baseline = budget.used();
    let retained = {
        let top = common::roots(&budget, case.bytes).unwrap();
        let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let track = common::child(movie, b"trak").unwrap();
        let stbl = common::child(
            &common::child(&common::child(&track, b"mdia").unwrap(), b"minf").unwrap(),
            b"stbl",
        )
        .unwrap();
        let table = common::table(stbl, &budget, 1).unwrap();
        let entry = table.descriptions()[0].raw().clone();
        let mut children = table.descriptions()[0]
            .extensions(triergon_isobmff::EntryLayout::Visual, Limits::default())
            .unwrap();
        let mut count = 0;
        loop {
            match children.poll(&mut WorkBudget::new(1)).unwrap() {
                BoxStep::Box(_) => count += 1,
                BoxStep::Yield => {}
                BoxStep::End => break,
            }
        }
        assert_eq!(count, 11);
        entry
    };
    assert!(budget.used() > baseline);
    assert!(retained.bytes().retained_capacity() >= case.bytes.len());
    assert!(retained.bytes().belongs_to(&budget));
    drop(retained);
    assert_eq!(budget.used(), baseline);
}
