//! Colour metadata: native parity, exact source values and complete measured work.

use super::checksum::{scalar_trace_checksum, work};
use super::{Context, Outcome, Scratch, argv, case_file, corpus_cases, execute, expected};
use crate::corpus;
use crate::harness::oracle::Kind;
use crate::harness::worker::{data, status};
use crate::json::Value;
use crate::pyrandom::PyRandom;

pub fn every_frozen_colour_profile_matches_native_and_exact_source(
    cx: &mut Context<'_>,
) -> Outcome {
    for row in corpus_cases(cx.root, "matroska-colour")? {
        let profile = row.get("profile").and_then(Value::as_str).unwrap_or("");
        let path = case_file(cx.root, "colour-fixtures", &row)?;
        cx.subtest(format!("profile={profile}"), |cx| {
            let workers = cx.workers;
            let left = execute(&argv(&workers.candidate, "metadata-trace", &path, &[]));
            let right = execute(&argv(&workers.reference, "metadata-trace", &path, &[]));
            cx.ok(&left, "candidate metadata-trace")?;
            cx.ok(&right, "reference metadata-trace")?;
            cx.no_differences(data(&left), data(&right), "candidate against reference")?;
            let source = execute(&argv(&workers.candidate, "metadata-source", &path, &[]));
            cx.ok(&source, "metadata-source")?;
            let oracle = expected(cx.root, Kind::Colour, &path)?;
            cx.no_differences(data(&source), &oracle, "source against the colour oracle")
        });
    }
    Ok(())
}

/// Python's `[rng.random() for _ in range(8)] + [rng.uniform(1, 10000), rng.random()]`.
fn mastering_values(rng: &mut PyRandom) -> Vec<f64> {
    let mut values: Vec<f64> = (0..8).map(|_| rng.random()).collect();
    values.push(1.0 + (10000.0 - 1.0) * rng.random());
    values.push(rng.random());
    values
}

/// 128 MasteringMetadata elements of alternating 8- and 4-byte floats, with values
/// from Python's `random.Random(9559)`: eight `random()`, one `uniform(1, 10000)`
/// and one more `random()` each.
pub fn seeded_mastering_rationals_match_the_public_native_representation(
    cx: &mut Context<'_>,
) -> Outcome {
    let source = corpus::Source::load(cx.root)?;
    let mut rng = PyRandom::new(9559);
    let scratch = Scratch::new()?;
    let workers = cx.workers;
    for n in 0..128 {
        let values = mastering_values(&mut rng);
        let width = if n % 2 == 1 { 4 } else { 8 };
        let data_bytes = corpus::colour_case(
            &source,
            Some(&Vec::new()),
            &corpus::mastering(width, false, &values),
            1,
        )?;
        let path = scratch.file("mastering.mkv", &data_bytes)?;
        let left = execute(&argv(&workers.candidate, "metadata-trace", &path, &[]));
        let right = execute(&argv(&workers.reference, "metadata-trace", &path, &[]));
        cx.same(status(&left), status(&right), format!("seed9559 case {n}"))?;
        cx.no_differences(
            data(&left),
            data(&right),
            format!("seed9559 case {n} {values:?}"),
        )?;
    }
    Ok(())
}

pub fn metadata_benchmarks_consume_every_canonical_field(cx: &mut Context<'_>) -> Outcome {
    let workers = cx.workers;
    for row in corpus_cases(cx.root, "matroska-colour")? {
        let path = case_file(cx.root, "colour-fixtures", &row)?;
        for worker in [&workers.candidate, &workers.reference] {
            let what = format!(
                "{} {}",
                worker.file_name().unwrap_or_default().to_string_lossy(),
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let trace = execute(&argv(worker, "metadata-trace", &path, &[]));
            let measured = execute(&argv(worker, "metadata-bench", &path, &[&1]));
            cx.same(status(&trace), status(&measured), format!("{what}: status"))?;
            let digest = scalar_trace_checksum(data(&trace)).map_err(|e| format!("{what}: {e}"))?;
            let at = |key: &str| data(&measured).get(key).cloned().unwrap_or(Value::Null);
            cx.equal(
                &at("digest"),
                &Value::int(digest),
                format!("{what}: digest"),
            )?;
            let (packets, _) = work(data(&trace)).map_err(|e| format!("{what}: {e}"))?;
            cx.equal(
                &at("packets"),
                &Value::int(packets as i128),
                format!("{what}: packets"),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_values_match_python() {
        // The first and last cases as Python's random.Random(9559) draws them.
        let mut rng = PyRandom::new(9559);
        let cases: Vec<Vec<f64>> = (0..128).map(|_| mastering_values(&mut rng)).collect();
        assert_eq!(
            cases[0],
            [
                0.33038400039194593,
                0.9962653281007027,
                0.05227672867159594,
                0.26512753137768286,
                0.9544313446132527,
                0.30948777232300795,
                0.6712282784582415,
                0.16541246147064448,
                1924.2820871676954,
                0.3179419270483341
            ]
        );
        assert_eq!(
            cases[127],
            [
                0.028444544276352346,
                0.79072728117924,
                0.49911521586045204,
                0.7729566871263351,
                0.28504326423790316,
                0.33991442884162304,
                0.2561392541551545,
                0.18829627012408967,
                9111.14353558351,
                0.2754549645009632
            ]
        );
    }
}
