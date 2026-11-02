//! Isolated harness adapter. Serialization and process startup are outside timed work.
#![forbid(unsafe_code)]
use safe_media::timing::ExactTime;
use safe_media::{Limits, matroska::Demuxer};
use std::{
    fmt::Write as _,
    io::{self, Cursor, Read},
    sync::atomic::AtomicBool,
    time::Instant,
};

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                write!(out, "\\u{:04x}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}
fn codec(s: &str) -> &str {
    match s {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "A_AAC" => "aac",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_VORBIS" => "vorbis",
        "A_PCM/INT/LIT" => "pcm_s16le",
        "S_TEXT/UTF8" => "subrip",
        _ => s,
    }
}

fn fold(digest: &mut u64, value: u64) {
    *digest = digest.wrapping_mul(31).wrapping_add(value);
}
fn fold_bytes(digest: &mut u64, bytes: &[u8]) {
    for b in bytes {
        fold(digest, u64::from(*b));
    }
}
fn sample_count(ns: u64, rate: f64) -> Result<u64, Box<dyn std::error::Error>> {
    if rate.fract() != 0.0 || !(1.0..=u32::MAX as f64).contains(&rate) {
        return Err("unsupported sample-trim rate".into());
    }
    Ok(u64::try_from(
        (u128::from(ns) * u128::from(rate as u32) + 500_000_000) / 1_000_000_000,
    )?)
}
fn trims(
    track: &safe_media::matroska::Track,
    p: &safe_media::matroska::Packet,
    first: bool,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let mut start = if first && track.kind == 2 {
        sample_count(track.codec_delay_ns, track.sample_rate)?
    } else {
        0
    };
    let mut end = 0;
    if p.discard_padding_ns < 0 {
        start = start
            .checked_add(sample_count(
                p.discard_padding_ns.unsigned_abs(),
                track.sample_rate,
            )?)
            .ok_or("sample trim overflow")?;
    }
    if p.discard_padding_ns > 0 {
        end = sample_count(p.discard_padding_ns as u64, track.sample_rate)?;
    }
    Ok((start, end))
}

struct Short<R> {
    inner: R,
    maximum: usize,
}
impl<R: Read> Read for Short<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = b.len().min(self.maximum);
        self.inner.read(&mut b[..n])
    }
}

fn exact(value: Option<ExactTime>) -> String {
    value.map_or("null".into(), |t| {
        format!(
            "{{\"numerator\":{},\"denominator\":{}}}",
            t.numerator(),
            t.denominator()
        )
    })
}

fn trace_source<R: Read>(mut demux: Demuxer<R>) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = String::from("{\"schema\":1,\"packets\":[");
    let mut first = true;
    while let Some(p) = demux.next_packet(&AtomicBool::new(false))? {
        if !first {
            out.push(',');
        }
        first = false;
        let t = p.timing.source()?;
        write!(
            out,
            "{{\"track\":{},\"pts\":{},\"duration\":{}}}",
            p.track,
            exact(t.pts),
            exact(t.duration)
        )?;
    }
    out.push_str("]}");
    println!("{out}");
    Ok(())
}

fn trace<R: Read>(mut d: Demuxer<R>) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = String::from("{\"schema\":1,\"operation\":\"container-demux-v1\",\"tracks\":[");
    for (n, t) in d.tracks().iter().enumerate() {
        if n != 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"codec\":{},\"kind\":{},\"private\":{},\"width\":{},\"height\":{},\"channels\":{},\"sample_rate\":{},\"language\":{},\"name\":{},\"default\":{},\"forced\":{}}}",
            quote(codec(&t.codec)),
            t.kind,
            quote(&hex(&t.codec_private)),
            t.width,
            t.height,
            if t.kind == 2 { t.channels } else { 0 },
            if t.kind == 2 { t.sample_rate } else { 0.0 },
            quote(&t.language),
            quote(&t.name),
            t.default,
            t.forced
        )?;
    }
    out.push_str("],\"packets\":[");
    let cancel = AtomicBool::new(false);
    let mut first = true;
    let mut seen = vec![false; d.tracks().len()];
    while let Some(p) = d.next_packet(&cancel)? {
        let clock = p.timing.ffmpeg9()?;
        if !first {
            out.push(',');
        }
        first = false;
        let track = &d.tracks()[p.track];
        let (start_trim, end_trim) = trims(track, &p, !seen[p.track])?;
        seen[p.track] = true;
        write!(
            out,
            "{{\"track\":{},\"pts_ns\":{},\"dts_ns\":{},\"duration_ns\":{},\"key\":{},\"trim_start_samples\":{},\"trim_end_samples\":{},\"payload\":{}}}",
            p.track,
            clock
                .pts
                .map(|t| t.rounded_ns())
                .transpose()?
                .map_or("null".into(), |v| v.to_string()),
            p.dts_ns.map_or("null".into(), |v| v.to_string()),
            clock
                .duration
                .map(|t| t.rounded_duration_ns())
                .transpose()?
                .map_or("null".into(), |v| v.to_string()),
            p.keyframe,
            start_trim,
            end_trim,
            quote(&hex(&p.data))
        )?;
        if out.len() > 128 * 1024 * 1024 {
            return Err("trace size limit".into());
        }
    }
    out.push_str("]}");
    println!("{out}");
    Ok(())
}

fn run(data: &[u8], iterations: u32) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    let mut packets = 0;
    let mut bytes = 0;
    let mut digest = 0_u64;
    let cancel = AtomicBool::new(false);
    for _ in 0..iterations {
        let mut d = Demuxer::open_seekable(Cursor::new(data), Limits::default())?;
        for t in d.tracks() {
            fold_bytes(&mut digest, codec(&t.codec).as_bytes());
            fold_bytes(&mut digest, &t.codec_private);
            fold_bytes(&mut digest, t.language.as_bytes());
            fold_bytes(&mut digest, t.name.as_bytes());
            for value in [
                t.kind,
                t.width,
                t.height,
                if t.kind == 2 { t.channels } else { 0 },
                if t.kind == 2 { t.sample_rate as u64 } else { 0 },
                u64::from(t.default),
                u64::from(t.forced),
            ] {
                fold(&mut digest, value);
            }
        }
        let mut seen = vec![false; d.tracks().len()];
        while let Some(p) = d.next_packet(&cancel)? {
            packets += 1;
            bytes += p.data.len() as u64;
            fold_bytes(&mut digest, &p.data);
            let time = p.timing.ffmpeg9()?;
            let (trim_start, trim_end) = trims(&d.tracks()[p.track], &p, !seen[p.track])?;
            seen[p.track] = true;
            for value in [
                p.track as u64,
                time.pts
                    .map(|p| p.rounded_ns())
                    .transpose()?
                    .unwrap_or(i64::MIN) as u64,
                i64::MIN as u64,
                time.duration
                    .map(|d| d.rounded_duration_ns())
                    .transpose()?
                    .unwrap_or(0),
                u64::from(p.keyframe),
                trim_start,
                trim_end,
            ] {
                fold(&mut digest, value);
            }
        }
    }
    Ok((packets, bytes, std::hint::black_box(digest)))
}

fn main() {
    if let Err(e) = entry() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
fn entry() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<_> = std::env::args().collect();
    let mode = a.get(1).ok_or(
        "usage: media-worker trace|trace-short|seek|bench|baseline FILE [iterations/chunk/ns]",
    )?;
    let file = std::fs::File::open(a.get(2).ok_or("missing input")?)?;
    let mut data = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut data)?;
    if data.len() > 64 * 1024 * 1024 {
        return Err("input limit: 64 MiB".into());
    }
    match mode.as_str() {
        "trace" => trace(Demuxer::open_seekable(
            Cursor::new(&data),
            Limits::default(),
        )?)?,
        "trace-source" => trace_source(Demuxer::open_seekable(
            Cursor::new(&data),
            Limits::default(),
        )?)?,
        "trace-source-short" => {
            let n: usize = a.get(3).ok_or("missing chunk")?.parse()?;
            if n == 0 {
                return Err("zero chunk".into());
            }
            trace_source(Demuxer::open(
                Short {
                    inner: Cursor::new(&data),
                    maximum: n,
                },
                Limits::default(),
            )?)?;
        }
        "trace-short" => {
            let n: usize = a.get(3).ok_or("missing chunk")?.parse()?;
            if n == 0 {
                return Err("zero chunk".into());
            }
            trace(Demuxer::open(
                Short {
                    inner: Cursor::new(&data),
                    maximum: n,
                },
                Limits::default(),
            )?)?;
        }
        "baseline" => {
            std::hint::black_box(&data);
            println!("{{\"input_bytes\":{}}}", data.len());
        }
        "bench" => {
            let n: u32 = a.get(3).ok_or("missing iterations")?.parse()?;
            if n == 0 || n > 1_000_000 {
                return Err("invalid iteration count".into());
            }
            let start = Instant::now();
            let (packets, bytes, digest) = run(&data, n)?;
            let ns = start.elapsed().as_nanos();
            println!(
                "{{\"schema\":1,\"iterations\":{n},\"elapsed_ns\":{ns},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest}}}"
            );
        }
        "memory" | "memory-baseline" => {
            use std::io::Write;
            let mut output = std::io::stdout().lock();
            output.write_all(b"SAFE_MEDIA_BEGIN_V1\n")?;
            output.flush()?;
            let result = if mode == "memory" {
                run(&data, 1)?
            } else {
                (0, 0, 0)
            };
            output.write_all(b"SAFE_MEDIA_END_V1\n")?;
            output.flush()?;
            writeln!(
                output,
                "{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
                result.0, result.1, result.2
            )?;
        }
        "seek" => {
            let target: i64 = a.get(3).ok_or("missing seek time")?.parse()?;
            let mut d = Demuxer::open_seekable(Cursor::new(&data), Limits::default())?;
            let cancel = AtomicBool::new(false);
            while d.next_packet(&cancel)?.is_some() {} // Deliberately seek after EOF.
            let track = d
                .tracks()
                .iter()
                .position(|t| t.kind == 1)
                .ok_or("no video")?;
            let receipt = d.seek_previous(track, target, &cancel)?;
            let p = d.next_packet(&cancel)?.ok_or("EOF after seek")?;
            println!(
                "{{\"actual_ns\":{},\"track\":{},\"pts_ns\":{},\"key\":{},\"payload\":{}}}",
                receipt.actual_ns,
                p.track,
                p.pts_ns.map_or("null".into(), |v| v.to_string()),
                p.keyframe,
                quote(&hex(&p.data))
            );
        }
        _ => return Err("unknown operation".into()),
    }
    Ok(())
}
