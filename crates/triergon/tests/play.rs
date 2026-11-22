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

#[test]
fn mp4_playback_loads_the_video_and_audio_sample_descriptions() -> pipeline::ExampleResult<()> {
    use triergon::playback::Format;
    use triergon::session::{Canvas, PcmBlock, Published};
    use triergon::{Budget, Bytes, Error, Result};

    #[derive(Default)]
    struct Output {
        pictures: usize,
        audio_samples: usize,
    }

    impl pipeline::Observer for Output {
        fn picture(&mut self, _: &Published) -> Result<()> {
            self.pictures += 1;
            Ok(())
        }

        fn pcm(&mut self, block: &PcmBlock) -> Result<()> {
            self.audio_samples += block.samples.len() / 2;
            Ok(())
        }
    }

    let pool = Budget::new(128 << 20)?;
    let input = include_bytes!("../../playback/tests/fixtures/mp4/encoded-hevc-aac-front.mp4");
    let mut output = Output::default();
    pipeline::play_with(
        &pool,
        Bytes::copy_from_slice(&pool, input)?,
        pipeline::Options {
            canvas: Canvas::new(640, 360).ok_or(Error::Invalid("example canvas"))?,
            format: Some(Format::Mp4),
            video: Some(0),
            audio: Some(1),
            transport: None,
        },
        &mut output,
    )?;
    assert_eq!(output.pictures, 2);
    assert_eq!(output.audio_samples, 2048);
    Ok(())
}
