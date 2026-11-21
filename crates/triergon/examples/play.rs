//! Decode a Matroska file and render a separate ASS script at a time in milliseconds.

use std::{env, path::Path};

#[path = "play/pipeline.rs"]
mod pipeline;

fn main() -> pipeline::ExampleResult<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let [media, subtitles, time] = args.as_slice() else {
        return Err("usage: play MEDIA.mkv SUBTITLES.ass TIME_MS".into());
    };
    let time = time.to_str().ok_or("TIME_MS must be an integer")?.parse()?;
    let played = pipeline::play(Path::new(media), Path::new(subtitles), time)?;
    println!(
        "{} video frames ({} RGBA bytes in the last), {} audio sample frames at {} Hz, {} subtitle images",
        played.video_frames,
        played.rgba_bytes,
        played.audio_samples,
        played.sample_rate,
        played.subtitle_images,
    );
    Ok(())
}
