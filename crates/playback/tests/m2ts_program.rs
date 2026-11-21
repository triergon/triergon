//! One forward-only M2TS program retains physical carrier provenance.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent, clock::SourceTimePolicy,
    compat::pes::RegisteredCarriage, psi::DescriptorNamespace,
};
use triergon_playback::{
    ByteSource, ClockChoice, Format, PlayerPolicy, Progress, RawEvent, TsNativeCarriage,
    TsNativeTrackSelection,
    io::{SelectedInput, SourceOwner},
    native_startup::{NativeStartup, StartupStep},
};

const RAW: &[u8] = include_bytes!("../../../harness/fixtures/ts-program/replay-avc-aac-00.m2ts");

#[test]
fn combined_program_keeps_carriers_clocks_and_order_through_physical_eof() {
    for quantum in [23, 32768] {
        let pool = Budget::new(16 << 20).unwrap();
        let id = SourceId(42);
        let input = InputCursor::new(
            &pool,
            id,
            SourceCapabilities {
                length: Some(RAW.len() as u64),
                random_access: false,
            },
            32768,
        )
        .unwrap();
        let source =
            ByteSource::new(id, Bytes::copy_from_slice(&pool, RAW).unwrap(), quantum).unwrap();
        let cancel = AtomicBool::new(false);
        let mut startup = NativeStartup::new_m2ts_prefix_v2(SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Ts(PacketFormat::M2ts192),
        })
        .unwrap();
        let mut ready = false;
        for _ in 0..100_000 {
            if startup.advance(&mut WorkBudget::new(1), &cancel).unwrap() == StartupStep::Ready {
                ready = true;
                break;
            }
        }
        assert!(ready);
        let mut player = startup
            .finish()
            .unwrap()
            .into_program_player(
                1,
                ParseLimits::default(),
                SourceTimePolicy::Unanchored,
                DescriptorNamespace::Unspecified,
                &[
                    TsNativeTrackSelection {
                        pid: 257,
                        carriage: TsNativeCarriage::Registered(RegisteredCarriage::AacAdts),
                    },
                    TsNativeTrackSelection {
                        pid: 256,
                        carriage: TsNativeCarriage::Registered(RegisteredCarriage::Avc),
                    },
                ],
                PlayerPolicy::default(),
            )
            .unwrap();
        player
            .set_clock_view(ClockChoice::ExplicitNativeProjection)
            .unwrap();
        let mut carriers = 0;
        let mut packets = Vec::new();
        let mut finished = false;
        let mut ended = false;
        for _ in 0..200_000 {
            let progress = player.advance(&mut WorkBudget::new(1), &cancel).unwrap();
            while let Some(output) = player.pop() {
                match output.event() {
                    RawEvent::Ts(event) => match &event.source {
                        TraceEvent::Transport(packet) => {
                            let offset = carriers * 192;
                            assert_eq!(packet.source_offset, offset as u64);
                            assert_eq!(packet.source_id, Some(id));
                            assert_eq!(packet.source.as_ref(), &RAW[offset..offset + 192]);
                            assert_eq!(
                                packet.prefix.as_ref().unwrap().as_ref(),
                                &RAW[offset..offset + 4]
                            );
                            carriers += 1;
                        }
                        TraceEvent::Finished => finished = true,
                        _ => {}
                    },
                    RawEvent::NativeTs { packet, clock } => {
                        assert_eq!(packet.source_origin.source, id);
                        assert_eq!(packet.source_origin.offset % 192, 0);
                        assert_eq!(packet.position, packet.source_origin.offset as i64);
                        for span in packet.spans.as_slice() {
                            assert_eq!(span.transport.entity, id);
                            assert_eq!(span.transport.offset % 192, 0);
                            assert_eq!(span.transport.bytes, 192);
                            let start = span.source.offset as usize;
                            assert_eq!(span.as_ref(), &RAW[start..start + span.source.bytes]);
                        }
                        packets.push((packet.pid, clock.pts90, clock.dts90, packet.payload_len));
                    }
                    RawEvent::NativeRefusal { error, .. } => panic!("{error:?}"),
                    _ => {}
                }
            }
            if matches!(progress, Progress::Ended) {
                ended = true;
                break;
            }
        }
        assert!(ended && finished);
        assert_eq!(carriers * 192, RAW.len());
        // Pinned no-parser reference packet order, including the final audio/video drain.
        assert_eq!(
            packets.iter().map(|p| p.0).collect::<Vec<_>>(),
            [
                256, 256, 257, 256, 256, 256, 257, 256, 256, 256, 257, 256, 256, 256, 257, 256,
                256, 257, 256, 256, 256, 257, 256, 256, 256, 257, 256, 256, 256, 257, 256, 257,
                256,
            ]
        );
        assert_eq!(packets.iter().filter(|p| p.0 == 256).count(), 24);
        assert_eq!(packets.iter().filter(|p| p.0 == 257).count(), 9);
        for (pid, pts, dts, _) in &packets {
            assert_eq!(pts, dts);
            assert!(pts.is_some());
            assert!([256, 257].contains(pid));
        }
        assert_eq!(packets[31], (257, Some(294960), Some(294960), 1845));
        assert_eq!(packets[32], (256, Some(300420), Some(300420), 16));
        assert_eq!(packets.iter().map(|p| p.3).sum::<usize>(), 26592);
        drop(player);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}
