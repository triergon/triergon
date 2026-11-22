use std::path::Path;

use crate::json::{self, Value};

pub(super) fn fixtures(dir: &Path) -> Result<Vec<Value>, String> {
    let mut cases = Vec::new();
    let mut add = |id: &str, profile, data: &[u8]| -> Result<(), String> {
        let file = format!("signature-{id}.case");
        super::write(dir, &file, data)?;
        cases.push(Value::object([
            ("id", Value::str(id)),
            ("profile", Value::str(profile)),
            ("file", Value::str(file)),
            ("sha256", Value::str(crate::sha256::hex(data))),
            ("rights", Value::str("authored-synthetic")),
            ("arguments", Value::Array(Vec::new())),
        ]));
        Ok(())
    };
    for (id, bytes) in [
        ("ebml-over-box", b"\x1a\x45\xdf\xa3ftyp".as_slice()),
        ("ordinary-box", b"\0\0\0\x10moof"),
        ("caller-box", b"\0\0\0\x10wide"),
        ("other-box", b"\0\0\0\x10junk"),
        ("ogg-or-box", b"OggS\0foo"),
        ("ogg-other-version", b"OggS\x01foo"),
    ] {
        add(id, "preamble", bytes)?;
    }

    let mut both = [0; 390];
    for at in [0, 188, 376, 4, 196, 388] {
        both[at] = 0x47;
    }
    add("both-grids", "transport", &both)?;
    for (id, offset) in [
        ("ts-first-mismatch", 0),
        ("ts-second-mismatch", 188),
        ("ts-third-mismatch", 376),
        ("m2ts-first-mismatch", 4),
        ("m2ts-second-mismatch", 196),
        ("m2ts-third-mismatch", 388),
    ] {
        let mut bytes = both;
        bytes[offset] = 0;
        add(id, "transport", &bytes)?;
    }
    both[188] = 0;
    both[196] = 0;
    add("neither-grid", "transport", &both)?;
    Ok(cases)
}

pub(super) fn write_manifest(root: &Path) -> Result<(), String> {
    let directory = ".research/fixtures-transport-views";
    let dir = root.join(directory);
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let rows = Value::object([
        ("schema", Value::int(1)),
        ("directory", Value::str(directory)),
        ("cases", Value::Array(fixtures(&dir)?)),
    ]);
    json::write(
        &root.join("harness/features/source-signatures/corpus.json"),
        &rows,
    )
}
