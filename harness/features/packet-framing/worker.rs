//! Packet-framing worker entry point. Enable the framing `reference-trace` feature.
//!
//! A binary calls `main` with its arguments excluding the executable name.
#![forbid(unsafe_code)]

#[path = "worker_model.rs"]
mod model;
#[path = "worker_operation.rs"]
mod operation;

use model::{Case, Outcome};
use std::io::{self, Read, Write};
use std::time::Instant;

fn execute(case: &Case, iterations: u32) -> triergon_core::Result<Outcome> {
    let mut total = Outcome::default();
    for _ in 0..iterations {
        let next = operation::run(case)?;
        total.digest.word(next.digest.0);
        total.digest.word(next.digest.1);
        total.packets = total
            .packets
            .checked_add(next.packets)
            .ok_or(triergon_core::Error::Overflow)?;
        total.bytes = total
            .bytes
            .checked_add(next.bytes)
            .ok_or(triergon_core::Error::Overflow)?;
    }
    Ok(total)
}

/// `nal-fold FILE`, `nal-bench FILE ITERATIONS`, `nal-memory FILE`, or `identity -`.
pub fn main(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if matches!(args, [mode] | [mode, _] if mode == "identity") {
        println!("{{\"operation\":\"NalPacketWholeFoldV1\",\"implementation\":\"safe-rust\"}}");
        return Ok(());
    }
    let mode = args.first().ok_or("mode required")?;
    let timing = mode == "nal-bench";
    let memory = mode == "nal-memory";
    if (!timing && !memory && mode != "nal-fold" && mode != "nal-check")
        || args.len() != if timing { 3 } else { 2 }
    {
        return Err("expected nal-fold FILE, nal-bench FILE ITERATIONS, or nal-memory FILE".into());
    }
    let iterations = if timing { args[2].parse::<u32>()? } else { 1 };
    if !(1..=1_000_000).contains(&iterations) {
        return Err("iterations must be 1..1000000".into());
    }
    let mut data = Vec::new();
    std::fs::File::open(&args[1])?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    let case = Case::parse(&data)?;
    if memory {
        println!("TRIERGON_BEGIN_V1");
        io::stdout().flush()?;
    }
    let start = timing.then(Instant::now);
    let total = execute(&case, iterations)?;
    let elapsed = start.map_or(0, |start| start.elapsed().as_nanos());
    if memory {
        println!("TRIERGON_END_V1");
        io::stdout().flush()?;
    }
    println!(
        "{{\"schema\":1,\"operation\":\"NalPacketWholeFoldV1\",\"iterations\":{iterations},\"packets\":{},\"bytes\":{},\"digest\":{},\"digest_b\":{},\"elapsed_ns\":{elapsed},\"eof\":true}}",
        total.packets, total.bytes, total.digest.0, total.digest.1
    );
    Ok(())
}

#[cfg(test)]
#[path = "fixtures.rs"]
mod fixtures;
#[cfg(test)]
#[path = "../../../xtask/src/json.rs"]
mod json;
#[cfg(test)]
#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_operation_matches_all_125_recorded_observations() {
        let fixtures = fixtures::load().unwrap();
        for fixture in &fixtures {
            let case = Case::parse(&fixture.bytes).unwrap();
            // Opening twice over the same immutable templates must not retain
            // configuration changes or packet charges from the first session.
            for _ in 0..2 {
                let once = operation::run(&case).unwrap();
                assert_eq!(once.packets, fixture.once.packets, "{} packets", fixture.id);
                assert_eq!(once.bytes, fixture.once.bytes, "{} bytes", fixture.id);
                assert_eq!(once.digest.0, fixture.once.digest, "{} digest", fixture.id);
                assert_eq!(
                    once.digest.1, fixture.once.digest_b,
                    "{} digest_b",
                    fixture.id
                );
                assert_eq!(case.pool.used(), case.baseline, "{} retention", fixture.id);
            }
            let folded = execute(&case, 1).unwrap();
            assert_eq!(
                folded.packets, fixture.expected.packets,
                "{} packets",
                fixture.id
            );
            assert_eq!(folded.bytes, fixture.expected.bytes, "{} bytes", fixture.id);
            assert_eq!(
                folded.digest.0, fixture.expected.digest,
                "{} digest",
                fixture.id
            );
            assert_eq!(
                folded.digest.1, fixture.expected.digest_b,
                "{} digest_b",
                fixture.id
            );
        }
        assert_eq!(fixtures.len(), 125);
    }

    #[test]
    fn avc_profile_extensions_match_recorded_reference_folds() {
        let fixtures = fixtures::load_extensions().unwrap();
        assert_eq!(fixtures.len(), 6);
        for fixture in fixtures {
            let case = Case::parse(&fixture.bytes).unwrap();
            let actual = execute(&case, 1).unwrap();
            let expected = fixture.expected;
            assert_eq!(actual.packets, expected.packets, "{} packets", fixture.id);
            assert_eq!(actual.bytes, expected.bytes, "{} bytes", fixture.id);
            assert_eq!(actual.digest.0, expected.digest, "{} digest", fixture.id);
            assert_eq!(
                actual.digest.1, expected.digest_b,
                "{} digest_b",
                fixture.id
            );
            assert_eq!(case.pool.used(), case.baseline, "{} retention", fixture.id);
        }
    }

    #[test]
    fn timed_iterations_fold_both_words_in_order_and_reset_each_session() {
        for fixture in fixtures::load().unwrap() {
            let case = Case::parse(&fixture.bytes).unwrap();
            let mut a = 0u64;
            let mut b = 0u64;
            for iterations in 1..=50 {
                // Deliberately use the byte recurrence, not the worker's grouped
                // digest helper, to check split words and iteration boundaries.
                for word in [fixture.once.digest, fixture.once.digest_b] {
                    for byte in word.to_le_bytes() {
                        a = a.wrapping_mul(31).wrapping_add(u64::from(byte));
                        b = b.wrapping_mul(131).wrapping_add(u64::from(byte));
                    }
                }
                if [1, 2, 50].contains(&iterations) {
                    let total = execute(&case, iterations).unwrap();
                    assert_eq!(total.digest.0, a, "{} iteration {iterations}", fixture.id);
                    assert_eq!(total.digest.1, b, "{} iteration {iterations}", fixture.id);
                    assert_eq!(total.packets, fixture.once.packets * u64::from(iterations));
                    assert_eq!(total.bytes, fixture.once.bytes * u64::from(iterations));
                    assert_eq!(case.pool.used(), case.baseline, "{} retention", fixture.id);
                }
            }
        }
    }

    #[test]
    fn script_envelope_refuses_truncation_and_trailing_data() {
        let fixtures = fixtures::load().unwrap();
        let raw = &fixtures[0].bytes;
        for length in 0..raw.len() {
            assert!(Case::parse(&raw[..length]).is_err(), "length {length}");
        }
        let mut trailing = raw.clone();
        trailing.push(0);
        assert!(Case::parse(&trailing).is_err());
    }
}
