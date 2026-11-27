use crate::support::players::*;
use crate::support::*;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::ByteSource;
use triergon_playback::{SourceOwner, *};

#[test]
fn real_ts_entity_transition_preserves_partial_pes_and_requires_explicit_logical_eof() {
    let a = include_bytes!("../fixtures/ts/hls-contiguous-part-a.ts");
    let bbytes = include_bytes!("../fixtures/ts/hls-contiguous-part-b.ts");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(a.len() as u64),
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = Engine::ts_entities(
        input,
        PacketFormat::Ts188,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        triergon_mpegts::psi::DescriptorNamespace::Unspecified,
    )
    .unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, a).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let mut out = Vec::new();
    let mut boundaries = 0;
    for _ in 0..200_000 {
        let step = player.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while let Some(o) = player.pop() {
            out.push(o);
        }
        if matches!(step, Progress::AwaitEntity) {
            boundaries += 1;
            if boundaries == 1 {
                assert!(pes(&out).is_empty());
                let id = SourceId(ID.0 + 1);
                let mut next = Some(
                    ByteSource::new(id, Bytes::copy_from_slice(&b, bbytes).unwrap(), 7).unwrap(),
                );
                assert!(
                    !player
                        .continue_entity(
                            id,
                            SourceCapabilities {
                                length: Some(bbytes.len() as u64),
                                random_access: false
                            },
                            &mut next,
                            false,
                            &mut WorkBudget::new(0),
                            &off()
                        )
                        .unwrap()
                );
                assert!(next.is_some());
                assert!(
                    player
                        .continue_entity(
                            id,
                            SourceCapabilities {
                                length: Some(bbytes.len() as u64),
                                random_access: false
                            },
                            &mut next,
                            false,
                            &mut WorkBudget::new(1),
                            &off()
                        )
                        .unwrap()
                );
                assert!(next.is_none());
            } else {
                assert!(
                    player
                        .finish_logical_source(&mut WorkBudget::new(1), &off())
                        .unwrap()
                );
            }
        }
        if matches!(step, Progress::Ended) {
            break;
        }
    }
    assert_eq!(boundaries, 2);
    let packets = pes(&out);
    assert_eq!(packets.len(), 1);
    let TraceEvent::Pes(p) = &packets[0].source else {
        panic!()
    };
    assert_eq!(
        p.payload().flatten().copied().collect::<Vec<_>>(),
        (0_u8..=255).cycle().take(512).collect::<Vec<_>>()
    );
    assert!(p.source_ranges().any(|r| r.entity == ID));
    assert!(p.source_ranges().any(|r| r.entity == SourceId(ID.0 + 1)));
    assert_eq!(
        out.iter()
            .filter(|o| matches!(o.event(), Event::EntityEnd(_)))
            .count(),
        2
    );
    drop(out);
    drop(player);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
