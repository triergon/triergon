//! Reader packet comparison, by the rules of `harness/codecs/reader-rules.json`.
//!
//! For every `reader-*` row of a manifest, `codec-worker packets FORMAT FILE` prints
//! what the container reader emits per packet, and the pinned `ffprobe -show_packets`
//! with the same demuxer forced is the reference. Packets are paired in order per
//! stream; the reader's times are projected into FFmpeg's stream time base and every
//! field the rules compare for the format is compared. A case whose differences are
//! exactly the ones a reviewed disposition records (by input and difference SHA-256)
//! takes that disposition; any other difference is a failure. Every case is a row.

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
const FIELDS: [&str; 7] = [
    "size",
    "position",
    "pts",
    "dts",
    "duration",
    "trim_start",
    "trim_end",
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

/// How a format's reader tracks pair with FFmpeg's streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Streams {
    /// The track's position among the tracks of its chain link.
    PositionInLink,
    /// The one track against FFmpeg's first audio stream.
    FirstAudio,
}

/// What the rules compare for one format.
#[derive(Clone, Debug, PartialEq)]
struct FormatRules {
    compare: Vec<String>,
    streams: Streams,
    /// FFmpeg's duration excludes the packet's discard padding.
    duration_excludes_padding: bool,
}

/// A reviewed set of differences of one input.
#[derive(Clone, Debug, PartialEq)]
struct Case {
    sha256: String,
    difference_sha256: String,
    dispositions: Vec<String>,
}

/// `harness/codecs/reader-rules.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    formats: Vec<(String, FormatRules)>,
    cases: Vec<Case>,
}

fn text_list(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}

impl Rules {
    pub fn load(root: &Path) -> Result<Rules, String> {
        let path = root.join("harness/codecs/reader-rules.json");
        Rules::parse(&crate::checks::read_json(&path)?)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(document: &Value) -> Result<Rules, String> {
        let Some(Value::Object(formats)) = document.get("formats") else {
            return Err("no formats".into());
        };
        let formats = formats
            .iter()
            .map(|(name, rules)| {
                let compare = text_list(rules.get("compare"))
                    .ok_or_else(|| format!("formats.{name}.compare"))?;
                if let Some(field) = compare.iter().find(|f| !FIELDS.contains(&f.as_str())) {
                    return Err(format!("formats.{name}: unknown field {field}"));
                }
                let streams = match rules.get("streams").and_then(Value::as_str) {
                    Some("position-in-link") => Streams::PositionInLink,
                    Some("first-audio") => Streams::FirstAudio,
                    _ => return Err(format!("formats.{name}.streams")),
                };
                let duration_excludes_padding = rules
                    .get("reference_duration_excludes_discard_padding")
                    .is_some_and(Value::truthy);
                Ok((
                    name.clone(),
                    FormatRules {
                        compare,
                        streams,
                        duration_excludes_padding,
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
            dispositions.push(id.to_owned());
        }
        let mut cases = Vec::new();
        for item in document
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("no cases")?
        {
            let field = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("case without {key}"))
            };
            let case = Case {
                sha256: field("sha256")?,
                difference_sha256: field("difference_sha256")?,
                dispositions: text_list(item.get("dispositions"))
                    .filter(|d| !d.is_empty())
                    .ok_or("case without dispositions")?,
            };
            if let Some(id) = case.dispositions.iter().find(|d| !dispositions.contains(d)) {
                return Err(format!(
                    "case {} names unknown disposition {id}",
                    case.sha256
                ));
            }
            cases.push(case);
        }
        Ok(Rules { formats, cases })
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

/// One packet in the reference time base: the compared fields, `None` where absent.
type Fields = [Option<i128>; 7];
/// A time base as numerator and denominator.
type Ratio = (i128, i128);
/// The time base of each FFmpeg stream index.
type TimeBases = Vec<(i128, Ratio)>;

fn field_index(name: &str) -> usize {
    FIELDS.iter().position(|f| *f == name).expect("known field")
}

/// One side's reading: the FFmpeg streams its tracks are, packets per stream, and how
/// reading ended.
#[derive(Default)]
struct Side {
    tracks: Vec<i128>,
    streams: Vec<(i128, Vec<Fields>)>,
    /// `None` for a clean end; the error otherwise.
    error: Option<String>,
}

impl Side {
    fn stream(&mut self, index: i128) -> &mut Vec<Fields> {
        if let Some(at) = self.streams.iter().position(|(i, _)| *i == index) {
            return &mut self.streams[at].1;
        }
        self.streams.push((index, Vec::new()));
        &mut self.streams.last_mut().expect("pushed").1
    }
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

/// The pinned `ffprobe` reading: audio packets by stream, the stream time bases, and
/// the packets of other streams counted.
struct Reference {
    side: Side,
    time_bases: TimeBases,
    audio: Vec<i128>,
    other_packets: i128,
    code: i64,
    stderr: String,
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
    let mut time_bases = Vec::new();
    let mut audio = Vec::new();
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
            time_bases.push((index, base));
        }
        if stream.get("codec_type").and_then(Value::as_str) == Some("audio") {
            audio.push(index);
        }
    }
    let mut side = Side {
        tracks: audio.clone(),
        ..Side::default()
    };
    let mut other_packets = 0;
    for packet in document
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let index = int(packet.get("stream_index")).unwrap_or(0);
        if !audio.contains(&index) {
            other_packets += 1;
            continue;
        }
        let trims = packet
            .get("side_data_list")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .find(|d| d.get("skip_samples").is_some() || d.get("discard_padding").is_some());
        let trim = |key: &str| int(trims.and_then(|d| d.get(key)));
        side.stream(index).push([
            int(packet.get("size")),
            int(packet.get("pos")),
            int(packet.get("pts")),
            int(packet.get("dts")),
            int(packet.get("duration")),
            trim("skip_samples"),
            trim("discard_padding"),
        ]);
    }
    if done.code != 0 {
        side.error = Some(python::tail(&done.stderr, 300));
    }
    Ok(Reference {
        side,
        time_bases,
        audio,
        other_packets,
        code: done.code,
        stderr: done.stderr,
    })
}

/// A reader packet as `codec-worker packets` prints it.
struct Emitted {
    track: i128,
    time_base: Ratio,
    fields: Fields,
}

/// The reader's reading, its tracks mapped to FFmpeg stream indices by `streams`.
fn candidate(
    worker: &Path,
    format: &str,
    path: &str,
    streams: Streams,
    audio: &[i128],
) -> Result<Result<(Side, TimeBases), String>, Stop> {
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
fn parse_candidate(
    stdout: &str,
    streams: Streams,
    audio: &[i128],
) -> Result<(Side, TimeBases), String> {
    let lines = python::splitlines(stdout);
    let (last, packets) = lines.split_last().ok_or("codec-worker printed nothing")?;
    let end = python::loads(last).ok_or("codec-worker: the last line is not JSON")?;
    let tracks = end
        .get("tracks")
        .and_then(Value::as_array)
        .ok_or("codec-worker: no track list")?;
    // FFmpeg replaces a link's streams in place: the n-th track of every chain link is
    // stream n. An elementary stream's one track is FFmpeg's first audio stream.
    let stream_of = |index: i128| -> Option<i128> {
        match streams {
            Streams::FirstAudio => Some(audio.first().copied().unwrap_or(0)),
            Streams::PositionInLink => {
                let track = tracks.iter().find(|t| int(t.get("index")) == Some(index))?;
                let link = int(track.get("link"));
                let position = tracks
                    .iter()
                    .filter(|t| int(t.get("link")) == link)
                    .position(|t| int(t.get("index")) == Some(index))?;
                Some(position as i128)
            }
        }
    };
    let mut mapped: Vec<i128> = tracks
        .iter()
        .filter_map(|t| stream_of(int(t.get("index"))?))
        .collect();
    mapped.sort_unstable();
    mapped.dedup();
    let mut side = Side {
        tracks: mapped,
        error: match end.get("end").and_then(Value::as_str) {
            Some("eof") => None,
            _ => Some(python::str(end.get("error").unwrap_or(&Value::Null))),
        },
        ..Side::default()
    };
    let mut time_bases = Vec::new();
    for line in packets {
        let packet = python::loads(line).ok_or("codec-worker: a packet line is not JSON")?;
        let base = packet
            .get("time_base")
            .and_then(Value::as_array)
            .and_then(|b| Some((int(b.first())?, int(b.get(1))?)))
            .ok_or("codec-worker: packet without time base")?;
        let emitted = Emitted {
            track: int(packet.get("track")).ok_or("codec-worker: packet without track")?,
            time_base: base,
            fields: FIELDS.map(|f| int(packet.get(f))),
        };
        let stream = stream_of(emitted.track).unwrap_or(-1 - emitted.track);
        if !time_bases.iter().any(|(s, _)| *s == stream) {
            time_bases.push((stream, emitted.time_base));
        }
        side.stream(stream).push(emitted.fields);
    }
    Ok((side, time_bases))
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
    let [pts, dts, duration] = ["pts", "dts", "duration"].map(field_index);
    let exact = packets.iter().all(|p| {
        [pts, dts, duration]
            .iter()
            .all(|&f| p[f].is_none_or(|v| rescale(v, from, to).1))
    });
    let mut clock = packets
        .first()
        .and_then(|p| p[pts])
        .map_or(0, |v| rescale(v, from, to).0);
    for packet in packets {
        let length = packet[duration].map(|d| rescale(d, from, to).0);
        if exact {
            for f in [pts, dts] {
                packet[f] = packet[f].map(|v| rescale(v, from, to).0);
            }
        } else {
            packet[pts] = Some(clock);
            packet[dts] = packet[dts].map(|_| clock);
            clock += length.unwrap_or(0);
        }
        packet[duration] = length;
    }
}

fn optional(value: Option<i128>) -> Value {
    value.map_or(Value::Null, Value::int)
}

/// Every difference between the two readings under `rules`, in a canonical order.
fn differences(ours: &Side, theirs: &Side, rules: &FormatRules) -> Vec<Value> {
    let compared: Vec<usize> = rules.compare.iter().map(|f| field_index(f)).collect();
    let mut out = Vec::new();
    let mut streams: Vec<i128> = ours
        .streams
        .iter()
        .chain(&theirs.streams)
        .map(|(s, _)| *s)
        .collect();
    streams.sort_unstable();
    streams.dedup();
    fn packets(side: &Side, stream: i128) -> &[Fields] {
        side.streams
            .iter()
            .find(|(s, _)| *s == stream)
            .map_or(&[], |(_, p)| p)
    }
    let trim_start = field_index("trim_start");
    let trim_end = field_index("trim_end");
    let duration = field_index("duration");
    for stream in streams {
        let (a, b) = (packets(ours, stream), packets(theirs, stream));
        if a.len() != b.len() {
            out.push(Value::object([
                ("stream", Value::int(stream)),
                ("field", Value::str("packets")),
                ("candidate", Value::int(a.len() as i128)),
                ("reference", Value::int(b.len() as i128)),
            ]));
        }
        for (index, (a, b)) in a.iter().zip(b).enumerate() {
            let mut b = *b;
            // An absent trim is a trim of zero on both sides.
            for f in [trim_start, trim_end] {
                b[f] = b[f].filter(|&v| v != 0);
            }
            if rules.duration_excludes_padding
                && let (Some(d), Some(p)) = (b[duration], b[trim_end])
            {
                b[duration] = Some(d + p);
            }
            let mut a = *a;
            for f in [trim_start, trim_end] {
                a[f] = a[f].filter(|&v| v != 0);
            }
            for &f in &compared {
                if a[f] != b[f] {
                    out.push(Value::object([
                        ("stream", Value::int(stream)),
                        ("packet", Value::int(index as i128)),
                        ("field", Value::str(FIELDS[f])),
                        ("candidate", optional(a[f])),
                        ("reference", optional(b[f])),
                    ]));
                }
            }
        }
    }
    if ours.tracks != theirs.tracks {
        let streams =
            |side: &Side| Value::Array(side.tracks.iter().map(|&s| Value::int(s)).collect());
        out.push(Value::object([
            ("field", Value::str("streams")),
            ("candidate", streams(ours)),
            ("reference", streams(theirs)),
        ]));
    }
    let end = |side: &Side| if side.error.is_some() { "error" } else { "eof" };
    if end(ours) != end(theirs) {
        out.push(Value::object([
            ("field", Value::str("end")),
            ("candidate", Value::str(end(ours))),
            ("reference", Value::str(end(theirs))),
        ]));
    }
    out
}

fn field_counts(differences: &[Value]) -> Value {
    let mut counts: Vec<(String, i128)> = Vec::new();
    for d in differences {
        let field = python::str(d.get("field").unwrap_or(&Value::Null));
        match counts.iter_mut().find(|(k, _)| *k == field) {
            Some((_, n)) => *n += 1,
            None => counts.push((field, 1)),
        }
    }
    Value::object(counts.into_iter().map(|(k, n)| (k, Value::int(n))))
}

fn grade_case(options: &Options, format: &str, path: &str, sha: &str) -> Result<Value, Stop> {
    let rules = options.rules.format(format)?;
    let theirs = reference(&options.ffprobe, format, path)?;
    let mut row = vec![
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
    let (mut ours, bases) = match candidate(
        &options.codec_worker,
        format,
        path,
        rules.streams,
        &theirs.audio,
    )? {
        Ok(reading) => reading,
        Err(error) => {
            row.push(("candidate_error".into(), Value::str(error)));
            row.push(("status".into(), Value::str("fail")));
            return Ok(Value::Object(row));
        }
    };
    for (stream, packets) in &mut ours.streams {
        let from = bases.iter().find(|(s, _)| s == stream).map(|(_, b)| *b);
        let to = theirs
            .time_bases
            .iter()
            .find(|(s, _)| s == stream)
            .map(|(_, b)| *b);
        if let (Some(from), Some(to)) = (from, to) {
            project(packets, from, to);
        }
    }
    let count = |side: &Side| {
        Value::Array(
            side.streams
                .iter()
                .map(|(s, p)| Value::Array(vec![Value::int(*s), Value::int(p.len() as i128)]))
                .collect(),
        )
    };
    row.push(("candidate_packets".into(), count(&ours)));
    row.push(("reference_packets".into(), count(&theirs.side)));
    row.push((
        "candidate_end".into(),
        ours.error.clone().map_or(Value::str("eof"), Value::str),
    ));
    let found = differences(&ours, &theirs.side, rules);
    let digest = sha256::hex(json::canonical(&Value::Array(found.clone())).as_bytes());
    row.push(("differences".into(), Value::int(found.len() as i128)));
    row.push(("difference_fields".into(), field_counts(&found)));
    row.push((
        "first_differences".into(),
        Value::Array(found.iter().take(SHOWN).cloned().collect()),
    ));
    row.push(("difference_sha256".into(), Value::str(digest.as_str())));
    let reviewed = options.rules.cases.iter().find(|c| c.sha256 == sha);
    let status = if found.is_empty() {
        if ours.tracks.is_empty() && ours.error.is_some() {
            "refused-by-both"
        } else {
            "pass"
        }
    } else if let Some(case) = reviewed.filter(|c| c.difference_sha256 == digest) {
        row.push((
            "dispositions".into(),
            Value::strings(case.dispositions.clone()),
        ));
        "disposition"
    } else {
        if let Some(case) = reviewed {
            row.push((
                "reviewed_difference_sha256".into(),
                Value::str(case.difference_sha256.as_str()),
            ));
        }
        "fail"
    };
    row.push(("status".into(), Value::str(status)));
    Ok(Value::Object(row))
}

/// Compare every reader row of the manifest (or those `--only` selects), appending one
/// JSON row per input to `--out` and printing one line each, then the status counts.
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
            match grade_case(options, format, &path, &sha) {
                Ok(Value::Object(fields)) => {
                    row.extend(fields);
                    python::str(
                        row.iter()
                            .rev()
                            .find(|(k, _)| k == "status")
                            .map_or(&Value::Null, |(_, v)| v),
                    )
                }
                Ok(_) => unreachable!("rows are objects"),
                Err(Stop::Timeout(argv)) => {
                    row.push(("argv".into(), Value::strings(argv)));
                    "timeout".to_owned()
                }
                Err(Stop::Fatal(message)) => return Err(message),
            }
        };
        if !row.iter().any(|(k, _)| k == "status") {
            row.push(("status".into(), Value::str(status.as_str())));
        }
        let row = Value::Object(row);
        writeln!(out, "{}", json::to_python(&row, None))
            .and_then(|()| out.flush())
            .map_err(|e| format!("{}: {e}", options.out.display()))?;
        let dispositions = row
            .get("dispositions")
            .and_then(Value::as_array)
            .map(|d| d.iter().map(python::str).collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        println!("{lane} {name} {status} {dispositions}");
        match counts.iter_mut().find(|(s, _)| *s == status) {
            Some((_, n)) => *n += 1,
            None => counts.push((status, 1)),
        }
    }
    let summary: Vec<String> = counts.iter().map(|(s, n)| format!("{s} {n}")).collect();
    println!("DONE {}", summary.join(", "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Result<Rules, String> {
        Rules::parse(&json::parse(text).unwrap())
    }

    #[test]
    fn projection_accumulates_rounded_durations_where_inexact() {
        // AC-3 at 44.1 kHz: 1536 samples are 3134.69 ticks of 1/90000.
        let packet =
            |pts: i128| -> Fields { [None, None, Some(pts), Some(pts), Some(1536), None, None] };
        let mut packets = vec![packet(0), packet(1536), packet(3072)];
        project(&mut packets, (1, 44100), (1, 90000));
        let times: Vec<_> = packets.iter().map(|p| (p[2], p[4])).collect();
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
        assert_eq!(packets[0][2], Some(-368_640));
        assert_eq!(packets[1][4], Some(491_520));
    }

    #[test]
    fn differences_follow_the_format_rules() {
        let format = FormatRules {
            compare: ["size", "pts", "duration", "trim_end"]
                .map(String::from)
                .to_vec(),
            streams: Streams::PositionInLink,
            duration_excludes_padding: true,
        };
        let side = |packets: Vec<Fields>| Side {
            tracks: vec![0],
            streams: vec![(0, packets)],
            error: None,
        };
        // The last Opus packet: 960 samples, 936 of them padding; FFmpeg reports 24.
        let ours = side(vec![[
            Some(9),
            Some(1),
            Some(0),
            Some(0),
            Some(960),
            None,
            Some(936),
        ]]);
        let theirs = side(vec![[
            Some(9),
            Some(7),
            Some(0),
            Some(0),
            Some(24),
            Some(0),
            Some(936),
        ]]);
        assert!(differences(&ours, &theirs, &format).is_empty());
        let late = side(vec![[
            Some(9),
            Some(1),
            Some(5),
            Some(5),
            Some(960),
            None,
            Some(936),
        ]]);
        let found = differences(&late, &theirs, &format);
        assert_eq!(
            json::canonical(&Value::Array(found)),
            r#"[{"candidate":5,"field":"pts","packet":0,"reference":0,"stream":0}]"#
        );
        let refused = Side::default();
        let found = differences(&refused, &theirs, &format);
        assert_eq!(found.len(), 2, "packet count and streams");
    }

    #[test]
    fn rules_name_reasons_and_known_dispositions() {
        let base = r#"{"formats": {"ogg": {"compare": ["pts"], "streams": "position-in-link"}},
            "dispositions": [{"id": "a", "reason": "why"}], "cases": CASES}"#;
        let with = |cases: &str| rules(&base.replace("CASES", cases));
        assert!(with("[]").is_ok());
        assert!(
            with(r#"[{"sha256": "x", "difference_sha256": "y", "dispositions": ["a"]}]"#).is_ok()
        );
        assert!(
            with(r#"[{"sha256": "x", "difference_sha256": "y", "dispositions": ["b"]}]"#).is_err()
        );
        assert!(rules(&base.replace("why", "").replace("CASES", "[]")).is_err());
        assert!(rules(&base.replace("\"pts\"", "\"bytes\"").replace("CASES", "[]")).is_err());
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
    }
}
