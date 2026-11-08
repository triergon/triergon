//! Native MP4 packets against FFprobe over the terminal-media matrix.
//!
//! Each input is read through the native packet view in twelve nonseekable
//! source geometries (unknown and known length, read sizes 1, 7 and 4096, work
//! quanta 1 and 4096), and each of the 31 original inputs, the ones whose
//! record carries a seekable read, once more from a seekable source of known
//! length. Each run is compared
//! with FFprobe's record of the same kind of read: every packet the run
//! delivers is projected into FFprobe's packet object and compared field by
//! field, and where FFprobe refused the header the run must refuse while
//! opening, before any packet.
mod common;
mod terminal_golds;

use common::{byte_input, read_units, run_index, run_open};
use terminal_golds::{CASES, Case, Packet, Probe};
use triergon_core::{Budget, Error, SourceCapabilities, SourceId, TimeBase};
use triergon_isobmff::compat::mov::packets::PacketOccurrence;
use triergon_playback::{
    mp4_open::{MetadataReadScope, Mp4Open, OpenLimits, OpenedMp4},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits, PreparedMp4},
    progressive::ProgressiveLimits,
    *,
};

/// Rows per build profile: twelve nonseekable reads of each of the 39 inputs
/// and one seekable read of each of the 31 original ones.
const ROWS: usize = 499;

#[derive(Clone, Copy, Debug)]
enum Opener {
    Strict,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Pipe,
    File,
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    known: bool,
    chunk: usize,
    work: u64,
    mode: Mode,
}

fn geometries(seekable: bool) -> impl Iterator<Item = Geometry> {
    let pipe = [false, true].into_iter().flat_map(|known| {
        [1, 7, 4096].into_iter().flat_map(move |chunk| {
            [1, 4096].into_iter().map(move |work| Geometry {
                known,
                chunk,
                work,
                mode: Mode::Pipe,
            })
        })
    });
    let file = seekable.then_some(Geometry {
        known: true,
        chunk: 7,
        work: 4096,
        mode: Mode::File,
    });
    pipe.chain(file)
}

/// One FFprobe packet object, owned.
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

impl From<&Packet> for Object {
    fn from(p: &Packet) -> Self {
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

/// FFprobe's media type of a handler; a handler it does not map is data.
fn codec_type(handler: [u8; 4]) -> &'static str {
    match &handler {
        b"vide" => "video",
        b"soun" => "audio",
        b"text" | b"sbtl" | b"subt" => "subtitle",
        _ => "data",
    }
}

/// The FFprobe fields of one native packet.
struct Native<'a> {
    handler: [u8; 4],
    stream_index: usize,
    base: TimeBase,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: i64,
    flags: u32,
    pos: u64,
    size: u64,
    data: &'a [u8],
}

impl<'a> Native<'a> {
    fn occurrence(
        handler: [u8; 4],
        stream_index: usize,
        n: &PacketOccurrence,
        size: u64,
        data: &'a [u8],
    ) -> Self {
        Self {
            handler,
            stream_index,
            base: n.media_clock.demux_base,
            pts: n.demux.pts,
            dts: n.demux.dts,
            duration: n.demux.duration,
            flags: n.demux.flags,
            pos: n.demux.source_range.start,
            size,
            data,
        }
    }

    fn object(&self) -> Object {
        let flag = |bit: u32, c: char| if self.flags & bit != 0 { c } else { '_' };
        let time = |ticks: i64| seconds(ticks, self.base);
        Object {
            codec_type: codec_type(self.handler),
            stream_index: self.stream_index,
            pts: self.pts,
            pts_time: self.pts.map(time),
            dts: self.dts,
            dts_time: self.dts.map(time),
            duration: (self.duration != 0).then_some(self.duration),
            duration_time: (self.duration != 0).then(|| time(self.duration)),
            size: self.size.to_string(),
            pos: Some(self.pos.to_string()),
            flags: [flag(1, 'K'), flag(4, 'D'), flag(2, 'C')].iter().collect(),
            data: self.data.to_vec(),
        }
    }
}

/// Project delivered outputs into FFprobe packet objects. Every event is
/// accounted for: native packets, short packets and empty packets project;
/// configurations and top-level boxes carry no FFprobe packet; source packets
/// without a native occurrence may only follow the end of the native view.
fn project(outputs: &[Output], tracks: &[u32]) -> Vec<Object> {
    let index = |id: u32| tracks.iter().position(|t| *t == id).expect("planned track");
    let mut ended = false;
    let mut objects = Vec::new();
    for output in outputs {
        let event = match output.event() {
            RawEvent::Mp4(event) => event,
            RawEvent::Mp4Root(_) => continue,
            other => panic!("unexpected event {other:?}"),
        };
        match event {
            Mp4Event::Packet(p) => {
                let Some(n) = &p.native else {
                    assert!(
                        ended,
                        "source packet without a native occurrence in the native view"
                    );
                    continue;
                };
                assert!(!ended, "native packet after the end of the native view");
                let range = &n.demux.source_range;
                let size = range.end - range.start;
                let (handler, data) = (p.configuration.track.handler, p.source.payload.as_ref());
                let native = Native::occurrence(handler, index(n.track_id), n, size, data);
                objects.push(native.object());
                ended = p.native_front_read_boundary_v1.is_some();
            }
            Mp4Event::NativeShortPacket(p) => {
                assert!(!ended, "short packet after the end of the native view");
                let n = &p.native;
                let (handler, data) = (p.configuration.track.handler, p.prefix.payload.as_ref());
                let native =
                    Native::occurrence(handler, index(n.track_id), n, data.len() as u64, data);
                objects.push(native.object());
                ended = true;
            }
            Mp4Event::NativeEmptyPacket(e) => {
                let empty = Native {
                    handler: e.handler,
                    stream_index: index(e.boundary.track_id),
                    base: e.time_base,
                    pts: Some(e.pts),
                    dts: Some(e.dts),
                    duration: e.duration,
                    flags: e.flags,
                    pos: e.position,
                    size: 0,
                    data: &[],
                };
                objects.push(empty.object());
                ended = true;
            }
            Mp4Event::NativeMissingSample(_) => ended = true,
            Mp4Event::Configuration(_) => {}
        }
    }
    objects
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Open,
    Index,
    Attach,
    Read,
}

fn open(pool: &Budget, raw: &[u8], g: Geometry, opener: Opener) -> Result<OpenedMp4<ByteSource>> {
    let capabilities = SourceCapabilities {
        length: g.known.then_some(raw.len() as u64),
        random_access: g.mode == Mode::File,
    };
    let selected = byte_input(pool, SourceId(9001), raw, g.chunk, capabilities, 4096)?;
    let mut open = match opener {
        Opener::Strict => Mp4Open::new(selected, OpenLimits::default())?,
        Opener::Recovery => Mp4Open::new_recovery(selected, OpenLimits::default())?,
    };
    run_open(&mut open, g.work)?;
    open.finish()
}

/// Whether the opener read the metadata before any media, which only a
/// nonseekable source with a front `moov` gives.
fn front(opened: &OpenedMp4<ByteSource>) -> bool {
    matches!(
        opened.movie.read_scope(),
        MetadataReadScope::FrontMetadataBeforeMdat(_)
    )
}

fn index(opened: &OpenedMp4<ByteSource>, g: Geometry) -> Result<PreparedMp4> {
    let mut job = if front(opened) {
        Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default())?
    } else {
        let view = Mp4View::PinnedMov9NoParseNoFillin;
        Mp4IndexJob::new(&opened.movie, view, PlanLimits::default())?
    };
    run_index(&mut job, g.work)?;
    job.finish()
}

fn attach(opened: OpenedMp4<ByteSource>, plan: PreparedMp4) -> Result<Player<ByteSource>> {
    let engine = if front(&opened) {
        let limits = ProgressiveLimits::default();
        Engine::mp4_native_terminal_v1(opened.input, plan, 1 << 20, limits)?
    } else {
        Engine::mp4(opened.input, plan, 1 << 20)?
    };
    let policy = PlayerPolicy::default();
    Player::new(engine, opened.source, Selection::All, policy)
}

/// Open, index, attach and read one input, appending the planned tracks and
/// what the player delivers.
fn stages(
    pool: &Budget,
    raw: &[u8],
    g: Geometry,
    opener: Opener,
    tracks: &mut Vec<u32>,
    outputs: &mut Vec<Output>,
) -> std::result::Result<(), (Stage, Error)> {
    let opened = open(pool, raw, g, opener).map_err(|e| (Stage::Open, e))?;
    assert!(
        !front(&opened) || g.mode == Mode::Pipe,
        "front read of a seekable source"
    );
    let plan = index(&opened, g).map_err(|e| (Stage::Index, e))?;
    tracks.extend(plan.tracks().iter().map(|t| t.snapshot.track_id));
    let mut player = attach(opened, plan).map_err(|e| (Stage::Attach, e))?;
    read_units(&mut player, g.work, outputs).map_err(|e| (Stage::Read, e))
}

/// The objects one run delivered, and the stage and error of a run that did
/// not reach the end.
fn run(raw: &[u8], g: Geometry, opener: Opener) -> (Vec<Object>, Option<(Stage, Error)>) {
    let pool = Budget::new(128 << 20).expect("test pool");
    let (mut tracks, mut outputs) = (Vec::new(), Vec::new());
    let failure = stages(&pool, raw, g, opener, &mut tracks, &mut outputs).err();
    (project(&outputs, &tracks), failure)
}

/// The outcome of one matrix row.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// FFprobe printed packets and the run delivered the same objects.
    Match,
    /// FFprobe refused the header as invalid data and the run refused while
    /// opening, with invalid, truncated or overflowing data, before any packet.
    MatchingRefusal,
    /// Both completed, and the objects first differ at `at`.
    Differs {
        at: usize,
        delivered: usize,
        expected: usize,
    },
    /// FFprobe printed packets and the run refused.
    Refused(Stage, Error),
    /// FFprobe refused the header and the run completed.
    Completed { delivered: usize },
    /// FFprobe refused the header and the run refused elsewhere.
    RefusedElsewhere(Stage, Error),
}

fn verdict(case: &Case, g: Geometry, opener: Opener) -> Outcome {
    let probe = match g.mode {
        Mode::Pipe => &case.pipe,
        Mode::File => case.seekable().expect("file record"),
    };
    let (objects, failure) = run(case.raw, g, opener);
    match (probe, failure) {
        (Probe::Packets(expected), None) => {
            let expected: Vec<Object> = expected.iter().map(Object::from).collect();
            match objects.iter().zip(&expected).position(|(a, b)| a != b) {
                None if objects.len() == expected.len() => Outcome::Match,
                at => Outcome::Differs {
                    at: at.unwrap_or(objects.len().min(expected.len())),
                    delivered: objects.len(),
                    expected: expected.len(),
                },
            }
        }
        (Probe::Packets(_), Some((stage, error))) => Outcome::Refused(stage, error),
        (
            Probe::InvalidHeader,
            Some((Stage::Open, Error::Invalid(_) | Error::Truncated | Error::Overflow)),
        ) if objects.is_empty() => Outcome::MatchingRefusal,
        (Probe::InvalidHeader, None) => Outcome::Completed {
            delivered: objects.len(),
        },
        (Probe::InvalidHeader, Some((stage, error))) => Outcome::RefusedElsewhere(stage, error),
    }
}

type Row = (&'static str, Mode, usize, Outcome);

/// The rows of the matrix that do not match, by input, mode and outcome in
/// matrix order, and the number of matching rows.
fn assert_matrix(opener: Opener, matches: usize, others: &[Row]) {
    let mut rows: Vec<Row> = Vec::new();
    for case in CASES {
        for g in geometries(case.seekable().is_some()) {
            let outcome = verdict(case, g, opener);
            match rows
                .iter_mut()
                .find(|r| r.0 == case.name && r.1 == g.mode && r.3 == outcome)
            {
                Some(row) => row.2 += 1,
                None => rows.push((case.name, g.mode, 1, outcome)),
            }
        }
    }
    assert_eq!(rows.iter().map(|r| r.2).sum::<usize>(), ROWS, "matrix rows");
    let (matched, rest): (Vec<Row>, Vec<Row>) =
        rows.into_iter().partition(|r| r.3 == Outcome::Match);
    assert_eq!(rest, others, "{opener:?} rows that do not match");
    assert_eq!(
        matched.iter().map(|r| r.2).sum::<usize>(),
        matches,
        "{opener:?} matches"
    );
}

/// With the MOV recovery opener every row matches FFprobe. Two are matching
/// refusals: from a seekable source FFprobe refuses the header of a file with
/// a zero-size fragment sample, and the recovery opener refuses it while
/// opening.
#[test]
fn recovery_native_view_matches_ffprobe_on_the_terminal_matrix() {
    assert_matrix(
        Opener::Recovery,
        497,
        &[
            (
                "open-ended--empty-sample-terminal",
                Mode::File,
                1,
                Outcome::MatchingRefusal,
            ),
            (
                "progressive--pending-empty-endpoint",
                Mode::File,
                1,
                Outcome::MatchingRefusal,
            ),
        ],
    );
}

/// The strict opener reads the same matrix by the source rules and recovers
/// nothing.
///
/// - It reads the zero-size fragment samples that FFprobe refuses from a
///   seekable source.
/// - It refuses a zero media timescale while opening.
/// - It refuses a chunk offset outside the source. The first chunk lies beyond
///   the end of the file and the next one before it. When the source declares
///   its length, the index refuses the first chunk; when it does not, the
///   chunk could lie in a later `mdat`, so the refusal comes at the physical
///   end of the source.
/// - It refuses a sample that runs past the end of the source. When the
///   source declares its length, a final `mdat` of size zero has a known end,
///   and a sample beyond it is outside the media extent (`Invalid`); when it
///   does not, the source ends inside the sample (`Truncated`). A final `mdat`
///   whose declared size runs past a declared length is refused as
///   `Truncated` while opening, and while reading when the length is unknown.
#[test]
fn strict_native_view_refuses_malformed_media_on_the_terminal_matrix() {
    use Mode::{File, Pipe};
    use Outcome::{Completed, Refused};
    use Stage::{Index, Open, Read};
    const OUTSIDE: Error = Error::Invalid("sample outside media extent");
    const ZERO_SCALE: Error = Error::Invalid("zero media/movie timescale");
    assert_matrix(
        Opener::Strict,
        446,
        &[
            (
                "open-ended--empty-sample-terminal",
                File,
                1,
                Completed { delivered: 9 },
            ),
            (
                "progressive--pending-empty-endpoint",
                File,
                1,
                Completed { delivered: 9 },
            ),
            (
                "mp4--bad-chunk-offset-outside-source",
                Pipe,
                6,
                Refused(Read, OUTSIDE),
            ),
            (
                "mp4--bad-chunk-offset-outside-source",
                Pipe,
                6,
                Refused(Index, OUTSIDE),
            ),
            (
                "mp4--bad-chunk-offset-outside-source",
                File,
                1,
                Refused(Index, OUTSIDE),
            ),
            (
                "mp4--bad-truncate-final-byte",
                Pipe,
                6,
                Refused(Read, Error::Truncated),
            ),
            (
                "mp4--bad-truncate-final-byte",
                Pipe,
                6,
                Refused(Open, Error::Truncated),
            ),
            (
                "mp4--bad-truncate-final-byte",
                File,
                1,
                Refused(Open, Error::Truncated),
            ),
            (
                "mp4--bad-zero-timescale",
                Pipe,
                12,
                Refused(Open, ZERO_SCALE),
            ),
            (
                "mp4--bad-zero-timescale",
                File,
                1,
                Refused(Open, ZERO_SCALE),
            ),
            (
                "short-before-empty",
                Pipe,
                6,
                Refused(Read, Error::Truncated),
            ),
            ("short-before-empty", Pipe, 6, Refused(Read, OUTSIDE)),
        ],
    );
}
