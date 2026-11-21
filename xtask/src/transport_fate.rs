//! Tracked TS identity coverage and black-box observations of original samples.
//! No upstream implementation or test definitions are opened by this command.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::checks::read_json;
use crate::corpus::write_json;
use crate::fetch::read_bounded;
use crate::harness::worker;
use crate::json::{self, Value};
use crate::sha256;

type R<T> = Result<T, String>;
const LIMIT: usize = 64 * 1024 * 1024;
const INDEXES: [&str; 2] = ["harness/fate/index.json", "harness/fate/formats-index.json"];
const INVENTORY: &str = "harness/fate/transport-coverage.json";
const OBSERVATION_CONTRACT: &str = "harness/fate/transport-observations-contract.json";
const REVISION: &str = "d32b387f2b0a484599d4587d651891f0c63c4238";

fn text<'a>(value: &'a Value, key: &str) -> R<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}
fn array<'a>(value: &'a Value, key: &str) -> R<&'a [Value]> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing {key}"))
}
fn strings(value: &Value, key: &str) -> R<Vec<String>> {
    array(value, key)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("invalid {key}"))
        })
        .collect()
}
fn relevant(row: &Value) -> R<bool> {
    Ok(text(row, "status")? == "unmapped"
        && (text(row, "id")?.contains("mpegts")
            || strings(row, "definition_files")?
                .iter()
                .any(|s| s.contains("mpegts.mak:"))
            || strings(row, "sample_references")?.iter().any(|s| {
                Path::new(s).extension().is_some_and(|extension| {
                    ["ts", "m2ts", "mts"]
                        .iter()
                        .any(|expected| extension.eq_ignore_ascii_case(expected))
                })
            })))
}

fn classification(id: &str) -> R<(&'static str, &'static str, Vec<&'static str>)> {
    Ok(match id {
        "fate-mpegts" => (
            "aggregate",
            "Aggregate identity; no independent operation or sample.",
            vec![],
        ),
        "fate-mpegts-probe-pmt-merge" => (
            "probe",
            "Stream discovery with PMT version merging; source PMT replacement is only a related control, not merged stream identity parity.",
            vec![
                "mpegts-pes:pmt-version-wrap-retypes-pid",
                "ts-program:program-pmt-retype",
            ],
        ),
        "fate-mpegts-probe-program" => (
            "program-choice",
            "Probe program catalogue, membership and selection on the original multi-program input; explicit program 1 playback is only related.",
            vec!["ts-program:program-eof-avc-aac"],
        ),
        "fate-mpegts-probe-latm" => (
            "probe",
            "LATM discovery, configuration and stream metadata on the original input; finite PES observations and strict LOAS framing are separate controls.",
            vec![
                "mpegts-access-units:au-latm-aac-lc-three-frames-in-pes",
                "mpegts-pes:carriage-aac-latm-direct",
            ],
        ),
        "fate-matroska-dvbsub-remux"
        | "fate-matroska-h264-remux"
        | "fate-matroska-mpegts-remux"
        | "fate-mov-mp4-disposition-mpegts-remux" => (
            "remux",
            "Complete remux output, metadata and destination-container behavior remain uncovered; TS packet extraction alone is insufficient.",
            vec![],
        ),
        "fate-sub-dvb" => (
            "codec",
            "DVB subtitle decoding of the original sample is distinct from transport framing and program discovery.",
            vec!["mpegts-access-units:dvb2-literals-fields"],
        ),
        "fate-mpegts-lcevc-h264-single-track" | "fate-mpegts-lcevc-hevc-single-track" => (
            "enhancement",
            "In-band LCEVC enhancement metadata is not established by base AVC/HEVC packet parity; no enhancement decoding is claimed.",
            vec![],
        ),
        "fate-mpegts-lcevc-h264-dual-track" | "fate-mpegts-lcevc-hevc-dual-track" => (
            "enhancement",
            "Separate LCEVC track identification, association and payload mapping remain uncovered; opaque PMT descriptor retention is insufficient.",
            vec![],
        ),
        "fate-mpegts-lcevc-vvc-single-track" | "fate-mpegts-lcevc-vvc-dual-track" => (
            "codec-and-enhancement",
            "VVC track mapping/framing and LCEVC enhancement identification/association remain uncovered; unknown stream types retain their original tag, not a guessed codec.",
            vec![],
        ),
        _ => return Err(format!("unclassified MPEG-TS identity: {id}")),
    })
}

fn inventory(root: &Path) -> R<Value> {
    let mut rows: BTreeMap<String, (Vec<Value>, Vec<String>)> = BTreeMap::new();
    let mut indexes = Vec::new();
    for path in INDEXES {
        let raw = read_bounded(&root.join(path), LIMIT)?;
        let index = json::parse(std::str::from_utf8(&raw).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        indexes.push(Value::object([
            ("file", Value::str(path)),
            ("sha256", Value::str(sha256::hex(&raw))),
        ]));
        for row in array(&index, "cases")? {
            if !relevant(row)? {
                continue;
            }
            let id = text(row, "id")?;
            classification(id)?;
            let (observed, samples) = rows.entry(id.to_owned()).or_default();
            observed.push(Value::object([
                ("index", Value::str(path)),
                ("identity", row.clone()),
            ]));
            for sample in strings(row, "sample_references")? {
                if !samples.contains(&sample) {
                    samples.push(sample);
                }
            }
        }
    }
    let cases = rows
        .into_iter()
        .map(|(id, (observations, samples))| {
            let (kind, boundary, related) = classification(&id)?;
            let exact_inputs = if samples.iter().any(|s| s == "sub/dvbsubtest_filter.ts") {
                vec!["subtitle-dvb:fate-dvbsubtest_filter".to_owned()]
            } else {
                vec![]
            };
            Ok(Value::object([
                ("id", Value::str(id)),
                ("kind", Value::str(kind)),
                ("samples", Value::strings(samples)),
                ("exact_equivalent_cases", Value::Array(vec![])),
                ("exact_input_cases", Value::strings(exact_inputs)),
                (
                    "related_controls",
                    Value::strings(related.into_iter().map(str::to_owned)),
                ),
                ("uncovered", Value::str(boundary)),
                ("observations", Value::Array(observations)),
            ]))
        })
        .collect::<R<Vec<_>>>()?;
    Ok(Value::object([
        ("schema", Value::int(1)),
        ("reference_revision", Value::str(REVISION)),
        ("qualification", Value::Bool(false)),
        ("indexes", Value::Array(indexes)),
        ("cases", Value::Array(cases)),
        (
            "missing_program_profiles",
            Value::strings(
                [
                    "original-pmt-version-merge",
                    "original-multiple-program-catalogue-and-selection",
                    "original-latm-discovery",
                    "automatic-single-program-selection-and-multiple-program-refusal",
                    "selected-program-removal-and-pmt-pid-change",
                    "m2ts192-complete-multi-pid-program",
                    "strict-original-pcr-pes-header-and-raw33-clock-facts",
                ]
                .into_iter()
                .map(str::to_owned),
            ),
        ),
        (
            "note",
            Value::str(
                "No exact equivalent upstream operation is established by the authored packet controls. Configured identities are not executed cases. Probe metadata, source facts, compatibility packets, remux output and codec decoding are separate comparisons.",
            ),
        ),
    ]))
}

fn pinned_file(root: &Path, spec: &Value) -> R<std::path::PathBuf> {
    let path = root.join(text(spec, "file")?);
    let bytes = read_bounded(&path, LIMIT)?;
    if bytes.len() > LIMIT || sha256::hex(&bytes) != text(spec, "sha256")? {
        return Err(format!(
            "missing, oversized or changed pin: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn probe_arguments(id: &str) -> R<Vec<String>> {
    let mut args: Vec<String> = ["-v", "error", "-bitexact", "-of", "json"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    match id {
        "fate-mpegts-probe-pmt-merge" => args.extend(
            [
                "-merge_pmt_versions",
                "1",
                "-show_programs",
                "-show_streams",
            ]
            .into_iter()
            .map(str::to_owned),
        ),
        "fate-mpegts-probe-program" => args.push("-show_programs".into()),
        "fate-mpegts-probe-latm" => args.push("-show_streams".into()),
        _ => {
            return Err(
                "observation requires an ordinary PMT merge, program or LATM probe identity".into(),
            );
        }
    }
    Ok(args)
}

fn packet_summary(raw: &[u8], operation: &str) -> R<Value> {
    let value = json::parse_strict(std::str::from_utf8(raw).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if value.get("operation").and_then(Value::as_str) != Some(operation)
        || value.get("eof") != Some(&Value::Bool(true))
        || value.get("refusal").is_some()
        || value.get("iterations") != Some(&Value::int(1))
    {
        return Err("packet operation did not complete one iteration through EOF".into());
    }
    let work = crate::harness::comparison::work_summary(&value, &[])?;
    Ok(Value::object([
        (
            "operation",
            value.get("operation").cloned().ok_or("missing operation")?,
        ),
        ("work", work),
    ]))
}

/// Reference metadata observations plus unmodified existing transport operations.
/// The reference outputs are retained even when the candidate refuses the input.
fn observe(root: &Path, spec: &Value, output: &Path) -> R<()> {
    let id = text(spec, "id")?;
    let mut probe_args = probe_arguments(id)?;
    if text(spec, "reference_revision")? != REVISION {
        return Err("reference revision differs from transport contracts".into());
    }
    let input_spec = spec.get("input").ok_or("missing input pin")?;
    let sample = text(input_spec, "sample")?;
    let coverage = inventory(root)?;
    let row = array(&coverage, "cases")?
        .iter()
        .find(|r| text(r, "id").ok() == Some(id))
        .ok_or("identity absent from indexes")?;
    if !strings(row, "samples")?.iter().any(|s| s == sample) {
        return Err("sample is not the indexed original input".into());
    }
    let contract = read_json(&root.join(OBSERVATION_CONTRACT))?;
    let originals = contract
        .get("input_sha256")
        .ok_or("missing original input pins")?;
    let expected_sha = text(originals, sample)?;
    if text(input_spec, "sha256")? != expected_sha {
        return Err("sample hash differs from the original input pin".into());
    }
    let input_path = root.join(text(input_spec, "file")?);
    if !input_path.exists() {
        let url = format!("https://fate-suite.ffmpeg.org/{sample}");
        let bytes = crate::fetch::pinned(
            &crate::fetch::Network,
            &[crate::fetch::Source::Url(&url)],
            expected_sha,
            LIMIT,
            "triergon-transport-observation",
        )?;
        let expected_path = root.join(".research/fate-samples").join(sample);
        if input_path != expected_path {
            return Err("downloads must use .research/fate-samples/SAMPLE".into());
        }
        fs::create_dir_all(input_path.parent().ok_or("input has no parent")?)
            .map_err(|e| e.to_string())?;
        crate::fetch::publish(&input_path, &bytes)?;
    }
    let input = pinned_file(root, input_spec)?;
    let mut pins = vec![input_spec];
    let build_pin = spec
        .get("reference_build")
        .ok_or("missing reference_build pin")?;
    let build = read_json(&pinned_file(root, build_pin)?)?;
    if text(&build, "revision")? != REVISION {
        return Err("reference build revision changed".into());
    }
    pins.push(build_pin);
    let mut binaries = Vec::new();
    for key in ["ffprobe", "reference_worker", "candidate_worker"] {
        let pin = spec.get(key).ok_or_else(|| format!("missing {key} pin"))?;
        binaries.push(pinned_file(root, pin)?);
        pins.push(pin);
    }
    let program = text(spec, "program")?
        .parse::<u16>()
        .map_err(|e| e.to_string())?;
    if program == 0 {
        return Err("program zero is not a media program".into());
    }
    fs::create_dir_all(output).map_err(|e| e.to_string())?;
    probe_args.push("-i".into());
    probe_args.push(input.display().to_string());
    let mut jobs = vec![("probe".to_owned(), binaries[0].clone(), probe_args)];
    for prefix in ["ts-", "ts-au-"] {
        for (name, binary) in [("reference", &binaries[1]), ("candidate", &binaries[2])] {
            jobs.push((
                format!("{prefix}{name}"),
                binary.clone(),
                vec![
                    format!("{prefix}fold"),
                    input.display().to_string(),
                    "188".into(),
                    program.to_string(),
                ],
            ));
        }
    }
    let mut outcomes = Vec::new();
    let mut summaries = BTreeMap::new();
    let mut failed = false;
    for (name, binary, arguments) in jobs {
        let argv = std::iter::once(binary.into_os_string())
            .chain(arguments.iter().map(Into::into))
            .collect::<Vec<_>>();
        let finished = worker::run_bounded(&argv, Duration::from_secs(120), 1024 * 1024);
        let (code, stdout, stderr, observation_error) = match finished {
            Ok(Some(r)) => (Value::int(r.code), r.stdout, r.stderr, Value::Null),
            Ok(None) => (
                Value::Null,
                vec![],
                vec![],
                Value::str("observation timed out; raw output unavailable"),
            ),
            Err(error) => (Value::Null, vec![], vec![], Value::str(error)),
        };
        failed |= code != Value::int(0);
        if name != "probe" {
            let operation = if name.starts_with("ts-au-") {
                "mpegts-access-unit-packets"
            } else {
                "mpegts-pes-packets"
            };
            let summary = packet_summary(&stdout, operation);
            failed |= summary.is_err();
            summaries.insert(name.clone(), summary);
        } else {
            let metadata = std::str::from_utf8(&stdout)
                .ok()
                .and_then(|raw| json::parse_strict(raw).ok());
            let required = if id == "fate-mpegts-probe-latm" {
                "streams"
            } else {
                "programs"
            };
            failed |= metadata
                .as_ref()
                .and_then(|v| v.get(required))
                .and_then(Value::as_array)
                .is_none();
        }
        fs::write(output.join(format!("{name}.stdout")), &stdout).map_err(|e| e.to_string())?;
        fs::write(output.join(format!("{name}.stderr")), &stderr).map_err(|e| e.to_string())?;
        outcomes.push(Value::object([
            ("operation", Value::str(name)),
            ("arguments", Value::strings(arguments)),
            ("exit_code", code),
            ("observation_error", observation_error),
            ("stdout_sha256", Value::str(sha256::hex(&stdout))),
            ("stderr_sha256", Value::str(sha256::hex(&stderr))),
        ]));
    }
    let comparisons = ["ts-", "ts-au-"]
        .into_iter()
        .map(|prefix| {
            let reference = &summaries[&format!("{prefix}reference")];
            let candidate = &summaries[&format!("{prefix}candidate")];
            let equal = matches!((reference, candidate), (Ok(a), Ok(b)) if a == b);
            failed |= !equal;
            Value::object([
                ("operation", Value::str(prefix)),
                ("packet_parity", Value::Bool(equal)),
            ])
        })
        .collect();
    // Check every input again, including binaries, before publishing the observation.
    for pin in pins {
        pinned_file(root, pin)?;
    }
    write_json(
        &output.join("observations.json"),
        &Value::object([
            ("schema", Value::int(1)),
            ("specification", spec.clone()),
            ("observation_contract", contract),
            ("coverage", coverage),
            (
                "tool_sha256",
                Value::str(sha256::hex(&read_bounded(
                    &root.join("xtask/src/transport_fate.rs"),
                    LIMIT,
                )?)),
            ),
            ("qualification", Value::Bool(false)),
            ("upstream_fate_executed", Value::Bool(false)),
            ("operations", Value::Array(outcomes)),
            ("comparisons", Value::Array(comparisons)),
            (
                "note",
                Value::str(
                    "Reference discovery observations and packet operations only. No probe parity, strict source-clock parity, upstream FATE result or native performance gate is claimed. Nonzero exits and timeouts remain outcomes.",
                ),
            ),
        ]),
    )?;
    if failed {
        return Err("observation recorded failures; see observations.json and raw outputs".into());
    }
    Ok(())
}

pub fn main(root: &Path, args: &[String]) -> R<()> {
    match args {
        [mode] if mode == "check" => {
            if read_json(&root.join(INVENTORY))? != inventory(root)? {
                return Err(
                    "transport coverage differs from tracked indexes; regenerate inventory".into(),
                );
            }
            Ok(())
        }
        [mode, output] if mode == "inventory" => write_json(Path::new(output), &inventory(root)?),
        [mode, spec, output] if mode == "observe" => {
            observe(root, &read_json(Path::new(spec))?, Path::new(output))
        }
        _ => Err(
            "usage: cargo xtask transport-fate check | inventory OUTPUT | observe SPEC OUTPUT_DIR"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn observations_keep_failed_original_packet_operations() {
        use std::os::unix::fs::PermissionsExt;
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = crate::walk::scratch("transport-fate-observe");
        fs::create_dir_all(root.join("harness/fate")).unwrap();
        fs::create_dir_all(root.join("xtask/src")).unwrap();
        for file in INDEXES
            .into_iter()
            .chain([OBSERVATION_CONTRACT, "xtask/src/transport_fate.rs"])
        {
            fs::copy(repository.join(file), root.join(file)).unwrap();
        }
        let pin = |name: &str, bytes: &[u8]| {
            fs::write(root.join(name), bytes).unwrap();
            Value::object([
                ("file", Value::str(name)),
                ("sha256", Value::str(sha256::hex(bytes))),
            ])
        };
        let mut input = pin("original.ts", b"authored host test; not an upstream sample");
        if let Value::Object(fields) = &mut input {
            fields.push(("sample".into(), Value::str("mpegts/pmtchange.ts")));
        }
        let build = pin(
            "build.json",
            format!("{{\"revision\":\"{REVISION}\"}}").as_bytes(),
        );
        let probe = pin("probe", b"#!/bin/sh\nprintf '%s\\n' '{\"programs\":[]}'\n");
        let reference = pin("reference", br#"#!/bin/sh
case "$1" in
ts-fold) operation=mpegts-pes-packets ;;
ts-au-fold) operation=mpegts-access-unit-packets ;;
*) exit 9 ;;
esac
printf '{"operation":"%s","iterations":1,"packets":2,"bytes":8,"digest":19,"eof":true}\n' "$operation"
"#);
        let candidate = pin(
            "candidate",
            b"#!/bin/sh\nprintf '%s\\n' '{\"eof\":false,\"refusal\":\"program context\"}'\n",
        );
        for binary in ["probe", "reference", "candidate"] {
            fs::set_permissions(root.join(binary), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let spec = Value::object([
            ("id", Value::str("fate-mpegts-probe-pmt-merge")),
            ("reference_revision", Value::str(REVISION)),
            ("program", Value::str("1")),
            ("input", input),
            ("reference_build", build),
            ("ffprobe", probe),
            ("reference_worker", reference),
            ("candidate_worker", candidate),
        ]);
        let output = root.join("observations");
        assert_eq!(
            observe(&root, &spec, &output).unwrap_err(),
            "sample hash differs from the original input pin"
        );
        assert!(!output.exists());
        let fixture_pin = spec.get("input").unwrap().get("sha256").unwrap().clone();
        write_json(
            &root.join(OBSERVATION_CONTRACT),
            &Value::object([(
                "input_sha256",
                Value::object([("mpegts/pmtchange.ts", fixture_pin)]),
            )]),
        )
        .unwrap();
        assert!(observe(&root, &spec, &output).is_err());
        let observation = read_json(&output.join("observations.json")).unwrap();
        assert_eq!(array(&observation, "operations").unwrap().len(), 5);
        assert_eq!(observation.get("qualification"), Some(&Value::Bool(false)));
        for comparison in array(&observation, "comparisons").unwrap() {
            assert_eq!(comparison.get("packet_parity"), Some(&Value::Bool(false)));
        }
        assert!(
            fs::read_to_string(output.join("ts-candidate.stdout"))
                .unwrap()
                .contains("program context")
        );
        let mut matching = spec.clone();
        if let Value::Object(fields) = &mut matching {
            fields.retain(|(key, _)| key != "candidate_worker");
            fields.push((
                "candidate_worker".into(),
                spec.get("reference_worker").unwrap().clone(),
            ));
        }
        let success = root.join("matching");
        observe(&root, &matching, &success).unwrap();
        let observation = read_json(&success.join("observations.json")).unwrap();
        for comparison in array(&observation, "comparisons").unwrap() {
            assert_eq!(comparison.get("packet_parity"), Some(&Value::Bool(true)));
        }
        assert_eq!(
            observation.get("upstream_fate_executed"),
            Some(&Value::Bool(false))
        );
        fs::write(root.join("original.ts"), b"changed input").unwrap();
        assert!(observe(&root, &spec, &root.join("changed")).is_err());
        assert!(!root.join("changed").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn packet_comparison_requires_complete_work_and_real_eof() {
        let complete = br#"{"operation":"mpegts-pes-packets","iterations":1,"packets":2,"bytes":8,"digest":19,"eof":true}"#;
        assert!(packet_summary(complete, "mpegts-pes-packets").is_ok());
        for raw in [
            br#"{"iterations":1,"packets":2,"bytes":8,"digest":19,"eof":false}"#.as_slice(),
            br#"{"iterations":1,"packets":2,"bytes":8,"eof":true}"#.as_slice(),
            br#"{"iterations":1,"packets":0,"bytes":0,"digest":0,"eof":true,"refusal":"unsupported"}"#.as_slice(),
        ] { assert!(packet_summary(raw, "mpegts-pes-packets").is_err()); }
    }
    #[test]
    fn tracked_inventory_covers_both_indexes_without_claiming_parity() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let actual = inventory(root).unwrap();
        assert_eq!(actual, read_json(&root.join(INVENTORY)).unwrap());
        assert_eq!(array(&actual, "cases").unwrap().len(), 15);
        for row in array(&actual, "cases").unwrap() {
            assert!(array(row, "exact_equivalent_cases").unwrap().is_empty());
        }
    }
    #[test]
    fn new_identities_require_a_classification() {
        assert!(classification("fate-mpegts-new").is_err());
        assert!(probe_arguments("fate-mpegts-lcevc-vvc-dual-track").is_err());
        assert!(
            probe_arguments("fate-mpegts-probe-pmt-merge")
                .unwrap()
                .windows(2)
                .any(|v| v == ["-merge_pmt_versions", "1"])
        );
    }
    #[test]
    fn pins_reject_changed_and_oversized_inputs() {
        let root = crate::walk::scratch("transport-fate-pin");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("input"), b"original").unwrap();
        let pin = Value::object([
            ("file", Value::str("input")),
            ("sha256", Value::str(sha256::hex(b"original"))),
        ]);
        assert!(pinned_file(&root, &pin).is_ok());
        fs::write(root.join("input"), b"changed").unwrap();
        assert!(pinned_file(&root, &pin).is_err());
        fs::File::create(root.join("input"))
            .unwrap()
            .set_len(LIMIT as u64 + 1)
            .unwrap();
        assert!(pinned_file(&root, &pin).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
