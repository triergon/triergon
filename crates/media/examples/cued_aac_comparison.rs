//! Candidate for the separately named cued AAC seek comparison.
#[path = "../tests/support/ebml.rs"]
mod ebml;

use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_matroska::{Limits, typed::BlockingReader};

fn main() {
    let path = std::env::args().nth(1).expect("fixture path");
    let (bytes, _, _) = ebml::cued_aac();
    std::fs::write(path, &bytes).unwrap();
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    let receipt = reader.seek_cued_aac(0, 1_010_000_000, &cancel).unwrap();
    println!("landing_ns={}", receipt.actual_ns);
    while let Some(packet) = reader.next_packet(&cancel).unwrap() {
        let hex: String = packet
            .data
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        println!(
            "packet track={} pts_ns={:?} dts_ns={:?} duration_ns={:?} bytes={hex}",
            packet.track, packet.pts_ns, packet.dts_ns, packet.duration_ns
        );
    }
}
