//! Exact rescale inputs for the public AVRational range.

use std::path::Path;

use crate::pyrandom::PyRandom;

use super::blocks::Cases;

pub(super) fn fixtures(root: &Path, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, "time-rescale");
    let mut ticks = Vec::new();
    for tick in [
        0_i64,
        1,
        -1,
        3,
        -3,
        1 << 31,
        -(1 << 31),
        1 << 62,
        -(1 << 62),
        i64::MIN + 1,
        i64::MAX,
    ] {
        ticks.extend(tick.to_le_bytes());
    }
    let (file, pin) = cases.authored("ticks.bin", &ticks, write)?;
    let (boundary, boundary_pin) = cases.authored("minimum.bin", &i64::MIN.to_le_bytes(), write)?;
    let clocks = [
        "1/1000",
        "1/90000",
        "1/48000",
        "1/44100",
        "1/1000000",
        "1/1000000000",
        "1001/30000",
        "1/24000",
        "1/22050",
        "1/32000",
        "1/192000",
    ];
    let mut random = PyRandom::new(44);
    let mut pair = |id: &str, profile: &str, src: &str, dst: &str, file: &str, pin: &str| {
        for path in ["direct", "prepared"] {
            cases.pinned(
                &format!("{id}-{path}"),
                profile,
                file,
                pin,
                "authored-synthetic",
                &[src, dst, path],
            );
        }
    };
    for (i, source) in clocks.iter().enumerate() {
        for (j, destination) in clocks.iter().enumerate() {
            pair(
                &format!("clock-{i:02}-{j:02}"),
                "clocks",
                source,
                destination,
                &file,
                &pin,
            );
        }
    }
    for i in 0..32 {
        let mut next = || {
            format!(
                "{}/{}",
                random.below(2_147_483_647) + 1,
                random.below(2_147_483_647) + 1
            )
        };
        pair(
            &format!("seed-{i:02}"),
            "seed",
            &next(),
            &next(),
            &file,
            &pin,
        );
    }
    pair("half", "edge", "1/2", "1/1", &file, &pin);
    pair("double", "edge", "2/1", "1/1", &file, &pin);
    pair(
        "minimum-identity",
        "edge",
        "1/1",
        "1/1",
        &boundary,
        &boundary_pin,
    );
    pair("minimum-sentinel", "edge", "1/1", "1/1", &file, &pin);
    pair(
        "maximum-ratio",
        "edge",
        "2147483647/1",
        "1/2147483647",
        &file,
        &pin,
    );
    pair(
        "minimum-ratio",
        "edge",
        "1/2147483647",
        "2147483647/1",
        &file,
        &pin,
    );
    cases.finish(write)
}
