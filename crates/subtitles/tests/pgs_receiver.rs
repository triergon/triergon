//! The receiver against recorded reference output: every packet of each
//! fixture in `pgs-receiver-fixtures/` (written by
//! `harness/subtitles/pgs_fixtures.py` from the pinned FFmpeg build's decoded
//! subtitles) through one `ReceiverDecoder`, at several work quanta.
//!
//! The fixtures cover what the receiver tolerates: packets refused while the
//! decoder keeps its state, segments read past their end, rejected canvases,
//! object and palette caches at their limits, ignored and failed displays and
//! their presentation PTS, and the run-length plane.
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, TimeBase, WorkBudget};
use triergon_subtitles::compat::pgs::ReceiverDecoder;
use triergon_subtitles::pgs::{DisplayConfig, DisplayLimits, PacketClock, RecoveryStatus};

struct Fixture {
    forced_only: bool,
    packets: Vec<(PacketClock, Vec<u8>)>,
    /// The recorded `refused`, `event` and `rect` lines, in order.
    expected: String,
}

fn hex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn clock(value: &str) -> Option<i64> {
    (value != "-").then(|| value.parse().unwrap())
}

fn parse(text: &str) -> Fixture {
    let timebase = TimeBase::new(1, 90_000).unwrap();
    let mut fixture = Fixture {
        forced_only: false,
        packets: Vec::new(),
        expected: String::new(),
    };
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields[0] {
            "options" => fixture.forced_only = fields[1] == "forced_only=1",
            "packet" => fixture.packets.push((
                PacketClock {
                    pts: clock(fields[1]),
                    dts: clock(fields[2]),
                    timebase,
                },
                hex(fields[3]),
            )),
            _ => writeln!(fixture.expected, "{line}").unwrap(),
        }
    }
    fixture
}

fn hex_of(bytes: impl IntoIterator<Item = String>) -> String {
    let text: String = bytes.into_iter().collect();
    if text.is_empty() { "-".into() } else { text }
}

/// The fixture's lines as this receiver produces them.
fn decode(fixture: &Fixture, quantum: u64) -> String {
    let budget = Budget::new(64 << 20).unwrap();
    let cancel = AtomicBool::new(false);
    let config = DisplayConfig {
        receiver_forced_only: fixture.forced_only,
    };
    let mut decoder = ReceiverDecoder::new(&budget, DisplayLimits::default(), config).unwrap();
    let mut out = String::new();
    for (index, (clock, data)) in fixture.packets.iter().enumerate() {
        if data.is_empty() {
            // An empty packet is a drain call, not a packet to decode.
            continue;
        }
        let mut raw = Some(Bytes::copy_from_slice(&budget, data).unwrap());
        decoder.push(&mut raw, *clock, &cancel).unwrap();
        let packet = loop {
            let mut work = WorkBudget::new(quantum);
            match decoder.poll(&mut work, &cancel).unwrap() {
                RecoveryStatus::OutputReady => break decoder.take_output(&cancel).unwrap(),
                RecoveryStatus::Yield => assert!(work.spent() <= quantum),
                status => panic!("packet {index}: {status:?}"),
            }
        };
        if packet.refusal().is_some() {
            writeln!(out, "refused {index}").unwrap();
        }
        let Some(event) = packet.receiver_output() else {
            continue;
        };
        let pts = event.pts_microseconds.map_or("-".into(), |v| v.to_string());
        let rects = event.rectangles();
        writeln!(out, "event {index} {pts} {}", rects.len()).unwrap();
        for r in rects {
            let palette = hex_of(r.palette().iter().map(|w| format!("{w:08x}")));
            let pixels = hex_of(r.pixels().iter().map(|b| format!("{b:02x}")));
            writeln!(
                out,
                "rect {} {} {} {} {} {palette} {pixels}",
                r.x,
                r.y,
                r.width,
                r.height,
                u8::from(r.forced)
            )
            .unwrap();
        }
    }
    decoder.finish_input(&cancel).unwrap();
    while decoder
        .poll(&mut WorkBudget::new(quantum), &cancel)
        .unwrap()
        != RecoveryStatus::Ended
    {}
    out
}

fn replay(name: &str, text: &str) {
    let fixture = parse(text);
    for quantum in [1, 7, 4096] {
        assert_eq!(
            decode(&fixture, quantum),
            fixture.expected,
            "{name} at work quantum {quantum}"
        );
    }
}

macro_rules! fixtures {
    ($($test:ident => $file:literal,)*) => {
        $(
            #[test]
            fn $test() {
                replay($file, include_str!(concat!("pgs-receiver-fixtures/", $file, ".txt")));
            }
        )*
    };
}

fixtures! {
    short_packets_are_refused_and_the_state_stays => "rx-short-packets-ts",
    a_partial_tail_header_has_length_zero => "rx-tail-partial-header-ts",
    an_overrunning_segment_ends_the_packet_except_end => "rx-tail-overrun-ts",
    composition_fields_read_past_a_short_segment => "rx-pcs-reads-on-ts",
    truncated_compositions_keep_whole_objects_only => "rx-pcs-truncated-sizes-ts",
    palette_fields_read_past_a_short_segment => "rx-pds-reads-on-ts",
    object_headers_cut_short_or_refused => "rx-ods-headers-ts",
    later_displays_are_ignored_and_failed_ones_zero_the_pts => "rx-displays-ts",
    missing_objects_carry_no_forced_flag => "rx-forced-empty-rects-ts",
    forced_only_hides_by_the_reference_flag => "rx-forced-empty-rects-only-ts",
    a_composition_after_a_display_keeps_its_pts => "rx-presentation-after-display-ts",
    only_the_high_state_bits_start_an_epoch => "rx-epoch-states-ts",
    a_rejected_canvas_is_zero_sized => "rx-rejected-canvas-ts",
    short_declared_object_lengths => "rx-ods-declared-length-ts",
    displays_before_any_composition_have_pts_zero => "rx-clock-before-composition-ts",
    the_run_length_plane_has_one_cursor => "rx-rle-plane-ts",
    objects_beyond_the_cache_limit_are_ignored => "many-objects-sup",
    canvas_sizes_and_colour_matrices => "canvas-sizes-sup",
}
