//! Inputs and reference pins for the framing and ISO BMFF views workers.

mod adts;
pub(crate) mod config;
pub(crate) mod dts;
pub(crate) mod nal;
mod obu;

use std::fs;
use std::path::{Path, PathBuf};

use crate::harness::{features, manifest, oracle_compare, report, worker};
use crate::json::{self, Value};

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [cmd, flag, family]
            if cmd == "fixtures" && flag == "--write-manifests" && family == "config" =>
        {
            println!(
                "{}",
                config::fixtures(
                    &crate::corpus::Paths {
                        root: root.into(),
                        out: root.into()
                    },
                    true
                )?
            );
            Ok(())
        }
        [cmd, family]
            if cmd == "fixtures"
                && (family == "config" || config::IDS.contains(&family.as_str())) =>
        {
            println!(
                "{}",
                config::fixtures(
                    &crate::corpus::Paths {
                        root: root.into(),
                        out: root.into()
                    },
                    false
                )?
            );
            config::ensure_fetched(root)?;
            if family == "config" {
                for id in config::IDS {
                    command(root, &[cmd.clone(), id.into()], "framing-views")?;
                }
                Ok(())
            } else {
                command(root, args, "framing-views")
            }
        }
        [cmd, flag, family]
            if cmd == "fixtures" && flag == "--write-manifests" && family == "streams" =>
        {
            stream_fixtures(root, true)
        }
        [cmd, family] if cmd == "fixtures" && family == "streams" => stream_fixtures(root, false),
        [cmd, id]
            if cmd == "fixtures"
                && [obu::IDS, adts::IDS]
                    .into_iter()
                    .flatten()
                    .any(|name| name == id) =>
        {
            stream_fixtures(root, false)?;
            command(root, args, "framing-views")
        }
        [cmd, flag, id] if cmd == "fixtures" && flag == "--write-manifests" && id == "nal" => {
            println!(
                "{}",
                nal::fixtures(
                    &crate::corpus::Paths {
                        root: root.into(),
                        out: root.into()
                    },
                    true
                )?
            );
            Ok(())
        }
        [cmd, id] if cmd == "fixtures" && (id == "nal" || nal::IDS.contains(&id.as_str())) => {
            println!(
                "{}",
                nal::fixtures(
                    &crate::corpus::Paths {
                        root: root.into(),
                        out: root.into()
                    },
                    false
                )?
            );
            if id == "nal" {
                for id in nal::IDS {
                    command(root, &[cmd.clone(), id.into()], "framing-views")?;
                }
                return Ok(());
            }
            command(root, args, "framing-views")
        }
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
    let usage = || {
        format!(
            "usage: cargo xtask {family} fixtures [FEATURE] | manifest --record [FEATURE [--cases ID...]]"
        )
    };
    let (record, id, selected) = match args {
        [cmd, flag, id, select, cases @ ..]
            if cmd == "manifest"
                && flag == "--record"
                && select == "--cases"
                && !cases.is_empty() =>
        {
            (true, Some(id), cases)
        }
        [cmd, flag, rest @ ..] if cmd == "manifest" && flag == "--record" && rest.len() <= 1 => {
            (true, rest.first(), &[][..])
        }
        [cmd, rest @ ..] if cmd == "fixtures" && rest.len() <= 1 => (false, rest.first(), &[][..]),
        _ => return Err(usage()),
    };
    let candidate = format!("{family}-worker");
    // A contract named by its path may use any prefix worker: the recorder serves the
    // contracts whose workers have no family command of their own.
    let named = id.filter(|id| id.ends_with("/contract.json"));
    let contracts: Vec<PathBuf> = if let Some(path) = named {
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
        if named.is_none()
            && contract.get("candidate").and_then(Value::as_str) != Some(candidate.as_str())
        {
            return Err(format!("{} does not use {candidate}", path.display()));
        }
        let corpus_path = path.with_file_name("corpus.json");
        let (mut listing, _) = manifest::read(root, &corpus_path)?;
        if record {
            if contract.get("oracle").is_some() {
                if !selected.is_empty() {
                    return Err("selected recording needs a reference worker".into());
                }
                oracle_compare::record(root, &contract, &mut listing)?;
                if contract.get("id").and_then(Value::as_str) == Some("dts-unpacking") {
                    dts::decode_pins(root, &mut listing, true)?;
                }
            } else {
                reference_pins(root, &path, &corpus_path, &contract, &mut listing, selected)?;
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

/// Records each case's reference fold as `expected` and, for every case the contract
/// lists on this target in any list of its registry, the candidate's fold as
/// `candidate_expected`, so that a listed case pins both outputs of its difference
/// and not only that one exists. A case that is not listed keeps no candidate pin;
/// a refusal, or a case not comparable with the reference, keeps no reference pin
/// and its reference is not run. A contract pinned per target records this host's entries of
/// `expected_by_target` and `candidate_expected_by_target` and keeps the other
/// targets'; a reference fold that went through a step C leaves to the target keeps
/// its flag.
fn reference_pins(
    root: &Path,
    contract_path: &Path,
    manifest_path: &Path,
    contract: &Value,
    listing: &mut Value,
    selected: &[String],
) -> Result<(), String> {
    let target = report::gate_target()?;
    let per_target = crate::generators::pinned_per_target(contract).then_some(target.as_str());
    let directory = directory(root, contract, listing);
    let feature =
        report::PrefixFeature::load(root, contract_path, manifest_path, directory.as_deref())?;
    let identity = json::read(&root.join("target/harness/identity.json"))?;
    let reference = feature.reference_worker(root, &identity)?;
    let open = report::open(contract, listing, &target)?;
    let candidate = if open.ids().is_empty() {
        None
    } else {
        Some(crate::generators::built_candidate(
            root,
            &identity,
            &feature.candidate,
        )?)
    };
    let operation = contract
        .get("operation")
        .and_then(Value::as_str)
        .ok_or("no operation")?;
    let timeout = worker::contract_parity_timeout(contract)?;
    let fold = |worker: &Path, prefix: &str, case: &Value| -> Result<Value, String> {
        let input = feature.verified_input(case)?;
        let mut command = vec![
            worker.as_os_str().to_owned(),
            format!("{prefix}fold").into(),
            input.into(),
        ];
        command.extend(report::resolved_case_arguments(
            &feature.corpus,
            case,
            report::case_arguments(case),
        )?);
        let result = worker::run_in(&command, &[], Some(root), timeout)?
            .ok_or_else(|| format!("{} timed out", worker.display()))?;
        if result.code != 0 {
            return Err(format!(
                "{} {}: {}",
                worker.display(),
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
            return Err(format!(
                "{}: incomplete or incorrect operation",
                worker.display()
            ));
        }
        let mut summary =
            crate::harness::comparison::work_summary(&observed, &feature.work_fields)?;
        if per_target.is_some() && crate::generators::host_dependent(&observed) {
            crate::harness::validation::put(
                &mut summary,
                crate::generators::HOST_DEPENDENT,
                Value::Bool(true),
            )?;
        }
        Ok(summary)
    };
    let reference_prefix = feature
        .reference_prefix
        .as_deref()
        .unwrap_or(&feature.prefix);
    let mut cases = Vec::new();
    let mut pending: std::collections::BTreeSet<_> = selected.iter().map(String::as_str).collect();
    for mut case in feature.cases.clone() {
        let id = case.get("id").and_then(Value::as_str).unwrap_or_default();
        if !selected.is_empty() && !pending.remove(id) {
            cases.push(case);
            continue;
        }
        let listed = open.get(id);
        let expected = match listed {
            Some(listed) if listed.candidate_only => None,
            _ => Some(fold(reference.as_path(), reference_prefix, &case)?),
        };
        let observed = match (&candidate, listed) {
            (Some(candidate), Some(_)) => Some(fold(candidate.as_path(), &feature.prefix, &case)?),
            _ => None,
        };
        if observed.is_some() && observed == expected {
            return Err(format!(
                "{id}: listed as open, but the candidate's fold equals the reference's"
            ));
        }
        let (reference_key, candidate_key) = match per_target {
            Some(_) => (
                crate::generators::EXPECTED_BY_TARGET,
                "candidate_expected_by_target",
            ),
            None => ("expected", "candidate_expected"),
        };
        set_pin(&mut case, reference_key, per_target, expected)?;
        set_pin(&mut case, candidate_key, per_target, observed)?;
        cases.push(case);
    }
    if !pending.is_empty() {
        return Err(format!("unknown selected case IDs: {pending:?}"));
    }
    crate::harness::validation::put(listing, "cases", Value::Array(cases))
}

/// Sets `case`'s `key` to `fold`, or removes it for `None`; with a target, sets or
/// removes that target's entry of `key` and keeps the other targets'.
fn set_pin(case: &mut Value, key: &str, target: Option<&str>, fold: Option<Value>) -> Result<()> {
    let Value::Object(fields) = case else {
        return Err("a case is not an object".into());
    };
    let Some(target) = target else {
        fields.retain(|(name, _)| name != key);
        fields.extend(fold.map(|fold| (key.to_owned(), fold)));
        return Ok(());
    };
    let mut targets = match fields.iter().find(|(name, _)| name == key) {
        Some((_, Value::Object(targets))) => targets.clone(),
        Some(_) => return Err(format!("{key} is not keyed by target")),
        None => Vec::new(),
    };
    targets.retain(|(name, _)| name != target);
    targets.extend(fold.map(|fold| (target.to_owned(), fold)));
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    fields.retain(|(name, _)| name != key);
    if !targets.is_empty() {
        fields.push((key.to_owned(), Value::Object(targets)));
    }
    Ok(())
}

type Result<T, E = String> = std::result::Result<T, E>;
const STREAM_DIRECTORY: &str = "stream-parser-fixtures";
const DECODER: &str = ".research/reference-decode/install/bin/ffmpeg";

struct StreamCorpus {
    directory: PathBuf,
    cases: Vec<Value>,
}

impl StreamCorpus {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<String> {
        let path = self.directory.join(name);
        if fs::read(&path).as_deref().ok() != Some(bytes) {
            let pending = path.with_extension(format!("{}.partial", std::process::id()));
            fs::write(&pending, bytes).map_err(|e| format!("{}: {e}", pending.display()))?;
            fs::rename(&pending, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(crate::sha256::hex(bytes))
    }

    fn case(
        &mut self,
        id: &str,
        profile: &str,
        file: &str,
        pin: &str,
        rights: &str,
        args: &[&str],
    ) {
        self.cases.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(pin)),
            ("rights", Value::str(rights)),
            ("arguments", Value::strings(args.iter().copied())),
        ]));
    }
}

fn verify_hash(path: &Path, pin: &str) -> Result<()> {
    let actual = crate::sha256::file_hex(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if actual != pin {
        return Err(format!(
            "{}: expected SHA-256 {pin}, found {actual}",
            path.display()
        ));
    }
    Ok(())
}

/// Author the stream parser input controls for static gate preflight.
pub(crate) fn stream_controls(paths: &crate::corpus::Paths) -> Result<()> {
    let mut corpus = StreamCorpus {
        directory: paths.fixtures(STREAM_DIRECTORY)?,
        cases: Vec::new(),
    };
    obu::authored(paths, &mut corpus)?;
    adts::authored(paths, &mut corpus)
}

fn stream_fixtures(root: &Path, drafts: bool) -> Result<()> {
    let paths = crate::corpus::Paths {
        root: root.into(),
        out: root.into(),
    };
    let directory = paths.fixtures(STREAM_DIRECTORY)?;
    let mut av1 = StreamCorpus {
        directory: directory.clone(),
        cases: Vec::new(),
    };
    obu::authored(&paths, &mut av1)?;
    obu::encoded(root, &mut av1)?;
    let mut adts = StreamCorpus {
        directory,
        cases: Vec::new(),
    };
    adts::authored(&paths, &mut adts)?;
    adts::fate(root, &mut adts)?;
    for (ids, corpus) in [(obu::IDS, av1), (adts::IDS, adts)] {
        for id in ids {
            let cases: Vec<_> = corpus
                .cases
                .iter()
                .filter(|case| {
                    // The wire oracle exercises every authored OBU at all
                    // three window sizes. The packet filter has no such input
                    // boundary, so one representative suffices for each.
                    id != obu::IDS[0]
                        || !case
                            .get("profile")
                            .and_then(Value::as_str)
                            .is_some_and(|profile| profile.starts_with("authored-"))
                        || case
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id.ends_with("-w1"))
                })
                .cloned()
                .collect();
            if drafts {
                json::write(
                    &corpus.directory.join(format!("{id}-draft.json")),
                    &Value::object([
                        ("schema", Value::int(1)),
                        (
                            "directory",
                            Value::str(format!(".research/{STREAM_DIRECTORY}")),
                        ),
                        ("cases", Value::Array(cases)),
                    ]),
                )?;
            } else {
                let path = root.join(format!("harness/features/{id}/corpus.json"));
                let (listing, _) = manifest::read(root, &path)?;
                let expected = listing
                    .get("cases")
                    .and_then(Value::as_array)
                    .ok_or("missing cases")?;
                if expected.len() != cases.len()
                    || expected.iter().zip(&cases).any(|(listed, generated)| {
                        ["id", "profile", "file", "sha256", "rights", "arguments"]
                            .into_iter()
                            .any(|field| listed.get(field) != generated.get(field))
                    })
                {
                    return Err(format!(
                        "{}: stream parser corpus membership changed",
                        path.display()
                    ));
                }
                println!("verified {} cases for {id}", expected.len());
            }
        }
    }
    Ok(())
}
