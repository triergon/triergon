//! The per-pair decode against the reference's reports of crafted packets
//! (one second apart unless stated, each lasting until the next).

use super::*;

const RCL: u8 = 0x20;
const RU2: u8 = 0x25;
const RDC: u8 = 0x29;
const RTD: u8 = 0x2b;
const EDM: u8 = 0x2c;
const CR: u8 = 0x2d;
const EOC: u8 = 0x2f;

fn with_parity(byte: u8) -> u8 {
    if odd_parity(byte) { byte } else { byte | 0x80 }
}

/// A field 1 construct of one pair.
fn code(hi: u8, lo: u8) -> [u8; 3] {
    [0xfc, with_parity(hi), with_parity(lo)]
}

/// A miscellaneous control code as broadcast: twice.
fn misc(command: u8) -> Vec<[u8; 3]> {
    vec![code(0x14, command); 2]
}

/// Row 15, white, column 0, sent twice.
fn pac15() -> Vec<[u8; 3]> {
    vec![code(0x14, 0x60); 2]
}

fn text(s: &str) -> Vec<[u8; 3]> {
    s.as_bytes()
        .chunks(2)
        .map(|c| code(c[0], c.get(1).copied().unwrap_or(0)))
        .collect()
}

/// The eighth note, a special character (a control code), sent once.
fn note() -> Vec<[u8; 3]> {
    vec![code(0x11, 0x37)]
}

fn row(row: usize, text: &str) -> String {
    format!("{{\\an7}}{{\\pos(38,{})}}{text}", [228, 243][row - 14])
}

/// One reported subtitle: pts in microseconds, end and `(ReadOrder, text)` lines.
#[derive(Debug, PartialEq, Eq)]
struct Event {
    pts: Option<i64>,
    end: u32,
    lines: Vec<(u32, String)>,
}

fn event(pts: Option<i64>, end: u32, lines: &[(u32, &str)]) -> Event {
    Event {
        pts,
        end,
        lines: lines.iter().map(|&(n, t)| (n, t.to_owned())).collect(),
    }
}

/// Decodes `packets` (pts in microseconds, `None` unknown; each packet lasts
/// until the next, the last 1000 ms) and drains: `(packet index, event)`.
fn run(options: Options, packets: &[(Option<i64>, Vec<[u8; 3]>)]) -> Vec<(usize, Event)> {
    let pool = Budget::new(1 << 20).expect("pool");
    let mut decoder = Decoder::new(&pool, options);
    let mut out = Subtitle::new(&pool);
    let mut events = Vec::new();
    let mut take = |index: usize, out: &Subtitle| {
        let lines = out.lines().map(|line| {
            let line = std::str::from_utf8(line).expect("utf-8");
            let (order, rest) = line.split_once(',').expect("order");
            let text = rest.splitn(8, ',').nth(7).expect("text");
            (order.parse().expect("order"), text.to_owned())
        });
        events.push((
            index,
            Event {
                pts: out.pts,
                end: out.end_ms,
                lines: lines.collect(),
            },
        ));
    };
    for (index, (pts, pairs)) in packets.iter().enumerate() {
        let next = packets.get(index + 1).and_then(|(next, _)| *next);
        let duration_ms = match (pts, next) {
            (Some(pts), Some(next)) => u32::try_from((next - pts) / 1000).expect("duration"),
            _ => 1000,
        };
        let data: Vec<u8> = pairs.iter().flatten().copied().collect();
        let mut work = WorkBudget::new(Decoder::packet_work(data.len()));
        let clock = Clock {
            pts: *pts,
            duration_ms,
        };
        if decoder
            .decode(&data, clock, &mut out, &mut work)
            .expect("decode")
        {
            take(index, &out);
        }
    }
    while decoder.drain(&mut out).expect("drain") {
        take(packets.len(), &out);
    }
    events
}

fn at(seconds: i64, parts: &[Vec<[u8; 3]>]) -> (Option<i64>, Vec<[u8; 3]>) {
    (Some(seconds * 1_000_000), parts.concat())
}

const REAL_TIME: Options = Options {
    real_time: true,
    real_time_latency_ms: 200,
    data_field: DataField::Auto,
};

/// A pop-on caption `A`, `body`, `B` shown at 1 s and erased at 2 s.
fn noted(body: &[Vec<[u8; 3]>]) -> String {
    let mut parts = vec![misc(RCL), pac15(), text("A")];
    parts.extend_from_slice(body);
    parts.extend([text("B"), misc(EOC)]);
    let events = run(Options::default(), &[at(1, &parts), at(2, &[misc(EDM)])]);
    let [(1, event)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!((event.pts, event.end), (Some(1_000_000), 1000));
    event.lines[0].1.clone()
}

#[test]
fn a_repeated_control_code_acts_once() {
    let once = row(15, "A♪B");
    for times in 1..=5 {
        assert_eq!(noted(&vec![note(); times]), once, "{times}");
    }
}

#[test]
fn any_other_pair_ends_the_repetition() {
    let between = |pair: [u8; 3]| noted(&[note(), vec![pair], note()]);
    assert_eq!(between(code(b'x', 0)), row(15, "A♪x♪B"));
    // A first byte failing parity is a block character.
    assert_eq!(between([0xfc, 0x11, 0x37]), row(15, "A♪█7♪B"));
    // An XDS pair, the code on data channel 2, a code without a meaning.
    assert_eq!(between(code(0x01, 0x03)), row(15, "A♪♪B"));
    assert_eq!(between(code(0x19, 0x37)), row(15, "A♪♪B"));
    assert_eq!(between(code(0x10, 0x60)), row(15, "A♪♪B"));
    // Padding and a second byte failing parity are dropped first.
    assert_eq!(between([0xfc, 0x80, 0x80]), row(15, "A♪B"));
    assert_eq!(between([0xfc, 0x91, 0xb7]), row(15, "A♪B"));
}

#[test]
fn a_repetition_spans_packets() {
    let events = run(
        Options::default(),
        &[
            at(1, &[misc(RCL), pac15(), text("A"), misc(EOC)]),
            at(2, &[misc(RCL), pac15(), text("B"), vec![code(0x14, EOC)]]),
            // The same end of caption again: ignored.
            at(3, &[vec![code(0x14, EOC)]]),
            at(4, &[misc(EDM)]),
        ],
    );
    assert_eq!(
        events,
        [
            (1, event(Some(1_000_000), 1000, &[(0, &row(15, "A"))])),
            (3, event(Some(2_000_000), 2000, &[(1, &row(15, "B"))])),
        ]
    );
}

#[test]
fn a_pop_on_caption_is_reported_when_it_leaves() {
    let caption = |s| [misc(RCL), pac15(), text(s), misc(EOC)].concat();
    let events = run(
        Options::default(),
        &[
            at(1, &[caption("A")]),
            // Two captions in one packet: one subtitle with the last
            // report's start, whose duration of 0 becomes the packet's.
            at(2, &[caption("B"), caption("C")]),
            at(3, &[misc(EDM)]),
        ],
    );
    assert_eq!(
        events,
        [
            (
                1,
                event(
                    Some(2_000_000),
                    1000,
                    &[(0, &row(15, "A")), (1, &row(15, "B"))]
                )
            ),
            (2, event(Some(2_000_000), 1000, &[(2, &row(15, "C"))])),
        ]
    );
}

#[test]
fn the_waiting_pop_on_caption_is_drained() {
    let caption = |s| [misc(RCL), pac15(), text(s), misc(EOC)].concat();
    let events = run(
        Options::default(),
        &[at(1, &[caption("A")]), at(2, &[caption("B")])],
    );
    assert_eq!(
        events,
        [
            (1, event(Some(1_000_000), 1000, &[(0, &row(15, "A"))])),
            (2, event(Some(2_000_000), 1000, &[(1, &row(15, "B"))])),
        ]
    );
}

#[test]
fn a_pop_on_caption_outlives_another_mode() {
    let events = run(
        Options::default(),
        &[
            at(1, &[misc(RCL), pac15(), text("A"), misc(EOC)]),
            at(2, &[misc(RU2), pac15(), text("C"), misc(CR)]),
            at(3, &[misc(RCL), pac15(), text("D"), misc(EOC)]),
            at(4, &[misc(EDM)]),
        ],
    );
    assert_eq!(
        events,
        [
            (1, event(Some(1_000_000), 1000, &[(0, &row(15, "C"))])),
            (2, event(Some(2_000_000), 1000, &[(1, &row(15, "A"))])),
            (3, event(Some(3_000_000), 1000, &[(2, &row(15, "D"))])),
        ]
    );
}

#[test]
fn a_carriage_return_reports_before_it_scrolls() {
    let roll = run(
        Options::default(),
        &[
            at(1, &[misc(RU2), pac15(), text("A")]),
            at(2, &[misc(CR), text("B"), misc(CR)]),
            at(3, &[misc(EDM)]),
        ],
    );
    let both = format!("{}\\N{}", row(14, "A"), row(15, "B"));
    assert_eq!(
        roll,
        [
            (
                1,
                event(Some(2_000_000), 1000, &[(0, &row(15, "A")), (1, &both)])
            ),
            (2, event(Some(2_000_000), 1000, &[(2, &row(14, "B"))])),
        ]
    );
    // Text mode does not scroll.
    let text_mode = run(
        Options::default(),
        &[
            at(1, &[misc(RTD), pac15(), text("A"), misc(CR)]),
            at(2, &[text("B"), misc(CR)]),
        ],
    );
    assert_eq!(
        text_mode,
        [
            (0, event(Some(0), 1000, &[(0, &row(15, "A"))])),
            (1, event(Some(1_000_000), 1000, &[(1, &row(15, "B"))])),
        ]
    );
}

#[test]
fn an_empty_capture_is_not_reported_but_starts_the_next() {
    let events = run(
        Options::default(),
        &[
            at(1, &[misc(EDM)]),
            at(2, &[misc(RDC), pac15(), text("A")]),
            at(3, &[misc(EDM), misc(RU2), misc(CR)]),
        ],
    );
    // The empty carriage return after the erase changes neither start nor end.
    assert_eq!(
        events,
        [(2, event(Some(1_000_000), 2000, &[(0, &row(15, "A"))]))]
    );
}

#[test]
fn real_time_reports_the_screen_as_it_is() {
    let caption = |s| [misc(RCL), pac15(), text(s), misc(EOC)].concat();
    let events = run(
        REAL_TIME,
        &[
            at(1, &[caption("A")]),
            at(2, &[caption("B"), caption("C")]),
            at(3, &[misc(EDM)]),
        ],
    );
    assert_eq!(
        events,
        [
            (0, event(Some(0), u32::MAX, &[(0, &row(15, "A"))])),
            (
                1,
                event(
                    Some(2_000_000),
                    u32::MAX,
                    &[(1, &row(15, "B")), (2, &row(15, "C"))]
                )
            ),
            (2, event(Some(2_000_000), u32::MAX, &[(3, "")])),
        ]
    );
}

#[test]
fn a_real_time_report_moves_the_latency_clock_to_its_start() {
    let ms = |ms: i64| Some(ms * 1000);
    let options = Options {
        real_time_latency_ms: 500,
        ..REAL_TIME
    };
    let events = run(
        options,
        &[
            (ms(5000), [misc(RDC), misc(EDM)].concat()),
            (ms(5132), [pac15(), text("A")].concat()),
            (ms(5165), misc(EDM)),
            // The clock is at 5000: 5198 is too early, 5528 is not.
            (ms(5198), text("B")),
            (ms(5528), text("C")),
            (ms(5660), text("D")),
        ],
    );
    assert_eq!(
        events,
        [
            (0, event(Some(0), u32::MAX, &[(0, "")])),
            (1, event(ms(5132), u32::MAX, &[(1, &row(15, "A"))])),
            (2, event(ms(5000), u32::MAX, &[(2, "")])),
            (4, event(ms(5528), u32::MAX, &[(3, &row(15, "ABC"))])),
        ]
    );
}

#[test]
fn a_real_time_report_clears_the_written_characters() {
    let events = run(
        REAL_TIME,
        &[
            at(1, &[misc(RDC), pac15(), text("A")]),
            at(2, &[pac15(), text("B"), misc(EDM)]),
            at(4, &[misc(RDC)]),
            at(5, &[pac15(), text("C")]),
        ],
    );
    assert_eq!(
        events,
        [
            (0, event(Some(1_000_000), u32::MAX, &[(0, &row(15, "A"))])),
            (1, event(Some(0), u32::MAX, &[(1, "")])),
            (3, event(Some(5_000_000), u32::MAX, &[(2, &row(15, "C"))])),
        ]
    );
}

#[test]
fn a_report_at_an_unknown_time() {
    let erased = run(
        Options::default(),
        &[
            (Some(900_000), [misc(RCL), pac15(), text("AB")].concat()),
            (Some(967_633), misc(EOC)),
            (None, misc(EDM)),
        ],
    );
    assert_eq!(
        erased,
        [(
            2,
            event(Some(967_633), 2_783_137_840, &[(0, &row(15, "AB"))])
        )]
    );
    let replaced = run(
        Options::default(),
        &[
            (Some(900_000), [misc(RCL), pac15(), text("AB")].concat()),
            (Some(934_267), [misc(EOC), text("CD")].concat()),
            (None, misc(EOC)),
        ],
    );
    assert_eq!(
        replaced,
        [
            (
                2,
                event(Some(934_267), 2_783_137_874, &[(0, &row(15, "AB"))])
            ),
            (3, event(None, 2_783_137_874, &[(1, &row(15, "CD"))])),
        ]
    );
    let painted = run(
        Options::default(),
        &[
            (Some(967_633), [misc(RDC), pac15(), text("AB")].concat()),
            (None, misc(EDM)),
        ],
    );
    assert_eq!(
        painted,
        [(1, event(Some(0), 1_511_828_488, &[(0, &row(15, "AB"))]))]
    );
}
