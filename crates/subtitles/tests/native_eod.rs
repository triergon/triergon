#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::compat::dvb::{Options, ReceiverDecoder, Subtitle};
use triergon_subtitles::dvb::{
    ClockOrigin, ClockValue, DecodeStatus, Decoder, DisplayEvent, DisplayLimits, PacketIdentity,
    PacketTiming, Size, SourceClock, SourceConfiguration,
};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    rgba: &'static [u8],
    indices: &'static [u8],
    words: &'static [u32],
    pts_raw: i64,
    end_ms: u32,
    canvas: [u32; 2],
    geometry: [u32; 4],
    source_error: Option<Error>,
}
#[path = "native-dvb-fixtures/cases.rs"]
mod cases;
struct ResultView {
    events: Vec<DisplayEvent>,
    error: Option<Error>,
}

fn decode(pool: &Budget, bytes: &[u8], quantum: u64) -> ResultView {
    let private = Bytes::copy_from_slice(pool, &[0, 1, 0, 2, 0x14]).unwrap();
    let config = SourceConfiguration::from_matroska_private(
        pool,
        &private,
        SourceClock::new(1, 1000).unwrap(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut decoder =
        Decoder::new_configured(pool, &config, 13, DisplayLimits::default(), &cancel).unwrap();
    let raw = Bytes::copy_from_slice(pool, bytes).unwrap();
    let packet = config
        .packet(
            pool,
            &raw,
            PacketTiming {
                pts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                dts: Some(ClockValue {
                    ticks: 1000,
                    origin: ClockOrigin::Source,
                }),
                duration: Some(ClockValue {
                    ticks: 2000,
                    origin: ClockOrigin::Source,
                }),
            },
            PacketIdentity {
                generation: 13,
                sequence: 0,
                container_position: None,
            },
        )
        .unwrap();
    decoder.provide(&packet).unwrap();
    let mut result = ResultView {
        events: Vec::new(),
        error: None,
    };
    let mut ended = false;
    for _ in 0..1_000_000 {
        let progress = match decoder.step(&mut WorkBudget::new(quantum), &cancel) {
            Ok(p) => p,
            Err(e) => {
                result.error = Some(e);
                return result;
            }
        };
        match progress.status {
            DecodeStatus::Yield => {}
            DecodeStatus::Event => result.events.push(progress.event.unwrap()),
            DecodeStatus::NeedPacket => {
                assert!(!ended);
                decoder.finish_input().unwrap();
                ended = true;
            }
            DecodeStatus::NeedOutput => panic!("small exact case cannot exhaust pool"),
            DecodeStatus::Finished => return result,
        }
    }
    panic!("bounded source exceeded poll cap")
}
/// One packet through the receiver decoder at one second.
fn receive(pool: &Budget, bytes: &[u8], check: impl FnOnce(Subtitle<'_>)) {
    let mut decoder = ReceiverDecoder::new(pool, &[0, 1, 0, 2, 0x14], Options::default());
    let mut work = WorkBudget::new(u64::MAX);
    let subtitle = decoder
        .decode(bytes, Some(1_000_000), &mut work, &AtomicBool::new(false))
        .unwrap()
        .expect("presented display set");
    check(subtitle);
}

fn rgba(words: &[u32]) -> Vec<u8> {
    words
        .iter()
        .flat_map(|w| [(w >> 16) as u8, (w >> 8) as u8, *w as u8, (w >> 24) as u8])
        .collect()
}

/// The receiver decoder against the recorded output of the reference
/// decoder: clock, display size, rectangle, index plane and palette.
fn check(index: usize) {
    let case = &cases::CASES[index];
    let pool = Budget::new(4 << 20).unwrap();
    receive(&pool, case.payload, |subtitle| {
        assert_eq!(subtitle.pts, Some(case.pts_raw), "{}", case.id);
        assert_eq!(
            (subtitle.start_display_ms, subtitle.end_display_ms),
            (0, case.end_ms)
        );
        assert_eq!(
            subtitle.display_size,
            Some(Size {
                width: case.canvas[0],
                height: case.canvas[1]
            })
        );
        let rects: Vec<_> = subtitle.rects().collect();
        assert_eq!(rects.len(), 1);
        let rect = &rects[0];
        let geometry = [
            rect.x,
            rect.y,
            i32::from(rect.width),
            i32::from(rect.height),
        ];
        assert_eq!(geometry.map(|v| v as u32), case.geometry);
        assert_eq!(rect.pixels, case.indices);
        assert_eq!(rect.palette, case.words);
        assert_eq!(rgba(rect.palette), case.rgba);
    });
}
#[test]
fn native_shared_ancillary_end() {
    check(0);
}
#[test]
fn native_nonempty_ancillary_end_tolerance() {
    check(1);
}
#[test]
fn native_composition_end_control() {
    check(2);
}
#[test]
fn native_shared_composition_end_tolerance() {
    check(3);
}
#[test]
fn native_foreign_end_filter_control() {
    check(4);
}

#[test]
fn source_profile_keeps_all_five_old_expectations_and_raw_colour() {
    for case in cases::CASES {
        let pool = Budget::new(4 << 20).unwrap();
        let observed = decode(&pool, case.payload, 7);
        assert_eq!(observed.error, case.source_error);
        assert_eq!(
            observed.events.len(),
            usize::from(case.source_error.is_none())
        );
        for event in observed.events {
            assert_eq!(
                &event.regions()[0].palette.rgba.as_ref()[..4],
                &[0, 0, 0, 0]
            );
        }
    }
}
/// A packet whose last five bytes are no segment, and one that ends after
/// the object data: the receiver presents what the packet held.
#[test]
fn native_complete_prefix_is_presented() {
    for bytes in [
        &include_bytes!("native-dvb-fixtures/input/tail106.dvb")[..],
        &include_bytes!("native-dvb-fixtures/input/prefix101.dvb")[..],
    ] {
        let pool = Budget::new(4 << 20).unwrap();
        receive(&pool, bytes, |subtitle| {
            assert_eq!(subtitle.rects().len(), 1);
        });
    }
}
#[test]
fn source_tail_and_prefix_stay_truncated_without_publication() {
    for bytes in [
        &include_bytes!("native-dvb-fixtures/input/tail106.dvb")[..],
        &include_bytes!("native-dvb-fixtures/input/prefix101.dvb")[..],
    ] {
        let pool = Budget::new(4 << 20).unwrap();
        let observed = decode(&pool, bytes, 7);
        assert_eq!(observed.error, Some(Error::Truncated));
        assert!(observed.events.is_empty());
    }
}
#[test]
fn native_default_clut_presents_the_automatic_palette() {
    let pool = Budget::new(4 << 20).unwrap();
    receive(
        &pool,
        include_bytes!("native-dvb-fixtures/input/default-clut2.dvb"),
        |subtitle| {
            let rects: Vec<_> = subtitle.rects().collect();
            assert_eq!(rects.len(), 1);
            // Grey-green levels, alpha in the top byte.
            assert!(
                rects[0]
                    .palette
                    .iter()
                    .all(|w| (w >> 24) as u8 == (w >> 8) as u8)
            );
        },
    );
}

#[test]
fn native_display_size_is_the_first_definition_or_the_default() {
    let pool = Budget::new(4 << 20).unwrap();
    let original = cases::CASES[0].payload;
    assert_eq!(original[1], 0x14);
    let definition_end = 6 + usize::from(u16::from_be_bytes([original[4], original[5]]));
    receive(&pool, &original[definition_end..], |subtitle| {
        assert_eq!(
            subtitle.display_size,
            Some(Size {
                width: 720,
                height: 576
            })
        );
    });
    let mut changed = original[..definition_end].to_vec();
    changed[6] ^= 0x10;
    changed[7..11].copy_from_slice(&[0, 31, 0, 23]);
    let mut packet = original[..definition_end].to_vec();
    packet.extend_from_slice(&changed);
    packet.extend_from_slice(&original[definition_end..]);
    receive(&pool, &packet, |subtitle| {
        assert_eq!(
            subtitle.display_size,
            Some(Size {
                width: cases::CASES[0].canvas[0],
                height: cases::CASES[0].canvas[1]
            })
        );
    });
}
