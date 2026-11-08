//! Compare the packets of each input with `ffprobe -show_packets`.
//!
//! `ffprobe_compare [--ffprobe PATH] [--packets] [--seek] FILE...` prints one line per file:
//! packet counts, the packets whose size, position, timestamps or trim differ, and the
//! first difference. The format follows the extension (`.flac`; `.mp1`, `.mp2`, `.mp3`,
//! `.bit`; `.aac`, `.adts`; `.ac3`, `.eac3`) and FFmpeg is told the same format.
//!
//! With `--seek`, each file is also sought to a third and two thirds of its packets; an
//! exact landing must be the packet that holds the target in the sequential reading.
//!
//! Times are compared in FFmpeg's stream time base. Where that base does not divide a
//! packet's sample count exactly (AC-3 at 44.1 kHz in 1/90000), FFmpeg rounds each
//! duration down and adds them up; the projection here does the same.
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::process::Command;
use triergon_core::Time;
use triergon_core::container::{PacketInfo, SeekPolicy, SeekRequest};
use triergon_elementary::{
    Ac3, Adts, BlockingReader, Flac, Format, Limits, MpegAudio, SeekBasis, TRACK,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Row {
    pts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    pos: Option<u64>,
    skip: Option<i64>,
    discard: Option<i64>,
}

/// Seek to a third and two thirds of `rows` and describe each landing.
fn seeks<F: Format>(path: &str, rows: &[Row], rate: u32) -> String {
    let (Some(first), Some(last)) = (
        rows.first().and_then(|r| r.pts),
        rows.last().and_then(|r| r.pts),
    ) else {
        return String::new();
    };
    let mut out = Vec::new();
    for fraction in [3, 2] {
        let span = last - first;
        let target = first
            + if fraction == 3 {
                span / 3
            } else {
                span * 2 / 3
            };
        let file = File::open(path).expect("input");
        let mut reader =
            BlockingReader::<F, _>::open(BufReader::new(file), Limits::default()).expect("open");
        let request = SeekRequest {
            track: TRACK,
            target: Time::new(i128::from(target), u128::from(rate)).expect("time"),
            policy: SeekPolicy::PreviousRandomAccess,
        };
        let receipt = match reader.seek(request) {
            Ok(receipt) => receipt,
            Err(error) => {
                out.push(format!("seek {target}: {error}"));
                continue;
            }
        };
        let basis = reader.reader().last_seek().map(|d| d.basis);
        let Ok(Some(packet)) = reader.next_packet() else {
            out.push(format!("seek {target}: no packet"));
            continue;
        };
        let holding = rows.iter().find(|r| {
            r.pts
                .zip(r.duration)
                .is_some_and(|(p, d)| p <= target && target < p + d)
        });
        let landed = (packet.pts(), packet.offset());
        let verdict = match (basis, holding) {
            (Some(SeekBasis::Exact), Some(row))
                if (row.pts, row.pos) == (landed.0, Some(landed.1)) =>
            {
                "exact".to_string()
            }
            (Some(SeekBasis::Exact), row) => {
                format!("WRONG, expected {:?}", row.map(|r| (r.pts, r.pos)))
            }
            (basis, row) => format!(
                "{basis:?} off by {} samples",
                landed.0.unwrap_or(0) - row.and_then(|r| r.pts).unwrap_or(0)
            ),
        };
        let _ = receipt;
        out.push(format!("seek {target}: {verdict}"));
    }
    out.join("; ")
}

fn ours<F: Format>(path: &str) -> Result<(Vec<Row>, u32), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut reader = BlockingReader::<F, _>::open(BufReader::new(file), Limits::default())
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    loop {
        match reader.next_packet() {
            Ok(Some(packet)) => {
                let trim = packet.trim();
                rows.push(Row {
                    pts: packet.pts(),
                    duration: packet.duration(),
                    size: packet.payload().len(),
                    pos: Some(packet.offset()),
                    skip: trim.start,
                    discard: trim.end,
                });
            }
            Ok(None) => break,
            Err(error) if rows.is_empty() => return Err(error.to_string()),
            Err(error) => {
                eprintln!("{path}: stopped after {} packets: {error}", rows.len());
                break;
            }
        }
    }
    let rate = reader
        .reader()
        .stream()
        .map_or(0, triergon_elementary::Track::sample_rate);
    Ok((rows, rate))
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split('|').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key || k.ends_with(&format!(":{key}"))).then_some(v)
    })
}

fn ffprobe(tool: &str, format: &str, path: &str) -> Result<(Vec<Row>, (u64, u64)), String> {
    let output = Command::new(tool)
        .args(["-v", "error", "-f", format, "-show_streams", "-show_packets"])
        .args(["-show_entries", "stream=index,time_base:packet=stream_index,pts,duration,size,pos:packet_side_data=skip_samples,discard_padding"])
        .args(["-of", "compact", path])
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    let mut time_base = (0, 0);
    for line in text.lines() {
        if line.starts_with("stream|") {
            if time_base.1 != 0 {
                continue;
            }
            if let Some((n, d)) = field(line, "time_base").and_then(|tb| tb.split_once('/')) {
                time_base = (n.parse().unwrap_or(0), d.parse().unwrap_or(0));
            }
            continue;
        }
        // Attached pictures are packets of another stream.
        if !line.starts_with("packet|") || field(line, "stream_index") != Some("0") {
            continue;
        }
        let number = |key: &str| field(line, key).and_then(|v| v.parse::<i64>().ok());
        rows.push(Row {
            pts: number("pts"),
            duration: number("duration"),
            size: number("size").unwrap_or(0) as usize,
            pos: number("pos").map(|p| p as u64),
            skip: number("skip_samples"),
            discard: number("discard_padding"),
        });
    }
    if rows.is_empty() && time_base.1 == 0 {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(error.lines().last().unwrap_or("no output").to_string());
    }
    Ok((rows, time_base))
}

/// Our sample times in FFmpeg's time base.
fn project(rows: &mut [Row], rate: u32, (num, den): (u64, u64)) {
    if rate == 0 || den == 0 {
        return;
    }
    let scale = |samples: i64| -> (i64, bool) {
        let ticks = i128::from(samples) * i128::from(den);
        let unit = i128::from(rate) * i128::from(num);
        ((ticks / unit) as i64, ticks % unit == 0)
    };
    let exact = rows
        .iter()
        .all(|r| r.duration.is_none_or(|d| scale(d).1) && r.pts.is_none_or(|p| scale(p).1));
    let mut clock = rows.first().and_then(|r| r.pts).map_or(0, |p| scale(p).0);
    for row in rows {
        let duration = row.duration.map(|d| scale(d).0);
        if exact {
            row.pts = row.pts.map(|p| scale(p).0);
        } else {
            row.pts = Some(clock);
            clock += duration.unwrap_or(0);
        }
        row.duration = duration;
    }
}

fn compare(ours: &[Row], theirs: &[Row]) -> (usize, String) {
    let mut differing = 0;
    let mut first = String::new();
    for (index, (a, b)) in ours.iter().zip(theirs).enumerate() {
        let mut fields = Vec::new();
        if a.size != b.size {
            fields.push(format!("size {} vs {}", a.size, b.size));
        }
        if a.pos != b.pos {
            fields.push(format!("pos {:?} vs {:?}", a.pos, b.pos));
        }
        if a.pts != b.pts {
            fields.push(format!("pts {:?} vs {:?}", a.pts, b.pts));
        }
        if a.duration != b.duration {
            fields.push(format!("duration {:?} vs {:?}", a.duration, b.duration));
        }
        let trim = |r: &Row| (r.skip.filter(|&s| s != 0), r.discard.filter(|&d| d != 0));
        if trim(a) != trim(b) {
            fields.push(format!("trim {:?} vs {:?}", trim(a), trim(b)));
        }
        if !fields.is_empty() {
            differing += 1;
            if first.is_empty() {
                first = format!("#{index}: {}", fields.join(", "));
            }
        }
    }
    (differing, first)
}

fn main() {
    let mut tool = "ffprobe".to_string();
    let mut files = Vec::new();
    let mut packets = false;
    let mut seek = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ffprobe" => tool = args.next().expect("--ffprobe needs a path"),
            "--packets" => packets = true,
            "--seek" => seek = true,
            _ => files.push(arg),
        }
    }
    let mut totals: HashMap<&str, (usize, usize)> = HashMap::new();
    println!("| file | ours | ffprobe | differing | first difference |");
    println!("|---|---|---|---|---|");
    for path in &files {
        let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let (format, result) = match extension.as_str() {
            "flac" => ("flac", ours::<Flac>(path)),
            "mp1" | "mp2" | "mp3" | "bit" => ("mp3", ours::<MpegAudio>(path)),
            "aac" | "adts" => ("aac", ours::<Adts>(path)),
            "ac3" => ("ac3", ours::<Ac3>(path)),
            "eac3" | "ec3" => ("eac3", ours::<Ac3>(path)),
            _ => {
                println!("| {path} | | | | unknown extension |");
                continue;
            }
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        let reference = ffprobe(&tool, format, path);
        if packets && let Ok((rows, _)) = &result {
            for row in rows {
                println!("{row:?}");
            }
        }
        let (mut rows, rate) = match result {
            Ok(value) => value,
            Err(error) => {
                let theirs =
                    reference.map_or_else(|e| format!("error: {e}"), |(r, _)| r.len().to_string());
                println!("| {name} | error: {error} | {theirs} | | |");
                continue;
            }
        };
        let (theirs, time_base) = match reference {
            Ok(value) => value,
            Err(error) => {
                println!("| {name} | {} | error: {error} | | |", rows.len());
                continue;
            }
        };
        let sought = if seek {
            let check = match format {
                "flac" => seeks::<Flac>,
                "mp3" => seeks::<MpegAudio>,
                "aac" => seeks::<Adts>,
                _ => seeks::<Ac3>,
            };
            format!(" {}", check(path, &rows, rate))
        } else {
            String::new()
        };
        project(&mut rows, rate, time_base);
        let (differing, first) = compare(&rows, &theirs);
        let count = if rows.len() != theirs.len() {
            format!(
                "; counts differ by {}",
                rows.len() as i64 - theirs.len() as i64
            )
        } else {
            String::new()
        };
        let entry = totals.entry(format).or_default();
        entry.0 += 1;
        entry.1 += usize::from(differing == 0 && rows.len() == theirs.len());
        println!(
            "| {name} | {} | {} | {differing} | {first}{count}{sought} |",
            rows.len(),
            theirs.len()
        );
    }
    for (format, (files, identical)) in totals {
        println!("{format}: {identical}/{files} files identical");
    }
}
