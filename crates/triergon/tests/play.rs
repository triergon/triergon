#![cfg(feature = "matroska")]

use std::path::Path;

#[path = "../examples/play/consumer.rs"]
mod consumer;

#[test]
fn a_matroska_file_decodes_and_an_ass_script_draws() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let media = root.join("crates/playback/tests/fixtures/mkv-feed/three-tracks.mkv");
    let script = root.join("harness/fixtures/ass-render/decorations-orientation.ass");
    let (played, images) = consumer::play(&media, &script, 500)?;
    assert_eq!(played.pictures, 8);
    assert_eq!(played.rgba, 640 * 360 * 4);
    assert_eq!(played.rate, 48_000);
    assert!(played.samples >= 48_000);
    assert!(images > 0);
    assert_eq!(consumer::play(&media, &script, 1000)?.1, 0);
    Ok(())
}
