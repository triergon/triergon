//! The `ccaption`-compatible receiver against FFmpeg's recorded events.
//!
//! The SCC fixtures under `tests/cc/` are authored by
//! `harness/subtitles/cc_streams.py`, which also records the reference's events
//! (`*.events`: packet, pts, end and ASS lines). Each test reads the SCC file as
//! FFmpeg's `scc` demuxer does, decodes every packet and drains the receiver as
//! the reference worker does.

use std::fs;
use std::path::PathBuf;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_subtitles::compat::cc::{
    ASS_HEADER, Clock, DataField, Decoder, Options, SccPackets, Subtitle,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/cc")
        .join(name)
}

/// One event as the reference reports it.
#[derive(Debug, PartialEq, Eq)]
struct Event {
    packet: usize,
    pts: Option<i64>,
    end: u32,
    lines: Vec<String>,
}

fn recorded(name: &str) -> Vec<Event> {
    fs::read_to_string(fixture(name))
        .expect("events")
        .lines()
        .map(|line| {
            let mut fields = line.split('\t');
            let mut next = || fields.next().expect("field");
            let packet = next().parse().expect("packet");
            let pts = match next() {
                "-" => None,
                pts => Some(pts.parse().expect("pts")),
            };
            let end = next().parse().expect("end");
            Event {
                packet,
                pts,
                end,
                lines: fields.map(str::to_owned).collect(),
            }
        })
        .collect()
}

fn decode(scc: &str, options: Options) -> Vec<Event> {
    decode_refusing(scc, options, false)
}

/// The events of `scc`; with `refuse`, each packet is first offered one
/// work unit short, which must be refused.
fn decode_refusing(scc: &str, options: Options, refuse: bool) -> Vec<Event> {
    let pool = Budget::new(1 << 24).expect("pool");
    let text = fs::read(fixture(scc)).expect("scc");
    let packets = SccPackets::read(&text, &pool, &mut WorkBudget::new(1 << 20)).expect("packets");
    let mut decoder = Decoder::new(&pool, options);
    assert_eq!(decoder.header(), ASS_HEADER);
    let mut subtitle = Subtitle::new(&pool);
    let mut events = Vec::new();
    let mut take = |packet: usize, subtitle: &Subtitle| {
        events.push(Event {
            packet,
            pts: subtitle.pts,
            end: subtitle.end_ms,
            lines: subtitle
                .lines()
                .map(|l| String::from_utf8(l.to_vec()).expect("utf-8"))
                .collect(),
        });
    };
    for (index, packet) in packets.iter().enumerate() {
        let clock = Clock {
            pts: Some(packet.pts * 1000),
            duration_ms: packet.duration as u32,
        };
        let needed = Decoder::packet_work(packet.data.len());
        if refuse {
            let mut short = WorkBudget::new(needed - 1);
            let refused = decoder.decode(packet.data, clock, &mut subtitle, &mut short);
            assert!(matches!(refused, Err(Error::Backpressure { .. })));
            assert_eq!(short.spent(), 0);
        }
        let mut work = WorkBudget::new(needed);
        let reported = decoder
            .decode(packet.data, clock, &mut subtitle, &mut work)
            .expect("decode");
        // One unit per construct and 512 per caption.
        let constructs = (packet.data.len() / 3) as u64;
        assert_eq!(
            work.spent(),
            constructs + 512 * subtitle.lines().len() as u64
        );
        if reported {
            take(index, &subtitle);
        }
    }
    while decoder.drain(&mut subtitle).expect("drain") {
        take(packets.len(), &subtitle);
    }
    events
}

#[test]
fn pop_on_captions_match_the_reference() {
    assert_eq!(
        decode("scc-popon.scc", Options::default()),
        recorded("scc-popon.events")
    );
}

#[test]
fn roll_up_paint_on_and_text_match_the_reference() {
    assert_eq!(
        decode("scc-rollup.scc", Options::default()),
        recorded("scc-rollup.events")
    );
}

#[test]
fn real_time_reports_match_the_reference() {
    let options = Options {
        real_time: true,
        ..Options::default()
    };
    assert_eq!(
        decode("scc-popon.scc", options),
        recorded("scc-popon-realtime.events")
    );
    assert_eq!(
        decode("scc-rollup.scc", options),
        recorded("scc-rollup-realtime.events")
    );
}

#[test]
fn a_packet_without_enough_work_changes_nothing() {
    for options in [
        Options::default(),
        Options {
            real_time: true,
            ..Options::default()
        },
    ] {
        for scc in ["scc-popon.scc", "scc-rollup.scc"] {
            assert_eq!(decode_refusing(scc, options, true), decode(scc, options));
        }
    }
}

#[test]
fn the_other_field_is_silent() {
    let options = Options {
        data_field: DataField::Second,
        ..Options::default()
    };
    assert!(decode("scc-popon.scc", options).is_empty());
}

/// One packet of constructs at `ms`.
fn packet(decoder: &mut Decoder, subtitle: &mut Subtitle, ms: i64, pairs: &[[u8; 3]]) -> bool {
    let data: Vec<u8> = pairs.iter().flatten().copied().collect();
    let clock = Clock {
        pts: Some(ms * 1000),
        duration_ms: 0,
    };
    let mut work = WorkBudget::new(Decoder::packet_work(data.len()));
    decoder
        .decode(&data, clock, subtitle, &mut work)
        .expect("decode")
}

#[test]
fn a_pop_on_caption_lasts_until_it_is_erased() {
    let pool = Budget::new(1 << 20).expect("pool");
    let mut decoder = Decoder::new(&pool, Options::default());
    let mut subtitle = Subtitle::new(&pool);
    // Resume caption loading, "HI", end of caption; a second later erase.
    let caption = [[0xfc, 0x94, 0x20], [0xfc, 0xc8, 0x49], [0xfc, 0x94, 0x2f]];
    assert!(!packet(&mut decoder, &mut subtitle, 1000, &caption));
    assert!(packet(
        &mut decoder,
        &mut subtitle,
        2000,
        &[[0xfc, 0x94, 0x2c]]
    ));
    assert_eq!(subtitle.pts, Some(1_000_000));
    assert_eq!(subtitle.end_ms, 1000);
    let lines: Vec<&[u8]> = subtitle.lines().collect();
    assert_eq!(
        lines,
        [&b"0,0,Default,,0,0,0,,{\\an7}{\\pos(38,182)}HI"[..]]
    );
    // The doubled erase is ignored: nothing more to report.
    assert!(!packet(
        &mut decoder,
        &mut subtitle,
        2033,
        &[[0xfc, 0x94, 0x2c]]
    ));
    assert!(!decoder.drain(&mut subtitle).expect("drain"));
}
