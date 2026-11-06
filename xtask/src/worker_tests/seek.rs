//! Reference seek initialisation and source preroll after seeks.

use super::{Context, Outcome, argv, items, output};
use crate::json::Value;

const TARGETS: [i64; 6] = [
    0,
    250_000_000,
    2_500_000_000,
    7_500_000_000,
    15_500_000_000,
    25_000_000_000,
];

pub fn reference_unknown_cluster_fresh_and_after_eof_agree(cx: &mut Context<'_>) -> Outcome {
    let path = cx
        .root
        .join(".research/seek-fixtures/cues-unknown-cluster.mkv");
    let worker = cx.workers.reference.clone();
    for target in TARGETS {
        let fresh = output(&argv(&worker, "seek-trace", &path, &[&target]))?;
        let replay = output(&argv(
            &worker,
            "seek-trace",
            &path,
            &[&target, &"after-eof"],
        ))?;
        cx.equal(&fresh, &replay, format!("seek target {target}"))?;
    }
    Ok(())
}

pub fn codec_delay_audio_is_retained_without_repeating_initial_trim(
    cx: &mut Context<'_>,
) -> Outcome {
    let path = cx
        .root
        .join(".research/seek-fixtures/cues-delay-preroll.mkv");
    let workers = cx.workers;
    let expected = TARGETS.into_iter().zip([
        (-50_000_000i64, 400i64),
        (-50_000_000, 400),
        (1_950_000_000, 0),
        (6_950_000_000, 0),
        (14_950_000_000, 0),
        (14_950_000_000, 0),
    ]);
    let payload: String = (0u8..32).map(|b| format!("{b:02x}")).collect();
    for worker in [&workers.candidate, &workers.reference] {
        let name = worker.file_name().unwrap_or_default().to_string_lossy();
        for (target, (first_ns, trim)) in expected.clone() {
            for after_eof in [false, true] {
                let label = format!("worker={name} target={target} after-eof={after_eof}");
                cx.subtest(label, |cx| {
                    let mut args: Vec<&dyn std::fmt::Display> = vec![&target];
                    if after_eof {
                        args.push(&"after-eof");
                    }
                    let trace = output(&argv(worker, "seek-trace", &path, &args))?;
                    let first = items(&trace, "packets")?
                        .first()
                        .ok_or("no packets")?
                        .clone();
                    let at = |key: &str| first.get(key).cloned().unwrap_or(Value::Null);
                    cx.equal(&at("track"), &Value::int(1), "track")?;
                    cx.equal(&at("pts_ns"), &Value::int(first_ns), "pts_ns")?;
                    cx.equal(
                        &at("trim_start_samples"),
                        &Value::int(trim),
                        "trim_start_samples",
                    )?;
                    cx.equal(&at("payload"), &Value::str(&payload), "payload")
                });
            }
        }
    }
    Ok(())
}
