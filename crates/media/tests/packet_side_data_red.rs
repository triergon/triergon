//! First activation uses ONLY today's public API. These authored opaque payloads
//! establish Matroska wire/transaction behavior, not compressed-codec validity.
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::Step;
use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, WorkBudget};
use triergon_matroska::{
    Error, Limits,
    typed::{BlockingReader, Event, Reader},
};

fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|byte| *byte == 0)
        .collect();
    let width = (1..=8)
        .find(|width| (bytes.len() as u64) < (1_u64 << (7 * width)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(bytes);
    out
}
fn u(id: u64, value: u64) -> Vec<u8> {
    e(id, &value.to_be_bytes())
}
fn track(extra: &[u8]) -> Vec<u8> {
    e(
        0xae,
        &[
            u(0xd7, 1),
            u(0x73c5, 1),
            u(0x83, 1),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x63a2, b"initial"),
            extra.to_vec(),
        ]
        .concat(),
    )
}
fn file(extra: &[u8], groups: &[Vec<u8>]) -> Vec<u8> {
    let mut cluster = u(0xe7, 7);
    for group in groups {
        cluster.extend(group);
    }
    let payload = [e(0x1654ae6b, &track(extra)), e(0x1f43b675, &cluster)].concat();
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(0x18538067, &payload),
    ]
    .concat()
}
fn block() -> Vec<u8> {
    e(0xa1, &[0x81, 0, 0, 0, b'V'])
}
fn additions(mores: &[Vec<u8>]) -> Vec<u8> {
    e(0x75a1, &mores.concat())
}
fn more(id: Option<&[u8]>, bytes: &[u8]) -> Vec<u8> {
    let mut raw = Vec::new();
    if let Some(id) = id {
        raw.extend(e(0xee, id));
    }
    raw.extend(e(0xa5, bytes));
    e(0xa6, &raw)
}
fn drive(raw: &[u8], known: bool, events: &mut Vec<Event>) -> triergon_matroska::Result<()> {
    let pool = Budget::new(256 * 1024)?;
    let mut reader = Reader::new(
        &pool,
        SourceId(17),
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: true,
        },
        Limits::default(),
        12,
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        let mut work = WorkBudget::new(1);
        let step = reader.step(&mut work, &cancel)?;
        assert!(work.spent() <= 1);
        match step {
            Step::Yield => (),
            Step::Ready(event) => events.push(event),
            Step::End(_) => return Ok(()),
            Step::NeedInput(request) => {
                let at = usize::try_from(request.offset).unwrap();
                assert!(at <= raw.len());
                if at == raw.len() {
                    reader.finish(request.source, request.token)?;
                } else {
                    assert_ne!(request.maximum, 0);
                    let bytes = Bytes::copy_from_slice(&pool, &raw[at..at + 1])?;
                    reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
            }
            other => panic!("ample-pool work-one job must progress: {other:?}"),
        }
    }
    panic!("work-one job did not finish")
}
#[test]
fn control_ordinary_group_keeps_initial_private_and_timestamp() {
    let raw = file(&[], &[e(0xa0, &block())]);
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open(Cursor::new(raw), Limits::default()).unwrap();
    let packet = reader.next_typed_packet(&cancel).unwrap().unwrap();
    assert_eq!(packet.packet.data.as_ref(), b"V");
    assert_eq!(packet.packet.pts_ns, Some(7_000_000));
    assert_eq!(
        packet.configuration.tracks()[0].codec_private.as_ref(),
        b"initial"
    );
    assert!(reader.next_typed_packet(&cancel).unwrap().is_none());
}
#[test]
fn unknown_addition_kind_and_default_id_do_not_discard_the_block() {
    let mapping = e(
        0x41e4,
        &[
            u(0x41f0, 4),
            u(0x41e7, 0x11223344),
            e(0x41ed, b"opaque-map"),
        ]
        .concat(),
    );
    let group = e(
        0xa0,
        &[
            block(),
            additions(&[more(None, b"default"), more(Some(&[4]), b"opaque-4")]),
        ]
        .concat(),
    );
    for known in [false, true] {
        let mut events = Vec::new();
        let result = drive(
            &file(
                &[u(0x55ee, 4), mapping.clone()].concat(),
                std::slice::from_ref(&group),
            ),
            known,
            &mut events,
        );
        assert!(
            result.is_ok(),
            "raw unknown additions must be carried: {result:?}"
        );
        let packets: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let Event::Packet(packet) = event {
                    Some(packet)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet.data.as_ref(), b"V");
    }
}
#[test]
fn empty_and_nonempty_codec_states_publish_configuration_before_their_packets() {
    let groups = [
        e(0xa0, &[block(), e(0xa4, b"new")].concat()),
        e(0xa0, &[block(), e(0xa4, b"")].concat()),
    ];
    let mut events = Vec::new();
    let result = drive(&file(&[], &groups), false, &mut events);
    assert!(
        result.is_ok(),
        "written codec state needs a real typed transaction: {result:?}"
    );
    let order: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Configuration(value) => Some((0, value.revision())),
            Event::Packet(value) => Some((1, value.configuration.revision())),
            _ => None,
        })
        .collect();
    assert_eq!(order, [(0, 1), (0, 2), (1, 2), (0, 3), (1, 3)]);
}
#[test]
fn duplicate_effective_addition_ids_are_invalid_before_publication() {
    // Omitted and empty BlockAddID both mean one: RFC8794 §6.1 and RFC9559 §5.1.3.5.2.3.
    let raw = file(
        &u(0x55ee, 1),
        &[e(
            0xa0,
            &[
                block(),
                additions(&[more(None, b"a"), more(Some(&[]), b"b")]),
            ]
            .concat(),
        )],
    );
    let mut events = Vec::new();
    let result = drive(&raw, true, &mut events);
    assert!(
        matches!(
            result,
            Err(Error::Invalid("duplicate effective BlockAddID"))
        ),
        "got {result:?}"
    );
    assert!(!events.iter().any(|event| matches!(event, Event::Packet(_))));
}
#[test]
fn group_crc_is_checked_before_any_packet_or_state_commit() {
    let raw = file(&[], &[e(0xa0, &[e(0xbf, &[0, 0, 0, 0]), block()].concat())]);
    let mut events = Vec::new();
    let result = drive(&raw, false, &mut events);
    assert!(
        matches!(result, Err(Error::Invalid("packet source CRC mismatch"))),
        "the authored bad CRC must fail: {result:?}"
    );
    assert!(!events.iter().any(|event| matches!(event, Event::Packet(_))));
}
#[test]
fn late_laced_checksum_failure_does_not_commit_a_codec_state() {
    let encoding = e(
        0x6d80,
        &e(0x6240, &[u(0x5032, 1), e(0x5034, &u(0x4254, 0))].concat()),
    );
    let abc = [120, 1, 1, 3, 0, 252, 255, b'a', b'b', b'c', 2, 77, 1, 39];
    let mut data = vec![0x81, 0, 0, 4, 1];
    data.extend(abc);
    data.extend(abc);
    *data.last_mut().unwrap() ^= 1;
    let raw = file(
        &encoding,
        &[e(
            0xa0,
            &[e(0xa1, &data), e(0xa4, b"must-not-commit")].concat(),
        )],
    );
    let mut events = Vec::new();
    let result = drive(&raw, true, &mut events);
    assert!(
        matches!(result, Err(Error::Invalid("Adler-32 mismatch"))),
        "late frame must still validate: {result:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Configuration(_)))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(event, Event::Packet(_))));
}
