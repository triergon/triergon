//! Writes the synthetic core streams used by the comparison and table probes.

#![allow(
    clippy::expect_used,
    unreachable_pub,
    reason = "fixture writer shares the integration targets' syntax tables"
)]

#[path = "../tests/support/scenarios.rs"]
mod scenarios;
#[path = "../tests/support/synth.rs"]
mod synth;

#[test]
#[ignore = "writes an explicitly requested stream for the reference scripts"]
fn write_stream() {
    let name = std::env::var("DCA_SYNTH_SCENARIO").expect("scenario");
    let path = std::env::var_os("DCA_SYNTH_OUT").expect("output path");
    let mut meta = String::new();
    let bytes = scenarios::scenarios(&name, &mut meta).expect("known scenario");
    std::fs::write(&path, bytes).expect("write stream");
    if !meta.is_empty() {
        let mut sidecar = path;
        sidecar.push(".meta");
        std::fs::write(sidecar, meta).expect("write sidecar");
    }
}
