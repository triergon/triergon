//! Isolated harness adapter. Serialization and process startup are outside timed work.
#![forbid(unsafe_code)]
mod catalogue;
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
        "S_TEXT/ASS" => "ass",
        "S_VOBSUB" => "dvd_subtitle",
        _ => s,
    }
}

fn fold(digest: &mut u64, value: u64) {
    *digest = digest.wrapping_mul(31).wrapping_add(value);
}
fn fold_bytes(digest: &mut u64, bytes: &[u8]) {
    // Exact expansion of eight ordered h <- 31*h + byte steps modulo 2^64.
    // The native reference uses the identical polynomial and byte order.
    let tail = |b: &[u8]| {
        u64::from(b[0])
            .wrapping_mul(27_512_614_111)
            .wrapping_add(u64::from(b[1]).wrapping_mul(887_503_681))
            .wrapping_add(u64::from(b[2]).wrapping_mul(28_629_151))
            .wrapping_add(u64::from(b[3]).wrapping_mul(923_521))
            .wrapping_add(u64::from(b[4]).wrapping_mul(29_791))
            .wrapping_add(u64::from(b[5]).wrapping_mul(961))
            .wrapping_add(u64::from(b[6]).wrapping_mul(31))
            .wrapping_add(u64::from(b[7]))
    };
    let bytes = if bytes.len() >= 256 {
        // Four independent recurrences expose instruction-level parallelism.
        // Recombine their exact polynomial positions, including the incoming
        // digest. Both timing workers use this algorithm; no byte is sampled.
        const STEP: u64 = 3_671_467_063_254_694_913; // 31^32 modulo 2^64
        let mut groups = bytes.chunks_exact(32);
        let mut exponent = groups.len();
        let mut power = 1_u64;
        let mut base = STEP;
        while exponent != 0 {
            if exponent & 1 != 0 {
                power = power.wrapping_mul(base);
            }
            base = base.wrapping_mul(base);
            exponent >>= 1;
        }
        let mut lanes = [0_u64; 4];
        for block in &mut groups {
            for (lane, b) in lanes.iter_mut().zip(block.chunks_exact(8)) {
                *lane = lane.wrapping_mul(STEP).wrapping_add(tail(b));
            }
        }
        *digest = digest
            .wrapping_mul(power)
            .wrapping_add(lanes[0].wrapping_mul(13_349_005_091_172_011_265))
            .wrapping_add(lanes[1].wrapping_mul(12_662_688_596_514_954_753))
            .wrapping_add(lanes[2].wrapping_mul(852_891_037_441))
            .wrapping_add(lanes[3]);
        groups.remainder()
    } else {
        bytes
    };
    let mut groups = bytes.chunks_exact(8);
    for b in &mut groups {
        *digest = digest.wrapping_mul(852_891_037_441).wrapping_add(tail(b));
    }
    for b in groups.remainder() {
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

// Optional decoder-input comparison view. The source-frame API and default trace
// retain every event. Empty PCM without packet trim side data contains no samples;
// FFmpeg delivers no AVPacket for it. Other codecs and side-only events stay visible.
fn decoder_packet(
    track: &safe_media::matroska::Track,
    packet: &safe_media::matroska::Packet,
) -> bool {
    !(track.kind == 2
        && track.codec == "A_PCM/INT/LIT"
        && packet.data.is_empty()
        && packet.discard_padding_ns == 0)
}

fn trace_colour_source<R: Read>(d: Demuxer<R>) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = String::from("{\"schema\":1,\"tracks\":[");
    for (i, t) in d.tracks().iter().enumerate() {
        if i != 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"raw_track\":{},\"colour\":",
            quote(&hex(&t.source_metadata))
        )?;
        if let Some(c) = t.colour {
            let integers = [
                Some(c.matrix_coefficients),
                Some(c.bits_per_channel),
                c.chroma_subsampling[0],
                c.chroma_subsampling[1],
                c.cb_subsampling[0],
                c.cb_subsampling[1],
                Some(c.chroma_siting[0]),
                Some(c.chroma_siting[1]),
                Some(c.range),
                Some(c.transfer_characteristics),
                Some(c.primaries),
                c.max_cll,
                c.max_fall,
            ];
            out.push_str("{\"integers\":[");
            for (i, v) in integers.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                out.push_str(&v.map_or("null".into(), |n| n.to_string()));
            }
            out.push_str("],\"mastering\":");
            if let Some(m) = c.mastering {
                out.push('[');
                for (i, v) in m
                    .chromaticity
                    .into_iter()
                    .chain([m.luminance_max, m.luminance_min])
                    .enumerate()
                {
                    if i != 0 {
                        out.push(',');
                    }
                    out.push_str(
                        &v.map_or("null".into(), |v| quote(&format!("{:016x}", v.to_bits()))),
                    );
                }
                out.push(']');
            } else {
                out.push_str("null");
            }
            out.push('}');
        } else {
            out.push_str("null");
        }
        out.push('}');
    }
    out.push_str("]}");
    println!("{out}");
    Ok(())
}

fn trace<R: Read>(
    mut d: Demuxer<R>,
    initial_trim: bool,
    codec_packets: bool,
    metadata: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = format!(
        "{{\"schema\":{},\"operation\":{},\"tracks\":[",
        if metadata == 2 { 2 } else { 1 },
        quote(if metadata == 2 {
            "container-catalogue-demux-v1"
        } else {
            "container-demux-v1"
        })
    );
    for (n, t) in d.tracks().iter().enumerate() {
        if n != 0 {
            out.push(',');
        }
        write!(
            out,
            "{{\"codec\":{},\"kind\":{},\"private\":{},\"width\":{},\"height\":{},\"channels\":{},\"sample_rate\":{},\"default\":{},\"forced\":{}",
            quote(codec(&t.codec)),
            t.kind,
            quote(&hex(&t.codec_private)),
            t.width,
            t.height,
            if t.kind == 2 { t.channels } else { 0 },
            if t.kind == 2 { t.sample_rate } else { 0. },
            t.default,
            t.forced
        )?;
        if metadata != 2 {
            write!(
                out,
                ",\"language\":{},\"name\":{}",
                quote(&t.language),
                quote(&t.name)
            )?;
        }
        if metadata != 0 {
            write!(
                out,
                ",\"colour\":{:?}",
                t.colour.unwrap_or_default().ffmpeg9()?
            )?;
        }
        out.push('}');
    }
    out.push_str("],\"packets\":[");
    let cancel = AtomicBool::new(false);
    let mut first = true;
    let mut seen = vec![false; d.tracks().len()];
    while let Some(p) = d.next_packet(&cancel)? {
        if codec_packets && !decoder_packet(&d.tracks()[p.track], &p) {
            continue;
        }
        let clock = p.timing.ffmpeg9()?;
        if !first {
            out.push(',');
        }
        first = false;
        let track = &d.tracks()[p.track];
        let (start_trim, end_trim) = trims(track, &p, initial_trim && !seen[p.track])?;
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
    out.push(']');
    if metadata == 2 {
        catalogue::CatalogueView::new(&d)?.output(Some(&mut out), &mut 0)?;
    }
    out.push('}');
    println!("{out}");
    Ok(())
}

fn run(
    data: &[u8],
    iterations: u32,
    seek: Option<i64>,
    codec_packets: bool,
    metadata: u8,
) -> Result<(u64, u64, u64), Box<dyn std::error::Error>> {
    let mut packets = 0;
    let mut bytes = 0;
    let mut digest = 0_u64;
    let cancel = AtomicBool::new(false);
    for _ in 0..iterations {
        let mut d = Demuxer::open_seekable(Cursor::new(data), Limits::default())?;
        let initial_trim = if let Some(target) = seek {
            d.seek_for_playback(0, target, &cancel)?.replays_from_start
        } else {
            true
        };
        for t in d.tracks() {
            fold_bytes(&mut digest, codec(&t.codec).as_bytes());
            fold_bytes(&mut digest, &t.codec_private);
            if metadata != 2 {
                fold_bytes(&mut digest, t.language.as_bytes());
                fold_bytes(&mut digest, t.name.as_bytes());
            }
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
            if metadata != 0 {
                for value in t.colour.unwrap_or_default().ffmpeg9()? {
                    fold(&mut digest, value as u64);
                }
            }
        }
        let mut seen = vec![false; d.tracks().len()];
        while let Some(p) = d.next_packet(&cancel)? {
            if codec_packets && !decoder_packet(&d.tracks()[p.track], &p) {
                continue;
            }
            packets += 1;
            bytes += p.data.len() as u64;
            fold_bytes(&mut digest, &p.data);
            let time = p.timing.ffmpeg9()?;
            let (trim_start, trim_end) =
                trims(&d.tracks()[p.track], &p, initial_trim && !seen[p.track])?;
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
        if metadata == 2 {
            catalogue::CatalogueView::new(&d)?.output(None, &mut digest)?;
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
        "trace" | "codec-trace" | "metadata-trace" | "catalogue-trace" => trace(
            Demuxer::open_seekable(Cursor::new(&data), Limits::default())?,
            true,
            mode == "codec-trace",
            if mode == "catalogue-trace" {
                2
            } else {
                u8::from(mode == "metadata-trace")
            },
        )?,
        "catalogue-source" => catalogue::source(Demuxer::open_seekable(
            Cursor::new(&data),
            Limits::default(),
        )?)?,
        "metadata-source" => trace_colour_source(Demuxer::open_seekable(
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
        "trace-short" | "codec-trace-short" | "metadata-trace-short" | "catalogue-trace-short" => {
            let n: usize = a.get(3).ok_or("missing chunk")?.parse()?;
            if n == 0 {
                return Err("zero chunk".into());
            }
            trace(
                Demuxer::open(
                    Short {
                        inner: Cursor::new(&data),
                        maximum: n,
                    },
                    Limits::default(),
                )?,
                true,
                mode == "codec-trace-short",
                if mode == "catalogue-trace-short" {
                    2
                } else {
                    u8::from(mode == "metadata-trace-short")
                },
            )?;
        }
        "baseline" => {
            std::hint::black_box(&data);
            println!("{{\"input_bytes\":{}}}", data.len());
        }
        "bench" | "seek-bench" | "codec-bench" | "metadata-bench" | "catalogue-bench" => {
            let n: u32 = a.get(3).ok_or("missing iterations")?.parse()?;
            if n == 0 || n > 1_000_000 {
                return Err("invalid iteration count".into());
            }
            let start = Instant::now();
            let seek = if mode == "seek-bench" {
                Some(a.get(4).ok_or("missing seek target")?.parse()?)
            } else {
                None
            };
            let (packets, bytes, digest) = run(
                &data,
                n,
                seek,
                mode == "codec-bench",
                if mode == "catalogue-bench" {
                    2
                } else {
                    u8::from(mode == "metadata-bench")
                },
            )?;
            let ns = start.elapsed().as_nanos();
            println!(
                "{{\"schema\":1,\"iterations\":{n},\"elapsed_ns\":{ns},\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest}}}"
            );
        }
        "memory" | "memory-baseline" | "seek-memory" | "codec-memory" | "metadata-memory"
        | "catalogue-memory" => {
            use std::io::Write;
            let mut output = std::io::stdout().lock();
            output.write_all(b"SAFE_MEDIA_BEGIN_V1\n")?;
            output.flush()?;
            let result = if mode == "memory"
                || mode == "seek-memory"
                || mode == "codec-memory"
                || mode == "metadata-memory"
                || mode == "catalogue-memory"
            {
                let seek = if mode == "seek-memory" {
                    Some(a.get(3).ok_or("missing seek target")?.parse()?)
                } else {
                    None
                };
                run(
                    &data,
                    1,
                    seek,
                    mode == "codec-memory",
                    if mode == "catalogue-memory" {
                        2
                    } else {
                        u8::from(mode == "metadata-memory")
                    },
                )?
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
        "seek-trace" => {
            let target = a.get(3).ok_or("missing seek target")?.parse()?;
            let mut demux = Demuxer::open_seekable(Cursor::new(&data), Limits::default())?;
            if a.get(4).is_some_and(|value| value == "after-eof") {
                while demux.next_packet(&AtomicBool::new(false))?.is_some() {}
            }
            let receipt = demux.seek_for_playback(0, target, &AtomicBool::new(false))?;
            trace(demux, receipt.replays_from_start, false, 0)?;
        }
        "inflate" => {
            let limits = safe_media::zlib::InflateLimits {
                output_bytes: a.get(3).map_or(Ok(16 * 1024 * 1024), |s| s.parse())?,
                expansion_ratio: a.get(4).map_or(Ok(1024), |s| s.parse())?,
                work: a.get(5).map_or(Ok(128 * 1024 * 1024), |s| s.parse())?,
            };
            let bytes = safe_media::zlib::decompress(&data, limits, &AtomicBool::new(false))?;
            println!("{}", quote(&hex(&bytes)));
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

#[cfg(test)]
mod checksum_tests {
    use super::*;

    #[test]
    fn polynomial_fold_is_identical_to_ordered_byte_recurrence() {
        let data: Vec<_> = (0..65_539)
            .map(|i| ((i * 193 + 71) ^ (i >> 3)) as u8)
            .collect();
        for state in [0, 1, u64::MAX, 0x89ab_cdef_0123_4567] {
            for length in (0..=300).chain([4095, 4096, 4097, 65_536, 65_539]) {
                let mut scalar = state;
                for byte in &data[..length] {
                    fold(&mut scalar, u64::from(*byte));
                }
                let mut combined = state;
                fold_bytes(&mut combined, &data[..length]);
                assert_eq!(combined, scalar);
            }
        }
    }
}
