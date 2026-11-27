//! Authored Matroska view fixtures and their reference pins.

mod metadata;
mod side_data;

use std::{fs, path::Path};

use crate::json::{self, Value};
use crate::sha256;

const DIRECTORY: &str = ".research/matroska-views";

const IDS: &[&str] = &[
    "mkv-side-data",
    "mkv-track-extensions",
    "mkv-track-extensions-oracle",
    "mkv-codec-state",
    "mkv-attachments",
    "mkv-metadata-fields",
    "mkv-metadata-oracle",
];

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    match args {
        [command] if command == "generate" => generate(root).map(|line| println!("{line}")),
        [command, output] if command == "draft" => draft(root, &root.join(output)),
        [command, ..] if command == "fixtures" => {
            generate(root)?;
            crate::framing_views::command(root, args, "matroska-views")
        }
        _ => crate::framing_views::command(root, args, "matroska-views"),
    }
}

fn authored() -> [side_data::Cases; 4] {
    [
        side_data::packet_cases(),
        side_data::extension_cases(),
        side_data::state_cases(),
        metadata::cases(),
    ]
}

pub(crate) fn generate(root: &Path) -> Result<String, String> {
    let directory = root.join(DIRECTORY);
    fs::create_dir_all(&directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut count = 0;
    for (name, bytes) in authored().into_iter().flatten() {
        let path = directory.join(format!("{name}.mkv"));
        if fs::read(&path).ok().as_deref() != Some(&bytes) {
            fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        count += 1;
    }
    Ok(format!("Generated {count} Matroska view fixtures"))
}

fn case(id: &str, file: &str, digest: &str, args: &[&str]) -> Value {
    Value::object([
        ("id", Value::str(id)),
        ("profile", Value::str(id)),
        ("file", Value::str(file)),
        ("sha256", Value::str(digest)),
        ("rights", Value::str("authored-synthetic")),
        ("arguments", Value::strings(args.iter().copied())),
    ])
}

fn draft(root: &Path, output: &Path) -> Result<(), String> {
    let inputs = authored();
    let catalogue =
        json::read(&root.join("harness/features/matroska-segment-metadata/corpus.json"))?;
    let catalogue = catalogue
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing catalogue cases")?;
    for (id, family) in IDS.iter().zip([0, 1, 1, 2, 3, 3, 3]) {
        let path = output.join(id).join("corpus.json");
        if path.exists() {
            return Err(format!(
                "{} already exists; preserve its recorded pins",
                path.display()
            ));
        }
        let source = id.ends_with("-oracle");
        let mut rows = Vec::new();
        for (name, bytes) in &inputs[family] {
            let args: &[&str] = match family {
                0 if name.contains("webm") => &["webm"],
                0 => &["matroska"],
                _ if source => &["source"],
                _ => &[],
            };
            rows.push(case(
                name,
                &format!("{DIRECTORY}/{name}.mkv"),
                &sha256::hex(bytes),
                args,
            ));
        }
        if family == 1 {
            let file = "crates/playback/tests/fixtures/mkv-feed/hevc-dolby.mkv";
            let bytes = fs::read(root.join(file)).map_err(|error| format!("{file}: {error}"))?;
            let args: &[&str] = if source { &["source"] } else { &[] };
            rows.push(case("hevc-dolby", file, &sha256::hex(&bytes), args));
        }
        if family == 3 {
            for old in catalogue {
                let string = |key| {
                    old.get(key)
                        .and_then(Value::as_str)
                        .ok_or("invalid catalogue case")
                };
                let args: &[&str] = if source { &["source"] } else { &[] };
                rows.push(case(
                    string("id")?,
                    &format!(".research/catalogue-fixtures/{}", string("file")?),
                    string("sha256")?,
                    args,
                ));
            }
        }
        json::write(
            &path,
            &Value::object([
                ("schema", Value::int(1)),
                ("directory", Value::str(".")),
                ("cases", Value::Array(rows)),
            ]),
        )?;
    }
    Ok(())
}
