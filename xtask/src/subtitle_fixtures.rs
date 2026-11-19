//! Record reference folds for authored subtitle fixtures and verify their manifests.
use crate::{json, json::Value, sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage: cargo xtask subtitle-fixtures record|verify FEATURE REFERENCE [--target TRIPLE] [--corpus DIR] [--manifest FILE]";
type R<T> = Result<T, String>;

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Text,
    HostText,
    PgsCapsules,
    Dvb,
    Teletext,
}

impl Family {
    fn named(name: &str) -> R<Self> {
        match name {
            "subtitle-text" => Ok(Self::Text),
            "subtitle-text-host" => Ok(Self::HostText),
            "subtitle-pgs-capsules" => Ok(Self::PgsCapsules),
            "subtitle-dvb" => Ok(Self::Dvb),
            "subtitle-teletext" => Ok(Self::Teletext),
            _ => Err(format!("unknown subtitle fixture family {name}")),
        }
    }

    fn repo_inputs(self) -> bool {
        matches!(self, Self::Text | Self::HostText | Self::PgsCapsules)
    }

    fn operation(self) -> &'static str {
        match self {
            Self::Text | Self::HostText => "subtitle-text-fold",
            Self::PgsCapsules => "pgs-fold",
            Self::Dvb | Self::Teletext => "sub-fold",
        }
    }

    fn inventory(self, root: &Path, name: &str, corpus: &Path) -> R<String> {
        let mut python = Command::new("python3");
        python.arg("-B").current_dir(root);
        match self {
            Self::Dvb => {
                python
                    .args(["harness/subtitles/dvb_streams.py", "--list-cases"])
                    .arg(corpus);
            }
            Self::Teletext => {
                python.args(["harness/subtitles/teletext_streams.py", "--list-cases"]);
            }
            _ => {
                python.args(["harness/subtitle_corpus.py", "--list-cases", name]);
            }
        }
        output(python)
    }

    fn fold(self, reference: &str, file: &Path, arguments: &[&str]) -> R<Value> {
        let mut cmd = Command::new(reference);
        cmd.arg(self.operation()).arg(file).args(arguments);
        let stdout = output(cmd)?;
        // The teletext caller reads the last line verbatim. The other recorders
        // strip surrounding whitespace before selecting the last line.
        let lines = if self == Self::Teletext {
            stdout.as_str()
        } else {
            stdout.trim()
        };
        let line = lines
            .lines()
            .next_back()
            .ok_or("reference emitted no fold")?;
        let fold = json::parse_strict(line).map_err(|e| e.to_string())?;
        if self.repo_inputs() && fold.get("eof") != Some(&Value::Bool(true)) {
            return Err("reference did not reach the end".into());
        }
        let mut fields = Vec::new();
        for key in ["packets", "bytes", "digest"] {
            let Some(Value::Number(n)) = fold.get(key) else {
                return Err(format!("missing or nonnumeric fold field {key}"));
            };
            let value = n
                .as_i128()
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| format!("invalid fold field {key}"))?;
            fields.push((key, Value::int(value)));
        }
        Ok(Value::object(fields))
    }
}

fn output(mut command: Command) -> R<String> {
    let child = command.output().map_err(|e| format!("{command:?}: {e}"))?;
    if !child.status.success() {
        return Err(format!(
            "{command:?}: {}: {}",
            child.status,
            String::from_utf8_lossy(&child.stderr).trim()
        ));
    }
    String::from_utf8(child.stdout).map_err(|e| e.to_string())
}

fn string<'a>(row: &'a Value, key: &str) -> R<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string {key}"))
}

fn cases(doc: &Value) -> R<&[Value]> {
    doc.get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing cases array".into())
}

struct Recorder<'a> {
    family: Family,
    reference: &'a str,
    target: Option<&'a str>,
    corpus: PathBuf,
}

impl Recorder<'_> {
    fn row(&self, input: &Value, previous: Option<&Value>) -> R<Value> {
        let file = string(input, "file")?;
        let path = self.corpus.join(
            input
                .get("input_file")
                .and_then(Value::as_str)
                .unwrap_or(file),
        );
        let sha = sha256::file_hex(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let args = input
            .get("arguments")
            .and_then(Value::as_array)
            .ok_or("missing arguments array")?
            .iter()
            .map(|arg| arg.as_str().ok_or("non-string worker argument"))
            .collect::<Result<Vec<_>, _>>()?;
        let expected = self.family.fold(self.reference, &path, &args)?;
        let mut fields = vec![
            ("id", Value::str(string(input, "id")?)),
            ("profile", Value::str(string(input, "profile")?)),
        ];
        if self.family.repo_inputs() {
            fields.push(("group", Value::str(string(input, "group")?)));
        }
        fields.extend([
            ("file", Value::str(file)),
            ("sha256", Value::str(sha)),
            ("rights", Value::str(string(input, "rights")?)),
        ]);
        if !self.family.repo_inputs() {
            fields.push(("source", Value::str(string(input, "source")?)));
        }
        fields.push(("arguments", Value::strings(args)));
        if let Some(target) = self.target {
            let mut pins = BTreeMap::new();
            if let Some(row) = previous {
                let Some(Value::Object(targets)) = row.get("expected_by_target") else {
                    return Err("missing expected_by_target object".into());
                };
                pins.extend(targets.iter().cloned());
            }
            pins.insert(target.to_owned(), expected);
            fields.push(("expected_by_target", Value::object(pins)));
        } else {
            fields.push(("expected", expected));
        }
        Ok(Value::object(fields))
    }

    fn document(&self, inventory: &Value, earlier: Option<&Value>) -> R<Value> {
        let rows = inventory
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or("empty or invalid case inventory")?;
        let mut old = BTreeMap::new();
        if let Some(doc) = earlier {
            for row in cases(doc)? {
                let id = string(row, "id")?;
                if old.insert(id, row).is_some() {
                    return Err(format!("{id}: duplicate previous case"));
                }
            }
        }
        let mut ids = BTreeSet::new();
        let mut recorded = Vec::with_capacity(rows.len());
        for input in rows {
            let id = string(input, "id")?;
            if !ids.insert(id) {
                return Err(format!("{id}: duplicate authored case"));
            }
            let case = self
                .row(input, old.remove(id))
                .map_err(|e| format!("{id}: {e}"))?;
            recorded.push(case);
        }
        if let Some((id, _)) = old.first_key_value() {
            return Err(format!(
                "{id}: formerly recorded case missing from authored inputs"
            ));
        }
        let mut doc = vec![("schema", Value::int(1))];
        if self.family.repo_inputs() {
            doc.push(("directory", Value::str(".")));
        }
        doc.push(("cases", Value::Array(recorded)));
        Ok(Value::object(doc))
    }
}

pub fn main(root: &Path, args: &[String]) -> R<()> {
    if args == ["--help"] {
        println!("{USAGE}");
        return Ok(());
    }
    let [mode, name, reference, options @ ..] = args else {
        return Err(USAGE.into());
    };
    let record = match mode.as_str() {
        "record" => true,
        "verify" => false,
        _ => return Err(USAGE.into()),
    };
    let family = Family::named(name)?;
    let mut recorder = Recorder {
        family,
        reference,
        target: None,
        corpus: if family.repo_inputs() {
            root.to_owned()
        } else {
            root.join(".research/subtitle-corpus")
        },
    };
    let mut manifest = root.join("harness/features").join(name).join("corpus.json");
    let mut stdin_inventory = false;
    let mut iter = options.iter();
    while let Some(key) = iter.next() {
        match key.as_str() {
            "--corpus" => {
                recorder.corpus =
                    std::path::absolute(iter.next().ok_or(USAGE)?).map_err(|e| e.to_string())?;
            }
            "--manifest" => manifest = PathBuf::from(iter.next().ok_or(USAGE)?),
            "--target" => {
                recorder.target = Some(iter.next().filter(|s| !s.is_empty()).ok_or(USAGE)?);
            }
            "--cases-stdin" => stdin_inventory = true,
            _ => return Err(USAGE.into()),
        }
    }
    if (family == Family::HostText) != recorder.target.is_some() {
        return Err("only subtitle-text-host takes a required --target TRIPLE".into());
    }
    let inventory = if stdin_inventory {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        text
    } else {
        family.inventory(root, name, &recorder.corpus)?
    };
    let inputs = json::parse_strict(&inventory).map_err(|e| format!("case inventory: {e}"))?;
    // Hold the temporary file while reading the previous target pins so two
    // recorders cannot both publish an update based on the same old manifest.
    let temp = manifest.with_extension("json.tmp");
    let pending = if record {
        Some(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| format!("{}: {e}", temp.display()))?,
        )
    } else {
        None
    };
    let result = (|| {
        let earlier = match fs::read_to_string(&manifest) {
            Ok(s) => Some(s),
            Err(e) if record && e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", manifest.display())),
        };
        let previous = earlier
            .as_deref()
            .map(json::parse_strict)
            .transpose()
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
        let doc = recorder.document(&inputs, previous.as_ref())?;
        let encoded = json::to_python(&doc, Some(2)) + "\n";
        if let Some(mut output) = pending {
            output
                .write_all(encoded.as_bytes())
                .map_err(|e| e.to_string())?;
            drop(output);
            fs::rename(&temp, &manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
        } else if earlier.as_deref() != Some(&encoded) {
            let old = cases(previous.as_ref().ok_or("missing manifest")?)?;
            for (i, row) in cases(&doc)?.iter().enumerate() {
                if old.get(i) != Some(row) {
                    return Err(format!(
                        "{}: case {} differs at row {}",
                        manifest.display(),
                        string(row, "id")?,
                        i + 1
                    ));
                }
            }
            return Err(format!("{}: manifest bytes differ", manifest.display()));
        }
        println!("{name}: {} cases", cases(&doc)?.len());
        Ok(())
    })();
    if record && result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "triergon-subtitle-fixtures-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            // Each test controls both the child's final stdout line and its exit.
            let script = dir.join("reference");
            fs::write(&script, "#!/bin/sh\ncat \"$2\"\nexit \"$3\"\n").unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
            Self(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const FOLD: &str = r#"{"packets":1,"bytes":0,"digest":18446744073709551615}"#;

    #[test]
    fn preserves_each_reference_protocol_and_refuses_partial_output_on_failure() {
        let f = Fixture::new();
        let file = f.0.join("input");
        let reference = f.0.join("reference");
        let fold = |family: Family, code| family.fold(reference.to_str().unwrap(), &file, &[code]);
        fs::write(&file, format!("diagnostic\n{FOLD}\n")).unwrap();
        for family in [Family::Dvb, Family::Teletext] {
            assert_eq!(
                fold(family, "0").unwrap(),
                json::parse_strict(FOLD).unwrap()
            );
            assert!(fold(family, "11").unwrap_err().contains("11"));
        }
        for family in [Family::Text, Family::HostText, Family::PgsCapsules] {
            assert!(
                fold(family, "0")
                    .unwrap_err()
                    .contains("did not reach the end")
            );
        }
        fs::write(&file, format!("{FOLD}\n\n")).unwrap();
        assert!(fold(Family::Dvb, "0").is_ok());
        assert!(fold(Family::Teletext, "0").is_err());
        for bad in [
            "",
            "truncated{",
            r#"{"bytes":2,"digest":2}"#,
            r#"{"packets":1,"bytes":2,"digest":-1}"#,
        ] {
            fs::write(&file, bad).unwrap();
            assert!(fold(Family::Dvb, "0").is_err());
        }
    }

    #[test]
    fn record_and_verify_keep_target_pins_order_and_previous_file_on_failure() {
        let f = Fixture::new();
        let input = f.0.join("one");
        fs::write(&input, r#"{"eof":true,"packets":0,"bytes":0,"digest":10}"#).unwrap();
        fs::write(
            f.0.join("two"),
            r#"{"eof":true,"packets":1,"bytes":2,"digest":20}"#,
        )
        .unwrap();
        let scripts = f.0.join("harness");
        fs::create_dir(&scripts).unwrap();
        let author = scripts.join("subtitle_corpus.py");
        fs::write(&author, r#"import json
print(json.dumps([dict(id=k, profile=k, group="srt", file=k, rights="authored-synthetic", arguments=["0"]) for k in ["two", "one"]]))"#).unwrap();
        let manifest = f.0.join("manifest.json");
        let run = |mode: &str, target: &str| {
            main(
                &f.0,
                &[
                    mode.to_owned(),
                    "subtitle-text-host".into(),
                    f.0.join("reference").display().to_string(),
                    "--target".into(),
                    target.into(),
                    "--manifest".into(),
                    manifest.display().to_string(),
                ],
            )
        };
        run("record", "z").unwrap();
        fs::write(
            f.0.join("reference"),
            "#!/bin/sh\nsed 's/20}/21}/' \"$2\"\nexit \"$3\"\n",
        )
        .unwrap();
        run("record", "a").unwrap();
        run("verify", "a").unwrap();
        let saved = fs::read_to_string(&manifest).unwrap();
        let doc = json::parse_strict(&saved).unwrap();
        let rows = cases(&doc).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| string(row, "id").unwrap())
                .collect::<Vec<_>>(),
            ["two", "one"]
        );
        let Value::Object(pins) = rows[0].get("expected_by_target").unwrap() else {
            panic!("missing target pins")
        };
        assert_eq!(
            pins.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert_eq!(pins[0].1.get("digest"), Some(&Value::int(21)));
        assert_eq!(pins[1].1.get("digest"), Some(&Value::int(20)));
        assert!(run("verify", "absent").is_err());
        fs::write(&input, r#"{"eof":true,"packets":0,"bytes":0,"digest":11}"#).unwrap();
        assert!(run("verify", "a").is_err());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), saved);
        fs::write(&input, r#"{"eof":false,"packets":0,"bytes":0,"digest":10}"#).unwrap();
        assert!(run("record", "a").is_err());
        assert_eq!(fs::read_to_string(&manifest).unwrap(), saved);
        assert!(!manifest.with_extension("json.tmp").exists());
    }
}
