//! Authored scripts, seeded scripts and independently recorded font store pins.

use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::Command;

use crate::harness::{build, oracle_compare, report};
use crate::json::{self, Value};
use crate::pyrandom::PyRandom;
use crate::{fetch, process, sha256};

const FEATURE: &str = "harness/features/font-store";
const INPUTS: &str = "harness/features/font-store/inputs";
const USAGE: &str = "usage: cargo xtask fonts store fixtures | manifest --record";
const AUTHORED: &[&str] = &[
    "oversized",
    "all-held",
    "access-order",
    "rebuild",
    "zero-size",
    "bookkeeping-only",
    "multiple-victims",
];
const SEEDED: &[(&str, u32, u32, u64)] = &[
    ("seeded-zero", 0, 0, 1951),
    ("seeded-one", 1, 2, 280),
    ("seeded-two", 2, 6, 7408),
    ("seeded-four", 4, 14, 15419),
    ("seeded-eight", 8, 48, 9559),
    ("seeded-spare", 12, 48, 11345),
];

fn seeded(slots: u32, extra: u32, seed: u64) -> String {
    let mut text = format!("budget {slots} {extra}\n");
    let mut rng = PyRandom::new(seed);
    for _ in 0..96 {
        let key = rng.below(8);
        match rng.below(8) {
            0 | 1 => writeln!(text, "hold {key}").unwrap(),
            2 | 3 => writeln!(text, "release {key}").unwrap(),
            _ => writeln!(text, "get {key} {}", rng.below(7)).unwrap(),
        }
    }
    text
}

fn case(id: &str, profile: &str, data: &[u8], overhead: &str) -> Value {
    Value::object([
        ("id", Value::str(id)),
        ("profile", Value::str(profile)),
        ("file", Value::str(format!("{id}.txt"))),
        ("sha256", Value::str(sha256::hex(data))),
        ("rights", Value::str("authored-synthetic")),
        ("arguments", Value::strings([overhead.to_owned()])),
    ])
}

/// Reproduce the seeded scripts and record the oracle's trace pins.
pub(super) fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let fixtures = match args {
        [cmd] if cmd == "fixtures" => true,
        [cmd, flag] if cmd == "manifest" && flag == "--record" => false,
        _ => return Err(USAGE.into()),
    };
    let feature = root.join(FEATURE);
    let inputs = root.join(INPUTS);
    if fixtures {
        for &(id, slots, extra, seed) in SEEDED {
            fetch::publish(
                &inputs.join(format!("{id}.txt")),
                seeded(slots, extra, seed).as_bytes(),
            )?;
        }
        println!("wrote {} seeded store scripts", SEEDED.len());
        return Ok(());
    }

    let identity = json::read(&root.join("target/harness/identity.json"))?;
    let worker = build::candidate_worker(root, &identity, "font-worker")?;
    let reported = process::output(Command::new(worker).arg("store-overhead"))?;
    let overhead = reported.trim();
    overhead
        .parse::<NonZeroU32>()
        .map_err(|_| "font-worker returned an invalid entry charge")?;

    let mut cases = Vec::new();
    for &id in AUTHORED {
        let path = inputs.join(format!("{id}.txt"));
        let data = fetch::read_bounded(&path, 64 << 10)?;
        cases.push(case(id, id, &data, overhead));
    }
    for &(id, slots, extra, seed) in SEEDED {
        let path = inputs.join(format!("{id}.txt"));
        let expected = seeded(slots, extra, seed);
        if fetch::read_bounded(&path, expected.len())? != expected.as_bytes() {
            return Err(format!("{} differs from its seeded script", path.display()));
        }
        cases.push(case(id, "seeded", expected.as_bytes(), overhead));
    }
    let mut listing = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(INPUTS)),
        ("cases", Value::Array(cases)),
    ]);
    let contract = json::read(&feature.join("contract.json"))?;
    oracle_compare::record(root, &contract, &mut listing)?;
    report::save_checkpoint(&feature.join("corpus.json"), &listing)?;
    println!(
        "recorded {} store trace pins",
        AUTHORED.len() + SEEDED.len()
    );
    Ok(())
}
