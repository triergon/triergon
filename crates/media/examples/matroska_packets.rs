//! Print the tracks and then the packets of a Matroska or WebM file.
//!
//! ```text
//! cargo run -p triergon-matroska --example matroska_packets -- FILE
//! ```
//!
//! Timestamps are nanoseconds with CodecDelay applied. Matroska carries no
//! decode timestamps, so `dts` is always `-`.

use std::{env, fs::File, io::BufReader, sync::atomic::AtomicBool};
use triergon_matroska::{Limits, typed::BlockingReader};

fn optional<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: matroska_packets FILE")?;
    let file = BufReader::new(File::open(path)?);
    let mut reader = BlockingReader::open_seekable(file, Limits::default())?;
    for track in reader.tracks() {
        let kind = match track.kind {
            1 => "video",
            2 => "audio",
            17 => "subtitle",
            _ => "other",
        };
        println!(
            "track {}: {kind} {} language {}",
            track.number,
            &*track.codec,
            track.effective_language()
        );
    }
    let cancel = AtomicBool::new(false);
    while let Some(packet) = reader.next_typed_packet(&cancel)? {
        let packet = packet.packet;
        println!(
            "track {} dts {} pts {} size {} key {}",
            reader.tracks()[packet.track].number,
            optional(packet.dts_ns),
            optional(packet.pts_ns),
            packet.data.len(),
            packet.keyframe
        );
    }
    Ok(())
}
