use crate::support::players::*;
use crate::support::*;
use triergon_core::{Budget, Time};
use triergon_mpegts::stream::TraceEvent;
use triergon_mpegts::{PacketFormat, clock::SourceTimePolicy};
use triergon_playback::*;

#[test]
fn ts188_and_m2ts_real_readers_preserve_complete_carriage_and_pes() {
    for (raw, format) in [
        (
            include_bytes!("../fixtures/ts/ts188-basic.ts").as_slice(),
            PacketFormat::Ts188,
        ),
        (
            include_bytes!("../fixtures/ts/m2ts192-opaque-prefix.m2ts").as_slice(),
            PacketFormat::M2ts192,
        ),
    ] {
        for q in [1, 7, 4096] {
            let (mut p, b) = ts(raw, format, q, SourceTimePolicy::RelativeEpochZero).unwrap();
            let out = collect(&mut p, 1).unwrap();
            let events = pes(&out);
            assert_eq!(events.len(), 1);
            let TraceEvent::Pes(pes) = &events[0].source else {
                panic!()
            };
            assert_eq!(pes.pid, 256);
            assert_eq!(pes.header.pts_raw33, Some(90_000));
            assert_eq!(
                pes.payload().flatten().copied().collect::<Vec<_>>(),
                b"lane-ts-original-payload\x00\x47\xff"
            );
            assert_eq!(events[0].demux_clock.unwrap().pts90, Some(90_000));
            assert_eq!(
                events[0].source_pts.unwrap().exact,
                Some(Time::new(1, 1).unwrap())
            );
            let carriers: Vec<_> = out
                .iter()
                .filter_map(|o| {
                    if let Event::Ts(TsEvent {
                        source: TraceEvent::Transport(t),
                        ..
                    }) = o.event()
                    {
                        Some(t)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(carriers.len(), 3);
            for (i, c) in carriers.iter().enumerate() {
                assert_eq!(
                    c.source.as_ref(),
                    &raw[i * format.bytes()..(i + 1) * format.bytes()]
                );
                if format == PacketFormat::M2ts192 {
                    assert_eq!(
                        c.prefix.as_ref().unwrap().as_ref(),
                        &raw[i * 192..i * 192 + 4]
                    );
                }
            }
            drop(out);
            drop(p);
            assert_eq!(b.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn actual_pmt_version_change_preserves_both_maps_and_epoch_order() {
    let (mut p, _) = ts(
        include_bytes!("../fixtures/ts/pmt-version-wrap-retypes-pid.ts"),
        PacketFormat::Ts188,
        1,
        SourceTimePolicy::Unanchored,
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let maps: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Event::Ts(TsEvent {
                source:
                    TraceEvent::ProgramMap {
                        map,
                        active: true,
                        configuration_epoch,
                    },
                ..
            }) = o.event()
            {
                Some((
                    map.header.version,
                    *configuration_epoch,
                    map.streams.as_slice()[0].stream_type,
                    map.source.clone(),
                ))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(maps.len(), 2);
    assert!(maps[1].1 > maps[0].1);
    assert_ne!(maps[0].2, maps[1].2);
    assert_ne!(maps[0].3.as_ref(), maps[1].3.as_ref());
    assert!(pes(&out).is_empty());
}

#[test]
fn source_ts_errors_are_retained_diagnostics_not_fabricated_pes() {
    for (raw, expected_prefix_pes) in [
        (
            include_bytes!("../fixtures/ts/psi-crc-invalid.ts").as_slice(),
            0,
        ),
        (
            include_bytes!("../fixtures/ts/pes-timestamp-marker.ts").as_slice(),
            0,
        ),
        (
            include_bytes!("../fixtures/ts/transport-partial-final-packet.ts").as_slice(),
            1,
        ),
    ] {
        let (mut p, _) = ts(raw, PacketFormat::Ts188, 1, SourceTimePolicy::Unanchored).unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert!(out.iter().any(|o| matches!(
            o.event(),
            Event::Ts(TsEvent {
                source: TraceEvent::Diagnostic(_),
                ..
            })
        )));
        assert_eq!(pes(&out).len(), expected_prefix_pes);
        if expected_prefix_pes == 1 {
            let TraceEvent::Pes(packet) = &pes(&out)[0].source else {
                panic!()
            };
            assert_eq!(packet.header.pts_raw33, Some(90_000));
            assert_eq!(packet.payload_len, 27);
        }
    }
}

#[test]
fn all_remaining_frozen_ts_cases_have_actual_terminal_owned_outcomes() {
    for raw in [
        include_bytes!("../fixtures/ts/pmt-opaque-language-descriptors.ts").as_slice(),
        include_bytes!("../fixtures/ts/pts-wrap-forward.ts").as_slice(),
        include_bytes!("../fixtures/ts/pts-dts-common-epoch.ts").as_slice(),
        include_bytes!("../fixtures/ts/carriage-hevc-direct.ts").as_slice(),
        include_bytes!("../fixtures/ts/carriage-aac-adts-direct.ts").as_slice(),
    ] {
        let (mut p, b) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert!(out.iter().any(|o| matches!(
            o.event(),
            Event::Ts(TsEvent {
                source: TraceEvent::Finished,
                ..
            })
        )));
        for e in pes(&out) {
            assert!(e.source_pts.unwrap().exact.is_none());
            assert!(e.demux_clock.is_some());
        }
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
