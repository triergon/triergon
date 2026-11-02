//! Source-only staged regressions. Literal fixtures/golds are the unchanged414 art.
#![forbid(unsafe_code)]

use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Time, WorkBudget};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, DvbPacket,
    ObjectRetention, PacketIdentity, PacketTiming, ReceiverView, SourceClock, SourceConfiguration,
};

const GENERATION: u64 = 61;
const PRIVATE: [u8; 5] = [0, 1, 0, 2, 0x10];
const OLD: &[u8] = include_bytes!("explicit-clut-fixtures/reduced-tail8.case");
const FRESH: &[u8] = include_bytes!("explicit-clut-fixtures/reduced-final-fresh.case");

struct Frame {
    body: Vec<u8>,
    source_rgba: Vec<u8>,
    plane: Vec<u8>,
    native_words: Vec<u32>,
}

fn bytes<const N: usize>(input: &mut Cursor<&[u8]>) -> [u8; N] {
    let mut out = [0; N];
    input.read_exact(&mut out).unwrap();
    out
}

fn vector(input: &mut Cursor<&[u8]>, count: usize) -> Vec<u8> {
    let mut out = vec![0; count];
    input.read_exact(&mut out).unwrap();
    out
}

fn first_frame(data: &[u8]) -> Frame {
    let mut input = Cursor::new(data);
    assert_eq!(&bytes::<8>(&mut input), b"SMCE0001");
    assert_eq!(bytes::<1>(&mut input)[0], 8);
    assert!(u16::from_be_bytes(bytes(&mut input)) > 0);
    // Existing native case source metadata stays unchanged in its artifact.
    // This API regression supplies its own explicit source clock/generation.
    let _original_packet_metadata = bytes::<40>(&mut input);
    let length = usize::try_from(u32::from_be_bytes(bytes(&mut input))).unwrap();
    let body = vector(&mut input, length);
    let _raw_source_tuples = vector(&mut input, 256 * 5);
    let source_rgba = vector(&mut input, 256 * 4);
    let _studio_rgba = vector(&mut input, 256 * 4);
    let plane = vector(&mut input, 256 * 2);
    let native_words = (0..256)
        .map(|_| u32::from_be_bytes(bytes(&mut input)))
        .collect();
    Frame {
        body,
        source_rgba,
        plane,
        native_words,
    }
}

#[derive(Clone, Copy)]
enum ResetKind {
    Stream,
    Configuration,
}

impl ResetKind {
    fn apply(self, decoder: &mut Decoder, next: &SourceConfiguration, cancel: &AtomicBool) {
        match self {
            Self::Stream => decoder.reset(GENERATION + 1, cancel).unwrap(),
            Self::Configuration => decoder
                .reset_with_configuration(next, GENERATION + 1, cancel)
                .unwrap(),
        }
    }

    fn next(
        self,
        first: &SourceConfiguration,
        second: &SourceConfiguration,
    ) -> SourceConfiguration {
        match self {
            Self::Stream => first.clone(),
            Self::Configuration => second.clone(),
        }
    }
}

fn configuration(pool: &Budget, private: &Bytes, denominator: u64) -> SourceConfiguration {
    SourceConfiguration::from_matroska_private(
        pool,
        private,
        SourceClock::new(1, denominator).unwrap(),
    )
    .unwrap()
}

fn limits(old: &Frame, fresh: &Frame) -> DisplayLimits {
    DisplayLimits {
        packet_bytes: old.body.len().max(fresh.body.len()) + 32,
        aggregate_region_pixels: 512,
        clut_families: 1,
        ..DisplayLimits::default()
    }
}

#[derive(Clone, Copy)]
struct Input<'a> {
    configuration: &'a SourceConfiguration,
    frame: &'a Frame,
    generation: u64,
}

impl Input<'_> {
    fn packet(self, pool: &Budget, sequence: u64) -> DvbPacket {
        let payload = Bytes::copy_from_slice(pool, &self.frame.body).unwrap();
        make_packet(
            pool,
            self.configuration,
            &payload,
            self.generation,
            sequence,
        )
    }
}

fn make_packet(
    pool: &Budget,
    configuration: &SourceConfiguration,
    payload: &Bytes,
    generation: u64,
    sequence: u64,
) -> DvbPacket {
    // Newly authored caller timing: exactly one second under each declaration.
    let ticks = i128::from(configuration.clock().raw_denominator());
    let clock = |value| {
        Some(ClockValue {
            ticks: value,
            origin: ClockOrigin::Source,
        })
    };
    configuration
        .packet(
            pool,
            payload,
            PacketTiming {
                pts: clock(ticks),
                dts: clock(ticks),
                duration: clock(ticks * 2),
            },
            PacketIdentity {
                generation,
                sequence,
                container_position: None,
            },
        )
        .unwrap()
}

fn feed(
    pool: &Budget,
    decoder: &mut Decoder,
    input: Input<'_>,
    quantum: u64,
    cancel: &AtomicBool,
) -> (DisplayEvent, u64) {
    let packet = input.packet(pool, 0);
    decoder.provide(&packet).unwrap();
    let mut event = None;
    let mut spent = 0_u64;
    for _ in 0..1_000_000 {
        let mut work = WorkBudget::new(quantum);
        let progress = decoder.step(&mut work, cancel).unwrap();
        spent += work.spent();
        match progress.status {
            DecodeStatus::Yield => assert!(work.spent() > 0),
            DecodeStatus::Event => {
                assert!(event.is_none());
                event = progress.event;
            }
            DecodeStatus::NeedPacket => return (event.unwrap(), spent),
            status => panic!("complete authored packet returned {status:?}"),
        }
    }
    panic!("finite reset fixture work ceiling")
}

fn assert_output(event: &DisplayEvent, input: Input<'_>, view: ReceiverView) {
    assert_eq!(event.receiver_view, view);
    assert_eq!(event.generation, input.generation);
    assert_eq!(event.pts, Some(Time::new(1, 1).unwrap()));
    assert_eq!(event.source_packets().len(), 1);
    let packet = &event.source_packets()[0];
    assert_eq!(packet.generation, input.generation);
    assert_eq!(packet.payload.as_ref(), input.frame.body);
    let source = packet.source.as_ref().unwrap();
    assert_eq!(source.configuration().original().as_ref(), PRIVATE);
    assert_eq!(source.configuration().clock(), input.configuration.clock());
    assert_eq!(
        source.timing().pts.unwrap().ticks,
        i128::from(input.configuration.clock().raw_denominator())
    );
    assert_eq!(event.regions().len(), 1);
    let region = &event.regions()[0];
    assert_eq!(region.plane.pixels.as_ref(), input.frame.plane);
    assert_eq!(region.palette.rgba.as_ref(), input.frame.source_rgba);
    match view {
        ReceiverView::Source => {
            assert!(region.palette.native_d32.is_none());
            assert!(event.native_publication.is_none());
        }
        ReceiverView::NativeD32 => {
            let native = region.palette.native_d32.as_ref().unwrap();
            assert_eq!(native.words.as_slice(), input.frame.native_words);
            let rgba: Vec<u8> = input
                .frame
                .native_words
                .iter()
                .flat_map(|w| [(w >> 16) as u8, (w >> 8) as u8, *w as u8, (w >> 24) as u8])
                .collect();
            assert_eq!(native.rgba.as_ref(), rgba);
            assert_eq!(
                event.native_publication.unwrap().exact_pts_microseconds,
                Some(1_000_000)
            );
        }
    }
}

fn poison_decoder(
    pool: &Budget,
    decoder: &mut Decoder,
    configuration: &SourceConfiguration,
    cancel: &AtomicBool,
) {
    // Six bytes avoid the known short-tail prefix tolerance. This packet has an
    // invalid sync byte and must take the sticky fatal-error path, not pressure.
    let payload = Bytes::copy_from_slice(pool, &[0; 6]).unwrap();
    let packet = make_packet(pool, configuration, &payload, GENERATION, 1);
    decoder.provide(&packet).unwrap();
    for _ in 0..1000 {
        match decoder.step(&mut WorkBudget::new(7), cancel) {
            Ok(progress) => assert_eq!(progress.status, DecodeStatus::Yield),
            Err(error) => {
                assert_eq!(error, Error::Invalid("DVB segment sync"));
                assert_eq!(
                    decoder.step(&mut WorkBudget::new(7), cancel).unwrap_err(),
                    error
                );
                return;
            }
        }
    }
    panic!("fatal sync fixture was not rejected")
}

fn native_transition(kind: ResetKind, fatal_before_reset: bool) {
    let old = first_frame(OLD);
    let fresh = first_frame(FRESH);
    assert_ne!(old.native_words.last(), fresh.native_words.last());
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        let private = Bytes::copy_from_slice(&pool, &PRIVATE).unwrap();
        let first = configuration(&pool, &private, 1000);
        let second = configuration(&pool, &private, 90000);
        let next = kind.next(&first, &second);
        let cancel = AtomicBool::new(false);
        let mut decoder = Decoder::new_configured_with_receiver_view(
            &pool,
            &first,
            GENERATION,
            limits(&old, &fresh),
            ReceiverView::NativeD32,
            &cancel,
        )
        .unwrap();
        let before = Input {
            configuration: &first,
            frame: &old,
            generation: GENERATION,
        };
        let after = Input {
            configuration: &next,
            frame: &fresh,
            generation: GENERATION + 1,
        };
        let (retained, _) = feed(&pool, &mut decoder, before, quantum, &cancel);
        assert_output(&retained, before, ReceiverView::NativeD32);
        if fatal_before_reset {
            poison_decoder(&pool, &mut decoder, &first, &cancel);
        }
        kind.apply(&mut decoder, &second, &cancel);
        let stale = before.packet(&pool, 0);
        assert_eq!(
            decoder.provide(&stale).unwrap_err(),
            Error::StaleGeneration {
                expected: GENERATION + 1,
                received: GENERATION,
            }
        );
        drop(stale);
        let (output, work) = feed(&pool, &mut decoder, after, quantum, &cancel);
        assert_output(&output, after, ReceiverView::NativeD32);
        assert_output(&retained, before, ReceiverView::NativeD32);
        let mut cold = Decoder::new_configured_with_receiver_view(
            &pool,
            &next,
            GENERATION + 1,
            limits(&old, &fresh),
            ReceiverView::NativeD32,
            &cancel,
        )
        .unwrap();
        let (cold_output, cold_work) = feed(&pool, &mut cold, after, quantum, &cancel);
        assert_output(&cold_output, after, ReceiverView::NativeD32);
        assert_eq!(
            work, cold_work,
            "reset must not reuse stale effective CLUT work/state"
        );
        drop(cold_output);
        drop(cold);
        drop(decoder);
        drop(next);
        drop(second);
        drop(first);
        drop(private);
        let alias = retained.regions()[0]
            .palette
            .native_d32
            .as_ref()
            .unwrap()
            .rgba
            .clone();
        assert_eq!(retained.receiver_view, ReceiverView::NativeD32);
        drop(retained);
        drop(output);
        assert!(pool.used() > baseline);
        assert!(!alias.is_empty());
        drop(alias);
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn native_view_survives_reset() {
    native_transition(ResetKind::Stream, false);
}

#[test]
fn native_view_survives_reset_with_configuration() {
    native_transition(ResetKind::Configuration, false);
}

#[test]
fn native_view_survives_fatal_error_then_reset() {
    native_transition(ResetKind::Stream, true);
}

#[test]
fn native_view_survives_fatal_error_then_configuration_reset() {
    native_transition(ResetKind::Configuration, true);
}

#[test]
fn reset_methods_release_old_effective_state_before_reacquisition() {
    let old = first_frame(OLD);
    let fresh = first_frame(FRESH);
    for kind in [ResetKind::Stream, ResetKind::Configuration] {
        let pool = Budget::new(4 << 20).unwrap();
        let baseline = pool.used();
        {
            let private = Bytes::copy_from_slice(&pool, &PRIVATE).unwrap();
            let first = configuration(&pool, &private, 1000);
            let second = configuration(&pool, &private, 90000);
            let cancel = AtomicBool::new(false);
            let mut decoder = Decoder::new_configured_with_receiver_view(
                &pool,
                &first,
                GENERATION,
                limits(&old, &fresh),
                ReceiverView::NativeD32,
                &cancel,
            )
            .unwrap();
            let empty = pool.used();
            let (event, _) = feed(
                &pool,
                &mut decoder,
                Input {
                    configuration: &first,
                    frame: &old,
                    generation: GENERATION,
                },
                7,
                &cancel,
            );
            drop(event);
            assert!(
                pool.used() > empty,
                "old CLUT/region state must actually be resident"
            );
            kind.apply(&mut decoder, &second, &cancel);
            assert_eq!(
                pool.used(),
                empty,
                "reset must release all old state before any new PCS"
            );
        }
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn source_policy_retention_work_and_bounds_are_preserved() {
    let old = first_frame(OLD);
    let fresh = first_frame(FRESH);
    for kind in [ResetKind::Stream, ResetKind::Configuration] {
        for retention in [
            ObjectRetention::CurrentDisplay,
            ObjectRetention::EpochExtension,
        ] {
            for quantum in [1, 7, 4096] {
                let pool = Budget::new(4 << 20).unwrap();
                let baseline = pool.used();
                {
                    let private = Bytes::copy_from_slice(&pool, &PRIVATE).unwrap();
                    let first = configuration(&pool, &private, 1000);
                    let second = configuration(&pool, &private, 90000);
                    let next = kind.next(&first, &second);
                    let cancel = AtomicBool::new(false);
                    let limits = limits(&old, &fresh);
                    let mut decoder = Decoder::new_configured_with_object_retention(
                        &pool, &first, GENERATION, limits, retention, &cancel,
                    )
                    .unwrap();
                    let before = Input {
                        configuration: &first,
                        frame: &old,
                        generation: GENERATION,
                    };
                    let after = Input {
                        configuration: &next,
                        frame: &fresh,
                        generation: GENERATION + 1,
                    };
                    let (retained, _) = feed(&pool, &mut decoder, before, quantum, &cancel);
                    kind.apply(&mut decoder, &second, &cancel);
                    let (output, work) = feed(&pool, &mut decoder, after, quantum, &cancel);
                    assert_output(&retained, before, ReceiverView::Source);
                    assert_output(&output, after, ReceiverView::Source);
                    assert_eq!(output.object_retention, retention);
                    let mut cold = Decoder::new_configured_with_object_retention(
                        &pool,
                        &next,
                        GENERATION + 1,
                        limits,
                        retention,
                        &cancel,
                    )
                    .unwrap();
                    let (cold_output, cold_work) = feed(&pool, &mut cold, after, quantum, &cancel);
                    assert_output(&cold_output, after, ReceiverView::Source);
                    assert_eq!(work, cold_work);
                    let mut oversized = old.body.clone();
                    oversized.resize(limits.packet_bytes + 1, 0);
                    let payload = Bytes::copy_from_slice(&pool, &oversized).unwrap();
                    let packet = make_packet(&pool, &next, &payload, GENERATION + 1, 1);
                    assert_eq!(
                        decoder.provide(&packet).unwrap_err(),
                        Error::Limit {
                            requested: limits.packet_bytes + 1,
                            maximum: limits.packet_bytes,
                        }
                    );
                }
                assert_eq!(pool.used(), baseline);
            }
        }
    }
}

#[test]
fn rejected_resets_do_not_change_native_policy_or_ownership() {
    let old = first_frame(OLD);
    let fresh = first_frame(FRESH);
    let pool = Budget::new(4 << 20).unwrap();
    let baseline = pool.used();
    {
        let private = Bytes::copy_from_slice(&pool, &PRIVATE).unwrap();
        let configuration = configuration(&pool, &private, 1000);
        let cancel = AtomicBool::new(false);
        let mut decoder = Decoder::new_configured_with_receiver_view(
            &pool,
            &configuration,
            GENERATION,
            limits(&old, &fresh),
            ReceiverView::NativeD32,
            &cancel,
        )
        .unwrap();
        let before = Input {
            configuration: &configuration,
            frame: &old,
            generation: GENERATION,
        };
        let (retained, _) = feed(&pool, &mut decoder, before, 7, &cancel);
        let used = pool.used();
        assert_eq!(
            decoder.reset(GENERATION, &cancel).unwrap_err(),
            Error::Invalid("DVB reset requires a new generation")
        );
        assert_eq!(
            decoder
                .reset_with_configuration(&configuration, GENERATION, &cancel)
                .unwrap_err(),
            Error::Invalid("DVB reset requires a new generation")
        );
        assert_eq!(pool.used(), used);
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            decoder.reset(GENERATION + 1, &cancel).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(
            decoder
                .reset_with_configuration(&configuration, GENERATION + 1, &cancel)
                .unwrap_err(),
            Error::Cancelled
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(pool.used(), used);
        let other = Budget::new(4096).unwrap();
        let foreign_private = Bytes::copy_from_slice(&other, &PRIVATE).unwrap();
        let foreign = SourceConfiguration::from_matroska_private(
            &other,
            &foreign_private,
            SourceClock::new(1, 90000).unwrap(),
        )
        .unwrap();
        assert_eq!(
            decoder
                .reset_with_configuration(&foreign, GENERATION + 1, &cancel)
                .unwrap_err(),
            Error::ForeignPool
        );
        assert_eq!(pool.used(), used);
        assert_output(&retained, before, ReceiverView::NativeD32);
    }
    assert_eq!(pool.used(), baseline);
}
