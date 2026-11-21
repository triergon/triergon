use super::*;
use crate::Reader;
use triergon_core::container::{Event, PacketInfo};

#[test]
fn coalescing_freeze_and_packet_publication_retry_without_duplicate_bytes_or_origins() {
    let raw = include_bytes!("../../../../playback/tests/fixtures/hls/baseline-stereo.ts");
    let pool = Budget::new(32 << 20).unwrap();
    let mut reader = Reader::new(
        &pool,
        SourceId(301),
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        ReaderLimits::default(),
        4096,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut checkpoints = 0;
    let mut expected = Vec::new();
    for _ in 0..1_000_000 {
        let ready = reader.lanes.as_slice().iter().find_map(|lane| {
            let build = lane.build.as_ref()?;
            let BuildUnit::Packet(AccessUnit::Video(unit)) = &build.unit else {
                return None;
            };
            let done = if checkpoints == 0 {
                build.bytes.is_none() && build.nal == unit.nals().len()
            } else {
                build.bytes.is_some() && build.origin == lane.pieces.len()
            };
            done.then(|| unit.segments().flatten().copied().collect::<Vec<_>>())
        });
        if checkpoints < 2
            && let Some(bytes) = ready
        {
            if expected.is_empty() {
                expected = bytes;
            } else {
                assert_eq!(expected, bytes);
            }
            let pressure = pool.reserve(pool.available()).unwrap();
            let mut refused = false;
            for _ in 0..10_000 {
                match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
                    Step::Yield => (),
                    Step::Backpressure(_) => {
                        refused = true;
                        break;
                    }
                    other => panic!("pending publication advanced under pressure: {other:?}"),
                }
            }
            assert!(refused);
            drop(pressure);
            checkpoints += 1;
        }
        match reader.step(&mut WorkBudget::new(1), &cancel).unwrap() {
            Step::Ready(Event::Packet(packet)) if checkpoints != 0 => {
                assert_eq!(checkpoints, 2);
                assert_eq!(packet.payload(), expected);
                assert_eq!(
                    packet
                        .sources()
                        .iter()
                        .map(|source| source.payload_range.len())
                        .sum::<usize>(),
                    expected.len()
                );
                return;
            }
            Step::NeedInput(r) => {
                let at = r.offset as usize;
                if at == raw.len() {
                    reader.finish(r.source, r.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(
                        &pool,
                        &raw[at..raw.len().min(at + r.maximum.min(7))],
                    )
                    .unwrap();
                    reader
                        .provide(&InputWindow::new(r.source, r.token, r.offset, bytes).unwrap())
                        .unwrap();
                }
            }
            Step::Yield | Step::Ready(_) => (),
            other => panic!("publication retry ended early: {other:?}"),
        }
    }
    panic!("publication checkpoints not reached");
}
