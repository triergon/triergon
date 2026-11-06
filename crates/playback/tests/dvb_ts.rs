use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, Time, WorkBudget};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent,
    clock::SourceTimePolicy,
    psi::{DescriptorNamespace, DvbNamespaceEvidence, PsiCrc32},
};
use triergon_playback::{
    ByteSource, Engine, Player, PlayerPolicy, Progress, RawEvent, Selection, TsEvent,
    dvb_ts::{DvbQueueAction, DvbServiceSelection, DvbTsBridge},
    io::SourceOwner,
};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, DisplayAction, DisplayLimits, PublicationReason,
    ReceiverView, Size,
};

#[test]
fn selected_ts_service_reaches_the_dvb_renderer_with_source_bytes_and_clocks() {
    let source = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let split = split_pes(source);
    let (multiple, selected_descriptor) = multiple_services(source);
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let truncated_source = include_bytes!("../../mpegts/tests/data/dvb/dvb-truncated-segment.ts");
    let truncated_pes =
        include_bytes!("../../mpegts/tests/data/dvb/dvb-truncated-segment.pes-data");
    let truncated_codec =
        include_bytes!("../../subtitles/tests/native-dvb-fixtures/input/prefix101.dvb");
    let empty_page = [
        0x20, 0x00, // PES data identifier and subtitle stream id
        0x0f, 0x10, 0, 1, 0, 2, 2, 0x0b, // acquisition page with no regions
        0x0f, 0x80, 0, 1, 0, 0, // end of display set
        0xff,
    ];
    let clear = replace_pes_data(source, &empty_page);
    let cancel = AtomicBool::new(false);
    let id = SourceId(331);
    let cases = [
        (
            source.as_slice(),
            1,
            37,
            SourceTimePolicy::RelativeEpochZero,
            Some(Time::new(1, 1).unwrap()),
            None,
        ),
        (&split, 2, 4096, SourceTimePolicy::Unanchored, None, None),
        (
            &multiple,
            1,
            37,
            SourceTimePolicy::RelativeEpochZero,
            Some(Time::new(1, 1).unwrap()),
            Some(DvbServiceSelection {
                descriptor: 1,
                entry: 1,
            }),
        ),
    ];
    let clear_case = (
        clear.as_slice(),
        1,
        37,
        SourceTimePolicy::RelativeEpochZero,
        Some(Time::new(1, 1).unwrap()),
        None,
    );
    let truncated_case = (
        truncated_source.as_slice(),
        1,
        37,
        SourceTimePolicy::RelativeEpochZero,
        Some(Time::new(1, 1).unwrap()),
        None,
    );
    for ((source, spans, read, policy, time, service), view, expected_pes, expected_codec) in cases
        .into_iter()
        .flat_map(|case| {
            [ReceiverView::Source, ReceiverView::NativeD32]
                .map(|view| (case, view, pes.as_slice(), None))
        })
        .chain([
            (
                clear_case,
                ReceiverView::NativeD32,
                empty_page.as_slice(),
                None,
            ),
            (
                cases[0],
                ReceiverView::NativeD32,
                pes.as_slice(),
                Some(&pes[2..pes.len() - 1]),
            ),
            (
                truncated_case,
                ReceiverView::NativeD32,
                truncated_pes.as_slice(),
                Some(truncated_codec.as_slice()),
            ),
        ])
    {
        let budget = Budget::new(16 * 1024 * 1024).unwrap();
        let input = InputCursor::new(
            &budget,
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
            ByteSource::new(id, Bytes::copy_from_slice(&budget, source).unwrap(), read).unwrap();
        let mut player = Player::new(
            engine,
            SourceOwner::new(source),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let mut bridge = None;
        let mut mailbox = None;
        let mut backing = None;
        let mut descriptor_backing = None;
        for _ in 0..100_000 {
            let state = player.advance(&mut WorkBudget::new(17), &cancel).unwrap();
            while let Some(output) = player.pop() {
                match output.event() {
                    RawEvent::Ts(TsEvent {
                        source:
                            TraceEvent::ProgramMap {
                                active: true, map, ..
                            },
                        ..
                    }) => {
                        descriptor_backing =
                            Some(map.streams.as_slice()[0].descriptor_bytes.clone());
                        let limits = DisplayLimits::default();
                        let bridge_for = |service| {
                            DvbTsBridge::new_for_service(
                                &output, 256, service, limits, view, &cancel,
                            )
                        };
                        bridge = Some(if expected_codec.is_some() {
                            DvbTsBridge::new_with_native_pes_parser(
                                &output, 256, service, limits, &cancel,
                            )
                            .unwrap()
                        } else if let Some(service) = service {
                            assert!(DvbTsBridge::new(&output, 256, limits, view, &cancel).is_err());
                            assert!(
                                bridge_for(DvbServiceSelection {
                                    descriptor: 2,
                                    entry: 0
                                })
                                .is_err()
                            );
                            assert!(
                                bridge_for(DvbServiceSelection {
                                    descriptor: 1,
                                    entry: 2
                                })
                                .is_err()
                            );
                            bridge_for(service).unwrap()
                        } else {
                            DvbTsBridge::new(&output, 256, limits, view, &cancel).unwrap()
                        });
                    }
                    RawEvent::Ts(TsEvent {
                        source: TraceEvent::Pes(packet),
                        ..
                    }) => {
                        assert_eq!(packet.spans().len(), spans);
                        backing = Some(packet.spans()[0].backing_bytes().clone());
                        assert!(mailbox.replace(output).is_none());
                    }
                    _ => {}
                }
            }
            if matches!(state, Progress::Ended) {
                break;
            }
        }
        let mut bridge = bridge.expect("active DVB PMT");
        assert!(bridge.push(&mut mailbox).unwrap());
        assert!(mailbox.is_none());
        assert_eq!(
            bridge
                .poll(&mut WorkBudget::new(0), &cancel)
                .unwrap()
                .status,
            DecodeStatus::Yield,
        );
        bridge.finish_input();
        let mut seen = 0;
        let mut finished = false;
        for _ in 0..100_000 {
            let next = bridge.poll(&mut WorkBudget::new(1), &cancel).unwrap();
            match next.status {
                DecodeStatus::Yield => {}
                DecodeStatus::Event => {
                    let mut event = next.event.unwrap();
                    assert_eq!(event.pts, time);
                    let packet = &event.source_packets()[0];
                    assert_eq!(packet.payload.as_ref(), expected_pes);
                    assert_eq!(
                        packet
                            .payload
                            .shares_storage_with(backing.as_ref().unwrap()),
                        spans == 1,
                    );
                    let facts = packet.source.as_ref().unwrap();
                    assert_eq!(
                        facts.configuration().language_bytes(),
                        Some(b"eng".as_slice())
                    );
                    let descriptor = facts.configuration().original();
                    assert!(descriptor.shares_storage_with(descriptor_backing.as_ref().unwrap()));
                    if service.is_some() {
                        assert_eq!(descriptor.as_ref(), &selected_descriptor);
                    }
                    assert_eq!(facts.container_position().unwrap().source, id);
                    assert_eq!(
                        facts.timing().pts.map(|pts| pts.ticks),
                        time.map(|_| 90_000)
                    );
                    assert!(facts.timing().dts.is_none());
                    if let Some(codec) = expected_codec {
                        let parsed = event.native_pes_packet().unwrap();
                        assert_eq!(parsed.codec().as_ref(), codec);
                        assert!(parsed.pes().shares_storage_with(&packet.payload));
                        assert!(parsed.codec().shares_storage_with(&packet.payload));
                        assert_eq!(parsed.codec_pts(), Some(90_000));
                        let publication = event.native_publication.unwrap();
                        assert_eq!(publication.consumed_prefix_bytes, codec.len());
                        if expected_pes == truncated_pes {
                            assert_eq!(publication.reason, PublicationReason::CompletePrefix);
                            let Some(DvbQueueAction::Show(picture)) =
                                bridge.packet_clock_action(&event).unwrap()
                            else {
                                panic!("expected native picture from the complete PES prefix");
                            };
                            assert_eq!(picture.start_ns, 1_000_000_000);
                            assert!(picture.rectangles().next().is_some());
                            seen += 1;
                            continue;
                        }
                    } else {
                        assert!(event.native_pes_packet().is_none());
                    }
                    if expected_pes == empty_page {
                        assert_eq!(event.action, DisplayAction::Clear);
                        assert!(event.regions().is_empty());
                        assert!(matches!(
                            bridge.packet_clock_action(&event).unwrap(),
                            Some(DvbQueueAction::Clear {
                                at_ns: 1_000_000_000
                            })
                        ));
                        seen += 1;
                        continue;
                    }
                    assert_eq!(event.action, DisplayAction::Present);
                    assert_eq!(event.regions()[0].plane.pixels.as_ref(), indices);
                    if view == ReceiverView::NativeD32 {
                        let Some(DvbQueueAction::Show(picture)) =
                            bridge.packet_clock_action(&event).unwrap()
                        else {
                            panic!("expected native queue picture");
                        };
                        assert_eq!(picture.start_ns, time.map_or(0, |_| 1_000_000_000));
                        assert_eq!(
                            picture.canvas,
                            Some(Size {
                                width: 1024,
                                height: 768
                            })
                        );
                        let region = picture.rectangles().next().unwrap();
                        assert!(std::ptr::eq(region, &raw const event.regions()[0]));
                        assert_eq!(region.plane.pixels.as_ref(), indices);
                        assert!(region.palette.native_d32.is_some());
                        let (_, timing) = event
                            .native_publication
                            .as_mut()
                            .unwrap()
                            .packet_clock
                            .as_mut()
                            .unwrap();
                        timing.dts = Some(ClockValue {
                            ticks: -45_001,
                            origin: ClockOrigin::Source,
                        });
                        timing.pts = None;
                        let Some(DvbQueueAction::Show(picture)) =
                            bridge.packet_clock_action(&event).unwrap()
                        else {
                            panic!("expected native queue picture with DTS");
                        };
                        assert_eq!(picture.start_ns, -500_011_111);
                    } else {
                        assert!(matches!(
                            bridge.packet_clock_action(&event),
                            Err(triergon_core::Error::Unsupported(_))
                        ));
                    }
                    seen += 1;
                }
                DecodeStatus::Finished => {
                    finished = true;
                    break;
                }
                other => panic!("unexpected DVB output {other:?}"),
            }
        }
        assert!(finished);
        assert_eq!(seen, 1);
        drop(bridge);
        drop(player);
        drop(backing);
        drop(descriptor_backing);
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
fn native_ts_decoder_accepts_a_continuation_after_a_rejected_or_partial_pes() {
    use triergon_core::Error;

    let template = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.ts");
    let pes = include_bytes!("../../mpegts/tests/data/dvb/dvb2-literals-fields.pes-data");
    let indices =
        include_bytes!("../../subtitles/tests/boundary-fixtures/dvb2-literals-fields.indices");
    let cancel = AtomicBool::new(false);
    for (cut, prefix) in [(4, 0), (25, 11)] {
        let source = pes_continuation(template, pes, cut);
        let pool = Budget::new(16 * 1024 * 1024).unwrap();
        let id = SourceId(201);
        let input = InputCursor::new(
            &pool,
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
            SourceTimePolicy::RelativeEpochZero,
            DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile),
        )
        .unwrap();
        let owner =
            ByteSource::new(id, Bytes::copy_from_slice(&pool, &source).unwrap(), 23).unwrap();
        let mut player = Player::new(
            engine,
            SourceOwner::new(owner),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let mut bridge = None;
        let mut inputs = Vec::new();
        for _ in 0..100_000 {
            let progress = player.advance(&mut WorkBudget::new(17), &cancel).unwrap();
            while let Some(output) = player.pop() {
                match output.event() {
                    RawEvent::Ts(TsEvent {
                        source: TraceEvent::ProgramMap { active: true, .. },
                        ..
                    }) => {
                        bridge = Some(
                            DvbTsBridge::new_with_native_pes_parser(
                                &output,
                                256,
                                None,
                                DisplayLimits::default(),
                                &cancel,
                            )
                            .unwrap(),
                        );
                    }
                    RawEvent::Ts(TsEvent {
                        source: TraceEvent::Pes(_),
                        ..
                    }) => inputs.push(output),
                    _ => {}
                }
            }
            if matches!(progress, Progress::Ended) {
                break;
            }
        }
        assert_eq!(inputs.len(), 2);
        let mut bridge = bridge.unwrap();
        let mut seen = 0;
        for (index, input) in inputs.into_iter().enumerate() {
            assert!(bridge.push(&mut Some(input)).unwrap());
            for _ in 0..100_000 {
                match bridge.poll(&mut WorkBudget::new(1), &cancel) {
                    Err(Error::Invalid("DVB native codec packet header")) => {
                        assert_eq!((prefix, index), (0, 0));
                        let rejected = bridge.take_rejected_native_pes().unwrap();
                        assert_eq!(rejected.pes().as_ref(), &pes[..cut]);
                        assert_eq!(rejected.codec().as_ref(), &pes[..cut]);
                        break;
                    }
                    Err(error) => panic!("unexpected native DVB error {error:?}"),
                    Ok(next) => match next.status {
                        DecodeStatus::Yield => continue,
                        DecodeStatus::NeedPacket => break,
                        DecodeStatus::Event => {
                            assert_eq!(index, 1);
                            let event = next.event.unwrap();
                            let parsed = event.native_pes_packet().unwrap();
                            assert_eq!(parsed.source_pts(), None);
                            assert_eq!(parsed.codec_pts(), Some(90_000));
                            assert_eq!(parsed.pes().as_ref(), &pes[cut..]);
                            assert_eq!(parsed.codec().as_ref(), &pes[2 + prefix..pes.len() - 1]);
                            assert_eq!(
                                parsed.prior_tail().unwrap().as_ref(),
                                &pes[2 + prefix..cut]
                            );
                            let Some(DvbQueueAction::Show(picture)) =
                                bridge.packet_clock_action(&event).unwrap()
                            else {
                                panic!("expected a bitmap after the continuation");
                            };
                            assert_eq!(picture.start_ns, 1_000_000_000);
                            let region = picture.rectangles().next().unwrap();
                            assert_eq!(region.plane.pixels.as_ref(), indices);
                            seen += 1;
                        }
                        other => panic!("unexpected native DVB progress {other:?}"),
                    },
                }
            }
        }
        assert_eq!(seen, 1);
        bridge.finish_input();
        assert_eq!(
            bridge
                .poll(&mut WorkBudget::new(1), &cancel)
                .unwrap()
                .status,
            DecodeStatus::Finished,
        );
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
