use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent,
    clock::SourceTimePolicy,
    psi::{DescriptorNamespace, DvbNamespaceEvidence, PsiCrc32},
};
use triergon_playback::{
    ByteSource, Engine, Output, Player, PlayerPolicy, Progress, RawEvent, Selection, TsEvent,
    dvb_ts::{DvbQueueAction, DvbServiceSelection, DvbStep, DvbTsBridge},
    io::SourceOwner,
};
use triergon_subtitles::dvb::Size;

const PACKET_LIMIT: usize = 1 << 20;

/// A player over `source` and its outputs: the active PMT and the PES packets.
fn play(
    budget: &Budget,
    source: &[u8],
    read: usize,
    policy: SourceTimePolicy,
) -> (Player<ByteSource>, Option<Output>, Vec<Output>) {
    let id = SourceId(331);
    let input = InputCursor::new(
        budget,
        id,
        SourceCapabilities {
            length: Some(source.len() as u64),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    let engine = Engine::ts_with_namespace(
        input,
        PacketFormat::Ts188,
        1,
        ParseLimits::default(),
        policy,
        DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
    )
    .unwrap();
    let source =
        ByteSource::new(id, Bytes::copy_from_slice(budget, source).unwrap(), read).unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let (mut map, mut pes) = (None, Vec::new());
    for _ in 0..100_000 {
        let state = player.advance(&mut WorkBudget::new(17), &cancel).unwrap();
        while let Some(output) = player.pop() {
            match output.event() {
                RawEvent::Ts(TsEvent {
                    source: TraceEvent::ProgramMap { active: true, .. },
                    ..
                }) => map = Some(output),
                RawEvent::Ts(TsEvent {
                    source: TraceEvent::Pes(_),
                    ..
                }) => pes.push(output),
                _ => {}
            }
        }
        if matches!(state, Progress::Ended) {
            break;
        }
    }
    (player, map, pes)
}

/// What one PES produced, polled one work unit at a time.
enum Seen {
    Show {
        start_ns: i64,
        canvas: Option<Size>,
        pixels: Vec<u8>,
    },
    Clear(i64),
    Nothing,
}

fn decode(bridge: &mut DvbTsBridge, pes: Output) -> Seen {
    let cancel = AtomicBool::new(false);
    assert!(bridge.push(&mut Some(pes)).unwrap());
    for polls in 0..100_000 {
        match bridge.poll(&mut WorkBudget::new(1), &cancel).unwrap() {
            DvbStep::Yield => {}
            DvbStep::NeedPacket => return Seen::Nothing,
            DvbStep::Action(DvbQueueAction::Clear { at_ns }) => return Seen::Clear(at_ns),
            DvbStep::Action(DvbQueueAction::Show(picture)) => {
                let pixels = picture.rectangles().next().unwrap().pixels.to_vec();
                // The decoder's work, at least a unit per region pixel, was
                // spread over the polls.
                assert!(polls >= pixels.len(), "{polls} polls");
                return Seen::Show {
                    start_ns: picture.start_ns,
                    canvas: picture.canvas,
                    pixels,
                };
            }
            DvbStep::Finished => panic!("finished before the pushed PES"),
        }
    }
    panic!("bounded PES decode")
}

#[test]
fn selected_ts_service_reaches_the_bitmap_queue() {
    let source = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let split = split_pes(source);
    let (multiple, _) = multiple_services(source);
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let truncated = include_bytes!("../../mpegts/tests/data/dvb/dvb-truncated-segment.ts");
    let empty_page = [
        0x20, 0x00, // PES data identifier and subtitle stream id
        0x0f, 0x10, 0, 1, 0, 2, 2, 0x0b, // acquisition page with no regions
        0x0f, 0x80, 0, 1, 0, 0, // end of display set
        0xff,
    ];
    let clear = replace_pes_data(source, &empty_page);
    let relative = SourceTimePolicy::RelativeEpochZero;
    let selected = DvbServiceSelection {
        descriptor: 1,
        entry: 1,
    };
    let cancel = AtomicBool::new(false);
    for (source, read, policy, service, show) in [
        (source.as_slice(), 37, relative, None, true),
        (&split, 4096, SourceTimePolicy::Unanchored, None, true),
        (&multiple, 37, relative, Some(selected), true),
        (truncated.as_slice(), 37, relative, None, true),
        (&clear, 37, relative, None, false),
    ] {
        let budget = Budget::new(16 * 1024 * 1024).unwrap();
        let (player, map, mut pes) = play(&budget, source, read, policy);
        let map = map.expect("active DVB PMT");
        let mut bridge = match service {
            Some(service) => {
                assert!(DvbTsBridge::new(&map, 256, PACKET_LIMIT, &cancel).is_err());
                for wrong in [
                    DvbServiceSelection {
                        descriptor: 2,
                        entry: 0,
                    },
                    DvbServiceSelection {
                        descriptor: 1,
                        entry: 2,
                    },
                ] {
                    assert!(
                        DvbTsBridge::new_for_service(&map, 256, wrong, PACKET_LIMIT, &cancel)
                            .is_err()
                    );
                }
                DvbTsBridge::new_for_service(&map, 256, service, PACKET_LIMIT, &cancel).unwrap()
            }
            None => DvbTsBridge::new(&map, 256, PACKET_LIMIT, &cancel).unwrap(),
        };
        assert_eq!(
            bridge.configuration().language_bytes(),
            Some(b"eng".as_slice())
        );
        assert_eq!(pes.len(), 1);
        let unanchored = policy == SourceTimePolicy::Unanchored;
        match decode(&mut bridge, pes.remove(0)) {
            Seen::Show {
                start_ns,
                canvas,
                pixels,
            } => {
                assert!(show);
                assert_eq!(start_ns, if unanchored { 0 } else { 1_000_000_000 });
                assert_eq!(
                    canvas,
                    Some(Size {
                        width: 1024,
                        height: 768
                    })
                );
                if source != truncated.as_slice() {
                    assert_eq!(pixels, indices);
                }
            }
            Seen::Clear(at_ns) => {
                assert!(!show);
                assert_eq!(at_ns, 1_000_000_000);
            }
            Seen::Nothing => panic!("no subtitle"),
        }
        bridge.finish_input();
        assert!(matches!(
            bridge.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
            DvbStep::Finished
        ));
        drop(bridge);
        drop(map);
        drop(player);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

// Preserve the fixture's selected PID and PES clock while changing its data field.
fn replace_pes_data(source: &[u8; 1504], data: &[u8]) -> [u8; 1504] {
    const TS: usize = 188;
    let original = &source[2 * TS..3 * TS];
    let start = 5 + usize::from(original[4]);
    let header_len = 9 + usize::from(original[start + 8]);
    let mut bytes = *source;
    let packet = &mut bytes[2 * TS..3 * TS];
    let new_start = TS - header_len - data.len();
    packet[4..new_start].fill(0xff);
    packet[4] = (new_start - 5) as u8;
    packet[5] = 0;
    packet[new_start..new_start + header_len].copy_from_slice(&original[start..start + header_len]);
    let pes_len = u16::try_from(header_len + data.len() - 6).unwrap();
    packet[new_start + 4..new_start + 6].copy_from_slice(&pes_len.to_be_bytes());
    packet[new_start + header_len..].copy_from_slice(data);
    bytes
}

// Split the fixture's one PES across its original TS packet and the following
// null packet, retaining the complete PES header and data field byte for byte.
fn split_pes(source: &[u8; 1504]) -> [u8; 1504] {
    const TS: usize = 188;
    let original = &source[2 * TS..3 * TS];
    let payload_start = 5 + usize::from(original[4]);
    assert_eq!(source[3 * TS + 1] & 0x1f, 0x1f);
    let (first, second) = original[payload_start..].split_at(80);
    let mut split = *source;
    for (index, part) in [first, second].into_iter().enumerate() {
        let packet = &mut split[(2 + index) * TS..(3 + index) * TS];
        let start = TS - part.len();
        packet.fill(0xff);
        packet[..4].copy_from_slice(&original[..4]);
        if index == 1 {
            packet[1] &= !0x40;
            packet[3] = (packet[3] & 0xf0) | ((packet[3] + 1) & 0x0f);
        }
        packet[4] = (start - 5) as u8;
        packet[5] = 0;
        packet[start..].copy_from_slice(part);
    }
    split
}

// Prepend another subtitle descriptor and unrelated ES metadata, then put
// the original service after an unrelated entry in the selected descriptor.
fn multiple_services(source: &[u8; 1504]) -> ([u8; 1504], [u8; 18]) {
    const TS: usize = 188;
    let mut bytes = *source;
    let packet = &mut bytes[TS..2 * TS];
    let old_start = 6 + usize::from(packet[4]);
    let mut section = packet[old_start..old_start + 17].to_vec();
    let mut selected = [
        0x59, 16, b'd', b'e', b'u', 0x14, 0, 5, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    selected[10..].copy_from_slice(&packet[old_start + 19..old_start + 27]);
    section.extend_from_slice(&[0x52, 1, 7, 0x59, 8, b'f', b'r', b'a', 0x14, 0, 3, 0, 4]);
    section.extend_from_slice(&selected);
    section[2] = u8::try_from(section.len() + 1).unwrap();
    section[16] = u8::try_from(section.len() - 17).unwrap();
    section.extend_from_slice(&PsiCrc32::of(&section).to_be_bytes());

    let start = TS - section.len();
    packet[4] = u8::try_from(start - 6).unwrap();
    packet[start - 1] = 0;
    packet[start..].copy_from_slice(&section);
    (bytes, selected)
}

#[test]
fn ts_bridge_accepts_a_continuation_after_a_refused_or_partial_pes() {
    let template = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let cancel = AtomicBool::new(false);
    for cut in [4, 25] {
        let source = pes_continuation(template, pes, cut);
        let pool = Budget::new(16 * 1024 * 1024).unwrap();
        let (_player, map, inputs) = play(&pool, &source, 23, SourceTimePolicy::RelativeEpochZero);
        let mut bridge = DvbTsBridge::new(&map.unwrap(), 256, PACKET_LIMIT, &cancel).unwrap();
        assert_eq!(inputs.len(), 2);
        let mut shown = 0;
        for input in inputs {
            match decode(&mut bridge, input) {
                Seen::Show {
                    start_ns, pixels, ..
                } => {
                    assert_eq!(start_ns, 1_000_000_000);
                    assert_eq!(pixels, indices);
                    shown += 1;
                }
                Seen::Nothing => {}
                Seen::Clear(_) => panic!("unexpected clear"),
            }
        }
        assert_eq!(shown, 1);
        bridge.finish_input();
        assert!(matches!(
            bridge.poll(&mut WorkBudget::new(1), &cancel).unwrap(),
            DvbStep::Finished
        ));
    }
}

/// A PES the pool cannot hold, at whatever point it runs short, waits in the
/// bridge and is decoded once the pool has room.
#[test]
fn ts_bridge_retries_a_pes_refused_for_memory() {
    let source = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let cancel = AtomicBool::new(false);
    let mut refusals = 0;
    for room in (0..8192).step_by(64) {
        let pool = Budget::new(16 * 1024 * 1024).unwrap();
        let (_player, map, mut inputs) =
            play(&pool, source, 37, SourceTimePolicy::RelativeEpochZero);
        let mut bridge = DvbTsBridge::new(&map.unwrap(), 256, PACKET_LIMIT, &cancel).unwrap();
        assert!(bridge.push(&mut Some(inputs.remove(0))).unwrap());
        let taken = pool.reserve(pool.available() - room).unwrap();
        let mut taken = Some(taken);
        let pixels = loop {
            match bridge.poll(&mut WorkBudget::new(u64::MAX), &cancel) {
                Ok(DvbStep::Action(DvbQueueAction::Show(picture))) => {
                    break picture.rectangles().next().unwrap().pixels.to_vec();
                }
                Ok(DvbStep::Yield) => {}
                Err(triergon_core::Error::Backpressure { .. }) => {
                    assert!(taken.take().is_some(), "refused with room, at {room}");
                    refusals += 1;
                }
                other => panic!("{other:?} at {room}"),
            }
        };
        assert_eq!(pixels, indices);
    }
    assert!(refusals > 1);
}

/// A PES whose regions the pool could never hold is refused with `Limit` and
/// dropped, and the bridge decodes the next one.
#[test]
fn ts_bridge_drops_a_pes_larger_than_its_pool() {
    const TS: usize = 188;
    let source = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let page = [pes[4], pes[5]];
    let segment = |kind: u8, body: &[u8]| {
        let mut bytes = vec![0x0f, kind, page[0], page[1]];
        bytes.extend((body.len() as u16).to_be_bytes());
        bytes.extend(body);
        bytes
    };
    // A page placing region 0, a 1280 x 1024 4-bit region, and the end.
    let mut data = vec![0x20, 0x00];
    data.extend(segment(0x10, &[5, 0x0b, 0, 0, 0, 0, 0, 0]));
    data.extend(segment(0x11, &[0, 0, 5, 0, 4, 0, 2 << 5 | 2 << 2, 0, 0, 0]));
    data.extend(segment(0x80, &[]));
    data.push(0xff);
    let mut large = replace_pes_data(source, &data);
    // An earlier time, 128 ticks before: a PES with the same time continues
    // the one before.
    let packet = &mut large[2 * TS..3 * TS];
    let start = packet
        .windows(4)
        .position(|w| w == [0, 0, 1, 0xbd])
        .unwrap();
    packet[start + 12] -= 1;
    // The large PES, then the original with the next continuity counter.
    let mut next = source[2 * TS..3 * TS].to_vec();
    next[3] = next[3] & 0xf0 | (next[3] + 1) & 0x0f;
    let two = [
        &source[..2 * TS],
        &large[2 * TS..3 * TS],
        &next,
        &source[3 * TS..],
    ]
    .concat();
    let cancel = AtomicBool::new(false);
    let pool = Budget::new(1 << 20).unwrap();
    let (_player, map, mut inputs) = play(&pool, &two, 37, SourceTimePolicy::RelativeEpochZero);
    let mut bridge = DvbTsBridge::new(&map.unwrap(), 256, PACKET_LIMIT, &cancel).unwrap();
    assert_eq!(inputs.len(), 2);
    assert!(bridge.push(&mut Some(inputs.remove(0))).unwrap());
    let mut refused = false;
    loop {
        match bridge.poll(&mut WorkBudget::new(u64::MAX), &cancel) {
            Ok(DvbStep::Yield) => {}
            Err(triergon_core::Error::Limit { .. }) => refused = true,
            Ok(DvbStep::NeedPacket) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(refused);
    match decode(&mut bridge, inputs.remove(0)) {
        Seen::Show { pixels, .. } => assert_eq!(pixels, indices),
        _ => panic!("the next PES presents nothing"),
    }
}

fn pes_continuation(template: &[u8; 1504], pes: &[u8], cut: usize) -> [u8; 1504] {
    const TS: usize = 188;
    let mut result = replace_pes_data(template, &pes[..cut]);
    let first = &result[2 * TS..3 * TS];
    let mut header: [u8; 4] = first[..4].try_into().unwrap();
    header[3] = (header[3] & 0xf0) | ((header[3] + 1) & 0x0f);
    let second = &mut result[3 * TS..4 * TS];
    let body = &pes[cut..];
    let start = TS - 9 - body.len();
    second[..4].copy_from_slice(&header);
    second[4..start].fill(0xff);
    second[4] = (start - 5) as u8;
    second[5] = 0;
    second[start..start + 4].copy_from_slice(&[0, 0, 1, 0xbd]);
    second[start + 4..start + 6].copy_from_slice(&((body.len() as u16 + 3).to_be_bytes()));
    second[start + 6..start + 9].copy_from_slice(&[0x80, 0, 0]);
    second[start + 9..].copy_from_slice(body);
    result
}
