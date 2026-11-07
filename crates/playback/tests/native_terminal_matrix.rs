//! Native MP4 packets against FFprobe over the terminal-media matrix.
//!
//! Each case is read through the native packet view once per source geometry:
//! unknown and known length, three read sizes and two work quanta from a
//! nonseekable source, and, for the original cases, once from a seekable
//! source of known length. Every native packet is projected into FFprobe's
//! packet object and compared field by field with the recorded output.
mod common;
use common::off;
use std::collections::BTreeMap;
use triergon_core::{
    Budget, Bytes, Error, InputCursor, SourceCapabilities, SourceId, TimeBase, WorkBudget,
};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{MetadataReadScope, Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
    *,
};
#[allow(dead_code)]
mod gold {
    include!("fixtures/native_terminal_matrix_golds.rs");
}

const STEPS: usize = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Opener {
    Strict,
    Recovery,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    known: bool,
    chunk: usize,
    work: u64,
    file: bool,
}

fn geometries(original: bool) -> Vec<Geometry> {
    let mut all = Vec::new();
    for known in [false, true] {
        for chunk in [1, 7, 4096] {
            for work in [1, 4096] {
                all.push(Geometry {
                    known,
                    chunk,
                    work,
                    file: false,
                });
            }
        }
    }
    if original {
        all.push(Geometry {
            known: true,
            chunk: 7,
            work: 4096,
            file: true,
        });
    }
    all
}

/// One FFprobe packet object, with FFprobe's field spelling.
#[derive(Debug, PartialEq, Eq)]
struct Object {
    codec_type: &'static str,
    stream_index: usize,
    pts: Option<i64>,
    pts_time: Option<String>,
    dts: Option<i64>,
    dts_time: Option<String>,
    duration: Option<i64>,
    duration_time: Option<String>,
    size: String,
    pos: Option<String>,
    flags: String,
    data: Vec<u8>,
}

impl From<&gold::Packet> for Object {
    fn from(p: &gold::Packet) -> Self {
        Self {
            codec_type: p.codec_type,
            stream_index: p.stream_index,
            pts: p.pts,
            pts_time: p.pts_time.map(str::to_owned),
            dts: p.dts,
            dts_time: p.dts_time.map(str::to_owned),
            duration: p.duration,
            duration_time: p.duration_time.map(str::to_owned),
            size: p.size.to_owned(),
            pos: p.pos.map(str::to_owned),
            flags: p.flags.to_owned(),
            data: p.data.to_vec(),
        }
    }
}

/// FFprobe prints `ts * av_q2d(time_base)` with six decimals.
#[allow(
    clippy::cast_precision_loss,
    reason = "FFprobe's own double arithmetic"
)]
fn seconds(ticks: i64, base: TimeBase) -> String {
    let value = ticks as f64 * (base.numerator() as f64 / base.denominator() as f64);
    format!("{value:.6}")
}

fn codec_type(handler: [u8; 4]) -> &'static str {
    match &handler {
        b"vide" => "video",
        b"soun" => "audio",
        b"text" | b"sbtl" | b"subt" => "subtitle",
        _ => "unknown",
    }
}

#[allow(clippy::too_many_arguments, reason = "one field per FFprobe key")]
fn object(
    handler: [u8; 4],
    stream_index: usize,
    base: TimeBase,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: i64,
    flags: u32,
    pos: u64,
    size: u64,
    data: &[u8],
) -> Object {
    let flag = |bit: u32, c: char| if flags & bit != 0 { c } else { '_' };
    Object {
        codec_type: codec_type(handler),
        stream_index,
        pts,
        pts_time: pts.map(|t| seconds(t, base)),
        dts,
        dts_time: dts.map(|t| seconds(t, base)),
        duration: (duration != 0).then_some(duration),
        duration_time: (duration != 0).then(|| seconds(duration, base)),
        size: size.to_string(),
        pos: Some(pos.to_string()),
        flags: [flag(1, 'K'), flag(4, 'D'), flag(2, 'C')].iter().collect(),
        data: data.to_vec(),
    }
}

/// Project one delivered event into FFprobe's packet object, if the native
/// view emits a packet for it.
fn project(event: &RawEvent, tracks: &[u32]) -> Option<Object> {
    let index = |id: u32| tracks.iter().position(|t| *t == id).expect("planned track");
    match event {
        RawEvent::Mp4(Mp4Event::Packet(p)) => {
            let n = p.native.as_ref()?;
            let range = &n.demux.source_range;
            Some(object(
                p.configuration.track.handler,
                index(n.track_id),
                n.media_clock.demux_base,
                n.demux.pts,
                n.demux.dts,
                n.demux.duration,
                n.demux.flags,
                range.start,
                range.end - range.start,
                p.source.payload.as_ref(),
            ))
        }
        RawEvent::Mp4(Mp4Event::NativeShortPacket(p)) => {
            let n = &p.native;
            Some(object(
                p.configuration.track.handler,
                index(n.track_id),
                n.media_clock.demux_base,
                n.demux.pts,
                n.demux.dts,
                n.demux.duration,
                n.demux.flags,
                n.demux.source_range.start,
                p.prefix.payload.len() as u64,
                p.prefix.payload.as_ref(),
            ))
        }
        RawEvent::Mp4(Mp4Event::NativeEmptyPacket(e)) => Some(object(
            e.handler,
            index(e.boundary.track_id),
            e.time_base,
            Some(e.pts),
            Some(e.dts),
            e.duration,
            e.flags,
            e.position,
            0,
            &[],
        )),
        _ => None,
    }
}

struct Run {
    objects: Vec<Object>,
    /// The phase and error of a run that did not reach the end.
    failure: Option<(&'static str, Error)>,
}

fn run(raw: &[u8], g: Geometry, opener: Opener) -> Run {
    let mut objects = Vec::new();
    let mut phase = "open";
    let failure = (|| -> Result<()> {
        let pool = Budget::new(128 << 20)?;
        let id = SourceId(9001);
        let source = ByteSource::new(id, Bytes::copy_from_slice(&pool, raw)?, g.chunk)?;
        let capabilities = SourceCapabilities {
            length: g.known.then_some(raw.len() as u64),
            random_access: g.file,
        };
        let selected = SelectedInput {
            input: InputCursor::new(&pool, id, capabilities, 4096)?,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        };
        let mut open = match opener {
            Opener::Strict => Mp4Open::new(selected, OpenLimits::default())?,
            Opener::Recovery => Mp4Open::new_recovery(selected, OpenLimits::default())?,
        };
        let mut ready = false;
        for _ in 0..STEPS {
            if matches!(
                open.advance(&mut WorkBudget::new(g.work), &off())?,
                OpenProgress::Ready
            ) {
                ready = true;
                break;
            }
        }
        assert!(ready, "bounded open");
        let opened = open.finish()?;
        phase = "index";
        let front = matches!(
            opened.movie.read_scope(),
            MetadataReadScope::FrontMetadataBeforeMdat(_)
        );
        let mut job = if front {
            Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default())?
        } else {
            Mp4IndexJob::new(
                &opened.movie,
                Mp4View::PinnedMov9NoParseNoFillin,
                PlanLimits::default(),
            )?
        };
        let mut ready = false;
        for _ in 0..STEPS {
            if job.poll(&mut WorkBudget::new(g.work), &off())? {
                ready = true;
                break;
            }
        }
        assert!(ready, "bounded index");
        let plan = job.finish()?;
        let tracks: Vec<_> = plan.tracks().iter().map(|t| t.snapshot.track_id).collect();
        phase = "attach";
        let engine = if front {
            Engine::mp4_native_terminal_v1(
                opened.input,
                plan,
                1 << 20,
                ProgressiveLimits::default(),
            )?
        } else {
            Engine::mp4(opened.input, plan, 1 << 20)?
        };
        let mut player = Player::new(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )?;
        phase = "read";
        for _ in 0..STEPS {
            let step = player.advance(&mut WorkBudget::new(g.work), &off());
            while let Some(output) = player.pop() {
                objects.extend(project(output.event(), &tracks));
            }
            if matches!(step?, Progress::Ended) {
                return Ok(());
            }
        }
        panic!("bounded read");
    })()
    .err()
    .map(|error| (phase, error));
    Run { objects, failure }
}

/// The outcome class of one matrix row.
fn verdict(case: &gold::Case, g: Geometry, opener: Opener) -> String {
    let probe = match (&case.file, g.file) {
        (Some(gold::File::Own(probe)), true) => probe,
        (Some(gold::File::SameAsPipe), true) | (_, false) => &case.pipe,
        (None, true) => panic!("file record"),
    };
    let run = run(case.raw, g, opener);
    let expected: Vec<Object> = probe.packets.iter().map(Object::from).collect();
    match (&run.failure, probe.rc) {
        (None, 0) if run.objects == expected => "match".into(),
        (None, 0) => {
            let at = run
                .objects
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(run.objects.len().min(expected.len()));
            format!(
                "packet difference at {at} ({} of {} packets)",
                run.objects.len(),
                expected.len()
            )
        }
        (Some((phase, error)), 0) => format!("{phase}: {error:?}"),
        (None, rc) => format!(
            "FFprobe rc {rc}; Triergon completed {} packets",
            run.objects.len()
        ),
        (Some((phase, error)), rc) => format!("FFprobe rc {rc}; {phase}: {error:?}"),
    }
}

fn matrix(opener: Opener) -> BTreeMap<(String, &'static str, String), usize> {
    let mut rows = BTreeMap::new();
    for (at, case) in gold::CASES.iter().enumerate() {
        let original = case.file.is_some();
        assert_eq!(original, at < 31);
        for g in geometries(original) {
            let mode = if g.file { "file" } else { "pipe" };
            *rows
                .entry((case.name.to_owned(), mode, verdict(case, g, opener)))
                .or_default() += 1;
        }
    }
    rows
}

type Row = (&'static str, &'static str, usize, &'static str);

/// Every row that does not match, by case, mode and outcome, and the number
/// of matching rows.
fn assert_matrix(opener: Opener, matches: usize, nonpasses: &[Row]) {
    let rows = matrix(opener);
    assert_eq!(rows.values().sum::<usize>(), 499, "matrix rows");
    let observed: Vec<Row> = rows
        .iter()
        .filter(|((_, _, verdict), _)| verdict != "match")
        .map(|((case, mode, verdict), count)| {
            let known = nonpasses
                .iter()
                .find(|row| row.0 == case && row.1 == *mode && row.3 == verdict);
            known.map_or_else(
                || panic!("{opener:?}: {count} {mode} rows of {case}: {verdict}"),
                |row| (row.0, row.1, *count, row.3),
            )
        })
        .collect();
    assert_eq!(observed, nonpasses, "{opener:?} nonpasses");
    let matched: usize = rows
        .iter()
        .filter(|((_, _, verdict), _)| verdict == "match")
        .map(|(_, count)| count)
        .sum();
    assert_eq!(matched, matches, "{opener:?} matches");
}

/// With the MOV recovery opener every row matches FFprobe except the two
/// seekable reads that FFprobe itself refuses: a zero-size fragment sample
/// fails its header, and the recovery opener refuses the same header.
#[test]
fn recovery_native_view_matches_ffprobe_on_the_terminal_matrix() {
    const REFUSAL: &str =
        "FFprobe rc 1; open: Invalid(\"zero-size fragment sample in native MOV header\")";
    assert_matrix(
        Opener::Recovery,
        497,
        &[
            ("open-ended--empty-sample-terminal", "file", 1, REFUSAL),
            ("progressive--pending-empty-endpoint", "file", 1, REFUSAL),
        ],
    );
}

/// The strict opener reads the same matrix by the source rules. It refuses
/// out-of-source chunk offsets, truncated samples and a zero media timescale
/// instead of recovering them, and it reads the zero-size fragment samples
/// that FFprobe refuses on a seekable source.
#[test]
fn strict_native_view_refuses_malformed_media_on_the_terminal_matrix() {
    const OUTSIDE: &str = "index: Invalid(\"sample outside media extent\")";
    const ORDER: &str =
        "index: Unsupported(\"physical merge needs nondecreasing per-track source offsets\")";
    const ZERO_SCALE: &str = "open: Invalid(\"zero media/movie timescale\")";
    const ZERO_SIZE: &str = "FFprobe rc 1; Triergon completed 9 packets";
    const SHORT: &str = "read: Invalid(\"sample outside media extent\")";
    assert_matrix(
        Opener::Strict,
        446,
        &[
            ("mp4--bad-chunk-offset-outside-source", "file", 1, OUTSIDE),
            ("mp4--bad-chunk-offset-outside-source", "pipe", 6, OUTSIDE),
            ("mp4--bad-chunk-offset-outside-source", "pipe", 6, ORDER),
            ("mp4--bad-truncate-final-byte", "file", 1, "open: Truncated"),
            ("mp4--bad-truncate-final-byte", "pipe", 6, "open: Truncated"),
            ("mp4--bad-truncate-final-byte", "pipe", 6, "read: Truncated"),
            ("mp4--bad-zero-timescale", "file", 1, ZERO_SCALE),
            ("mp4--bad-zero-timescale", "pipe", 12, ZERO_SCALE),
            ("open-ended--empty-sample-terminal", "file", 1, ZERO_SIZE),
            ("progressive--pending-empty-endpoint", "file", 1, ZERO_SIZE),
            ("short-before-empty", "pipe", 6, SHORT),
            ("short-before-empty", "pipe", 6, "read: Truncated"),
        ],
    );
}
