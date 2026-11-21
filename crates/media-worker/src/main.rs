//! The Matroska comparison worker: the traces, folds, timings and memory windows
//! the harness compares with the reference. Serialization and process startup stay
//! outside timed work.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]
mod catalogue;
use std::{
    fmt::Write as _,
    io::{self, Cursor, Read},
    process::ExitCode,
    sync::atomic::AtomicBool,
};
use triergon_harness_protocol::{
    digest::{fold, fold_bytes},
    hex, quoted,
};
use triergon_harness_runner::{BenchLine, Boundary, Timer, command};
use triergon_matroska::compat::seek::at_block_start;
use triergon_matroska::reference_trace::seek_initial_trim;
use triergon_matroska::timing::ExactTime;
use triergon_matroska::{BlockingReader as Demuxer, Limits, compat::clock::packet_timing};

/// The error a mode reports on stderr before the worker exits with failure.
type Error = Box<dyn std::error::Error>;
type Outcome = Result<(), Error>;

/// A reader whose packets share their input windows (`SharedBounded`); the library
/// default is `OwnedExact`.
fn open_shared<R: Read>(source: R, limits: Limits) -> triergon_matroska::Result<Demuxer<R>> {
    Demuxer::open_with_payload_ownership(
        source,
        limits,
        triergon_matroska::PayloadOwnership::SharedBounded,
    )
}

/// A seekable reader over the whole input, held as one charged allocation that the
/// packets share.
fn open_owned(
    data: Vec<u8>,
    limits: Limits,
) -> triergon_matroska::Result<Demuxer<Cursor<triergon_core::OwnedInputBytes>>> {
    let pool = triergon_core::Budget::new(limits.live_storage_bytes)?;
    let source =
        triergon_core::OwnedInputBytes::try_from_vec(&pool, data).map_err(|error| error.error)?;
    Demuxer::open_owned_seekable_in(
        source,
        limits,
        &pool,
        triergon_matroska::PayloadOwnership::SharedBounded,
    )
}

/// The reference's codec name for a CodecID, or the CodecID itself.
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

/// `ns` nanoseconds as a rounded count of samples at an integral `rate` in Hz.
fn sample_count(ns: u64, rate: f64) -> Result<u64, Error> {
    if rate.fract() != 0.0 || !(1.0..=u32::MAX as f64).contains(&rate) {
        return Err("unsupported sample-trim rate".into());
    }
    Ok(u64::try_from(
        (u128::from(ns) * u128::from(rate as u32) + 500_000_000) / 1_000_000_000,
    )?)
}
/// The samples to trim from the start and end of a packet: the CodecDelay on a
/// track's `first` packet, then negative or positive DiscardPadding.
fn trims(
    track: &triergon_matroska::Track,
    p: &triergon_matroska::Packet,
    first: bool,
) -> Result<(u64, u64), Error> {
    let mut start = if first && track.kind == 2 {
        // Opus pre-skip is expressed in 48 kHz samples (RFC 7845 §5.1).
        let rate = if track.codec == "A_OPUS" {
            48_000.0
        } else {
            track.sample_rate
        };
        sample_count(track.codec_delay_ns, rate)?
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

/// A reader that returns at most `maximum` bytes per call.
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

/// An exact time as a JSON rational, or `null`.
fn exact(value: Option<ExactTime>) -> String {
    value.map_or("null".into(), |t| {
        format!(
            "{{\"numerator\":{},\"denominator\":{}}}",
            t.numerator(),
            t.denominator()
        )
    })
}

/// Every packet's track and exact source time.
fn trace_source<R: Read>(mut demux: Demuxer<R>) -> Outcome {
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
fn decoder_packet(track: &triergon_matroska::Track, packet: &triergon_matroska::Packet) -> bool {
    !(track.kind == 2
        && track.codec == "A_PCM/INT/LIT"
        && packet.data.is_empty()
        && packet.discard_padding_ns == 0)
}

/// Every track's raw TrackEntry and source colour fields.
fn trace_colour_source<R: Read>(d: Demuxer<R>) -> Outcome {
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

/// The demux trace: tracks, then every packet in the reference's clock with its
/// sample trims and payload, under `view`. `initial_trim` applies CodecDelay to each
/// track's first packet.
fn trace<R: Read>(mut d: Demuxer<R>, initial_trim: bool, view: View) -> Outcome {
    let catalogue = view.metadata == Metadata::Catalogue;
    let mut out = format!(
        "{{\"schema\":{},\"operation\":{},\"tracks\":[",
        if catalogue { 2 } else { 1 },
        quoted(if catalogue {
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
        if !catalogue {
            write!(
                out,
                ",\"language\":{},\"name\":{}",
                quoted(&t.language),
                quoted(&t.name)
            )?;
        }
        if view.metadata != Metadata::None {
            write!(
                out,
                ",\"colour\":{:?}",
                t.colour.unwrap_or_default().trace_fields()?
            )?;
        }
        out.push('}');
    }
    out.push_str("],\"packets\":[");
    let cancel = AtomicBool::new(false);
    let mut first = true;
    let mut seen = vec![false; d.tracks().len()];
    while let Some(p) = d.next_packet(&cancel)? {
        if view.codec_packets && !decoder_packet(&d.tracks()[p.track], &p) {
            continue;
        }
        let clock = packet_timing(p.timing)?;
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
                .map(ExactTime::rounded_ns)
                .transpose()?
                .map_or("null".into(), |v| v.to_string()),
            p.dts_ns.map_or("null".into(), |v| v.to_string()),
            clock
                .duration
                .map(ExactTime::rounded_duration_ns)
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
    if catalogue {
        catalogue::CatalogueView::new(&d)?.output(Some(&mut out), &mut 0)?;
    }
    out.push('}');
    println!("{out}");
    Ok(())
}

/// Demuxes the input `iterations` times, after a block-start seek to `seek` when
/// given, folding what `view` traces; returns the packets, payload bytes and digest.
#[expect(
    clippy::redundant_closure_for_method_calls,
    reason = "method paths change this measured loop's code"
)]
fn run(
    data: &mut Vec<u8>,
    iterations: u32,
    seek: Option<i64>,
    view: View,
) -> Result<(u64, u64, u64), Error> {
    let mut packets = 0;
    let mut bytes = 0;
    let mut digest = 0_u64;
    let cancel = AtomicBool::new(false);
    for _ in 0..iterations {
        let mut d = open_owned(std::mem::take(data), Limits::default())?;
        let initial_trim = if let Some(target) = seek {
            let landing = at_block_start(&mut d, 0, target, &cancel)?;
            seek_initial_trim(&d, 0, &landing)?
        } else {
            true
        };
        for t in d.tracks() {
            fold_bytes(&mut digest, codec(&t.codec).as_bytes());
            fold_bytes(&mut digest, &t.codec_private);
            if view.metadata != Metadata::Catalogue {
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
            if view.metadata != Metadata::None {
                for value in t.colour.unwrap_or_default().trace_fields()? {
                    fold(&mut digest, value as u64);
                }
            }
        }
        let mut seen = vec![false; d.tracks().len()];
        while let Some(p) = d.next_packet(&cancel)? {
            if view.codec_packets && !decoder_packet(&d.tracks()[p.track], &p) {
                continue;
            }
            packets += 1;
            bytes += p.data.len() as u64;
            fold_bytes(&mut digest, &p.data);
            let time = packet_timing(p.timing)?;
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
        if view.metadata == Metadata::Catalogue {
            catalogue::CatalogueView::new(&d)?.output(None, &mut digest)?;
        }
        // Release the bookkeeping before the reader hands the input back.
        drop(seen);
        *data = d
            .into_inner()
            .into_inner()
            .try_into_vec()
            .map_err(|_| "completed operation retained its immutable input owner")?;
    }
    Ok((packets, bytes, std::hint::black_box(digest)))
}

#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    command(None, 1, entry)
}

/// C's entry point, the one exception to the worker's `deny(unsafe_code)`
/// (`triergon-worker-entry`).
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}

/// What a demux fold or trace carries beyond the default view.
#[derive(Clone, Copy)]
struct View {
    /// Only the packets a decoder receives (`codec-`).
    codec_packets: bool,
    metadata: Metadata,
}

/// The metadata a demux fold or trace adds to the tracks and packets.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metadata {
    None,
    /// The colour fields of each track (`metadata-`).
    Colour,
    /// The catalogue in place of track languages and names (`catalogue-`).
    Catalogue,
}

impl View {
    const PLAIN: Self = Self {
        codec_packets: false,
        metadata: Metadata::None,
    };

    /// The view a mode's prefix names, and the mode without it.
    fn of(mode: &str) -> (Self, &str) {
        let view = |codec_packets, metadata| Self {
            codec_packets,
            metadata,
        };
        if let Some(rest) = mode.strip_prefix("codec-") {
            (view(true, Metadata::None), rest)
        } else if let Some(rest) = mode.strip_prefix("metadata-") {
            (view(false, Metadata::Colour), rest)
        } else if let Some(rest) = mode.strip_prefix("catalogue-") {
            (view(false, Metadata::Catalogue), rest)
        } else {
            (Self::PLAIN, mode)
        }
    }
}

enum Source {
    Catalogue,
    Metadata,
    Trace,
    ShortTrace,
}

enum Operation {
    Source(Source),
    Baseline,
    SeekTrace,
    PlaybackSeekTrace,
    Inflate,
    Seek,
    Trace(View),
    TraceShort(View),
    Bench(View),
    SeekBench,
    Memory(View),
    SeekMemory,
    MemoryBaseline,
}

impl Operation {
    fn parse(mode: &str) -> Result<Self, Error> {
        Ok(match mode {
            "catalogue-source" => Self::Source(Source::Catalogue),
            "metadata-source" => Self::Source(Source::Metadata),
            "trace-source" => Self::Source(Source::Trace),
            "trace-source-short" => Self::Source(Source::ShortTrace),
            "baseline" => Self::Baseline,
            "seek-trace" => Self::SeekTrace,
            "playback-seek-trace" => Self::PlaybackSeekTrace,
            "inflate" => Self::Inflate,
            "seek" => Self::Seek,
            "seek-bench" => Self::SeekBench,
            "seek-memory" => Self::SeekMemory,
            "memory-baseline" => Self::MemoryBaseline,
            other => match View::of(other) {
                (view, "trace") => Self::Trace(view),
                (view, "trace-short") => Self::TraceShort(view),
                (view, "bench") => Self::Bench(view),
                (view, "memory") => Self::Memory(view),
                _ => return Err("unknown operation".into()),
            },
        })
    }
}

fn entry(args: &[String]) -> Outcome {
    let (mode, args) = args.split_first().ok_or(
        "usage: media-worker trace|trace-short|seek|bench|baseline FILE [iterations/chunk/ns]",
    )?;
    let (path, words) = args.split_first().ok_or("missing input")?;
    let file = std::fs::File::open(path)?;
    let mut data = Vec::new();
    file.take(64 * 1024 * 1024 + 1).read_to_end(&mut data)?;
    if data.len() > 64 * 1024 * 1024 {
        return Err("input limit: 64 MiB".into());
    }
    match Operation::parse(mode)? {
        Operation::Source(mode) => source(mode, data, words),
        Operation::Baseline => {
            std::hint::black_box(&data);
            println!("{{\"input_bytes\":{}}}", data.len());
            Ok(())
        }
        Operation::SeekTrace => seek_trace(data, words, false),
        Operation::PlaybackSeekTrace => seek_trace(data, words, true),
        Operation::Inflate => inflate(&data, words),
        Operation::Seek => seek(data, words),
        Operation::Trace(view) => traced(data, None, view),
        Operation::TraceShort(view) => traced(data, Some(words), view),
        Operation::Bench(view) => bench(data, words, false, view),
        Operation::SeekBench => bench(data, words, true, View::PLAIN),
        Operation::Memory(view) => memory(data, words, Some(false), view),
        Operation::SeekMemory => memory(data, words, Some(true), View::PLAIN),
        Operation::MemoryBaseline => memory(data, words, None, View::PLAIN),
    }
}

// Every mode below runs out of line, so that the dispatch's frame holds none of
// their state and the measured `memory` mode's stack only its own operation's.

/// `[VIEW-]trace FILE`, or with `short` (`[VIEW-]trace-short FILE CHUNK`) read
/// through CHUNK-byte reads.
#[inline(never)]
fn traced(data: Vec<u8>, short: Option<&[String]>, view: View) -> Outcome {
    match short {
        None => trace(open_owned(data, Limits::default())?, true, view),
        Some(words) => trace(
            open_shared(self::short(&data, words)?, Limits::default())?,
            true,
            view,
        ),
    }
}

/// The source-frame views: `trace-source FILE`, `trace-source-short FILE CHUNK`,
/// `catalogue-source FILE` and `metadata-source FILE`.
#[inline(never)]
fn source(mode: Source, data: Vec<u8>, words: &[String]) -> Outcome {
    match mode {
        Source::Catalogue => catalogue::source(open_owned(data, Limits::default())?),
        Source::Metadata => trace_colour_source(open_owned(data, Limits::default())?),
        Source::ShortTrace => trace_source(open_shared(short(&data, words)?, Limits::default())?),
        Source::Trace => trace_source(open_owned(data, Limits::default())?),
    }
}

/// The input read at most `CHUNK` bytes per call (the first word).
fn short<'a>(data: &'a Vec<u8>, words: &[String]) -> Result<Short<Cursor<&'a Vec<u8>>>, Error> {
    let n: usize = words.first().ok_or("missing chunk")?.parse()?;
    if n == 0 {
        return Err("zero chunk".into());
    }
    Ok(Short {
        inner: Cursor::new(data),
        maximum: n,
    })
}

/// The seek target in nanoseconds at word `at`.
fn seek_target(words: &[String], at: usize) -> Result<i64, Error> {
    Ok(words.get(at).ok_or("missing seek target")?.parse()?)
}

/// `[VIEW-]bench FILE ITERATIONS` and `seek-bench FILE ITERATIONS NS`.
#[inline(never)]
fn bench(mut data: Vec<u8>, words: &[String], seek: bool, view: View) -> Outcome {
    let n: u32 = words.first().ok_or("missing iterations")?.parse()?;
    if n == 0 || n > 1_000_000 {
        return Err("invalid iteration count".into());
    }
    let start = Timer::start(true);
    let seek = if seek {
        Some(seek_target(words, 1)?)
    } else {
        None
    };
    let (packets, bytes, digest) = run(&mut data, n, seek, view)?;
    let ns = start.elapsed().as_nanos();
    let line = BenchLine::with_schema(u64::from(n), ns, (packets, bytes, digest));
    println!("{line}");
    Ok(())
}

/// One operation inside the allocation-window markers: `[VIEW-]memory FILE`,
/// `seek-memory FILE NS` (`Some(true)`), or `memory-baseline FILE` (`None`),
/// which runs nothing.
#[inline(never)]
fn memory(mut data: Vec<u8>, words: &[String], seek: Option<bool>, view: View) -> Outcome {
    use std::io::Write;
    let mut output = io::stdout().lock();
    Boundary::Begin.mark(&mut output)?;
    let result = match seek {
        None => (0, 0, 0),
        Some(seek) => {
            let seek = if seek {
                Some(seek_target(words, 0)?)
            } else {
                None
            };
            run(&mut data, 1, seek, view)?
        }
    };
    Boundary::End.mark(&mut output)?;
    writeln!(
        output,
        "{{\"packets\":{},\"bytes\":{},\"digest\":{}}}",
        result.0, result.1, result.2
    )?;
    Ok(())
}

/// `seek-trace FILE NS [after-eof]` or `playback-seek-trace FILE NS [after-eof]`.
#[inline(never)]
fn seek_trace(data: Vec<u8>, words: &[String], playback: bool) -> Outcome {
    let target = seek_target(words, 0)?;
    let mut demux = open_owned(data, Limits::default())?;
    let cancel = AtomicBool::new(false);
    if words.get(1).is_some_and(|value| value == "after-eof") {
        while demux.next_packet(&cancel)?.is_some() {}
    }
    let landing = if playback {
        demux.seek_for_playback(0, target, &cancel)?
    } else {
        at_block_start(&mut demux, 0, target, &cancel)?
    };
    let initial_trim = seek_initial_trim(&demux, 0, &landing)?;
    trace(demux, initial_trim, View::PLAIN)
}

/// `inflate FILE [OUTPUT_BYTES [EXPANSION_RATIO [WORK]]]`.
#[inline(never)]
fn inflate(data: &[u8], words: &[String]) -> Outcome {
    let limits = triergon_matroska::reference_trace::zlib::InflateLimits {
        output_bytes: words.first().map_or(Ok(16 * 1024 * 1024), |s| s.parse())?,
        expansion_ratio: words.get(1).map_or(Ok(1024), |s| s.parse())?,
        work: words.get(2).map_or(Ok(128 * 1024 * 1024), |s| s.parse())?,
    };
    let bytes = triergon_matroska::reference_trace::zlib::decompress(
        data,
        limits,
        &AtomicBool::new(false),
    )?;
    println!("{}", quoted(&hex(&bytes)));
    Ok(())
}

/// `seek FILE NS`: after the end of file, seek the first video track to the
/// key frame before NS and print the packet there.
#[inline(never)]
fn seek(data: Vec<u8>, words: &[String]) -> Outcome {
    let target: i64 = words.first().ok_or("missing seek time")?.parse()?;
    let mut d = open_owned(data, Limits::default())?;
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
