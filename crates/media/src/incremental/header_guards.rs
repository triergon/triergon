//! A buffered header read at once must fail exactly where the byte-wise read
//! fails when the scan allowance ends inside the header (independent review).
use super::*;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};

fn failure_position(scan: u64) -> Option<u64> {
    let data = [
        0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0x01, 0xff,
    ];
    let pool = Budget::new(1 << 20).unwrap();
    let limits = Limits {
        scan_bytes: scan,
        ..Limits::default()
    };
    let caps = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let mut reader = ElementReader::new(&pool, SourceId(9), caps, limits, 4096).unwrap();
    let cancel = AtomicBool::new(false);
    for _ in 0..20 {
        match reader.step(&mut WorkBudget::new(1000), &cancel) {
            Ok(ParseStep::NeedInput(request)) => {
                let bytes =
                    Bytes::copy_from_slice(&pool, &data[request.offset as usize..]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(request.source, request.token, request.offset, bytes)
                            .unwrap(),
                    )
                    .unwrap();
            }
            Err(Error::Policy("operation read bytes")) => return Some(reader.position()),
            Err(other) => panic!("unexpected {other:?}"),
            Ok(_) => (),
        }
    }
    None
}

#[test]
fn scan_limit_inside_a_buffered_header_fails_at_the_byte_wise_position() {
    // Recorded from the preimage's byte-wise header reads.
    let expected = [
        Some(1),
        Some(5),
        Some(5),
        Some(5),
        Some(5),
        Some(6),
        Some(10),
        Some(10),
        Some(10),
        Some(10),
        Some(11),
        None,
    ];
    for (scan, want) in expected.into_iter().enumerate() {
        assert_eq!(failure_position(scan as u64), want, "scan {scan}");
    }
}

#[test]
fn oversized_growth_reports_the_pool_limit_not_overflow() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut vec: BudgetVec<u8> = BudgetVec::new(&pool);
    vec.try_extend_from_slice(&[1; 16]).unwrap();
    assert!(matches!(
        vec.try_reserve_exact(usize::MAX - 16),
        Err(triergon_core::Error::Limit { .. })
    ));
}
