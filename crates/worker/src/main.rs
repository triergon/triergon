//! Isolated harness adapter. Serialization and process startup are outside timed work.
#![forbid(unsafe_code)]
mod catalogue;
use std::{
    fmt::Write as _,
    io::{self, Cursor, Read},
    sync::atomic::AtomicBool,
    time::Instant,
};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    hex, quoted,
};
use triergon_matroska::timing::ExactTime;
use triergon_matroska::{Limits, typed::BlockingReader as Demuxer};

// Explicit private operation binding. Library defaults remain OwnedExact.
fn open_shared<R: Read>(source: R, limits: Limits) -> triergon_matroska::Result<Demuxer<R>> {
    Demuxer::open_with_payload_ownership(
        source,
        limits,
        triergon_matroska::typed::PayloadOwnership::SharedBounded,
    )
}

fn open_owned(
    data: Vec<u8>,
    limits: Limits,
) -> triergon_matroska::Result<Demuxer<Cursor<triergon_matroska::storage::OwnedInputBytes>>> {
    let pool = triergon_matroska::storage::Budget::new(limits.live_storage_bytes)?;
    let source = triergon_matroska::storage::OwnedInputBytes::try_from_vec(&pool, data)
        .map_err(|error| error.error())?;
    Demuxer::open_owned_seekable_in(
        source,
        limits,
        &pool,
        triergon_matroska::typed::PayloadOwnership::SharedBounded,
    )
}

fn codec(s: &str) -> &str {
    match s {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "A_AAC" => "aac",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_VORBIS" => "vorbis",
        "A_OPUS" => "opus",
        "A_PCM/INT/LIT" => "pcm_s16le",
        "S_TEXT/UTF8" => "subrip",
        "S_TEXT/ASS" => "ass",
        "S_VOBSUB" => "dvd_subtitle",
        _ => s,
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
    track: &triergon_matroska::matroska::Track,
    p: &triergon_matroska::matroska::Packet,
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
    track: &triergon_matroska::matroska::Track,
    packet: &triergon_matroska::matroska::Packet,
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
            quoted(&hex(&t.source_metadata))
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
                        &v.map_or("null".into(), |v| quoted(&format!("{:016x}", v.to_bits()))),
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
        quoted(if metadata == 2 {
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
            quoted(codec(&t.codec)),
            t.kind,
            quoted(&hex(&t.codec_private)),
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
                quoted(&t.language),
                quoted(&t.name)
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
            quoted(&hex(&p.data))
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
    data: &mut Vec<u8>,
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
        let mut d = open_owned(std::mem::take(data), Limits::default())?;
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
        // Match the old reverse-local drop order: bookkeeping before reader.
        drop(seen);
        *data = d
            .into_inner()
            .into_inner()
            .try_into_vec()
            .map_err(|_| "completed operation retained its immutable input owner")?;
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
            open_owned(std::mem::take(&mut data), Limits::default())?,
            true,
            mode == "codec-trace",
            if mode == "catalogue-trace" {
                2
            } else {
                u8::from(mode == "metadata-trace")
            },
        )?,
        "catalogue-source" => {
            catalogue::source(open_owned(std::mem::take(&mut data), Limits::default())?)?
        }
        "metadata-source" => {
            trace_colour_source(open_owned(std::mem::take(&mut data), Limits::default())?)?
        }
        "trace-source" => trace_source(open_owned(std::mem::take(&mut data), Limits::default())?)?,
        "trace-source-short" => {
            let n: usize = a.get(3).ok_or("missing chunk")?.parse()?;
            if n == 0 {
                return Err("zero chunk".into());
            }
            trace_source(open_shared(
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
                open_shared(
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
                &mut data,
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
                    &mut data,
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
            let mut demux = open_owned(std::mem::take(&mut data), Limits::default())?;
            if a.get(4).is_some_and(|value| value == "after-eof") {
                while demux.next_packet(&AtomicBool::new(false))?.is_some() {}
            }
            let receipt = demux.seek_for_playback(0, target, &AtomicBool::new(false))?;
            trace(demux, receipt.replays_from_start, false, 0)?;
        }
        "inflate" => {
            let limits = triergon_matroska::zlib::InflateLimits {
                output_bytes: a.get(3).map_or(Ok(16 * 1024 * 1024), |s| s.parse())?,
                expansion_ratio: a.get(4).map_or(Ok(1024), |s| s.parse())?,
                work: a.get(5).map_or(Ok(128 * 1024 * 1024), |s| s.parse())?,
            };
            let bytes =
                triergon_matroska::zlib::decompress(&data, limits, &AtomicBool::new(false))?;
            println!("{}", quoted(&hex(&bytes)));
        }
        "seek" => {
            let target: i64 = a.get(3).ok_or("missing seek time")?.parse()?;
            let mut d = open_owned(std::mem::take(&mut data), Limits::default())?;
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
                quoted(&hex(&p.data))
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
