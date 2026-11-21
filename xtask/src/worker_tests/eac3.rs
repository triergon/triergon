//! Opt-in functional experiment; deliberately absent from the default test list.
use super::{Outcome, Scratch, TIMEOUT, argv, finish, output};
use crate::json::{self, Value};
use std::path::Path;

fn trace(
    worker: &Path,
    op: &str,
    file: &Path,
    extra: &[&dyn std::fmt::Display],
) -> Result<Vec<Value>, String> {
    let result = finish(&argv(worker, op, file, extra), TIMEOUT)?;
    if result.code != 0 {
        return Err(format!(
            "{op}: exit {}: {}",
            result.code,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    std::str::from_utf8(&result.stdout)
        .map_err(|e| e.to_string())?
        .lines()
        .map(|line| json::parse(line).map_err(|e| e.to_string()))
        .collect()
}

pub(super) fn run(root: &Path) -> Outcome {
    let scratch = Scratch::new()?;
    crate::transport_fixtures::eac3_probe::main(root, &[scratch.path().display().to_string()])?;
    crate::harness::build::eac3_experiment_workers(root, scratch.path())?;
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), Into::into);
    let candidate = target
        .join(crate::harness::gate_build::release_subdir()?)
        .join("transport-worker");
    let manifest = crate::checks::read_json(&scratch.path().join("inputs.json"))?;
    // Literal expected folds of the original authored inputs, in generator order.
    let folds: [u64; 10] = [
        4931118181656872344,
        9010230201963547148,
        9341172979077238791,
        8335105489591770401,
        10056975916167440644,
        9836296303524275752,
        4393127636328960007,
        7620691429583559969,
        6814399669389160617,
        10729622621970055899,
    ];
    let cases = super::items(&manifest, "cases")?;
    if cases.len() != folds.len() {
        return Err("expected exactly ten authored cases".into());
    }
    let mut failures = Vec::new();
    let mut matched = 0;
    let mut stock_matches = 0;
    for (case, fold) in cases.iter().zip(folds) {
        let name = case
            .get("file")
            .and_then(Value::as_str)
            .ok_or("missing file")?;
        let file = scratch.path().join(name);
        let c = trace(&candidate, "ts-au-trace", &file, &[&188, &7, &"--atsc"])?;
        let r = trace(
            &scratch.path().join("proof"),
            "ts-eac3-proof-trace",
            &file,
            &[],
        )?;
        let stock = output(&argv(
            &scratch.path().join("stock"),
            "ts-au-fold",
            &file,
            &[&188, &7, &"--atsc"],
        ))?;
        let ordinary = output(&argv(
            &candidate,
            "ts-au-fold",
            &file,
            &[&188, &7, &"--atsc"],
        ))?;
        let summary = c.last().ok_or("missing candidate summary")?;
        let reference = r.last().ok_or("missing adapter summary")?;
        let same_fold = |other: &Value| {
            ["packets", "bytes", "digest", "eof"]
                .iter()
                .all(|key| summary.get(key) == other.get(key))
        };
        let ok = c.len() == 4
            && r.len() == 4
            && c[..3] == r[..3]
            && same_fold(reference)
            && summary == &ordinary
            && summary.get("digest") == Some(&Value::int(fold))
            && summary.get("packets") == Some(&Value::int(3))
            && summary.get("eof") == Some(&Value::Bool(true))
            && reference.get("refusal").and_then(Value::as_str) == Some("");
        let stock_ok = same_fold(&stock);
        matched += usize::from(ok);
        stock_matches += usize::from(stock_ok);
        println!(
            "{name}: six-block {} (digest {fold}); stock exact fold {}",
            if ok { "match" } else { "FAIL" },
            if stock_ok { "match" } else { "different" }
        );
        if !ok {
            failures.push(name.to_owned());
        }
        if stock.get("eof") != Some(&Value::Bool(true)) {
            failures.push(format!("{name}: stock missing EOF"));
        }
    }
    println!(
        "six-block experiment: {matched}/10; stock ts-au-fold: {stock_matches}/10 exact matches (distinct operation)"
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join(", "))
    }
}
