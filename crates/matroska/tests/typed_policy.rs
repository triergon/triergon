//! Resource/work and cancellation regressions for retained typed operations.
#[path = "support/deflate.rs"]
mod deflate;
#[path = "support/ebml.rs"]
mod ebml;
use ebml::{element as e, mkv};
use std::sync::atomic::AtomicBool;
use triergon_core::Step;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};
use triergon_matroska::{Error, Event, Limits, Reader};

#[test]
fn an_atomic_frame_larger_than_its_own_pool_minimum_is_a_permanent_limit_not_an_endless_wait() {
    let (compressed, _) = deflate::repeated(128);
    let encoding = e(
        0x6d80,
        &e(
            0x6240,
            &[e(0x5032, &[1]), e(0x5034, &e(0x4254, &[0]))].concat(),
        ),
    );
    let track = e(
        0xae,
        &[
            e(0xd7, &[1]),
            e(0x73c5, &[1]),
            e(0x83, &[1]),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            encoding,
        ]
        .concat(),
    );
    let block = [&[0x81, 0, 0, 0x80][..], compressed.as_slice()].concat();
    let raw = mkv(&[
        e(0x1654ae6b, &track),
        e(0x1f43b675, &[e(0xe7, &[0]), e(0xa3, &block)].concat()),
    ]
    .concat());
    let pool = Budget::new(4096).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(202),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        Limits::default(),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut limit = None;
    let mut pressure = 0;
    for _ in 0..20_000 {
        let mut work = WorkBudget::new(64);
        match reader.step(&mut work, &cancel) {
            Ok(Step::Yield) => (),
            Ok(Step::Ready(Event::Packet(_))) => {
                panic!("the complete decoded frame cannot fit this pool")
            }
            Ok(Step::Ready(_)) => (),
            Ok(Step::NeedInput(request)) => {
                let at = request.offset as usize;
                if at == raw.len() {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Ok(Step::Backpressure(_)) => pressure += 1,
            Err(Error::Policy(message)) => {
                limit = Some(message);
                break;
            }
            other => panic!("valid compressed source must retain failure distinctions: {other:?}"),
        }
        assert!(work.spent() <= 64);
    }
    assert_eq!(
        limit,
        Some("operation retained storage"),
        "no caller retains a packet/configuration or reservation; repeated own-storage pressure ({pressure} polls) must be permanent"
    );
}
