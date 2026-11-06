//! Finite logical resource accounting and retained ownership across a tolerated resize.
#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, DvbPacket,
    PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};

const INITIAL: &[u8] = include_bytes!("functional-state-fixtures/reject-in-epoch-resize.0.dvb");
const GROW: &[u8] = include_bytes!("functional-state-fixtures/reject-in-epoch-resize.1.dvb");

fn packet(pool: &Budget, config: &SourceConfiguration, raw: &[u8], sequence: u64) -> DvbPacket {
    let payload = Bytes::copy_from_slice(pool, raw).unwrap();
    let clock = Some(ClockValue {
        ticks: i128::from(sequence + 1) * 1000,
        origin: ClockOrigin::Source,
    });
    config
        .packet(
            pool,
            &payload,
            PacketTiming {
                pts: clock,
                dts: clock,
                duration: None,
            },
            PacketIdentity {
                generation: 73,
                sequence,
                container_position: None,
            },
        )
        .unwrap()
}

fn event(decoder: &mut Decoder) -> DisplayEvent {
    for _ in 0..100000 {
        let next = decoder
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap();
        match next.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => return next.event.unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("bounded source")
}

#[test]
fn native_resize_cannot_bypass_aggregate_pixel_limit_or_invalidate_retained_outputs() {
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let private = Bytes::copy_from_slice(&pool, &[0, 1, 0, 2, 0x14]).unwrap();
        let config = SourceConfiguration::from_matroska_private(
            &pool,
            &private,
            SourceClock::new(1, 1000).unwrap(),
        )
        .unwrap();
        let mut decoder = Decoder::new_configured_with_receiver_view(
            &pool,
            &config,
            73,
            DisplayLimits {
                aggregate_region_pixels: 12,
                ..DisplayLimits::default()
            },
            ReceiverView::Receiver,
            &AtomicBool::new(false),
        )
        .unwrap();
        let first = packet(&pool, &config, INITIAL, 0);
        decoder.provide(&first).unwrap();
        let output = event(&mut decoder);
        loop {
            match decoder
                .step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
                .unwrap()
                .status
            {
                DecodeStatus::Yield => {}
                DecodeStatus::NeedPacket => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        let grow = packet(&pool, &config, GROW, 1);
        decoder.provide(&grow).unwrap();
        let failure = loop {
            match decoder.step(&mut WorkBudget::new(quantum), &AtomicBool::new(false)) {
                Ok(step) => assert_eq!(step.status, DecodeStatus::Yield),
                Err(error) => break error,
            }
        };
        assert_eq!(
            failure,
            Error::Limit {
                requested: 14,
                maximum: 12
            }
        );
        assert_eq!(
            decoder
                .step(&mut WorkBudget::new(4096), &AtomicBool::new(false))
                .unwrap_err(),
            failure
        );
        assert_eq!(decoder.provide(&grow).unwrap_err(), failure);
        assert_eq!(grow.payload.as_ref(), GROW);
        drop(decoder);
        drop(grow);
        drop(first);
        drop(config);
        drop(private);
        assert_eq!(output.regions().len(), 2);
        assert_eq!(output.regions()[0].plane.pixels.as_ref(), [1, 1, 2, 2]);
        assert_eq!(
            output.regions()[1].plane.pixels.as_ref(),
            [1, 2, 2, 1, 2, 1, 1, 2]
        );
        assert_eq!(
            output.regions()[0]
                .palette
                .receiver
                .as_ref()
                .unwrap()
                .words
                .as_slice(),
            [34816, 4294967295, 4278190080, 4294967295]
        );
        assert_eq!(output.source_packets()[0].payload.as_ref(), INITIAL);
        assert!(pool.used() > baseline);
        drop(output);
        assert_eq!(pool.used(), baseline);
    }
}
