#![cfg(feature = "matroska")]

use std::path::Path;

#[path = "../examples/play/pipeline.rs"]
mod pipeline;

#[test]
fn a_matroska_file_decodes_and_an_ass_script_draws() -> pipeline::ExampleResult<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let media = root.join("crates/playback/tests/fixtures/mkv-feed/three-tracks.mkv");
    let script = root.join("harness/fixtures/ass-render/decorations-orientation.ass");
    for path in [&media, &script] {
        if !path.try_exists()? {
            println!("skipped: missing {}", path.display());
            return Ok(());
        }
    }
    let played = pipeline::play(&media, &script, 500)?;
    assert_eq!(played.video_frames, 8);
    assert_eq!(played.rgba_bytes, 640 * 360 * 4);
    assert_eq!(played.sample_rate, 48_000);
    assert!(played.audio_samples >= 48_000);
    assert!(played.subtitle_images > 0);
    assert_eq!(pipeline::play(&media, &script, 1000)?.subtitle_images, 0);
    Ok(())
}
