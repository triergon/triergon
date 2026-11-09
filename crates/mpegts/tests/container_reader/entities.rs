use super::{ID, OFF, PID, Transport, adts};
use triergon_core::container::{ContainerReader, Event, PacketInfo};
use triergon_core::{
    Budget, Bytes, Error, InputToken, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::{
    AudioEofPolicy, EntityTransition, Packet, Reader, ReaderEvent, ReaderLimits, TraceEvent,
};

struct Entity<'a> {
    source: SourceId,
    raw: &'a [u8],
    transition: EntityTransition,
}

impl Entity<'_> {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            length: Some(self.raw.len() as u64),
            random_access: false,
        }
    }
}

fn read(entities: &[Entity<'_>]) -> Vec<Event<Packet, ReaderEvent>> {
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = Reader::new(
        &pool,
        entities[0].source,
        entities[0].capabilities(),
        ReaderLimits {
            audio_eof_policy: AudioEofPolicy::ReportIncomplete,
            ..ReaderLimits::default()
        },
        64,
    )
    .unwrap();
    let mut events = Vec::new();
    let mut index = 0;
    let mut catalogue = 0;
    let mut previous_token: Option<InputToken> = None;
    for _ in 0..100_000 {
        match reader.step(&mut WorkBudget::new(1), &OFF).unwrap() {
            Step::Ready(event) => {
                if let Event::Other(ReaderEvent::Source(source)) = &event {
                    match source.event() {
                        TraceEvent::ProgramMap {
                            map, active: true, ..
                        } => {
                            catalogue = map.streams.len();
                        }
                        TraceEvent::Discontinuity { .. } => catalogue = 0,
                        TraceEvent::Finished => assert_eq!(index, entities.len() - 1),
                        _ => (),
                    }
                }
                events.push(event);
            }
            Step::NeedInput(request) => {
                let entity = &entities[index];
                assert_eq!(request.source, entity.source);
                assert_eq!(reader.capabilities(), entity.capabilities());
                if let Some(token) = previous_token.take() {
                    assert_eq!(request.offset, 0);
                    assert_eq!(request.token.generation(), token.generation());
                    assert_eq!(request.token.request(), token.request() + 1);
                    assert!(matches!(
                        reader.finish(request.source, token),
                        Err(Error::StaleRequest { .. })
                    ));
                }
                let at = usize::try_from(request.offset).unwrap();
                let end = (at + request.maximum).min(entity.raw.len());
                let data = Bytes::copy_from_slice(&pool, &entity.raw[at..end]).unwrap();
                reader
                    .provide(
                        &InputWindow::new(
                            request.source,
                            request.token.clone(),
                            request.offset,
                            data,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                if end == entity.raw.len() {
                    if let Some(next) = entities.get(index + 1) {
                        reader
                            .finish_entity(
                                request.source,
                                request.token.clone(),
                                next.source,
                                next.capabilities(),
                                next.transition,
                            )
                            .unwrap();
                        previous_token = Some(request.token);
                        index += 1;
                    } else {
                        reader.finish(request.source, request.token).unwrap();
                    }
                }
            }
            Step::Yield => (),
            Step::End(end) => {
                assert_eq!(index, entities.len() - 1);
                assert_eq!(end.source, entities[index].source);
                assert_eq!(end.offset, entities[index].raw.len() as u64);
                let reset = reader.reset().unwrap();
                assert_eq!(reset.source, end.source);
                assert_eq!(reset.offset, 0);
                return events;
            }
            Step::Backpressure(pressure) => panic!("unexpected backpressure: {pressure:?}"),
        }
        assert_eq!(reader.track_count(), catalogue);
    }
    panic!("ordered input made no bounded progress");
}

#[test]
fn initialization_and_partial_pes_continue_across_entities() {
    let mut transport = Transport::new();
    transport.tables(&[(0x0f, PID, &[])], 0);
    let map_end = transport.bytes.len();
    let frame = adts(&[0x56; 250]);
    transport.pes(PID, 0xc0, &frame, Some(90_000), None);
    let split = map_end + 188;
    let events = read(&[
        Entity {
            source: ID,
            raw: &transport.bytes[..map_end],
            transition: EntityTransition::Contiguous,
        },
        Entity {
            source: SourceId(100),
            raw: &transport.bytes[map_end..split],
            transition: EntityTransition::Contiguous,
        },
        Entity {
            source: SourceId(5),
            raw: &transport.bytes[split..],
            transition: EntityTransition::Contiguous,
        },
    ]);
    let mut packets = events.iter().filter_map(|event| match event {
        Event::Packet(packet) => Some(packet),
        Event::Other(ReaderEvent::Interrupted { .. } | ReaderEvent::InterruptedPes { .. }) => {
            panic!("contiguous entities interrupted a PES")
        }
        _ => None,
    });
    let packet = packets.next().unwrap();
    assert!(packets.next().is_none());
    assert_eq!(packet.payload(), frame);
    let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
        panic!("packet has no PES provenance");
    };
    assert_eq!(pes.header.pts_raw33, Some(90_000));
    assert_eq!(pes.header_source_ranges.as_slice()[0].entity, SourceId(100));
    assert_eq!(
        pes.source_ranges()
            .map(|range| range.entity)
            .collect::<Vec<_>>(),
        [SourceId(100), SourceId(5)]
    );
}

#[test]
fn a_discontinuity_interrupts_partial_audio_and_rebuilds_tracks() {
    let mut old = Transport::new();
    old.tables(&[(0x0f, PID, &[])], 0);
    let partial = adts(b"incomplete source frame");
    old.pes(PID, 0xc0, &partial[..9], Some(90_000), None);
    let mut new = Transport::new();
    new.tables(&[(0x0f, PID + 1, &[])], 0);
    let frame = adts(b"fresh source frame");
    new.pes(PID + 1, 0xc0, &frame, Some(10), None);
    let events = read(&[
        Entity {
            source: ID,
            raw: &old.bytes,
            transition: EntityTransition::Contiguous,
        },
        Entity {
            source: SourceId(90),
            raw: &new.bytes,
            transition: EntityTransition::Discontinuity,
        },
    ]);
    let mut interrupted = 0;
    let mut packets = 0;
    let mut discontinuities = 0;
    for event in events {
        match event {
            Event::Other(ReaderEvent::Interrupted {
                track,
                cause,
                elementary_range,
            }) => {
                assert_eq!(track.stream().pid, PID);
                assert_eq!(elementary_range, 0..9);
                assert!(matches!(cause.event(), TraceEvent::Discontinuity { .. }));
                interrupted += 1;
            }
            Event::Other(ReaderEvent::Source(source)) => {
                if let TraceEvent::Discontinuity { epochs } = source.event() {
                    assert_eq!(epochs.clock, 1);
                    discontinuities += 1;
                }
            }
            Event::Other(ReaderEvent::IncompleteAudio(_)) => {
                panic!("entity transition drained audio")
            }
            Event::Packet(packet) => {
                assert_eq!(discontinuities, 1);
                assert_eq!(packet.track_info().stream().pid, PID + 1);
                assert_eq!(packet.payload(), frame);
                let TraceEvent::Pes(pes) = packet.sources()[0].pes.event() else {
                    panic!("packet has no PES provenance");
                };
                assert_eq!(pes.header.pts_raw33, Some(10));
                assert_eq!(pes.epochs.clock, 1);
                packets += 1;
            }
            _ => (),
        }
    }
    assert_eq!((interrupted, discontinuities, packets), (1, 1, 1));
}

#[test]
fn a_later_entity_pmt_interrupts_the_open_pes_from_the_previous_entity() {
    let mut transport = Transport::new();
    transport.tables(&[(0x0f, PID, &[])], 0);
    let mut body = Transport::new();
    let crossing = adts(&[0x56; 250]);
    body.pes(PID, 0xc0, &crossing, Some(90_000), None);
    transport.bytes.extend_from_slice(&body.bytes[..188]);
    let split = transport.bytes.len();
    transport.tables(&[(0x0f, PID, &[])], 1);
    transport.bytes.extend_from_slice(&body.bytes[188..]);
    transport.counters[usize::from(PID)] = body.counters[usize::from(PID)];
    let fresh = adts(b"fresh PES");
    transport.pes(PID, 0xc0, &fresh, Some(180_000), None);
    let events = read(&[
        Entity {
            source: ID,
            raw: &transport.bytes[..split],
            transition: EntityTransition::Contiguous,
        },
        Entity {
            source: SourceId(3),
            raw: &transport.bytes[split..],
            transition: EntityTransition::Contiguous,
        },
    ]);
    let mut interrupted = 0;
    let mut packets = 0;
    for event in events {
        match event {
            Event::Other(ReaderEvent::InterruptedPes { source, boundary }) => {
                let TraceEvent::Pes(pes) = source.event() else {
                    panic!("missing source PES")
                };
                assert_eq!(pes.header_source_ranges.as_slice()[0].entity, ID);
                assert!(
                    matches!(boundary.event(), TraceEvent::ProgramMap { map, .. } if map.header.version == 1)
                );
                interrupted += 1;
            }
            Event::Packet(packet) => {
                assert_eq!(interrupted, 1);
                assert_eq!(packet.payload(), fresh);
                packets += 1;
            }
            _ => (),
        }
    }
    assert_eq!((interrupted, packets), (1, 1));
}
