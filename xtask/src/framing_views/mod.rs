//! Inputs and reference pins for the framing and ISO BMFF views workers.

pub(crate) mod dts;

use std::path::{Path, PathBuf};

use crate::harness::{features, manifest, oracle_compare, report, worker};
use crate::json::{self, Value};

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [cmd, id] if cmd == "fixtures" && id == "dts" => {
            dts::fixtures(root)?;
            for id in dts::IDS {
                command(root, &[cmd.clone(), id.into()], "framing-views")?;
            }
            Ok(())
        }
        [cmd, flag, id] if cmd == "manifest" && flag == "--record" && id == "dts" => {
            let ordered = ["dts-unpacking"]
                .into_iter()
                .chain(dts::IDS.into_iter().filter(|id| *id != "dts-unpacking"));
            for id in ordered {
                command(
                    root,
                    &[cmd.clone(), flag.clone(), id.into()],
                    "framing-views",
                )?;
            }
            Ok(())
        }
        _ => command(root, args, "framing-views"),
    }
}

pub(super) fn command(root: &Path, args: &[String], family: &str) -> Result<(), String> {
    let usage =
        || format!("usage: cargo xtask {family} fixtures [FEATURE] | manifest --record [FEATURE]");
    let (record, id) = match args {
        [cmd, flag, rest @ ..] if cmd == "manifest" && flag == "--record" && rest.len() <= 1 => {
            (true, rest.first())
        }
        [cmd, rest @ ..] if cmd == "fixtures" && rest.len() <= 1 => (false, rest.first()),
        _ => return Err(usage()),
    };
    let candidate = format!("{family}-worker");
    let contracts: Vec<PathBuf> = if let Some(path) = id.filter(|id| id.ends_with("/contract.json"))
    {
        vec![root.join(path)]
    } else {
        features::discover(root)?
            .into_iter()
            .filter(|contract| {
                contract.candidate() == Some(&candidate) && id.is_none_or(|id| *id == contract.id)
            })
            .map(|contract| contract.path)
            .collect()
    };
    if contracts.is_empty() {
        return Err(format!("{family} has no matching contract"));
    }
    for path in contracts {
        let contract = json::read(&path)?;
        if contract.get("candidate").and_then(Value::as_str) != Some(candidate.as_str()) {
            return Err(format!("{} does not use {candidate}", path.display()));
        }
        let corpus_path = path.with_file_name("corpus.json");
        let (mut listing, _) = manifest::read(root, &corpus_path)?;
        if record {
            if contract.get("oracle").is_some() {
                oracle_compare::record(root, &contract, &mut listing)?;
                if contract.get("id").and_then(Value::as_str) == Some("dts-unpacking") {
                    dts::decode_pins(root, &mut listing, true)?;
                }
            } else {
                reference_pins(root, &path, &corpus_path, &contract, &mut listing)?;
            }
            report::save_checkpoint(&corpus_path, &listing)?;
            println!("recorded {}", corpus_path.display());
        } else {
            let directory = directory(root, &contract, &listing);
            let directory = directory.as_deref().ok_or("no corpus directory")?;
            for case in listing
                .get("cases")
                .and_then(Value::as_array)
                .ok_or("manifest needs cases")?
            {
                if let Some(problem) = report::input_problem(
                    directory,
                    case,
                    contract.get("worker_operation").and_then(Value::as_str),
                )? {
                    return Err(format!("{}: {problem}", corpus_path.display()));
                }
            }
            println!("verified {}", corpus_path.display());
        }
    }
    Ok(())
}

fn directory(root: &Path, contract: &Value, listing: &Value) -> Option<PathBuf> {
    contract
        .get("corpus_directory")
        .and_then(Value::as_str)
        .map(|dir| root.join(dir))
        .or_else(|| report::recorded_corpus(root, listing))
}

fn reference_pins(
    root: &Path,
    contract_path: &Path,
    manifest_path: &Path,
    contract: &Value,
    listing: &mut Value,
) -> Result<(), String> {
    if crate::generators::pinned_per_target(contract) {
        return Err("per-target folds need a feature-specific recorder".into());
    }
    let directory = directory(root, contract, listing);
    let feature =
        report::PrefixFeature::load(root, contract_path, manifest_path, directory.as_deref())?;
    let identity = json::read(&root.join("target/harness/identity.json"))?;
    let reference = feature.reference_worker(root, &identity)?;
    let operation = contract
        .get("operation")
        .and_then(Value::as_str)
        .ok_or("no operation")?;
    let timeout = worker::contract_parity_timeout(contract)?;
    let mut cases = Vec::new();
    for mut case in feature.cases.clone() {
        let input = feature.verified_input(&case)?;
        let mut command = vec![
            reference.as_os_str().to_owned(),
            format!("{}fold", feature.prefix).into(),
            input.into(),
        ];
        command.extend(report::resolved_case_arguments(
            &feature.corpus,
            &case,
            report::case_arguments(&case),
        )?);
        let result =
            worker::run_in(&command, &[], Some(root), timeout)?.ok_or("reference timed out")?;
        if result.code != 0 {
            return Err(format!(
                "{}: {}",
                command[2].to_string_lossy(),
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        let observed = json::parse_observations(
            std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if observed.get("operation").and_then(Value::as_str) != Some(operation)
            || observed.get("eof") != Some(&Value::Bool(true))
            || observed.get("iterations") != Some(&Value::int(1))
        {
            return Err("incomplete or incorrect reference operation".into());
        }
        let summary = crate::harness::comparison::work_summary(&observed, &feature.work_fields)?;
        crate::harness::validation::put(&mut case, "expected", summary)?;
        cases.push(case);
    }
    crate::harness::validation::put(listing, "cases", Value::Array(cases))
}
