//! Compare the packets of Ogg files with `ffprobe -show_packets`.
//!
//! `cargo run -p triergon-ogg --example ogg_compare -- [--detail] [--ffprobe PATH] FILE...`
//!
//! For each file, each FFmpeg stream's packets are paired in order with the packets of
//! the tracks at that position in every chain link. A row counts packets whose
//! [`compat::clock`](triergon_ogg::compat::clock) timing (pts, dts, duration, skip
//! samples, discard padding) and size equal FFmpeg's, and the packets whose exact
//! timing (pts, duration, trims) differs from FFmpeg's.
//!
//! Besides the FATE Vorbis samples, inputs can be generated with FFmpeg, for example
//! `ffmpeg -f lavfi -i sine=d=1.234:r=48000 -ac 2 -c:a libopus -page_duration 100000 a.opus`,
//! `-strict -2 -c:a vorbis a.ogg`, `-c:a flac -f ogg a.oga`, two inputs mapped into one
//! file for a multiplexed stream, and files concatenated with `cat` for a chained one.
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{PacketInfo, TrackInfo};
use triergon_ogg::{BlockingReader, Limits, compat::clock::packet_timing};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Row {
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    skip: i64,
    padding: i64,
}

fn ffprobe(tool: &str, path: &str) -> Result<BTreeMap<usize, Vec<Row>>, String> {
    let output = Command::new(tool)
        .args([
            "-v",
            "error",
            "-show_entries",
            "packet=stream_index,pts,dts,duration,size:packet_side_data=skip_samples,discard_padding",
            "-of",
            "compact=p=0",
            path,
        ])
        .output()
        .map_err(|e| format!("{tool}: {e}"))?;
    let mut streams: BTreeMap<usize, Vec<Row>> = BTreeMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut row = Row::default();
        let mut stream = 0;
        for field in line.split('|') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            let key = key.rsplit(':').next().unwrap_or(key);
            let number = value.parse::<i64>().ok();
            match key {
                "stream_index" => stream = number.unwrap_or(0) as usize,
                "pts" => row.pts = number,
                "dts" => row.dts = number,
                "duration" => row.duration = number,
                "size" => row.size = number.unwrap_or(0) as usize,
                "skip_samples" => row.skip = number.unwrap_or(0),
                "discard_padding" => row.padding = number.unwrap_or(0),
                _ => {}
            }
        }
        streams.entry(stream).or_default().push(row);
    }
    Ok(streams)
}

struct Ours {
    reference: Row,
    pts: Option<i64>,
    duration: Option<i64>,
    trim: (i64, i64),
}

fn ours(path: &str) -> Result<(BTreeMap<usize, Vec<Ours>>, String), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let mut reader = BlockingReader::open(file, Limits::default()).map_err(|e| e.to_string())?;
    let mut streams: BTreeMap<usize, Vec<Ours>> = BTreeMap::new();
    loop {
        match reader.next_packet(&cancel) {
            Ok(Some(packet)) => {
                let track = &reader.tracks()[packet.track_index()];
                let position = reader
                    .tracks()
                    .iter()
                    .filter(|t| t.link() == track.link())
                    .position(|t| t.serial() == track.serial())
                    .unwrap_or(0);
                let reference = packet_timing(&packet);
                streams.entry(position).or_default().push(Ours {
                    reference: Row {
                        pts: reference.pts,
                        dts: reference.pts,
                        duration: reference.duration,
                        size: packet.payload().len(),
                        skip: i64::from(reference.skip_samples),
                        padding: i64::from(reference.discard_padding),
                    },
                    pts: packet.pts(),
                    duration: packet.duration(),
                    trim: (
                        packet.trim().start.unwrap_or(0),
                        packet.trim().end.unwrap_or(0),
                    ),
                });
            }
            Ok(None) => break,
            Err(error) => return Ok((streams, format!("error: {error}"))),
        }
    }
    let codecs: Vec<String> = reader
        .tracks()
        .iter()
        .map(|t| format!("{:?}", t.codec()))
        .collect();
    Ok((streams, codecs.join("+")))
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let detail = args.iter().any(|a| a == "--detail");
    args.retain(|a| a != "--detail");
    let mut tool = "ffprobe".to_string();
    if let Some(at) = args.iter().position(|a| a == "--ffprobe") {
        tool = args.remove(at + 1);
        args.remove(at);
    }
    println!(
        "| File | Tracks | Packets (ours/FFmpeg) | compat::clock equal | Exact timing differs |"
    );
    println!("|---|---|---|---|---|");
    for path in &args {
        let name = std::path::Path::new(path)
            .file_name()
            .map_or(path.clone(), |n| n.to_string_lossy().into_owned());
        let reference = match ffprobe(&tool, path) {
            Ok(reference) => reference,
            Err(error) => {
                println!("| {name} | | | {error} | |");
                continue;
            }
        };
        let (mine, note) = match ours(path) {
            Ok(result) => result,
            Err(error) => {
                println!("| {name} | {error} | | | |");
                continue;
            }
        };
        let (mut count, mut theirs, mut equal, mut exact) = (0, 0, 0, 0);
        for (stream, rows) in &reference {
            let empty = Vec::new();
            let packets = mine.get(stream).unwrap_or(&empty);
            count += packets.len();
            theirs += rows.len();
            for (index, (row, packet)) in rows.iter().zip(packets).enumerate() {
                if *row == packet.reference {
                    equal += 1;
                } else if detail {
                    println!(
                        "  {name} stream {stream} packet {index}: ffmpeg {row:?} compat {:?}",
                        packet.reference
                    );
                }
                let presented = row.duration.map(|d| d + row.padding);
                let shifted = row.pts;
                if packet.pts != shifted
                    || packet.duration != presented
                    || packet.trim != (row.skip, row.padding)
                {
                    exact += 1;
                    if detail {
                        println!(
                            "  {name} stream {stream} packet {index}: exact pts {:?} duration {:?} trim {:?}",
                            packet.pts, packet.duration, packet.trim
                        );
                    }
                }
            }
        }
        println!("| {name} | {note} | {count}/{theirs} | {equal}/{theirs} | {exact} |");
    }
}
