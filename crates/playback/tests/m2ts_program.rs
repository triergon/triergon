//! One forward-only M2TS program retains physical carrier provenance.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_mpegts::{
    PacketFormat, ParseLimits, TraceEvent, clock::SourceTimePolicy,
    compat::pes::RegisteredCarriage, psi::DescriptorNamespace,
};
use triergon_playback::{
    ByteSource, ClockChoice, Format, Player, PlayerPolicy, Progress, RawEvent, TsNativeCarriage,
    TsNativeTrackSelection,
    io::{SelectedInput, SourceOwner},
    native_startup::{NativeStartup, StartupStep},
};

const RAW: &[u8] = include_bytes!("../../../harness/fixtures/ts-program/replay-avc-aac-00.m2ts");

const SOURCE: SourceId = SourceId(42);

fn open_program(pool: &Budget, raw: &[u8], length: usize, quantum: usize) -> Player<ByteSource> {
    let input = InputCursor::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length: Some(length as u64),
            random_access: false,
        },
        32768,
    )
    .unwrap();
    let source =
        ByteSource::new(SOURCE, Bytes::copy_from_slice(pool, raw).unwrap(), quantum).unwrap();
    let mut startup = NativeStartup::new_m2ts_prefix_v2(SelectedInput {
        input,
        source: SourceOwner::new(source),
        format: Format::Ts(PacketFormat::M2ts192),
    })
    .unwrap();
    for _ in 0..100_000 {
        if startup
            .advance(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == StartupStep::Ready
        {
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
            return player;
        }
    }
    panic!("M2TS startup exhausted its work allowance");
}

#[test]
fn combined_program_keeps_carriers_clocks_and_order_through_physical_eof() {
    for quantum in [23, 32768] {
        let pool = Budget::new(16 << 20).unwrap();
        let id = SOURCE;
        let cancel = AtomicBool::new(false);
        let mut player = open_program(&pool, RAW, RAW.len(), quantum);
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

#[test]
fn combined_program_reports_a_short_finite_source() {
    let pool = Budget::new(16 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut player = open_program(&pool, &RAW[..RAW.len() - 1], RAW.len(), 97);
    let fault = (0..200_000).find_map(|_| {
        let progress = player.advance(&mut WorkBudget::new(8), &cancel);
        assert!(!matches!(progress, Ok(Progress::Ended)));
        while player.pop().is_some() {}
        progress.err()
    });
    assert_eq!(fault, Some(Error::Truncated));
    drop(player);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn combined_program_retries_pressure_and_cancels_with_retained_packets() {
    let pool = Budget::new(16 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let mut player = open_program(&pool, RAW, RAW.len(), 113);
    let full = pool.reserve(pool.available()).unwrap();
    let pressure = (0..2_000).find_map(|_| {
        let progress = player.advance(&mut WorkBudget::new(16), &cancel).unwrap();
        matches!(progress, Progress::Backpressure { .. }).then_some(progress)
    });
    assert!(pressure.is_some());
    drop(full);

    let mut video = None;
    let mut audio = None;
    for _ in 0..200_000 {
        let progress = player.advance(&mut WorkBudget::new(8), &cancel).unwrap();
        assert!(!matches!(progress, Progress::Ended));
        while let Some(output) = player.pop() {
            if let RawEvent::NativeTs { packet, .. } = output.event() {
                match packet.pid {
                    256 if video.is_none() => video = Some(output),
                    257 if audio.is_none() => audio = Some(output),
                    _ => {}
                }
            }
        }
        if video.is_some() && audio.is_some() {
            break;
        }
    }
    assert!(video.is_some() && audio.is_some());
    assert_eq!(
        player
            .advance(&mut WorkBudget::new(1), &AtomicBool::new(true))
            .err(),
        Some(Error::Cancelled)
    );
    assert!(matches!(
        player.advance(&mut WorkBudget::new(1), &cancel).unwrap(),
        Progress::Stopped
    ));
    drop(player);
    assert!(pool.used() > Budget::control_storage_bytes());
    drop((video, audio));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
