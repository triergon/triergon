//! Timing boundaries of the exact source oracle: sub-tick, negative and
//! fractional lace values.

use super::{Context, Outcome, Scratch, expected, items};
use crate::ebml::{element as e, uint as u};
use crate::harness::oracle::Kind;
use crate::json::{self, Value};

/// One PCM track at `scale` nanoseconds per tick with `frames` laced frames in
/// one block at relative time `relative`: a SimpleBlock, or a BlockGroup with
/// `group_ticks` of BlockDuration.
fn document(
    scale: u64,
    duration: Option<u64>,
    delay: u64,
    group_ticks: Option<u64>,
    frames: u8,
    relative: i16,
) -> Vec<u8> {
    let header = e(
        0x1A45DFA3,
        &[
            u(0x4286, 1),
            u(0x42F7, 1),
            u(0x42F2, 4),
            u(0x42F3, 8),
            e(0x4282, b"matroska"),
            u(0x4287, 4),
            u(0x4285, 2),
        ]
        .concat(),
    );
    let mut track = [
        u(0xD7, 1),
        u(0x73C5, 1),
        u(0x83, 2),
        e(0x86, b"A_PCM/INT/LIT"),
        u(0x56AA, delay),
    ]
    .concat();
    if let Some(duration) = duration {
        track.extend(u(0x23E383, duration));
    }
    track.extend(e(
        0xE1,
        &[
            u(0x9F, 1),
            u(0x6264, 16),
            e(0xB5, &[0x40, 0xbf, 0x40, 0, 0, 0, 0, 0]),
        ]
        .concat(),
    ));
    let mut block = vec![0x81];
    block.extend(relative.to_be_bytes());
    block.push(if frames > 1 { 0x84 } else { 0x80 });
    if frames > 1 {
        block.push(frames - 1);
    }
    for _ in 0..frames {
        block.extend([0, 1]);
    }
    let payload = match group_ticks {
        None => e(0xA3, &block),
        Some(ticks) => e(0xA0, &[e(0xA1, &block), u(0x9B, ticks)].concat()),
    };
    let segment = [
        e(0x1549A966, &u(0x2AD7B1, scale)),
        e(0x1654AE6B, &e(0xAE, &track)),
        e(0x1F43B675, &[u(0xE7, 0), payload].concat()),
    ]
    .concat();
    [header, e(0x18538067, &segment)].concat()
}

fn source_packets(cx: &Context<'_>, data: &[u8]) -> Result<Vec<Value>, String> {
    let scratch = Scratch::new()?;
    let path = scratch.file("source.mkv", data)?;
    let trace = expected(cx.root, Kind::Timing, &path)?;
    Ok(items(&trace, "packets")?.to_vec())
}

fn rational(text: &str) -> Value {
    json::parse(text).expect("literal")
}

pub fn exact_source_oracle_keeps_sub_tick_and_negative_values(cx: &mut Context<'_>) -> Outcome {
    let packets = source_packets(
        cx,
        &document(1_000_000, Some(3_500_000), 2_500_000, None, 3, 0),
    )?;
    let numerators = packets
        .iter()
        .map(|p| {
            p.get("pts")
                .and_then(|pts| pts.get("numerator"))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    cx.equal(
        &Value::Array(numerators),
        &Value::Array(vec![
            Value::int(-2_500_000),
            Value::int(1_000_000),
            Value::int(4_500_000),
        ]),
        "presentation times",
    )?;
    let first = packets.first().ok_or("no packets")?;
    cx.equal(
        first.get("duration").unwrap_or(&Value::Null),
        &rational(r#"{"numerator": 3500000, "denominator": 1}"#),
        "first duration",
    )
}

pub fn independent_source_oracle_keeps_fractional_lace_duration(cx: &mut Context<'_>) -> Outcome {
    let packets = source_packets(cx, &document(1_000_000, None, 0, Some(1), 3, 0))?;
    let packet = packets.get(2).ok_or("fewer than three packets")?;
    cx.equal(
        packet.get("pts").unwrap_or(&Value::Null),
        &rational(r#"{"numerator": 2000000, "denominator": 3}"#),
        "third presentation time",
    )?;
    cx.equal(
        packet.get("duration").unwrap_or(&Value::Null),
        &rational(r#"{"numerator": 1000000, "denominator": 3}"#),
        "third duration",
    )
}
