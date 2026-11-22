//! Author short probe controls from the checked-in transport and QuickTime fixtures.

use std::fs;
use std::path::Path;

const INPUTS: &str = "harness/features/playback-probe/inputs";
const TS: &str = "crates/playback/tests/fixtures/ts/ts188-basic.ts";
const MOV: &str = "crates/playback/tests/fixtures/mp4/r-front-stco.mov";

pub(super) fn author(root: &Path) -> Result<(), String> {
    let read = |path| fs::read(root.join(path)).map_err(|error| format!("{path}: {error}"));
    let ts = read(TS)?;
    let mov = read(MOV)?;
    let out = root.join(INPUTS);
    fs::create_dir_all(&out).map_err(|error| format!("{}: {error}", out.display()))?;
    let write = |name: &str, bytes: &[u8]| {
        let path = out.join(name);
        fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))
    };
    if ts.len() < 376 || mov.len() < 8 {
        return Err("probe source fixture is too short".into());
    }
    write("ts188-two-carriers.ts", &ts[..376])?;
    let mut ambiguous = vec![0; 3 * 192];
    for index in [0, 4, 188, 196, 376, 388] {
        ambiguous[index] = 0x47;
    }
    write("ts188-m2ts192-tie.bin", &ambiguous)?;
    for length in [1, 7, 32, 128, 188, 192, 376, 389, 1024, 4096] {
        let mut junk = vec![0x65; length];
        junk.extend_from_slice(&ts);
        write(&format!("ts188-junk-{length}.ts"), &junk)?;
    }
    for (kind, name) in [(b"wide", "wide-first.mov"), (b"moof", "moof-first.mp4")] {
        // Replace the type of the existing first box. Keep every later chunk offset.
        let mut changed = mov.clone();
        changed[4..8].copy_from_slice(kind);
        write(name, &changed)?;
    }
    Ok(())
}
