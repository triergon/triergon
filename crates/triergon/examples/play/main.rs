#![forbid(unsafe_code)]

//! Decode a Matroska file and render a separate ASS script at a time in milliseconds.

use std::{env, path::Path};

mod consumer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let [media, subtitles, time] = args.as_slice() else {
        return Err("usage: play MEDIA.mkv SUBTITLES.ass TIME_MS".into());
    };
    let time = time.to_str().ok_or("TIME_MS must be an integer")?.parse()?;
    let (played, images) = consumer::play(Path::new(media), Path::new(subtitles), time)?;
    println!(
        "{} video frames ({} RGBA bytes in the last), {} audio sample frames at {} Hz, {images} subtitle images",
        played.pictures, played.rgba, played.samples, played.rate,
    );
    Ok(())
}
