//! Opaque VVC source-PES controls; no access-unit parser is involved.

use super::{
    Case, Codec, ORIGINAL, R, Transport, cases, expected_for, observation_hash, original_inputs,
    part, private_directory, put, save_reference_result, text,
};
use crate::checks::read_json;
use crate::corpus::{authored, write_json};
use crate::harness::worker;
use crate::json::Value;
use crate::sha256;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) const OPERATION: &str = "mpegts-pes-packets";
const IDS: [&str; 2] = ["vvc-finite", "vvc-unbounded"];

fn controls() -> R<Vec<Case>> {
    let codec = Codec {
        name: "vvc",
        stream_type: 0x33,
        stream_id: 0xe0,
        dvb: false,
        bytes: Vec::new(),
        first: 0,
        header: 0,
    };
    IDS.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let mut ts = Transport::new();
            ts.tables(&codec, &[])?;
            for n in 0..3u8 {
                let body = [0, 0, 1, 0, n, 0x0f, 0xff, 0xf8];
                let mut p = part(
                    &body,
                    Some(3003 * u64::from(n + 1)),
                    Some(3003 * u64::from(n)),
                );
                p.unbounded = index == 1;
                ts.pes(0xe0, &p)?;
            }
            let file = format!("harness/fixtures/mpegts/{id}.ts");
            Ok(Case {
                row: authored(
                    id,
                    &file,
                    &ts.bytes,
                    vec![("arguments", Value::strings(["188", "7"]))],
                ),
                bytes: Some(ts.bytes),
                following: Vec::new(),
            })
        })
        .collect()
}

fn recording(root: &Path, args: &[String]) -> R<Option<(PathBuf, String, PathBuf)>> {
    if args.len() == 6 {
        if args[2] != "--record" || args[4] != "--observations" {
            return Err("expected --record REFERENCE_ROOT --observations DIR".into());
        }
        let reference_root = Path::new(&args[3]);
        let identity = read_json(&reference_root.join("target/harness/identity.json"))?;
        let registered =
            crate::harness::comparison::reference_named(&identity, "ffmpeg-formats-native")?;
        let build = registered
            .get("reference_build")
            .ok_or("missing reference build")?;
        if text(build, "revision")? != "d32b387f2b0a484599d4587d651891f0c63c4238" {
            return Err("VVC PES recording requires the pinned d32b387f formats build".into());
        }
        let worker = reference_root.join(text(&registered, "path")?);
        let hash = text(&registered, "sha256")?.to_owned();
        if sha256::file_hex(&worker).map_err(|e| e.to_string())? != hash {
            return Err("reference worker differs from its formats identity".into());
        }
        let directory = private_directory(root, Path::new(&args[5]))?;
        Ok(Some((worker, hash, directory)))
    } else {
        Ok(None)
    }
}

pub fn main(root: &Path, args: &[String]) -> R<()> {
    let usage = "transport-vvc --output ROOT [--record REFERENCE_ROOT --observations DIR]";
    if !matches!(args.len(), 2 | 6) || args[0] != "--output" {
        return Err(usage.into());
    }
    let output = Path::new(&args[1]);
    let recording = recording(root, args)?;
    let mut manifest = read_json(&root.join(ORIGINAL))?;
    // Validate every original input and request association before appending.
    original_inputs(manifest.clone())?;
    let mut rows = cases(&manifest)?.to_vec();
    if rows.len() != 97
        && (rows.len() != 99
            || IDS
                .iter()
                .zip(&rows[97..])
                .any(|(id, r)| text(r, "id").ok() != Some(*id)))
    {
        return Err("unexpected PES corpus membership".into());
    }
    for (index, mut case) in controls()?.into_iter().enumerate() {
        let id = text(&case.row, "id")?.to_owned();
        let file = text(&case.row, "file")?;
        let input = output.join(file);
        fs::create_dir_all(input.parent().ok_or("missing input parent")?)
            .map_err(|e| e.to_string())?;
        fs::write(&input, case.bytes.take().ok_or("missing VVC input")?)
            .map_err(|e| e.to_string())?;
        if let Some((worker_path, hash, directory)) = &recording {
            let argv = vec![
                worker_path.as_os_str().to_owned(),
                "ts-fold".into(),
                input.canonicalize().map_err(|e| e.to_string())?.into(),
                "188".into(),
                "7".into(),
            ];
            let done = worker::run_in(&argv, &[], Some(root), worker::DEFAULT_TIMEOUT)?
                .ok_or("reference timeout")?;
            let observation = save_reference_result(&id, directory, &argv, &done)?;
            let expected = expected_for(&observation, OPERATION);
            if expected == Value::Null {
                return Err(format!("{id}: reference did not complete at EOF"));
            }
            put(&mut case.row, "expected", expected)?;
            put(&mut case.row, "reference_worker_sha256", Value::str(hash))?;
            put(
                &mut case.row,
                "reference_result_sha256",
                Value::str(observation_hash(&observation)),
            )?;
        } else {
            let pin = rows
                .get(97 + index)
                .ok_or("record VVC reference pins first")?;
            for key in ["id", "profile", "file", "sha256", "rights", "arguments"] {
                if case.row.get(key) != pin.get(key) {
                    return Err(format!("{id}: authored input differs from pin: {key}"));
                }
            }
            case.row = pin.clone();
        }
        if rows.len() > 97 + index {
            rows[97 + index] = case.row;
        } else {
            rows.push(case.row);
        }
    }
    put(&mut manifest, "cases", Value::Array(rows))?;
    write_json(&output.join(ORIGINAL), &manifest)?;
    let path = "harness/features/mpegts-pes/contract.json";
    let mut contract = read_json(&root.join(path))?;
    let mut profiles = contract
        .get("required_profiles")
        .and_then(Value::as_array)
        .ok_or("missing profiles")?
        .to_vec();
    for id in IDS {
        if !profiles.contains(&Value::str(id)) {
            profiles.push(Value::str(id));
        }
    }
    if let Value::Object(fields) = &mut contract {
        let (_, value) = fields
            .iter_mut()
            .find(|(key, _)| key == "required_profiles")
            .ok_or("missing profiles")?;
        *value = Value::Array(profiles);
    }
    write_json(&output.join(path), &contract)?;
    println!("99 required PES cases: 97 retained and 2 VVC controls");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regeneration_preserves_all_inputs_pins_and_required_profiles() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let output = crate::walk::scratch("vvc-pes-regeneration");
        main(
            root,
            &["--output".into(), output.to_string_lossy().into_owned()],
        )
        .unwrap();
        for case in controls().unwrap() {
            let file = text(&case.row, "file").unwrap();
            assert_eq!(
                fs::read(root.join(file)).unwrap(),
                fs::read(output.join(file)).unwrap()
            );
        }
        for file in [ORIGINAL, "harness/features/mpegts-pes/contract.json"] {
            assert_eq!(
                fs::read(root.join(file)).unwrap(),
                fs::read(output.join(file)).unwrap()
            );
        }
        fs::remove_dir_all(output).unwrap();
    }
}
