//! Reader packet comparison, by the rules of `harness/codecs/reader-rules.json` and the
//! reviewed differences of `harness/codecs/reader-differences.json`.
//!
//! For every `reader-*` row of a manifest, `codec-worker packets FORMAT FILE` prints
//! what the container reader emits per packet, and the pinned `ffprobe -show_packets`
//! with the same demuxer forced is the reference. Packets are aligned per stream, the
//! reader's times are projected into FFmpeg's stream time base and every field the
//! rules compare for the format is compared, in each graded view: the reader's own
//! (`native`) and, for Ogg, its FFmpeg-compatible policy (`compat`). A native
//! difference the compat view does not have is reproduced by the policy. Every other
//! difference must equal the reviewed list of its case and view, and each must fall in
//! the scope of exactly one disposition the case lists; otherwise the view fails.
//! Every case is a row, and the run writes its unexplained differences, attributed
//! where a scope matches, to `OUT.differences.json` for review.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::correctness::file_sha;
use crate::json::{self, Value};
use crate::process::{Stop, run_text};
use crate::{python, sha256};

const RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Differences kept in a row besides their count, per-field counts and digest.
const SHOWN: usize = 8;
/// Alignment table cells beyond which packets are paired in order.
const ALIGN_CELLS: usize = 40_000_000;
const FIELDS: [&str; 7] = [
    "size",
    "position",
    "pts",
    "dts",
    "duration",
    "trim_start",
    "trim_end",
];
/// Difference kinds besides the packet fields: an unpaired packet, the packet counts of
/// a stream only one side has packets in, the set of streams, how reading ended.
const OTHER: [&str; 4] = ["unmatched", "packets", "streams", "end"];
const RULES: &str = "harness/codecs/reader-rules.json";
const DIFFERENCES: &str = "harness/codecs/reader-differences.json";
pub const DIFFERENCES_SCHEMA: &str = "triergon.codecs.reader-differences.v1";
/// The columns of one recorded difference.
const COLUMNS: [&str; 7] = [
    "stream",
    "candidate_packet",
    "reference_packet",
    "field",
    "candidate",
    "reference",
    "disposition",
];

/// The demuxer a reader lane forces on both sides.
pub fn format(lane: &str) -> Option<&'static str> {
    Some(match lane {
        "reader-ogg" => "ogg",
        "reader-flac" => "flac",
        "reader-mp3" => "mp3",
        "reader-aac" => "aac",
        "reader-ac3" => "ac3",
        "reader-eac3" => "eac3",
        _ => return None,
    })
}

/// File name suffixes a reader lane's manifest takes.
pub fn suffixes(lane: &str) -> Option<&'static [&'static str]> {
    Some(match lane {
        "reader-ogg" => &[".ogg", ".oga", ".opus"],
        "reader-flac" => &[".flac"],
        "reader-mp3" => &[".mp1", ".mp2", ".mp3", ".bit"],
        "reader-aac" => &[".aac", ".adts"],
        "reader-ac3" => &[".ac3"],
        "reader-eac3" => &[".eac3", ".ec3"],
        _ => return None,
    })
}

/// A graded view of the reader's packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// What the reader reports.
    Native,
    /// The reader's FFmpeg-compatible policy.
    Compat,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Native => "native",
            View::Compat => "compat",
        }
    }

    fn parse(name: &str) -> Option<View> {
        match name {
            "native" => Some(View::Native),
            "compat" => Some(View::Compat),
            _ => None,
        }
    }
}

/// How a format's reader tracks pair with FFmpeg's streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streams {
    /// The track's position among the tracks of its chain link.
    PositionInLink,
    /// The one track against FFmpeg's first audio stream.
    FirstAudio,
}

/// What one view compares.
#[derive(Clone, Debug, PartialEq)]
struct ViewRules {
    compare: Vec<usize>,
    /// FFmpeg's duration excludes the packet's discard padding.
    duration_excludes_padding: bool,
}

/// What the rules compare for one format.
#[derive(Clone, Debug, PartialEq)]
struct FormatRules {
    streams: Streams,
    /// The packet field both sides' packets are aligned by.
    align_by: usize,
    native: ViewRules,
    compat: Option<ViewRules>,
}

/// Which packets a disposition covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packets {
    /// A difference of the whole reading or of a whole stream: the streams, how
    /// reading ended, the packet counts of a stream only one side has packets in.
    File,
    /// Every paired or unpaired packet.
    Any,
    /// A packet one side has and the other does not.
    Unmatched,
    /// A reference packet that ends before the reader's first packet starts.
    UnmatchedBeforeFirst,
    /// The first packet of a reader track.
    FirstOfTrack,
    /// The first packet of a track of a chained link after the first.
    FirstOfLaterLink,
    /// The last packet of a stream on either side.
    LastOfStream,
    /// The first packet of a track after the reader reported pages of it lost.
    AfterLost,
    /// The first packet after the reader skipped bytes (a bad CRC, garbage).
    AfterResync,
    /// A reader packet the reference does not have.
    UnmatchedCandidate,
    /// A run of packets whose sizes differ or that one side lacks, where both sides'
    /// packets span the same bytes: the same source cut into different packets.
    Resplit,
    /// A paired packet right after an unpaired one.
    AfterUnmatched,
}

impl Packets {
    fn parse(name: &str) -> Option<Packets> {
        Some(match name {
            "file" => Packets::File,
            "any" => Packets::Any,
            "unmatched" => Packets::Unmatched,
            "unmatched-before-first" => Packets::UnmatchedBeforeFirst,
            "first-of-track" => Packets::FirstOfTrack,
            "first-of-later-link" => Packets::FirstOfLaterLink,
            "last-of-stream" => Packets::LastOfStream,
            "after-lost" => Packets::AfterLost,
            "after-resync" => Packets::AfterResync,
            "unmatched-candidate" => Packets::UnmatchedCandidate,
            "resplit" => Packets::Resplit,
            "after-unmatched" => Packets::AfterUnmatched,
            _ => return None,
        })
    }
}

/// How the reference value relates to the reader's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Relation {
    Any,
    /// FFmpeg has no value.
    ReferenceAbsent,
    /// FFmpeg prints the negative value unsigned: reader value + 2^32.
    Unsigned32,
    /// FFmpeg's value is the reader's plus 128 (an ID3v1 tag).
    Plus128,
    /// FFmpeg's time is the reader's plus the durations of the reference packets the
    /// reader does not have before this one.
    PlusUnmatchedDurations,
    /// Every difference of this field in the stream differs by one nonzero offset.
    ConstantOffset,
    /// FFmpeg's value is the reader's in the reader's own time base, unprojected.
    Unprojected,
}

/// The differences a disposition covers.
#[derive(Clone, Debug, PartialEq)]
struct Scope {
    views: Vec<View>,
    fields: Vec<String>,
    packets: Packets,
    reference: Relation,
}

#[derive(Clone, Debug, PartialEq)]
struct Disposition {
    id: String,
    /// The disposition covers a difference any of its scopes covers.
    scopes: Vec<Scope>,
}

/// The reviewed differences of one case and view.
#[derive(Clone, Debug, PartialEq)]
struct Reviewed {
    sha256: String,
    view: View,
    dispositions: Vec<String>,
    difference_sha256: String,
    /// Each difference with the disposition it was reviewed under.
    attributed: Vec<String>,
}

/// `harness/codecs/reader-rules.json` and `harness/codecs/reader-differences.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    formats: Vec<(String, FormatRules)>,
    dispositions: Vec<Disposition>,
    reviewed: Vec<Reviewed>,
}

fn text_list(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

fn field_index(name: &str) -> Option<usize> {
    FIELDS.iter().position(|f| *f == name)
}

fn view_rules(value: &Value, at: &str) -> Result<ViewRules, String> {
    let compare = text_list(value.get("compare"))
        .ok_or_else(|| format!("{at}.compare"))?
        .iter()
        .map(|f| field_index(f).ok_or_else(|| format!("{at}: unknown field {f}")))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(ViewRules {
        compare,
        duration_excludes_padding: value
            .get("reference_duration_excludes_discard_padding")
            .is_some_and(Value::truthy),
    })
}

fn scopes(value: Option<&Value>, id: &str) -> Result<Vec<Scope>, String> {
    match value {
        Some(Value::Array(items)) if !items.is_empty() => {
            items.iter().map(|item| scope(item, id)).collect()
        }
        Some(item @ Value::Object(_)) => Ok(vec![scope(item, id)?]),
        _ => Err(format!("disposition {id} has no scope")),
    }
}

fn scope(value: &Value, id: &str) -> Result<Scope, String> {
    let views = text_list(value.get("views"))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{id}: scope.views"))?
        .iter()
        .map(|v| View::parse(v).ok_or_else(|| format!("{id}: unknown view {v}")))
        .collect::<Result<Vec<_>, String>>()?;
    let fields = text_list(value.get("fields"))
        .filter(|f| !f.is_empty())
        .ok_or_else(|| format!("{id}: scope.fields"))?;
    if let Some(field) = fields
        .iter()
        .find(|f| field_index(f).is_none() && !OTHER.contains(&f.as_str()))
    {
        return Err(format!("{id}: unknown field {field}"));
    }
    let packets = value
        .get("packets")
        .and_then(Value::as_str)
        .and_then(Packets::parse)
        .ok_or_else(|| format!("{id}: scope.packets"))?;
    let reference = match value.get("reference").and_then(Value::as_str) {
        None | Some("any") => Relation::Any,
        Some("absent") => Relation::ReferenceAbsent,
        Some("candidate-plus-2^32") => Relation::Unsigned32,
        Some("candidate-plus-128") => Relation::Plus128,
        Some("candidate-plus-unmatched-durations") => Relation::PlusUnmatchedDurations,
        Some("constant-offset") => Relation::ConstantOffset,
        Some("candidate-unprojected") => Relation::Unprojected,
        Some(other) => return Err(format!("{id}: unknown reference relation {other}")),
    };
    Ok(Scope {
        views,
        fields,
        packets,
        reference,
    })
}

/// The canonical text a difference list is hashed by: its rows without dispositions.
fn list_sha256(rows: &[Value]) -> String {
    sha256::hex(json::canonical(&Value::Array(rows.to_vec())).as_bytes())
}

impl Rules {
    pub fn load(root: &Path) -> Result<Rules, String> {
        let rules = root.join(RULES);
        let differences = root.join(DIFFERENCES);
        Rules::parse(
            &crate::checks::read_json(&rules)?,
            &crate::checks::read_json(&differences)?,
        )
        .map_err(|e| format!("{} / {}: {e}", rules.display(), differences.display()))
    }

    fn parse(document: &Value, differences: &Value) -> Result<Rules, String> {
        let Some(Value::Object(formats)) = document.get("formats") else {
            return Err("no formats".into());
        };
        let formats = formats
            .iter()
            .map(|(name, rules)| {
                let streams = match rules.get("streams").and_then(Value::as_str) {
                    Some("position-in-link") => Streams::PositionInLink,
                    Some("first-audio") => Streams::FirstAudio,
                    _ => return Err(format!("formats.{name}.streams")),
                };
                let align_by = rules
                    .get("align_by")
                    .and_then(Value::as_str)
                    .and_then(field_index)
                    .ok_or_else(|| format!("formats.{name}.align_by"))?;
                let views = rules
                    .get("views")
                    .ok_or_else(|| format!("formats.{name}.views"))?;
                let native = view_rules(
                    views
                        .get("native")
                        .ok_or_else(|| format!("formats.{name}: no native view"))?,
                    &format!("formats.{name}.views.native"),
                )?;
                let compat = views
                    .get("compat")
                    .map(|v| view_rules(v, &format!("formats.{name}.views.compat")))
                    .transpose()?;
                Ok((
                    name.clone(),
                    FormatRules {
                        streams,
                        align_by,
                        native,
                        compat,
                    },
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut dispositions = Vec::new();
        for item in document
            .get("dispositions")
            .and_then(Value::as_array)
            .ok_or("no dispositions")?
        {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .ok_or("disposition id")?;
            if item
                .get("reason")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(format!("disposition {id} has no reason"));
            }
            dispositions.push(Disposition {
                id: id.to_owned(),
                scopes: scopes(item.get("scope"), id)?,
            });
        }
        if differences.get("schema").and_then(Value::as_str) != Some(DIFFERENCES_SCHEMA) {
            return Err(format!("differences schema is not {DIFFERENCES_SCHEMA}"));
        }
        let mut reviewed = Vec::new();
        for item in differences
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("no reviewed cases")?
        {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("reviewed case without {key}"))
            };
            let (sha, name) = (field("sha256")?, field("name")?);
            let view = View::parse(&field("view")?).ok_or("reviewed case with unknown view")?;
            let listed = text_list(item.get("dispositions"))
                .filter(|d| !d.is_empty())
                .ok_or_else(|| format!("{name}: no dispositions"))?;
            if let Some(id) = listed
                .iter()
                .find(|d| !dispositions.iter().any(|known| known.id == **d))
            {
                return Err(format!("{name}: unknown disposition {id}"));
            }
            let rows = item
                .get("differences")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name}: no differences"))?;
            let mut plain = Vec::new();
            let mut attributed = Vec::new();
            for row in rows {
                let cells = row
                    .as_array()
                    .filter(|c| c.len() == COLUMNS.len())
                    .ok_or_else(|| format!("{name}: a difference is not {COLUMNS:?}"))?;
                plain.push(Value::Array(cells[..COLUMNS.len() - 1].to_vec()));
                attributed.push(
                    cells[COLUMNS.len() - 1]
                        .as_str()
                        .ok_or_else(|| format!("{name}: a difference without disposition"))?
                        .to_owned(),
                );
            }
            let difference_sha256 = field("difference_sha256")?;
            if list_sha256(&plain) != difference_sha256 {
                return Err(format!(
                    "{name} ({}): the differences do not hash to difference_sha256",
                    view.name()
                ));
            }
            reviewed.push(Reviewed {
                sha256: sha,
                view,
                dispositions: listed,
                difference_sha256,
                attributed,
            });
        }
        Ok(Rules {
            formats,
            dispositions,
            reviewed,
        })
    }

    fn format(&self, name: &str) -> Result<&FormatRules, String> {
        self.formats
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, rules)| rules)
            .ok_or_else(|| format!("reader-rules.json has no rules for {name}"))
    }
}

/// The arguments of `readers`.
pub struct Options {
    pub manifest: PathBuf,
    pub root: PathBuf,
    pub ffprobe: String,
    pub codec_worker: PathBuf,
    pub out: PathBuf,
    pub only: Option<String>,
    pub rules: Rules,
}

/// One packet's compared fields, `None` where absent.
type Fields = [Option<i128>; 7];
/// A time base as numerator and denominator.
type Ratio = (i128, i128);

const SIZE: usize = 0;
const POSITION: usize = 1;
const PTS: usize = 2;
const DTS: usize = 3;
const DURATION: usize = 4;
const TRIM_START: usize = 5;
const TRIM_END: usize = 6;

/// A reader packet as `codec-worker packets` prints it, with what the scopes ask of it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Emitted {
    native: Fields,
    /// The FFmpeg-compatible view, where the reader has one.
    compat: Option<Fields>,
    time_base: Ratio,
    first_of_track: bool,
    later_link: bool,
    after_lost: bool,
    after_resync: bool,
}

/// The reader's reading: the FFmpeg streams its tracks are, packets per stream, and
/// how reading ended.
#[derive(Debug, Default)]
struct Candidate {
    tracks: Vec<i128>,
    streams: Vec<(i128, Vec<Emitted>)>,
    error: Option<String>,
}

/// The pinned `ffprobe` reading.
#[derive(Debug, Default)]
struct Reference {
    tracks: Vec<i128>,
    streams: Vec<(i128, Vec<Fields>)>,
    time_bases: Vec<(i128, Ratio)>,
    error: Option<String>,
    other_packets: i128,
    code: i64,
    stderr: String,
}

fn entry<T>(streams: &mut Vec<(i128, Vec<T>)>, index: i128) -> &mut Vec<T> {
    if let Some(at) = streams.iter().position(|(i, _)| *i == index) {
        return &mut streams[at].1;
    }
    streams.push((index, Vec::new()));
    &mut streams.last_mut().expect("pushed").1
}

fn packets_of<T>(streams: &[(i128, Vec<T>)], index: i128) -> &[T] {
    streams
        .iter()
        .find(|(s, _)| *s == index)
        .map_or(&[], |(_, p)| p)
}

fn int(value: Option<&Value>) -> Option<i128> {
    value
        .filter(|v| !matches!(v, Value::Null))
        .and_then(python::int)
}

fn ratio(text: &str) -> Option<Ratio> {
    let (n, d) = text.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?)).filter(|&(n, d)| n > 0 && d > 0)
}

fn reference(ffprobe: &str, format: &str, path: &str) -> Result<Reference, Stop> {
    let argv: Vec<String> = [
        ffprobe,
        "-v",
        "error",
        "-f",
        format,
        "-show_entries",
        "stream=index,codec_type,time_base:packet=stream_index,pts,dts,duration,size,pos:packet_side_data=skip_samples,discard_padding",
        "-of",
        "json",
        path,
    ]
    .map(String::from)
    .to_vec();
    let done = run_text(&argv, RUN_TIMEOUT)?;
    let document = python::loads(&done.stdout).unwrap_or(Value::Null);
    let mut reference = Reference {
        code: done.code,
        error: (done.code != 0).then(|| python::tail(&done.stderr, 300)),
        ..Reference::default()
    };
    for stream in document
        .get("streams")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let Some(index) = int(stream.get("index")) else {
            continue;
        };
        if let Some(base) = stream
            .get("time_base")
            .and_then(Value::as_str)
            .and_then(ratio)
        {
            reference.time_bases.push((index, base));
        }
        if stream.get("codec_type").and_then(Value::as_str) == Some("audio") {
            reference.tracks.push(index);
        }
    }
    for packet in document
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let index = int(packet.get("stream_index")).unwrap_or(0);
        if !reference.tracks.contains(&index) {
            reference.other_packets += 1;
            continue;
        }
        let trims = packet
            .get("side_data_list")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .find(|d| d.get("skip_samples").is_some() || d.get("discard_padding").is_some());
        let trim = |key: &str| int(trims.and_then(|d| d.get(key)));
        entry(&mut reference.streams, index).push([
            int(packet.get("size")),
            int(packet.get("pos")),
            int(packet.get("pts")),
            int(packet.get("dts")),
            int(packet.get("duration")),
            trim("skip_samples"),
            trim("discard_padding"),
        ]);
    }
    reference.stderr = done.stderr;
    Ok(reference)
}

fn candidate(
    worker: &Path,
    format: &str,
    path: &str,
    streams: Streams,
    audio: &[i128],
) -> Result<Result<Candidate, String>, Stop> {
    let argv = vec![
        python::path(worker),
        "packets".to_owned(),
        format.to_owned(),
        path.to_owned(),
    ];
    let done = run_text(&argv, RUN_TIMEOUT)?;
    if done.code != 0 {
        return Ok(Err(format!(
            "codec-worker exited {}: {}",
            done.code,
            python::tail(&done.stderr, 300)
        )));
    }
    Ok(parse_candidate(&done.stdout, streams, audio))
}

/// Read `codec-worker packets` output; an error describes output that breaks its format.
fn parse_candidate(stdout: &str, streams: Streams, audio: &[i128]) -> Result<Candidate, String> {
    let lines = python::splitlines(stdout);
    let (last, lines) = lines.split_last().ok_or("codec-worker printed nothing")?;
    let end = python::loads(last).ok_or("codec-worker: the last line is not JSON")?;
    let tracks = end
        .get("tracks")
        .and_then(Value::as_array)
        .ok_or("codec-worker: no track list")?;
    let link = |index: i128| {
        tracks
            .iter()
            .find(|t| int(t.get("index")) == Some(index))
            .and_then(|t| int(t.get("link")))
    };
    // FFmpeg replaces a link's streams in place: the n-th track of every chain link is
    // stream n. An elementary stream's one track is FFmpeg's first audio stream.
    let stream_of = |index: i128| -> Option<i128> {
        match streams {
            Streams::FirstAudio => Some(audio.first().copied().unwrap_or(0)),
            Streams::PositionInLink => {
                let link = link(index);
                tracks
                    .iter()
                    .filter(|t| int(t.get("link")) == link)
                    .position(|t| int(t.get("index")) == Some(index))
                    .map(|p| p as i128)
            }
        }
    };
    let mut mapped: Vec<i128> = tracks
        .iter()
        .filter_map(|t| stream_of(int(t.get("index"))?))
        .collect();
    mapped.sort_unstable();
    mapped.dedup();
    let mut reading = Candidate {
        tracks: mapped,
        error: match end.get("end").and_then(Value::as_str) {
            Some("eof") => None,
            _ => Some(python::str(end.get("error").unwrap_or(&Value::Null))),
        },
        ..Candidate::default()
    };
    let mut seen: Vec<i128> = Vec::new();
    let mut lost: Vec<i128> = Vec::new();
    let mut resync = false;
    for line in lines {
        let packet = python::loads(line).ok_or("codec-worker: a line is not JSON")?;
        match packet.get("event").and_then(Value::as_str) {
            Some("lost") => {
                lost.push(int(packet.get("track")).ok_or("codec-worker: lost without track")?);
                continue;
            }
            Some("resync") => {
                resync = true;
                continue;
            }
            Some(other) => return Err(format!("codec-worker: unknown event {other}")),
            None => {}
        }
        let track = int(packet.get("track")).ok_or("codec-worker: packet without track")?;
        let time_base = packet
            .get("time_base")
            .and_then(Value::as_array)
            .and_then(|b| Some((int(b.first())?, int(b.get(1))?)))
            .ok_or("codec-worker: packet without time base")?;
        let compat = packet.get("compat").map(|c| {
            let pts = int(c.get("pts"));
            let mut fields = [None; 7];
            fields[SIZE] = int(packet.get("size"));
            fields[POSITION] = int(packet.get("position"));
            fields[PTS] = pts;
            // The policy reports one time for presentation and decode.
            fields[DTS] = pts;
            fields[DURATION] = int(c.get("duration"));
            fields[TRIM_START] = int(c.get("skip_samples"));
            fields[TRIM_END] = int(c.get("discard_padding"));
            fields
        });
        let emitted = Emitted {
            native: FIELDS.map(|f| int(packet.get(f))),
            compat,
            time_base,
            first_of_track: !seen.contains(&track),
            later_link: link(track).is_some_and(|l| l > 0),
            after_lost: lost.contains(&track),
            after_resync: std::mem::take(&mut resync),
        };
        seen.push(track);
        lost.retain(|&t| t != track);
        let stream = stream_of(track).unwrap_or(-1 - track);
        entry(&mut reading.streams, stream).push(emitted);
    }
    Ok(reading)
}

/// Ticks of time base `from` in time base `to`, rounded down, and whether that is exact.
fn rescale(ticks: i128, from: Ratio, to: Ratio) -> (i128, bool) {
    let (numerator, denominator) = (ticks * from.0 * to.1, from.1 * to.0);
    (
        numerator.div_euclid(denominator),
        numerator.rem_euclid(denominator) == 0,
    )
}

/// The reader's times in FFmpeg's time base. Where every time converts exactly they are
/// rescaled; otherwise FFmpeg's demuxer rounds each duration down and accumulates them
/// from the first packet's time, and so does the projection.
fn project(packets: &mut [Fields], from: Ratio, to: Ratio) {
    let exact = packets.iter().all(|p| {
        [PTS, DTS, DURATION]
            .iter()
            .all(|&f| p[f].is_none_or(|v| rescale(v, from, to).1))
    });
    let mut clock = packets
        .first()
        .and_then(|p| p[PTS])
        .map_or(0, |v| rescale(v, from, to).0);
    for packet in packets {
        let length = packet[DURATION].map(|d| rescale(d, from, to).0);
        if exact {
            for f in [PTS, DTS] {
                packet[f] = packet[f].map(|v| rescale(v, from, to).0);
            }
        } else {
            packet[PTS] = Some(clock);
            packet[DTS] = packet[DTS].map(|_| clock);
            clock += length.unwrap_or(0);
        }
        packet[DURATION] = length;
    }
}

/// Packets as the alignment sees them: the aligned field and the size.
type Key = [Option<i128>; 2];
/// One alignment step: a reader packet, a reference packet, or both paired.
type Pair = (Option<usize>, Option<usize>);

/// The in-order alignment of two packet sequences at the least cost: a pair costs the
/// number of its keys that differ, an unpaired packet 1, and of equal costs a pair is
/// preferred, then an unpaired reader packet.
fn align(a: &[Key], b: &[Key]) -> Vec<Pair> {
    let (n, m) = (a.len(), b.len());
    if n == m && a == b || (n + 1) * (m + 1) > ALIGN_CELLS {
        let mut pairs: Vec<_> = (0..n.min(m)).map(|i| (Some(i), Some(i))).collect();
        pairs.extend((m..n).map(|i| (Some(i), None)));
        pairs.extend((n..m).map(|j| (None, Some(j))));
        return pairs;
    }
    // cost[i][j]: the alignment of a[i..] with b[j..]; step 0 pairs, 1 leaves a[i]
    // unpaired, 2 leaves b[j] unpaired.
    let width = m + 1;
    let mut cost = vec![0u32; (n + 1) * width];
    let mut step = vec![0u8; (n + 1) * width];
    for i in (0..=n).rev() {
        for j in (0..=m).rev() {
            let at = i * width + j;
            let mut best = (u32::MAX, 0u8);
            if i < n && j < m {
                let unequal = a[i].iter().zip(&b[j]).filter(|(x, y)| x != y).count();
                let pair = cost[(i + 1) * width + j + 1] + unequal as u32;
                best = (pair, 0);
            }
            if i < n && cost[(i + 1) * width + j] + 1 < best.0 {
                best = (cost[(i + 1) * width + j] + 1, 1);
            }
            if j < m && cost[i * width + j + 1] + 1 < best.0 {
                best = (cost[i * width + j + 1] + 1, 2);
            }
            if best.0 != u32::MAX {
                (cost[at], step[at]) = best;
            }
        }
    }
    let (mut i, mut j, mut pairs) = (0, 0, Vec::new());
    while i < n || j < m {
        match step[i * width + j] {
            0 => {
                pairs.push((Some(i), Some(j)));
                (i, j) = (i + 1, j + 1);
            }
            1 => {
                pairs.push((Some(i), None));
                i += 1;
            }
            _ => {
                pairs.push((None, Some(j)));
                j += 1;
            }
        }
    }
    pairs
}

/// One difference between the two readings.
#[derive(Clone, Debug, PartialEq)]
struct Difference {
    stream: Option<i128>,
    candidate: Option<usize>,
    reference: Option<usize>,
    field: &'static str,
    ours: Value,
    theirs: Value,
}

fn optional<T: Into<i128>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, |v| Value::int(v))
}

impl Difference {
    fn row(&self) -> Value {
        Value::Array(vec![
            optional(self.stream),
            optional(self.candidate.map(|c| c as i128)),
            optional(self.reference.map(|r| r as i128)),
            Value::str(self.field),
            self.ours.clone(),
            self.theirs.clone(),
        ])
    }

    fn key(&self) -> (Option<i128>, Option<usize>, Option<usize>, &'static str) {
        (self.stream, self.candidate, self.reference, self.field)
    }
}

/// Both readings, aligned per stream and with the reader's times in FFmpeg's base.
struct Comparison<'a> {
    candidate: &'a Candidate,
    reference: &'a Reference,
    streams: Vec<Aligned>,
}

/// One stream: the alignment and the reader's projected native and compat fields.
struct Aligned {
    stream: i128,
    alignment: Vec<Pair>,
    native: Vec<Fields>,
    compat: Vec<Fields>,
}

impl<'a> Comparison<'a> {
    fn new(candidate: &'a Candidate, reference: &'a Reference, rules: &FormatRules) -> Self {
        let mut indices: Vec<i128> = candidate
            .streams
            .iter()
            .map(|(s, _)| *s)
            .chain(reference.streams.iter().map(|(s, _)| *s))
            .collect();
        indices.sort_unstable();
        indices.dedup();
        let streams = indices
            .into_iter()
            .map(|stream| {
                let ours = packets_of(&candidate.streams, stream);
                let theirs = packets_of(&reference.streams, stream);
                let to = reference
                    .time_bases
                    .iter()
                    .find(|(s, _)| *s == stream)
                    .map(|(_, b)| *b);
                let projected = |view: fn(&Emitted) -> Option<Fields>| {
                    let mut fields: Vec<Fields> = ours.iter().filter_map(view).collect();
                    if let (Some(first), Some(to)) = (ours.first(), to) {
                        project(&mut fields, first.time_base, to);
                    }
                    fields
                };
                let native = projected(|p| Some(p.native));
                let compat = projected(|p| p.compat);
                let keys = |packets: &[Fields]| -> Vec<Key> {
                    packets
                        .iter()
                        .map(|p| [p[rules.align_by], p[SIZE]])
                        .collect()
                };
                let alignment = align(&keys(&native), &keys(theirs));
                Aligned {
                    stream,
                    alignment,
                    native,
                    compat,
                }
            })
            .collect();
        Comparison {
            candidate,
            reference,
            streams,
        }
    }

    /// Every difference of `view` under `rules`, in a canonical order.
    fn differences(&self, view: View, rules: &ViewRules) -> Vec<Difference> {
        let mut out = Vec::new();
        for Aligned {
            stream,
            alignment,
            native,
            compat,
        } in &self.streams
        {
            let ours = match view {
                View::Native => native,
                View::Compat => compat,
            };
            let theirs = packets_of(&self.reference.streams, *stream);
            // A stream only one side has packets in is one difference, of the counts.
            if ours.is_empty() != theirs.is_empty() {
                out.push(Difference {
                    stream: Some(*stream),
                    candidate: None,
                    reference: None,
                    field: "packets",
                    ours: Value::int(ours.len() as i128),
                    theirs: Value::int(theirs.len() as i128),
                });
                continue;
            }
            for &(c, r) in alignment {
                let difference = |field, ours, theirs| Difference {
                    stream: Some(*stream),
                    candidate: c,
                    reference: r,
                    field,
                    ours,
                    theirs,
                };
                let (a, b) = match (c.and_then(|c| ours.get(c)), r.map(|r| theirs[r])) {
                    (Some(a), Some(b)) => (*a, b),
                    (a, b) => {
                        out.push(difference(
                            "unmatched",
                            optional(a.and_then(|p| p[SIZE])),
                            optional(b.and_then(|p| p[SIZE])),
                        ));
                        continue;
                    }
                };
                let mut a = a;
                let mut b = b;
                // An absent trim is a trim of zero on both sides.
                for f in [TRIM_START, TRIM_END] {
                    a[f] = a[f].filter(|&v| v != 0);
                    b[f] = b[f].filter(|&v| v != 0);
                }
                if rules.duration_excludes_padding
                    && let (Some(d), Some(p)) = (b[DURATION], b[TRIM_END])
                {
                    b[DURATION] = Some(d + p);
                }
                for &f in &rules.compare {
                    if a[f] != b[f] {
                        out.push(difference(FIELDS[f], optional(a[f]), optional(b[f])));
                    }
                }
            }
        }
        if self.candidate.tracks != self.reference.tracks {
            let streams =
                |tracks: &[i128]| Value::Array(tracks.iter().map(|&s| Value::int(s)).collect());
            out.push(Difference {
                stream: None,
                candidate: None,
                reference: None,
                field: "streams",
                ours: streams(&self.candidate.tracks),
                theirs: streams(&self.reference.tracks),
            });
        }
        let end =
            |error: &Option<String>| Value::str(if error.is_some() { "error" } else { "eof" });
        let (ours, theirs) = (end(&self.candidate.error), end(&self.reference.error));
        if !crate::harness::diff::equal(&ours, &theirs) {
            out.push(Difference {
                stream: None,
                candidate: None,
                reference: None,
                field: "end",
                ours,
                theirs,
            });
        }
        out
    }

    /// The alignment of `stream` and the position in it of `d`'s packets.
    fn placed(&self, stream: i128, d: &Difference) -> Option<(&[Pair], usize)> {
        let alignment = &self.streams.iter().find(|s| s.stream == stream)?.alignment;
        let at = alignment
            .iter()
            .position(|&pair| pair == (d.candidate, d.reference))?;
        Some((alignment, at))
    }

    /// Whether the packets around `at` that one side lacks or that differ in size span
    /// the same bytes on both sides.
    fn resplit(&self, stream: i128, alignment: &[Pair], at: usize) -> bool {
        let ours = packets_of(&self.candidate.streams, stream);
        let theirs = packets_of(&self.reference.streams, stream);
        let span = |p: &Fields| p[POSITION].zip(p[SIZE]).map(|(at, size)| (at, at + size));
        let broken = |&(c, r): &Pair| match (c, r) {
            (Some(c), Some(r)) => ours[c].native[SIZE] != theirs[r][SIZE],
            _ => true,
        };
        if !broken(&alignment[at]) {
            return false;
        }
        let mut lo = at;
        while lo > 0 && broken(&alignment[lo - 1]) {
            lo -= 1;
        }
        let mut hi = at;
        while hi + 1 < alignment.len() && broken(&alignment[hi + 1]) {
            hi += 1;
        }
        let extent = |spans: Vec<Option<(i128, i128)>>| -> Option<(i128, i128)> {
            let spans: Option<Vec<(i128, i128)>> = spans.into_iter().collect();
            let spans = spans.filter(|s| !s.is_empty())?;
            Some((
                spans.iter().map(|s| s.0).min()?,
                spans.iter().map(|s| s.1).max()?,
            ))
        };
        let run = &alignment[lo..=hi];
        let candidate = extent(
            run.iter()
                .filter_map(|(c, _)| c.map(|c| span(&ours[c].native)))
                .collect(),
        );
        let reference = extent(
            run.iter()
                .filter_map(|(_, r)| r.map(|r| span(&theirs[r])))
                .collect(),
        );
        candidate.is_some() && candidate == reference
    }

    /// Whether `scope` covers difference `d` of `view`, among all of `found`.
    fn covers(&self, scope: &Scope, view: View, d: &Difference, found: &[Difference]) -> bool {
        if !scope.views.contains(&view) || !scope.fields.iter().any(|f| f == d.field) {
            return false;
        }
        let (ours_value, theirs_value) = (python::int(&d.ours), python::int(&d.theirs));
        let offset = ours_value.zip(theirs_value).map(|(o, t)| t - o);
        let relation = match scope.reference {
            Relation::Any => true,
            Relation::ReferenceAbsent => matches!(d.theirs, Value::Null),
            Relation::Unsigned32 => offset == Some(1 << 32),
            Relation::Plus128 => offset == Some(128),
            Relation::ConstantOffset => {
                offset.is_some_and(|o| o != 0)
                    && found
                        .iter()
                        .filter(|e| e.stream == d.stream && e.field == d.field)
                        .all(|e| {
                            python::int(&e.ours)
                                .zip(python::int(&e.theirs))
                                .map(|(o, t)| t - o)
                                == offset
                        })
            }
            Relation::Unprojected => d.stream.zip(d.candidate).is_some_and(|(stream, c)| {
                let raw = packets_of(&self.candidate.streams, stream)
                    .get(c)
                    .and_then(|p| field_index(d.field).and_then(|f| p.native[f]));
                raw.is_some() && raw == theirs_value
            }),
            Relation::PlusUnmatchedDurations => {
                let skipped = d.stream.and_then(|stream| {
                    let (alignment, at) = self.placed(stream, d)?;
                    let theirs = packets_of(&self.reference.streams, stream);
                    let mut total = 0;
                    for &(c, r) in &alignment[..at] {
                        if let (None, Some(r)) = (c, r) {
                            total += theirs[r][DURATION]?;
                        }
                    }
                    Some(total)
                });
                skipped.is_some_and(|total| total > 0 && offset == Some(total))
            }
        };
        if !relation {
            return false;
        }
        if d.candidate.is_none() && d.reference.is_none() {
            return scope.packets == Packets::File;
        }
        let Some(stream) = d.stream else {
            return false;
        };
        let ours = packets_of(&self.candidate.streams, stream);
        let theirs = packets_of(&self.reference.streams, stream);
        let emitted = d.candidate.and_then(|c| ours.get(c));
        let flag = |test: fn(&Emitted) -> bool| emitted.is_some_and(test);
        match scope.packets {
            Packets::File => false,
            Packets::Any => true,
            Packets::Unmatched => d.field == "unmatched",
            Packets::UnmatchedBeforeFirst => {
                let first = ours.first().and_then(|p| p.native[POSITION]);
                let packet = d.reference.map(|r| theirs[r]);
                d.field == "unmatched"
                    && d.candidate.is_none()
                    && match (packet.and_then(|p| p[POSITION].zip(p[SIZE])), first) {
                        (Some((position, size)), Some(first)) => position + size <= first,
                        _ => false,
                    }
            }
            Packets::FirstOfTrack => flag(|p| p.first_of_track),
            Packets::FirstOfLaterLink => flag(|p| p.first_of_track && p.later_link),
            Packets::LastOfStream => {
                d.candidate.is_some_and(|c| c + 1 == ours.len())
                    || d.reference.is_some_and(|r| r + 1 == theirs.len())
            }
            Packets::AfterLost => flag(|p| p.after_lost),
            Packets::AfterResync => flag(|p| p.after_resync),
            Packets::UnmatchedCandidate => d.field == "unmatched" && d.reference.is_none(),
            Packets::Resplit => self
                .placed(stream, d)
                .is_some_and(|(alignment, at)| self.resplit(stream, alignment, at)),
            Packets::AfterUnmatched => self.placed(stream, d).is_some_and(|(alignment, at)| {
                d.candidate.is_some()
                    && d.reference.is_some()
                    && at > 0
                    && matches!(alignment[at - 1], (None, _) | (_, None))
            }),
        }
    }
}

/// The result of attributing one view's unexplained differences.
struct Attribution {
    /// The disposition of each difference that exactly one listed scope covers.
    each: Vec<Option<String>>,
    unattributed: usize,
    ambiguous: usize,
}

fn attribute(
    comparison: &Comparison<'_>,
    view: View,
    found: &[Difference],
    dispositions: &[&Disposition],
) -> Attribution {
    let mut result = Attribution {
        each: Vec::with_capacity(found.len()),
        unattributed: 0,
        ambiguous: 0,
    };
    for d in found {
        let covering: Vec<&str> = dispositions
            .iter()
            .filter(|disposition| {
                disposition
                    .scopes
                    .iter()
                    .any(|scope| comparison.covers(scope, view, d, found))
            })
            .map(|disposition| disposition.id.as_str())
            .collect();
        match covering.as_slice() {
            [one] => result.each.push(Some((*one).to_owned())),
            [] => {
                result.unattributed += 1;
                result.each.push(None);
            }
            _ => {
                result.ambiguous += 1;
                result.each.push(None);
            }
        }
    }
    result
}

fn field_counts(found: &[Difference]) -> Value {
    let mut counts: Vec<(&str, i128)> = Vec::new();
    for d in found {
        match counts.iter_mut().find(|(k, _)| *k == d.field) {
            Some((_, n)) => *n += 1,
            None => counts.push((d.field, 1)),
        }
    }
    Value::object(counts.into_iter().map(|(k, n)| (k, Value::int(n))))
}

/// One view's grade, and its unexplained differences proposed for review.
struct Graded {
    row: Value,
    status: &'static str,
    proposal: Option<Value>,
}

fn grade_view(
    rules: &Rules,
    comparison: &Comparison<'_>,
    view: View,
    found: &[Difference],
    reproduced: usize,
    case: (&str, &str),
) -> Graded {
    let (name, sha) = case;
    let rows: Vec<Value> = found.iter().map(Difference::row).collect();
    let digest = list_sha256(&rows);
    let mut row = vec![
        ("differences".to_owned(), Value::int(found.len() as i128)),
        ("difference_fields".to_owned(), field_counts(found)),
        (
            "first_differences".to_owned(),
            Value::Array(rows.iter().take(SHOWN).cloned().collect()),
        ),
        ("difference_sha256".to_owned(), Value::str(digest.as_str())),
    ];
    if view == View::Native
        && comparison
            .candidate
            .streams
            .iter()
            .any(|(_, p)| p.iter().any(|p| p.compat.is_some()))
    {
        row.push((
            "reproduced_by_compat".into(),
            Value::int(reproduced as i128),
        ));
    }
    if found.is_empty() {
        let status = if reproduced > 0 {
            "reproduced-by-compat"
        } else if comparison.candidate.tracks.is_empty() && comparison.candidate.error.is_some() {
            "refused-by-both"
        } else {
            "pass"
        };
        row.push(("status".into(), Value::str(status)));
        return Graded {
            row: Value::Object(row),
            status,
            proposal: None,
        };
    }
    let reviewed = rules
        .reviewed
        .iter()
        .find(|r| r.sha256 == sha && r.view == view);
    // The proposal attributes among the reviewed case's dispositions, or among all of
    // them for differences not reviewed yet (where scopes may overlap: unattributed).
    let considered: Vec<&Disposition> = rules
        .dispositions
        .iter()
        .filter(|d| {
            reviewed
                .filter(|r| r.difference_sha256 == digest)
                .is_none_or(|r| r.dispositions.contains(&d.id))
        })
        .collect();
    let proposed = attribute(comparison, view, found, &considered);
    let proposal = Value::object([
        ("name", Value::str(name)),
        ("sha256", Value::str(sha)),
        ("view", Value::str(view.name())),
        ("dispositions", {
            let mut ids: Vec<&str> = proposed.each.iter().flatten().map(String::as_str).collect();
            ids.sort_unstable();
            ids.dedup();
            Value::strings(ids)
        }),
        ("difference_sha256", Value::str(digest.as_str())),
        (
            "differences",
            Value::Array(
                rows.iter()
                    .zip(&proposed.each)
                    .map(|(r, d)| {
                        let mut cells = r.as_array().unwrap_or_default().to_vec();
                        cells.push(d.as_deref().map_or(Value::Null, Value::str));
                        Value::Array(cells)
                    })
                    .collect(),
            ),
        ),
    ]);
    let mut problems = Vec::new();
    match reviewed {
        None => problems.push("no reviewed differences".to_owned()),
        Some(reviewed) if reviewed.difference_sha256 != digest => {
            row.push((
                "reviewed_difference_sha256".into(),
                Value::str(reviewed.difference_sha256.as_str()),
            ));
            problems.push("the differences are not the reviewed ones".to_owned());
        }
        Some(reviewed) => {
            let listed: Vec<&Disposition> = rules
                .dispositions
                .iter()
                .filter(|d| reviewed.dispositions.contains(&d.id))
                .collect();
            let attribution = attribute(comparison, view, found, &listed);
            if attribution.unattributed > 0 {
                problems.push(format!(
                    "{} differences in no listed disposition's scope",
                    attribution.unattributed
                ));
            }
            if attribution.ambiguous > 0 {
                problems.push(format!(
                    "{} differences in more than one listed scope",
                    attribution.ambiguous
                ));
            }
            for id in &reviewed.dispositions {
                if !attribution.each.iter().flatten().any(|d| d == id) {
                    problems.push(format!("{id} covers none of the differences"));
                }
            }
            if attribution
                .each
                .iter()
                .zip(&reviewed.attributed)
                .any(|(found, reviewed)| found.as_deref() != Some(reviewed.as_str()))
                && attribution.unattributed + attribution.ambiguous == 0
            {
                problems.push("a difference is attributed other than reviewed".to_owned());
            }
            row.push((
                "dispositions".into(),
                Value::strings(reviewed.dispositions.clone()),
            ));
        }
    }
    let status = if problems.is_empty() {
        "disposition"
    } else {
        row.push(("problems".into(), Value::strings(problems)));
        "fail"
    };
    row.push(("status".into(), Value::str(status)));
    Graded {
        row: Value::Object(row),
        status,
        proposal: Some(proposal),
    }
}

/// One case's row fields, overall status and review proposals.
struct CaseGrade {
    fields: Vec<(String, Value)>,
    status: String,
    proposals: Vec<Value>,
}

fn grade_case(
    options: &Options,
    format: &str,
    path: &str,
    case: (&str, &str),
) -> Result<CaseGrade, Stop> {
    let rules = options.rules.format(format)?;
    let theirs = reference(&options.ffprobe, format, path)?;
    let mut fields = vec![
        ("ffprobe_rc".to_owned(), Value::int(theirs.code)),
        (
            "ffprobe_stderr_tail".to_owned(),
            Value::str(python::tail(&theirs.stderr, 300)),
        ),
        (
            "reference_other_packets".to_owned(),
            Value::int(theirs.other_packets),
        ),
    ];
    let ours = match candidate(
        &options.codec_worker,
        format,
        path,
        rules.streams,
        &theirs.tracks,
    )? {
        Ok(reading) => reading,
        Err(error) => {
            fields.push(("candidate_error".into(), Value::str(error)));
            return Ok(CaseGrade {
                fields,
                status: "fail".into(),
                proposals: Vec::new(),
            });
        }
    };
    let count = |streams: Vec<(i128, usize)>| {
        Value::Array(
            streams
                .into_iter()
                .map(|(s, n)| Value::Array(vec![Value::int(s), Value::int(n as i128)]))
                .collect(),
        )
    };
    fields.push((
        "candidate_packets".into(),
        count(ours.streams.iter().map(|(s, p)| (*s, p.len())).collect()),
    ));
    fields.push((
        "reference_packets".into(),
        count(theirs.streams.iter().map(|(s, p)| (*s, p.len())).collect()),
    ));
    fields.push((
        "candidate_end".into(),
        ours.error.clone().map_or(Value::str("eof"), Value::str),
    ));
    let comparison = Comparison::new(&ours, &theirs, rules);
    let native = comparison.differences(View::Native, &rules.native);
    let compat = rules
        .compat
        .as_ref()
        .map(|view| comparison.differences(View::Compat, view));
    // A native difference the compat view does not have, the policy reproduces.
    let (unexplained, reproduced): (Vec<Difference>, Vec<Difference>) = match &compat {
        Some(compat) => native
            .into_iter()
            .partition(|d| compat.iter().any(|c| c.key() == d.key())),
        None => (native, Vec::new()),
    };
    let mut views = vec![(
        View::Native,
        grade_view(
            &options.rules,
            &comparison,
            View::Native,
            &unexplained,
            reproduced.len(),
            case,
        ),
    )];
    if let Some(compat) = &compat {
        views.push((
            View::Compat,
            grade_view(&options.rules, &comparison, View::Compat, compat, 0, case),
        ));
    }
    let rank = |status: &str| match status {
        "fail" => 3,
        "disposition" => 2,
        "reproduced-by-compat" => 1,
        _ => 0,
    };
    let status = views
        .iter()
        .map(|(_, g)| g.status)
        .max_by_key(|s| rank(s))
        .unwrap_or("fail")
        .to_owned();
    let proposals = views
        .iter()
        .filter_map(|(_, g)| g.proposal.clone())
        .collect();
    fields.push((
        "views".into(),
        Value::Object(
            views
                .into_iter()
                .map(|(view, g)| (view.name().to_owned(), g.row))
                .collect(),
        ),
    ));
    Ok(CaseGrade {
        fields,
        status,
        proposals,
    })
}

fn view_status(row: &Value, view: &str) -> Option<String> {
    row.get("views")
        .and_then(|v| v.get(view))
        .and_then(|v| v.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Compare every reader row of the manifest (or those `--only` selects), appending one
/// JSON row per input to `--out` and printing one line each, then the status counts
/// per lane and view; the unexplained differences go to `OUT.differences.json`.
pub fn run(options: &Options) -> Result<(), String> {
    let manifest = crate::checks::read_json(&options.manifest)?;
    let vectors = manifest
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("manifest has no vectors")?;
    let only: Option<Vec<&str>> = options.only.as_deref().map(|o| o.split(',').collect());
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.out)
        .map_err(|e| format!("{}: {e}", options.out.display()))?;
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut proposals = Vec::new();
    for v in vectors {
        let text = |key: &str| -> Result<String, String> {
            v.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("vector without {key}"))
        };
        let lane = text("lane")?;
        let Some(format) = format(&lane) else {
            continue;
        };
        let (name, relative, sha) = (text("name")?, text("path")?, text("sha256")?);
        if let Some(only) = &only
            && !only.iter().any(|o| name.contains(o) || *o == lane)
        {
            continue;
        }
        let path = python::path(&options.root.join(&relative));
        let mut row = vec![
            ("lane".to_owned(), Value::str(lane.as_str())),
            ("name".to_owned(), Value::str(name.as_str())),
            ("path".to_owned(), Value::str(relative.as_str())),
            ("sha256".to_owned(), Value::str(sha.as_str())),
        ];
        let status = if !Path::new(&path).is_file() {
            "input-missing".to_owned()
        } else if file_sha(Path::new(&path))? != sha {
            "input-hash-mismatch".to_owned()
        } else {
            match grade_case(options, format, &path, (&name, &sha)) {
                Ok(graded) => {
                    row.extend(graded.fields);
                    proposals.extend(graded.proposals);
                    graded.status
                }
                Err(Stop::Timeout(argv)) => {
                    row.push(("argv".into(), Value::strings(argv)));
                    "timeout".to_owned()
                }
                Err(Stop::Fatal(message)) => return Err(message),
            }
        };
        row.push(("status".into(), Value::str(status.as_str())));
        let row = Value::Object(row);
        writeln!(out, "{}", json::to_python(&row, None))
            .and_then(|()| out.flush())
            .map_err(|e| format!("{}: {e}", options.out.display()))?;
        let views: Vec<String> = ["native", "compat"]
            .iter()
            .filter_map(|view| {
                let status = view_status(&row, view)?;
                let ids = row
                    .get("views")
                    .and_then(|v| v.get(view))
                    .and_then(|v| v.get("dispositions"))
                    .and_then(Value::as_array)
                    .map(|d| d.iter().map(python::str).collect::<Vec<_>>().join(","))
                    .unwrap_or_default();
                Some(format!(
                    "{view}={status}{}{ids}",
                    if ids.is_empty() { "" } else { ":" }
                ))
            })
            .collect();
        println!("{lane} {name} {status} {}", views.join(" "));
        let mut keys = vec![format!("{lane} {status}")];
        for view in ["native", "compat"] {
            if let Some(status) = view_status(&row, view) {
                keys.push(format!("{lane} {view} {status}"));
            }
        }
        for key in keys {
            match counts.iter_mut().find(|(k, _)| *k == key) {
                Some((_, n)) => *n += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    let record = options.out.with_extension("differences.json");
    let document = Value::object([
        ("schema", Value::str(DIFFERENCES_SCHEMA)),
        ("columns", Value::strings(COLUMNS)),
        ("cases", Value::Array(proposals)),
    ]);
    fs::write(&record, differences_text(&document))
        .map_err(|e| format!("{}: {e}", record.display()))?;
    counts.sort();
    for (key, n) in &counts {
        println!("COUNT {key} {n}");
    }
    println!("DONE");
    Ok(())
}

/// A differences document with one case member and one difference per line.
pub fn differences_text(document: &Value) -> String {
    let line = |value: &Value| json::to_python(value, None);
    let mut out = String::from("{\n");
    for key in ["schema", "columns"] {
        let value = document.get(key).unwrap_or(&Value::Null);
        out.push_str(&format!(" {}: {},\n", line(&Value::str(key)), line(value)));
    }
    out.push_str(" \"cases\": [");
    let cases = document
        .get("cases")
        .and_then(Value::as_array)
        .unwrap_or_default();
    for (at, case) in cases.iter().enumerate() {
        out.push_str(if at == 0 { "\n  {\n" } else { ",\n  {\n" });
        let Value::Object(members) = case else {
            continue;
        };
        let mut first = true;
        for (key, value) in members {
            out.push_str(if first { "   " } else { ",\n   " });
            first = false;
            out.push_str(&line(&Value::str(key.as_str())));
            out.push_str(": ");
            match (key.as_str(), value) {
                ("differences", Value::Array(rows)) => {
                    out.push('[');
                    for (i, row) in rows.iter().enumerate() {
                        out.push_str(if i == 0 { "\n    " } else { ",\n    " });
                        out.push_str(&line(row));
                    }
                    out.push_str("\n   ]");
                }
                _ => out.push_str(&line(value)),
            }
        }
        out.push_str("\n  }");
    }
    out.push_str(if cases.is_empty() {
        "]\n}\n"
    } else {
        "\n ]\n}\n"
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pts: i128) -> Fields {
        [None, None, Some(pts), Some(pts), Some(1536), None, None]
    }

    #[test]
    fn projection_accumulates_rounded_durations_where_inexact() {
        // AC-3 at 44.1 kHz: 1536 samples are 3134.69 ticks of 1/90000.
        let mut packets = vec![packet(0), packet(1536), packet(3072)];
        project(&mut packets, (1, 44100), (1, 90000));
        let times: Vec<_> = packets.iter().map(|p| (p[PTS], p[DURATION])).collect();
        assert_eq!(
            times,
            [
                (Some(0), Some(3134)),
                (Some(3134), Some(3134)),
                (Some(6268), Some(3134))
            ]
        );
        // 1152 and 1536 samples at 44.1 kHz are exactly 368640 and 491520 ticks of 1/14112000.
        let mut packets = vec![packet(-1152), packet(0)];
        project(&mut packets, (1, 44100), (1, 14_112_000));
        assert_eq!(packets[0][PTS], Some(-368_640));
        assert_eq!(packets[1][DURATION], Some(491_520));
    }

    #[test]
    fn alignment_pairs_unequal_keys_before_leaving_packets_unpaired() {
        let k = |v: &[i128]| v.iter().map(|&x| [Some(x), Some(1)]).collect::<Vec<_>>();
        // A leading reference packet, then equal positions.
        assert_eq!(
            align(&k(&[46, 880]), &k(&[0, 46, 880])),
            [(None, Some(0)), (Some(0), Some(1)), (Some(1), Some(2))]
        );
        // Shifted positions pair in order.
        assert_eq!(
            align(&k(&[10, 20]), &k(&[5, 15])),
            [(Some(0), Some(0)), (Some(1), Some(1))]
        );
        // Four reader packets one reference packet holds.
        assert_eq!(
            align(&k(&[0, 9, 10, 11, 12, 20]), &k(&[0, 9, 20])),
            [
                (Some(0), Some(0)),
                (Some(1), Some(1)),
                (Some(2), None),
                (Some(3), None),
                (Some(4), None),
                (Some(5), Some(2))
            ]
        );
    }

    fn reading(fields: Vec<Fields>, compat: Option<Vec<Fields>>) -> Candidate {
        let packets = fields
            .iter()
            .enumerate()
            .map(|(i, f)| Emitted {
                native: *f,
                compat: compat.as_ref().map(|c| c[i]),
                time_base: (1, 48000),
                first_of_track: i == 0,
                ..Emitted::default()
            })
            .collect();
        Candidate {
            tracks: vec![0],
            streams: vec![(0, packets)],
            error: None,
        }
    }

    fn reference_of(fields: Vec<Fields>) -> Reference {
        Reference {
            tracks: vec![0],
            streams: vec![(0, fields)],
            time_bases: vec![(0, (1, 48000))],
            ..Reference::default()
        }
    }

    fn format_rules() -> FormatRules {
        let all = vec![SIZE, PTS, DTS, DURATION, TRIM_START, TRIM_END];
        FormatRules {
            streams: Streams::PositionInLink,
            align_by: SIZE,
            native: ViewRules {
                compare: all.clone(),
                duration_excludes_padding: true,
            },
            compat: Some(ViewRules {
                compare: all,
                duration_excludes_padding: false,
            }),
        }
    }

    #[test]
    fn native_differences_the_compat_view_reproduces_are_set_apart() {
        // The last Opus packet: 960 samples, 936 of them padding; FFmpeg reports 24.
        let ffmpeg = vec![[
            Some(9),
            None,
            Some(0),
            Some(0),
            Some(24),
            Some(0),
            Some(936),
        ]];
        let native = vec![[Some(9), None, Some(0), Some(0), Some(960), None, Some(936)]];
        let ours = reading(native, Some(ffmpeg.clone()));
        let theirs = reference_of(ffmpeg);
        let rules = format_rules();
        let comparison = Comparison::new(&ours, &theirs, &rules);
        assert!(
            comparison
                .differences(View::Native, &rules.native)
                .is_empty()
        );
        assert!(
            comparison
                .differences(View::Compat, rules.compat.as_ref().unwrap())
                .is_empty()
        );
        // A late native time the compat view gets right.
        let late = vec![[Some(9), None, Some(5), Some(5), Some(960), None, Some(936)]];
        let ffmpeg = vec![[
            Some(9),
            None,
            Some(0),
            Some(0),
            Some(24),
            Some(0),
            Some(936),
        ]];
        let ours = reading(late, Some(ffmpeg.clone()));
        let theirs = reference_of(ffmpeg);
        let comparison = Comparison::new(&ours, &theirs, &rules);
        let native = comparison.differences(View::Native, &rules.native);
        assert_eq!(
            json::canonical(&Value::Array(native.iter().map(Difference::row).collect())),
            r#"[[0,0,0,"pts",5,0],[0,0,0,"dts",5,0]]"#
        );
        assert!(
            comparison
                .differences(View::Compat, rules.compat.as_ref().unwrap())
                .is_empty()
        );
    }

    #[test]
    fn scopes_cover_only_their_fields_packets_and_values() {
        let rules = format_rules();
        let ours = reading(
            vec![
                [Some(1), None, Some(0), Some(0), Some(10), None, None],
                [Some(2), None, Some(10), Some(10), Some(-4), None, None],
            ],
            None,
        );
        let theirs = reference_of(vec![
            [Some(1), None, Some(0), Some(0), Some(10), None, None],
            [
                Some(2),
                None,
                Some(10),
                Some(10),
                Some((1 << 32) - 4),
                None,
                None,
            ],
        ]);
        let comparison = Comparison::new(&ours, &theirs, &rules);
        let found = comparison.differences(View::Native, &rules.native);
        assert_eq!(found.len(), 1);
        let scope = |fields: &[&str], packets, reference| Scope {
            views: vec![View::Native],
            fields: fields.iter().map(|f| (*f).to_owned()).collect(),
            packets,
            reference,
        };
        let covers = |s: &Scope| comparison.covers(s, View::Native, &found[0], &found);
        assert!(covers(&scope(
            &["duration"],
            Packets::LastOfStream,
            Relation::Unsigned32
        )));
        assert!(!covers(&scope(
            &["pts"],
            Packets::LastOfStream,
            Relation::Unsigned32
        )));
        assert!(!covers(&scope(
            &["duration"],
            Packets::FirstOfTrack,
            Relation::Any
        )));
        assert!(!covers(&scope(
            &["duration"],
            Packets::Any,
            Relation::ReferenceAbsent
        )));
        let mut compat_only = scope(&["duration"], Packets::Any, Relation::Any);
        compat_only.views = vec![View::Compat];
        assert!(!covers(&compat_only));
    }

    #[test]
    fn resplit_and_shifted_times_cover_one_source_cut_differently() {
        // Bytes 0..30 are one reader packet and two reference packets of 10 samples
        // each; the next frame is paired, timed one reference packet later.
        let packet = |position, size, pts| -> Fields {
            [
                Some(size),
                Some(position),
                Some(pts),
                Some(pts),
                Some(10),
                None,
                None,
            ]
        };
        let mut rules = format_rules();
        rules.align_by = POSITION;
        rules.native.compare = vec![SIZE, POSITION, PTS];
        let ours = reading(vec![packet(0, 30, 0), packet(30, 5, 10)], None);
        let theirs = reference_of(vec![
            packet(0, 12, 0),
            packet(12, 18, 10),
            packet(30, 5, 20),
        ]);
        let comparison = Comparison::new(&ours, &theirs, &rules);
        let found = comparison.differences(View::Native, &rules.native);
        let rows: Vec<Value> = found.iter().map(Difference::row).collect();
        assert_eq!(
            json::canonical(&Value::Array(rows)),
            r#"[[0,0,0,"size",30,12],[0,null,1,"unmatched",null,18],[0,1,2,"pts",10,20]]"#
        );
        let scope = |fields: &[&str], packets, reference| Scope {
            views: vec![View::Native],
            fields: fields.iter().map(|f| (*f).to_owned()).collect(),
            packets,
            reference,
        };
        let resplit = scope(&["size", "unmatched"], Packets::Resplit, Relation::Any);
        let shifted = scope(&["pts"], Packets::Any, Relation::PlusUnmatchedDurations);
        let covered = |s: &Scope| {
            found
                .iter()
                .map(|d| comparison.covers(s, View::Native, d, &found))
                .collect::<Vec<_>>()
        };
        assert_eq!(covered(&resplit), [true, true, false]);
        assert_eq!(covered(&shifted), [false, false, true]);
    }

    #[test]
    fn reviewed_lists_must_hash_to_their_digest_and_name_known_dispositions() {
        let rules = r#"{"formats": {"ogg": {"streams": "position-in-link", "align_by": "size",
            "views": {"native": {"compare": ["pts"]}}}},
            "dispositions": [{"id": "a", "reason": "why", "scope": {"views": ["native"],
            "fields": ["pts"], "packets": "any"}}]}"#;
        let rows = r#"[[0, 0, 0, "pts", 1, 2]]"#;
        let digest = list_sha256(json::parse(rows).unwrap().as_array().unwrap());
        let differences = |id: &str, digest: &str| {
            format!(
                r#"{{"schema": "{DIFFERENCES_SCHEMA}", "cases": [{{"name": "n", "sha256": "s",
                "view": "native", "dispositions": ["{id}"], "difference_sha256": "{digest}",
                "differences": [[0, 0, 0, "pts", 1, 2, "{id}"]]}}]}}"#
            )
        };
        let parse =
            |d: String| Rules::parse(&json::parse(rules).unwrap(), &json::parse(&d).unwrap());
        assert!(parse(differences("a", &digest)).is_ok());
        assert!(parse(differences("a", "0")).is_err());
        assert!(parse(differences("b", &digest)).is_err());
        assert!(
            Rules::parse(
                &json::parse(&rules.replace(r#""packets": "any""#, r#""packets": "some""#))
                    .unwrap(),
                &json::parse(&differences("a", &digest)).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn the_repository_rules_load() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let rules = Rules::load(&root).unwrap();
        for lane in [
            "reader-ogg",
            "reader-flac",
            "reader-mp3",
            "reader-aac",
            "reader-ac3",
            "reader-eac3",
        ] {
            rules.format(format(lane).unwrap()).unwrap();
            assert!(suffixes(lane).is_some());
        }
        assert!(rules.format("ogg").unwrap().compat.is_some());
    }
}
