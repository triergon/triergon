//! Original controls for an explicit native omission, never a Source repair.
#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::compat::dvb::GeometryPolicy;
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayAction, DisplayEvent, DisplayLimits,
    DvbPacket, PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};

const EMPTY: &[u8] = &[
    15, 16, 0, 1, 0, 8, 2, 11, 1, 255, 0, 0, 0, 0, 15, 17, 0, 1, 0, 10, 1, 15, 0, 2, 0, 2, 39, 0,
    0, 3, 15, 128, 0, 1, 0, 0,
];
fn payload(x: u16, y: u16) -> Vec<u8> {
    let mut bytes = EMPTY.to_vec();
    bytes[10..12].copy_from_slice(&x.to_be_bytes());
    bytes[12..14].copy_from_slice(&y.to_be_bytes());
    bytes
}
fn config(pool: &Budget) -> SourceConfiguration {
    SourceConfiguration::from_matroska_private(
        pool,
        &Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14]).unwrap(),
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap()
}
fn packet(
    pool: &Budget,
    config: &SourceConfiguration,
    bytes: &Bytes,
    generation: u64,
) -> DvbPacket {
    config
        .packet(
            pool,
            bytes,
            PacketTiming {
                pts: None,
                dts: None,
                duration: None,
            },
            PacketIdentity {
                generation,
                sequence: 0,
                container_position: None,
            },
        )
        .unwrap()
}
fn native(pool: &Budget, config: &SourceConfiguration, generation: u64) -> Decoder {
    Decoder::new_configured_with_geometry_policy(
        pool,
        config,
        generation,
        DisplayLimits::default(),
        GeometryPolicy::OmitUnrendered,
        &AtomicBool::new(false),
    )
    .unwrap()
}
fn event(decoder: &mut Decoder, quantum: u64) -> Result<DisplayEvent, Error> {
    for _ in 0..100_000 {
        let p = decoder.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?;
        match p.status {
            DecodeStatus::Yield => (),
            DecodeStatus::Event => return Ok(p.event.unwrap()),
            status => panic!("unexpected status {status:?}"),
        }
    }
    panic!("bounded original control stalled")
}

#[test]
fn empty_clear_retains_each_invalid_source_geometry_and_owner() {
    for (x, y) in [(719, 0), (0, 575)] {
        for q in [1, 7, 4096] {
            let pool = Budget::new(4 << 20).unwrap();
            let baseline = pool.used();
            let config = config(&pool);
            let source = payload(x, y);
            let owner = Bytes::copy_from_slice(&pool, &source).unwrap();
            let packet = config
                .packet(
                    &pool,
                    &owner,
                    PacketTiming {
                        pts: Some(ClockValue {
                            ticks: 1000,
                            origin: ClockOrigin::Source,
                        }),
                        dts: None,
                        duration: None,
                    },
                    PacketIdentity {
                        generation: 13,
                        sequence: 0,
                        container_position: None,
                    },
                )
                .unwrap();
            let mut decoder = native(&pool, &config, 13);
            decoder.provide(&packet).unwrap();
            let output = event(&mut decoder, q).unwrap();
            assert_eq!(output.action, DisplayAction::Clear);
            assert!(
                output
                    .receiver_publication
                    .unwrap()
                    .context_canvas
                    .is_none()
            );
            assert!(output.regions().is_empty());
            assert_eq!(output.geometry_policy, GeometryPolicy::OmitUnrendered);
            assert_eq!(output.omitted_regions().len(), 1);
            let omitted = &output.omitted_regions()[0];
            assert_eq!(omitted.id, 1);
            assert_eq!(
                [omitted.source_address.x, omitted.source_address.y],
                [u32::from(x), u32::from(y)]
            );
            assert_eq!(
                [omitted.source_size.width, omitted.source_size.height],
                [2, 2]
            );
            assert_eq!(
                omitted.source_error,
                Some(Error::Invalid("DVB region outside display window"))
            );
            assert_eq!(omitted.original_composition.as_ref(), &source[14..30]);
            assert!(omitted.original_composition.shares_storage_with(&owner));
            assert!(
                output.source_packets()[0]
                    .payload
                    .shares_storage_with(&owner)
            );
            assert_eq!(output.pts, Some(Time::new(1, 1).unwrap()));
            let publication = output.receiver_publication.unwrap();
            assert_eq!(publication.exact_pts_microseconds, Some(1_000_000));
            assert_eq!(
                [publication.start_display_ms, publication.end_display_ms],
                [0, 2000]
            );
            assert!(
                output.source_packets()[0]
                    .source
                    .as_ref()
                    .unwrap()
                    .timing()
                    .dts
                    .is_none()
            );
            drop(decoder);
            drop(packet);
            drop(config);
            drop(owner);
            assert_eq!(output.source_packets()[0].payload.as_ref(), source);
            drop(output);
            assert_eq!(pool.used(), baseline);
        }
    }
}

#[test]
fn both_legacy_receiver_views_keep_source_bounds_refusals() {
    for view in [ReceiverView::Source, ReceiverView::Receiver] {
        for (x, y) in [(719, 0), (0, 575)] {
            let pool = Budget::new(4 << 20).unwrap();
            let config = config(&pool);
            let bytes = Bytes::copy_from_slice(&pool, &payload(x, y)).unwrap();
            let packet = packet(&pool, &config, &bytes, 13);
            let mut decoder = Decoder::new_configured_with_receiver_view(
                &pool,
                &config,
                13,
                DisplayLimits::default(),
                view,
                &AtomicBool::new(false),
            )
            .unwrap();
            decoder.provide(&packet).unwrap();
            assert!(matches!(
                event(&mut decoder, 1),
                Err(Error::Invalid("DVB region outside display window"))
            ));
        }
    }
}

#[test]
fn valid_unrendered_region_does_not_invent_a_source_error() {
    let pool = Budget::new(4 << 20).unwrap();
    let config = config(&pool);
    let bytes = Bytes::copy_from_slice(&pool, &payload(1, 2)).unwrap();
    let mut decoder = native(&pool, &config, 13);
    decoder
        .provide(&packet(&pool, &config, &bytes, 13))
        .unwrap();
    let output = event(&mut decoder, 1).unwrap();
    assert!(output.regions().is_empty());
    assert_eq!(output.omitted_regions().len(), 1);
    assert_eq!(output.omitted_regions()[0].source_error, None);
    assert!(output.pts.is_none());
    assert!(
        output
            .receiver_publication
            .unwrap()
            .exact_pts_microseconds
            .is_none()
    );
}

#[test]
fn actual_rendered_objects_still_require_valid_geometry() {
    let pool = Budget::new(4 << 20).unwrap();
    let config = config(&pool);
    let mut source =
        include_bytes!("native-order-fixtures/input/original-pcs12-rcs12.dvb").to_vec();
    let mut at = 0;
    while at < source.len() {
        let length = usize::from(u16::from_be_bytes([source[at + 4], source[at + 5]]));
        if source[at + 1] == 0x10 {
            source[at + 10..at + 12].copy_from_slice(&15u16.to_be_bytes());
            break;
        }
        at += 6 + length;
    }
    let bytes = Bytes::copy_from_slice(&pool, &source).unwrap();
    let mut decoder = native(&pool, &config, 13);
    decoder
        .provide(&packet(&pool, &config, &bytes, 13))
        .unwrap();
    assert!(matches!(
        event(&mut decoder, 1),
        Err(Error::Invalid("DVB region outside display window"))
    ));
}

#[test]
fn malformed_composition_is_not_an_empty_clear() {
    let pool = Budget::new(4 << 20).unwrap();
    let config = config(&pool);
    let mut source = payload(719, 0);
    source[18..20].copy_from_slice(&u16::MAX.to_be_bytes());
    let bytes = Bytes::copy_from_slice(&pool, &source).unwrap();
    let mut decoder = native(&pool, &config, 13);
    decoder
        .provide(&packet(&pool, &config, &bytes, 13))
        .unwrap();
    assert!(matches!(event(&mut decoder, 1), Err(Error::Truncated)));
}

#[test]
fn zero_work_pressure_retry_and_cancellation_remain_observable() {
    let pool = Budget::new(4 << 20).unwrap();
    let baseline = pool.used();
    let config = config(&pool);
    let bytes = Bytes::copy_from_slice(&pool, &payload(719, 0)).unwrap();
    let packet = packet(&pool, &config, &bytes, 13);
    let mut decoder = native(&pool, &config, 13);
    decoder.provide(&packet).unwrap();
    let cancel = AtomicBool::new(false);
    let before = pool.used();
    assert_eq!(
        decoder
            .step(&mut WorkBudget::new(0), &cancel)
            .unwrap()
            .status,
        DecodeStatus::Yield
    );
    assert_eq!(pool.used(), before);
    let blocker = pool.reserve(pool.available()).unwrap();
    let mut saw_pressure = false;
    for _ in 0..100_000 {
        let p = decoder.step(&mut WorkBudget::new(1), &cancel).unwrap();
        if p.status == DecodeStatus::NeedOutput {
            saw_pressure = true;
            break;
        }
        assert_eq!(p.status, DecodeStatus::Yield);
    }
    assert!(saw_pressure);
    drop(blocker);
    let output = event(&mut decoder, 1).unwrap();
    assert_eq!(output.omitted_regions().len(), 1);
    decoder.reset(14, &cancel).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(
        decoder.step(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        decoder.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
    drop(output);
    drop(decoder);
    drop(packet);
    drop(bytes);
    drop(config);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn reset_preserves_policy_and_old_output_without_carrying_regions() {
    let pool = Budget::new(4 << 20).unwrap();
    let baseline = pool.used();
    let config = config(&pool);
    let bytes = Bytes::copy_from_slice(&pool, &payload(719, 0)).unwrap();
    let mut decoder = native(&pool, &config, 13);
    decoder
        .provide(&packet(&pool, &config, &bytes, 13))
        .unwrap();
    let old = event(&mut decoder, 1).unwrap();
    decoder
        .reset_with_configuration(&config, 14, &AtomicBool::new(false))
        .unwrap();
    assert!(matches!(
        decoder.provide(&packet(&pool, &config, &bytes, 13)),
        Err(Error::StaleGeneration { .. })
    ));
    decoder
        .provide(&packet(&pool, &config, &bytes, 14))
        .unwrap();
    let next = event(&mut decoder, 7).unwrap();
    assert_eq!(old.generation, 13);
    assert_eq!(next.generation, 14);
    assert_eq!(next.geometry_policy, GeometryPolicy::OmitUnrendered);
    assert_eq!(next.omitted_regions().len(), 1);
    drop(decoder);
    drop(config);
    drop(bytes);
    assert_eq!(
        old.source_packets()[0].payload.as_ref(),
        next.source_packets()[0].payload.as_ref()
    );
    drop(old);
    drop(next);
    assert_eq!(pool.used(), baseline);
}
