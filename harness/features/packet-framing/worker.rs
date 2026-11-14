//! Packet-framing worker entry point. Enable the framing `reference-trace` feature.
#![forbid(unsafe_code)]

#[path = "worker_model.rs"]
mod model;
#[path = "worker_operation.rs"]
mod operation;

use model::{CAP, Case, Outcome};
use std::env::ArgsOs;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Instant;
use triergon_core::Error;

#[derive(Debug)]
enum Failure {
    Usage(&'static str),
    Io(io::Error),
    Framing(Error),
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Framing(error)
    }
}

#[cold]
fn failed(error: Failure) -> ExitCode {
    match error {
        Failure::Usage(message) => eprintln!("framing worker: {message}"),
        Failure::Io(error) => eprintln!("framing worker: {error}"),
        Failure::Framing(error) => eprintln!("framing worker: {error}"),
    }
    ExitCode::FAILURE
}

fn execute(case: &Case, iterations: u32) -> triergon_core::Result<Outcome> {
    let mut total = Outcome::default();
    for _ in 0..iterations {
        let next = operation::run(case)?;
        total.digest.word(next.digest.0);
        total.digest.word(next.digest.1);
        total.packets = total
            .packets
            .checked_add(next.packets)
            .ok_or(Error::Overflow)?;
        total.bytes = total.bytes.checked_add(next.bytes).ok_or(Error::Overflow)?;
    }
    Ok(total)
}

fn read_chunk(input: &mut impl Read, output: &mut [u8]) -> io::Result<usize> {
    loop {
        match input.read(output) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn read_case(mut input: impl Read) -> Result<Case, Failure> {
    let mut chunk = [0; 1024];
    let mut filled = 0;
    while filled < chunk.len() {
        let length = read_chunk(&mut input, &mut chunk[filled..])?;
        if length == 0 {
            return Ok(Case::parse(&chunk[..filled])?);
        }
        filled += length;
    }
    // Small scripts never need a separate input allocation. Larger scripts
    // use the same scratch block and keep the complete input capped.
    let mut script = Vec::new();
    loop {
        let length = script.len() + filled;
        if length > CAP {
            return Err(Error::Limit {
                requested: length,
                maximum: CAP,
            }
            .into());
        }
        script.try_reserve(filled).map_err(|_| Error::Allocation)?;
        script.extend_from_slice(&chunk[..filled]);
        filled = read_chunk(&mut input, &mut chunk)?;
        if filled == 0 {
            return Ok(Case::parse(&script)?);
        }
    }
}

fn write_report(
    output: &mut impl Write,
    iterations: u32,
    total: Outcome,
    elapsed_ns: u64,
) -> io::Result<()> {
    const KEYS: [&[u8]; 6] = [
        b"{\"schema\":1,\"operation\":\"NalPacketWholeFoldV1\",\"iterations\":",
        b",\"packets\":",
        b",\"bytes\":",
        b",\"digest\":",
        b",\"digest_b\":",
        b",\"elapsed_ns\":",
    ];
    for (key, mut number) in KEYS.into_iter().zip([
        u64::from(iterations),
        total.packets,
        total.bytes,
        total.digest.0,
        total.digest.1,
        elapsed_ns,
    ]) {
        let mut digits = [0; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        output.write_all(key)?;
        output.write_all(&digits[start..])?;
    }
    output.write_all(b",\"eof\":true}\n")
}

/// `nal-fold FILE`, `nal-bench FILE ITERATIONS`, `nal-memory FILE`, or `identity -`.
pub fn main(args: ArgsOs) -> ExitCode {
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => failed(error),
    }
}

fn run(mut args: ArgsOs) -> Result<(), Failure> {
    args.next();
    let mode = args.next();
    let mode = mode
        .as_deref()
        .and_then(OsStr::to_str)
        .ok_or(Failure::Usage("mode required"))?;
    let mut output = io::stdout().lock();
    if mode == "identity" && args.len() <= 1 {
        output.write_all(
            b"{\"operation\":\"NalPacketWholeFoldV1\",\"implementation\":\"safe-rust\"}\n",
        )?;
        return Ok(());
    }
    let timing = mode == "nal-bench";
    let memory = mode == "nal-memory";
    if (!timing && !memory && mode != "nal-fold" && mode != "nal-check")
        || args.len() != if timing { 2 } else { 1 }
    {
        return Err(Failure::Usage(
            "expected nal-fold FILE, nal-bench FILE ITERATIONS, or nal-memory FILE",
        ));
    }
    let path = args.next().ok_or(Failure::Usage("file required"))?;
    let iterations = if timing {
        args.next()
            .as_deref()
            .and_then(OsStr::to_str)
            .and_then(|count| count.parse::<u32>().ok())
            .ok_or(Failure::Usage("invalid iteration count"))?
    } else {
        1
    };
    if !(1..=1_000_000).contains(&iterations) {
        return Err(Failure::Usage("iterations must be 1..1000000"));
    }
    let case = read_case(File::open(path)?)?;
    if memory {
        output.write_all(b"TRIERGON_BEGIN_V1\n")?;
        output.flush()?;
    }
    let start = timing.then(Instant::now);
    let total = execute(&case, iterations)?;
    let elapsed = if let Some(start) = start {
        u64::try_from(start.elapsed().as_nanos()).map_err(|_| Error::Overflow)?
    } else {
        0
    };
    if memory {
        output.write_all(b"TRIERGON_END_V1\n")?;
        output.flush()?;
    }
    Ok(write_report(&mut output, iterations, total, elapsed)?)
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
    fn script_reader_keeps_bytes_across_the_scratch_boundary() {
        for length in [900, 1011, 1012, 2600] {
            let data: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            let mut script = b"SMBSF001\x01".to_vec();
            script.extend((length as u32).to_be_bytes());
            script.extend(&data);
            script.extend(0_u32.to_be_bytes());
            let case = read_case(script.as_slice()).unwrap();
            assert_eq!(case.configuration.as_ref(), data);
            assert!(case.events.is_empty());
        }
    }

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
