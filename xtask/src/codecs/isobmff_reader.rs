//! `cargo xtask codecs isobmff-reader`: the neutral ISO BMFF reader
//! (`triergon_isobmff::Reader`) against the pinned FFmpeg MOV demuxer, by the rules of
//! `harness/features/isobmff-reader/rules.json` and the reviewed differences of
//! `differences.json` beside it.
//!
//! For every case of the feature's `corpus.json`, `codec-worker packets mov FILE [SEEK]`
//! prints what the reader emits per packet, and the formats profile's
//! `ffprobe -show_packets` with the options the rules give is the reference. Tracks
//! pair with FFmpeg's streams by `track_ID`, packets are aligned per track by source
//! position and size, the reader's times are expressed in FFmpeg's stream time base,
//! and every field the rules compare is compared, as are the order of the packets
//! across tracks, the set of tracks and how reading ended. A difference must equal the
//! reviewed list of its case and fall in the scope of exactly one disposition the case
//! lists; otherwise the case fails. The run writes one row per case and its unexplained
//! differences, attributed where a scope matches, to `OUT.differences.json` for review,
//! and exits 1 when a case fails.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::correctness::file_sha;
use super::readers::{
    COLUMNS, DIFFERENCES_SCHEMA, Key, Pair, align, differences_text, list_sha256,
};
use crate::json::{self, Value};
use crate::process::{Stop, run_text};
use crate::python;

const FEATURE: &str = "harness/features/isobmff-reader";
pub const RULES_SCHEMA: &str = "triergon.isobmff.reader-rules.v1";
pub const CORPUS_SCHEMA: &str = "triergon.isobmff.reader-corpus.v1";
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// Differences kept in a row besides their count, per-field counts and digest.
const SHOWN: usize = 8;

/// The packet fields a rule may compare.
const FIELDS: [&str; 7] = ["size", "position", "hash", "sync", "pts", "dts", "duration"];
const SIZE: usize = 0;
const POSITION: usize = 1;
/// Difference kinds besides the packet fields: an unpaired packet, the packet counts of
/// a track only one side has packets of, a run of packets the two sides order
/// differently across tracks, the set of tracks, how reading ended.
const OTHER: [&str; 5] = ["unmatched", "packets", "order", "streams", "end"];

fn field_index(name: &str) -> Option<usize> {
    FIELDS.iter().position(|f| *f == name)
}

/// One case of the corpus: an input read through, or read after one seek.
#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub name: String,
    pub path: String,
    pub sha256: String,
    /// The track (`track_ID`) and the target in microseconds.
    pub seek: Option<(u64, i64)>,
    pub first_entry_colour: bool,
}

/// The corpus inputs, once each, with their pinned SHA-256.
pub fn inputs(root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let mut inputs: Vec<(PathBuf, String)> = corpus(root)?
        .into_iter()
        .map(|case| (root.join(&case.path), case.sha256))
        .collect();
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

/// Check that every corpus input is present with its pinned SHA-256.
pub fn verify_inputs(root: &Path) -> Result<String, String> {
    let inputs = inputs(root)?;
    for (path, pin) in &inputs {
        let actual = file_sha(path)?;
        if actual != *pin {
            return Err(format!(
                "{}: SHA-256 {actual}, pinned {pin}",
                path.display()
            ));
        }
    }
    Ok(format!("{} inputs", inputs.len()))
}

/// The cases of `corpus.json`, checked against the counts the contract states.
pub fn corpus(root: &Path) -> Result<Vec<Case>, String> {
    let manifest = crate::checks::read_json(&root.join(FEATURE).join("corpus.json"))?;
    if manifest.get("schema").and_then(Value::as_str) != Some(CORPUS_SCHEMA) {
        return Err(format!(
            "{FEATURE}/corpus.json: schema is not {CORPUS_SCHEMA}"
        ));
    }
    let mut cases = Vec::new();
    for item in manifest
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("corpus.json has no cases")?
    {
        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("a case without {key}"))
        };
        let seek = match item.get("seek") {
            None | Some(Value::Null) => None,
            Some(seek) => Some((
                int(seek.get("track"))
                    .and_then(|t| u64::try_from(t).ok())
                    .ok_or("a seek without track")?,
                int(seek.get("microseconds"))
                    .and_then(|t| i64::try_from(t).ok())
                    .filter(|t| *t >= 0)
                    .ok_or("a seek without microseconds")?,
            )),
        };
        cases.push(Case {
            name: text("name")?,
            path: text("path")?,
            sha256: text("sha256")?,
            seek,
            first_entry_colour: match item.get("first_entry_colour") {
                None => false,
                Some(Value::Bool(value)) => *value,
                _ => return Err("invalid first_entry_colour".into()),
            },
        });
    }
    let contract = crate::checks::read_json(&root.join(FEATURE).join("contract.json"))?;
    let stated = |key: &str| {
        contract
            .get("counts")
            .and_then(|c| c.get(key))
            .and_then(python::int)
    };
    let mut inputs: Vec<&str> = cases.iter().map(|c| c.sha256.as_str()).collect();
    inputs.sort_unstable();
    inputs.dedup();
    let seeks = cases.iter().filter(|c| c.seek.is_some()).count();
    for (key, found) in [
        ("cases", cases.len()),
        ("reads", cases.len() - seeks),
        ("seeks", seeks),
        ("distinct_inputs", inputs.len()),
    ] {
        if stated(key) != Some(found as i128) {
            return Err(format!(
                "{FEATURE}: the contract states {key} {:?}, corpus.json has {found}",
                stated(key)
            ));
        }
    }
    let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|w| w[0] == w[1]) {
        return Err("corpus.json names a case twice".into());
    }
    Ok(cases)
}

/// Which packets a disposition covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packets {
    /// A difference of the whole reading or of a whole track: the tracks, how reading
    /// ended, the packet counts of a track only one side has packets of.
    File,
    /// Every packet, paired or not.
    Any,
    /// The first packet of a track on either side.
    FirstOfStream,
    /// A packet FFmpeg delivers before the reader's first packet of a track other
    /// than the one a seek case seeks on.
    LeadingInOtherTrack,
    /// A packet the reader delivers before FFmpeg's first packet of a track.
    LeadingInReader,
    /// A run of packets whose sizes differ or that one side lacks, where both sides'
    /// packets span the same bytes: the same source cut into other packets.
    Resplit,
    /// A run of packets ordered differently across tracks.
    Order,
}

impl Packets {
    fn parse(name: &str) -> Option<Packets> {
        Some(match name {
            "file" => Packets::File,
            "any" => Packets::Any,
            "first-of-stream" => Packets::FirstOfStream,
            "leading-in-other-track" => Packets::LeadingInOtherTrack,
            "leading-in-reader" => Packets::LeadingInReader,
            "resplit" => Packets::Resplit,
            "order" => Packets::Order,
            _ => return None,
        })
    }
}

/// How the reference value relates to the reader's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Relation {
    Any,
    /// FFmpeg has no value.
    ReferenceAbsent,
    /// The reader has no value.
    CandidateAbsent,
    /// The reader's value is zero or empty.
    CandidateZero,
    /// FFmpeg's value is zero or empty.
    ReferenceZero,
    /// Every difference of this field in the track where both sides have a value
    /// differs by one nonzero offset.
    ConstantOffset,
    /// The reader's value is this text: how a reading ended.
    Candidate(Vec<String>),
}

/// The differences a disposition covers.
#[derive(Clone, Debug, PartialEq)]
struct Scope {
    fields: Vec<String>,
    packets: Packets,
    reference: Relation,
    /// The disposition covers only seek cases.
    seeks: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Disposition {
    id: String,
    scopes: Vec<Scope>,
}

/// The reviewed differences of one case.
#[derive(Clone, Debug, PartialEq)]
struct Reviewed {
    name: String,
    sha256: String,
    dispositions: Vec<String>,
    difference_sha256: String,
    attributed: Vec<String>,
}

/// `rules.json` and `differences.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rules {
    ffprobe_options: Vec<String>,
    compare: Vec<usize>,
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

fn scope(value: &Value, id: &str) -> Result<Scope, String> {
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
    let reference = match (value.get("reference"), value.get("candidate")) {
        (_, Some(texts)) => Relation::Candidate(
            text_list(Some(texts))
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("{id}: scope.candidate"))?,
        ),
        (None, None) => Relation::Any,
        (Some(relation), None) => match relation.as_str() {
            Some("any") => Relation::Any,
            Some("absent") => Relation::ReferenceAbsent,
            Some("candidate-absent") => Relation::CandidateAbsent,
            Some("candidate-zero") => Relation::CandidateZero,
            Some("reference-zero") => Relation::ReferenceZero,
            Some("constant-offset") => Relation::ConstantOffset,
            _ => return Err(format!("{id}: unknown reference relation")),
        },
    };
    Ok(Scope {
        fields,
        packets,
        reference,
        seeks: value.get("cases").and_then(Value::as_str) == Some("seeks"),
    })
}

impl Rules {
    pub fn load(root: &Path) -> Result<Rules, String> {
        let rules = root.join(FEATURE).join("rules.json");
        let differences = root.join(FEATURE).join("differences.json");
        Rules::parse(
            &crate::checks::read_json(&rules)?,
            &crate::checks::read_json(&differences)?,
        )
        .map_err(|e| format!("{} / {}: {e}", rules.display(), differences.display()))
    }

    fn parse(document: &Value, differences: &Value) -> Result<Rules, String> {
        if document.get("schema").and_then(Value::as_str) != Some(RULES_SCHEMA) {
            return Err(format!("rules schema is not {RULES_SCHEMA}"));
        }
        let ffprobe_options =
            text_list(document.get("ffprobe_options")).ok_or("no ffprobe_options")?;
        let compare = text_list(document.get("compare"))
            .ok_or("no compare")?
            .iter()
            .map(|f| field_index(f).ok_or_else(|| format!("compare: unknown field {f}")))
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
            let scopes = match item.get("scope") {
                Some(Value::Array(items)) if !items.is_empty() => items
                    .iter()
                    .map(|s| scope(s, id))
                    .collect::<Result<Vec<_>, String>>()?,
                _ => return Err(format!("disposition {id} has no scope")),
            };
            if dispositions.iter().any(|d: &Disposition| d.id == id) {
                return Err(format!("disposition {id} is listed twice"));
            }
            dispositions.push(Disposition {
                id: id.to_owned(),
                scopes,
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
            let name = field("name")?;
            let listed = text_list(item.get("dispositions"))
                .filter(|d| !d.is_empty())
                .ok_or_else(|| format!("{name}: no dispositions"))?;
            if let Some(id) = listed
                .iter()
                .find(|d| !dispositions.iter().any(|known| known.id == **d))
            {
                return Err(format!("{name}: unknown disposition {id}"));
            }
            let mut plain = Vec::new();
            let mut attributed = Vec::new();
            for row in item
                .get("differences")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name}: no differences"))?
            {
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
                    "{name}: the differences do not hash to difference_sha256"
                ));
            }
            if reviewed.iter().any(|r: &Reviewed| r.name == name) {
                return Err(format!("{name} is reviewed twice"));
            }
            reviewed.push(Reviewed {
                name,
                sha256: field("sha256")?,
                dispositions: listed,
                difference_sha256,
                attributed,
            });
        }
        Ok(Rules {
            ffprobe_options,
            compare,
            dispositions,
            reviewed,
        })
    }
}

fn int(value: Option<&Value>) -> Option<i128> {
    value
        .filter(|v| !matches!(v, Value::Null))
        .and_then(python::int)
}

/// One packet's fields, `Null` where absent; times in FFmpeg's stream time base.
type Fields = [Value; 7];

/// One side's reading: its tracks by `track_ID`, packets per track in delivery order
/// with their place in the whole delivery, and how reading ended.
#[derive(Debug, Default)]
struct Reading {
    tracks: Vec<i128>,
    colours: Vec<(i128, Value)>,
    streams: Vec<(i128, Vec<(usize, Fields)>)>,
    error: Option<String>,
}

fn packets_of(reading: &Reading, track: i128) -> &[(usize, Fields)] {
    reading
        .streams
        .iter()
        .find(|(t, _)| *t == track)
        .map_or(&[], |(_, p)| p)
}

fn push(reading: &mut Reading, track: i128, at: usize, fields: Fields) {
    match reading.streams.iter_mut().find(|(t, _)| *t == track) {
        Some((_, packets)) => packets.push((at, fields)),
        None => reading.streams.push((track, vec![(at, fields)])),
    }
}

/// A time base as numerator and denominator.
type Ratio = (i128, i128);

fn ratio(text: &str) -> Option<Ratio> {
    let (n, d) = text.split_once('/')?;
    Some((n.parse().ok()?, d.parse().ok()?)).filter(|&(n, d)| n > 0 && d > 0)
}

/// `ticks` of base `from` in base `to`: an integer where exact, else the fraction.
fn project(ticks: i128, from: Ratio, to: Ratio) -> Value {
    let (numerator, denominator) = (ticks * from.0 * to.1, from.1 * to.0);
    if numerator % denominator == 0 {
        return Value::int(numerator / denominator);
    }
    let gcd = {
        let (mut a, mut b) = (numerator.abs(), denominator);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    Value::str(format!("{}/{}", numerator / gcd, denominator / gcd))
}

/// The pinned `ffprobe` reading, how ffprobe exited, and each track's time base.
struct Referenced {
    reading: Reading,
    summary: Value,
    bases: Vec<(i128, Ratio)>,
}

/// The pinned `ffprobe` reading of `path`, after `seek` when given.
fn reference(
    ffprobe: &str,
    options: &[String],
    path: &str,
    seek: Option<(u64, i64)>,
    colour: bool,
) -> Result<Referenced, Stop> {
    let mut argv = vec![ffprobe.to_owned(), "-v".into(), "error".into()];
    argv.extend(options.iter().cloned());
    if let Some((_, micros)) = seek {
        argv.push("-read_intervals".into());
        argv.push(format!("{}.{:06}%", micros / 1_000_000, micros % 1_000_000));
    }
    let entries = if colour {
        "stream=index,id,codec_type,time_base,color_primaries,color_transfer,color_space,color_range:packet=stream_index,pts,dts,duration,size,pos,flags,data_hash"
    } else {
        "stream=index,id,codec_type,time_base:packet=stream_index,pts,dts,duration,size,pos,flags,data_hash"
    };
    argv.extend(
        [
            "-show_entries",
            entries,
            "-show_data_hash",
            "MD5",
            "-of",
            "json",
            path,
        ]
        .map(String::from),
    );
    let done = run_text(&argv, RUN_TIMEOUT)?;
    let document = python::loads(&done.stdout).unwrap_or(Value::Null);
    let mut reading = Reading {
        error: (done.code != 0).then(|| python::tail(&done.stderr, 300)),
        ..Reading::default()
    };
    // A stream without a track ID is keyed by its index, below every track ID.
    let mut keys = Vec::new();
    let mut bases = Vec::new();
    for stream in document
        .get("streams")
        .and_then(Value::as_array)
        .unwrap_or_default()
    {
        let Some(index) = int(stream.get("index")) else {
            continue;
        };
        let key = stream
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.strip_prefix("0x"))
            .and_then(|hex| i128::from_str_radix(hex, 16).ok())
            .unwrap_or(-1 - index);
        keys.push((index, key));
        reading.tracks.push(key);
        if colour {
            let code = |field: &str| match stream.get(field).and_then(Value::as_str) {
                Some("bt709") => Value::int(1),
                Some("bt2020") | Some("bt2020nc") => Value::int(9),
                Some("smpte2084") => Value::int(16),
                Some(other) => Value::str(other),
                None => Value::Null,
            };
            let range = match stream.get("color_range").and_then(Value::as_str) {
                None | Some("unknown") => Value::Null,
                Some(value) => Value::str(value),
            };
            reading.colours.push((
                key,
                Value::Array(vec![
                    code("color_primaries"),
                    code("color_transfer"),
                    code("color_space"),
                    range,
                ]),
            ));
        }
        if let Some(base) = stream
            .get("time_base")
            .and_then(Value::as_str)
            .and_then(ratio)
        {
            bases.push((key, base));
        }
    }
    reading.tracks.sort_unstable();
    reading.colours.sort_by_key(|(id, _)| *id);
    for (at, packet) in document
        .get("packets")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let index = int(packet.get("stream_index")).unwrap_or(-1);
        let key = keys
            .iter()
            .find(|(i, _)| *i == index)
            .map_or(-1 - index, |(_, k)| *k);
        let number = |name: &str| int(packet.get(name)).map_or(Value::Null, Value::int);
        let flags = packet.get("flags").and_then(Value::as_str);
        let fields: Fields = [
            number("size"),
            number("pos"),
            packet
                .get("data_hash")
                .and_then(Value::as_str)
                .and_then(|h| h.strip_prefix("MD5:"))
                .map_or(Value::Null, Value::str),
            flags.map_or(Value::Null, |f| Value::int(i128::from(f.contains('K')))),
            number("pts"),
            number("dts"),
            number("duration"),
        ];
        push(&mut reading, key, at, fields);
    }
    let summary = Value::object([
        ("rc", Value::int(done.code)),
        ("stderr_tail", Value::str(python::tail(&done.stderr, 300))),
    ]);
    Ok(Referenced {
        reading,
        summary,
        bases,
    })
}

/// The reader's reading of `path` through `codec-worker`, its times in the reference
/// bases; an error describes output that breaks the worker's format.
fn candidate(
    worker: &Path,
    path: &str,
    seek: Option<(u64, i64)>,
    bases: &[(i128, Ratio)],
    colour: bool,
) -> Result<Result<(Reading, Value), String>, Stop> {
    let mut argv = vec![
        python::path(worker),
        "packets".to_owned(),
        if colour { "mov-colour" } else { "mov" }.to_owned(),
        path.to_owned(),
    ];
    if let Some((track, micros)) = seek {
        argv.push(format!("{track}@{micros}"));
    }
    let done = run_text(&argv, RUN_TIMEOUT)?;
    if done.code != 0 {
        return Ok(Err(format!(
            "codec-worker exited {}: {}",
            done.code,
            python::tail(&done.stderr, 300)
        )));
    }
    Ok(parse_candidate(&done.stdout, bases, colour))
}

fn parse_candidate(
    stdout: &str,
    bases: &[(i128, Ratio)],
    colour: bool,
) -> Result<(Reading, Value), String> {
    let lines = python::splitlines(stdout);
    let (last, lines) = lines.split_last().ok_or("codec-worker printed nothing")?;
    let end = python::loads(last).ok_or("codec-worker: the last line is not JSON")?;
    let tracks = end
        .get("tracks")
        .and_then(Value::as_array)
        .ok_or("codec-worker: no track list")?;
    let mut reading = Reading {
        tracks: tracks.iter().filter_map(|t| int(t.get("id"))).collect(),
        error: match end.get("end").and_then(Value::as_str) {
            Some("eof") => None,
            _ => Some(python::str(end.get("error").unwrap_or(&Value::Null))),
        },
        ..Reading::default()
    };
    reading.tracks.sort_unstable();
    if colour {
        for track in tracks {
            let id = int(track.get("id")).ok_or("codec-worker: track without id")?;
            let value = track
                .get("colour")
                .ok_or("codec-worker: track without colour field")?;
            reading.colours.push((id, value.clone()));
        }
    }
    reading.colours.sort_by_key(|(id, _)| *id);
    let mut receipt = Value::Null;
    let mut at = 0;
    for line in lines {
        let packet = python::loads(line).ok_or("codec-worker: a line is not JSON")?;
        match packet.get("event").and_then(Value::as_str) {
            Some("seek") => {
                receipt = packet;
                continue;
            }
            Some(other) => return Err(format!("codec-worker: unknown event {other}")),
            None => {}
        }
        let track = int(packet.get("track_id")).ok_or("codec-worker: packet without track")?;
        let from = packet
            .get("time_base")
            .and_then(Value::as_array)
            .and_then(|b| Some((int(b.first())?, int(b.get(1))?)))
            .filter(|&(n, d)| n > 0 && d > 0)
            .ok_or("codec-worker: packet without time base")?;
        let to = bases
            .iter()
            .find(|(t, _)| *t == track)
            .map_or(from, |(_, b)| *b);
        let number = |name: &str| int(packet.get(name)).map_or(Value::Null, Value::int);
        let time = |name: &str| int(packet.get(name)).map_or(Value::Null, |t| project(t, from, to));
        let fields: Fields = [
            number("size"),
            number("position"),
            packet
                .get("hash")
                .and_then(Value::as_str)
                .map_or(Value::Null, Value::str),
            number("sync"),
            time("pts"),
            time("dts"),
            time("duration"),
        ];
        push(&mut reading, track, at, fields);
        at += 1;
    }
    Ok((reading, receipt))
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

fn optional(value: Option<usize>) -> Value {
    value.map_or(Value::Null, |v| Value::int(v as i128))
}

impl Difference {
    fn row(&self) -> Value {
        Value::Array(vec![
            self.stream.map_or(Value::Null, Value::int),
            optional(self.candidate),
            optional(self.reference),
            Value::str(self.field),
            self.ours.clone(),
            self.theirs.clone(),
        ])
    }
}

/// Both readings, aligned per track.
struct Comparison<'a> {
    candidate: &'a Reading,
    reference: &'a Reading,
    streams: Vec<(i128, Vec<Pair>)>,
}

fn number(value: &Value) -> Option<i128> {
    python::int(value)
}

/// A count of zero or an empty list.
fn zero(value: &Value) -> bool {
    number(value) == Some(0) || value.as_array().is_some_and(<[Value]>::is_empty)
}

impl<'a> Comparison<'a> {
    fn new(candidate: &'a Reading, reference: &'a Reading) -> Self {
        let mut tracks: Vec<i128> = candidate
            .streams
            .iter()
            .chain(&reference.streams)
            .map(|(t, _)| *t)
            .collect();
        tracks.sort_unstable();
        tracks.dedup();
        let keys = |packets: &[(usize, Fields)]| -> Vec<Key> {
            packets
                .iter()
                .map(|(_, p)| [number(&p[POSITION]), number(&p[SIZE])])
                .collect()
        };
        let streams = tracks
            .into_iter()
            .map(|track| {
                let ours = packets_of(candidate, track);
                let theirs = packets_of(reference, track);
                (track, align(&keys(ours), &keys(theirs), false))
            })
            .collect();
        Comparison {
            candidate,
            reference,
            streams,
        }
    }

    /// Every difference under the compared fields, in a canonical order: per track,
    /// then the order across tracks, the set of tracks and the end.
    fn differences(&self, compare: &[usize]) -> Vec<Difference> {
        let mut out = Vec::new();
        for (track, alignment) in &self.streams {
            let ours = packets_of(self.candidate, *track);
            let theirs = packets_of(self.reference, *track);
            if ours.is_empty() != theirs.is_empty() {
                out.push(Difference {
                    stream: Some(*track),
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
                    stream: Some(*track),
                    candidate: c,
                    reference: r,
                    field,
                    ours,
                    theirs,
                };
                let (Some(c), Some(r)) = (c, r) else {
                    let size = |side: Option<&(usize, Fields)>| {
                        side.map_or(Value::Null, |(_, p)| p[SIZE].clone())
                    };
                    out.push(difference(
                        "unmatched",
                        size(c.map(|c| &ours[c])),
                        size(r.map(|r| &theirs[r])),
                    ));
                    continue;
                };
                let (a, b) = (&ours[c].1, &theirs[r].1);
                for &f in compare {
                    if !crate::harness::diff::equal(&a[f], &b[f]) {
                        out.push(difference(FIELDS[f], a[f].clone(), b[f].clone()));
                    }
                }
            }
        }
        out.extend(self.order());
        if self.candidate.tracks != self.reference.tracks {
            let tracks =
                |tracks: &[i128]| Value::Array(tracks.iter().map(|&t| Value::int(t)).collect());
            out.push(Difference {
                stream: None,
                candidate: None,
                reference: None,
                field: "streams",
                ours: tracks(&self.candidate.tracks),
                theirs: tracks(&self.reference.tracks),
            });
        }
        let ours = self
            .candidate
            .error
            .as_ref()
            .map_or(Value::str("eof"), |e| Value::str(format!("error: {e}")));
        let theirs = Value::str(if self.reference.error.is_some() {
            "error"
        } else {
            "eof"
        });
        if (self.candidate.error.is_some()) != (self.reference.error.is_some()) {
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

    /// The paired packets in each side's delivery order, as (track, candidate place,
    /// reference place); each run of places where the two orders name different packets
    /// is one difference, at its first place, of the two packets' tracks.
    fn order(&self) -> Vec<Difference> {
        let mut paired = Vec::new();
        for (track, alignment) in &self.streams {
            let ours = packets_of(self.candidate, *track);
            let theirs = packets_of(self.reference, *track);
            for &(c, r) in alignment {
                if let (Some(c), Some(r)) = (c, r) {
                    paired.push((*track, ours[c].0, theirs[r].0));
                }
            }
        }
        let mut by_candidate = paired.clone();
        by_candidate.sort_by_key(|p| p.1);
        let mut by_reference = paired;
        by_reference.sort_by_key(|p| p.2);
        let mut out = Vec::new();
        let mut inside = false;
        for (ours, theirs) in by_candidate.iter().zip(&by_reference) {
            let differ = ours.1 != theirs.1;
            if differ && !inside {
                out.push(Difference {
                    stream: None,
                    candidate: Some(ours.1),
                    reference: Some(theirs.2),
                    field: "order",
                    ours: Value::int(ours.0),
                    theirs: Value::int(theirs.0),
                });
            }
            inside = differ;
        }
        out
    }

    fn placed(&self, stream: i128, d: &Difference) -> Option<(&[Pair], usize)> {
        let alignment = &self.streams.iter().find(|(t, _)| *t == stream)?.1;
        let at = alignment
            .iter()
            .position(|&pair| pair == (d.candidate, d.reference))?;
        Some((alignment, at))
    }

    /// Whether the packets around `at` that one side lacks or that differ in size span
    /// the same bytes on both sides.
    fn resplit(&self, stream: i128, alignment: &[Pair], at: usize) -> bool {
        let ours = packets_of(self.candidate, stream);
        let theirs = packets_of(self.reference, stream);
        let broken = |&(c, r): &Pair| match (c, r) {
            (Some(c), Some(r)) => ours[c].1[SIZE] != theirs[r].1[SIZE],
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
        let span = |p: &Fields| {
            number(&p[POSITION])
                .zip(number(&p[SIZE]))
                .map(|(at, size)| (at, at + size))
        };
        // The bytes the packets cover, merged where they are contiguous.
        let extent = |spans: Vec<Option<(i128, i128)>>| -> Option<Vec<(i128, i128)>> {
            let mut spans: Vec<(i128, i128)> = spans.into_iter().collect::<Option<_>>()?;
            spans.sort_unstable();
            let mut merged: Vec<(i128, i128)> = Vec::new();
            for (start, end) in spans {
                match merged.last_mut() {
                    Some(last) if start <= last.1 => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            Some(merged).filter(|m| !m.is_empty())
        };
        let run = &alignment[lo..=hi];
        let candidate = extent(
            run.iter()
                .filter_map(|(c, _)| c.map(|c| span(&ours[c].1)))
                .collect(),
        );
        let reference = extent(
            run.iter()
                .filter_map(|(_, r)| r.map(|r| span(&theirs[r].1)))
                .collect(),
        );
        candidate.is_some() && candidate == reference
    }

    /// Whether `scope` covers difference `d` among all of `found`; `seek` is the track
    /// a seek case seeks on.
    fn covers(
        &self,
        scope: &Scope,
        d: &Difference,
        found: &[Difference],
        seek: Option<i128>,
    ) -> bool {
        if !scope.fields.iter().any(|f| f == d.field) || (scope.seeks && seek.is_none()) {
            return false;
        }
        let offset = number(&d.ours)
            .zip(number(&d.theirs))
            .map(|(ours, theirs)| theirs - ours);
        let relation = match &scope.reference {
            Relation::Any => true,
            Relation::ReferenceAbsent => d.theirs == Value::Null,
            Relation::CandidateAbsent => d.ours == Value::Null,
            Relation::CandidateZero => zero(&d.ours),
            Relation::ReferenceZero => zero(&d.theirs),
            Relation::ConstantOffset => {
                offset.is_some_and(|o| o != 0)
                    && found
                        .iter()
                        .filter(|e| e.stream == d.stream && e.field == d.field)
                        .filter_map(|e| number(&e.ours).zip(number(&e.theirs)))
                        .all(|(ours, theirs)| Some(theirs - ours) == offset)
            }
            Relation::Candidate(texts) => d
                .ours
                .as_str()
                .is_some_and(|ours| texts.iter().any(|t| t == ours)),
        };
        if !relation {
            return false;
        }
        if d.field == "order" {
            return scope.packets == Packets::Order;
        }
        let Some(stream) = d
            .stream
            .filter(|_| d.candidate.is_some() || d.reference.is_some())
        else {
            return scope.packets == Packets::File;
        };
        match scope.packets {
            Packets::File | Packets::Order => false,
            Packets::Any => true,
            Packets::FirstOfStream => d.candidate == Some(0) || d.reference == Some(0),
            Packets::LeadingInReader => {
                d.reference.is_none()
                    && self.placed(stream, d).is_some_and(|(alignment, at)| {
                        alignment[..at].iter().all(|(_, r)| r.is_none())
                    })
            }
            Packets::LeadingInOtherTrack => {
                seek.is_some_and(|track| track != stream)
                    && d.candidate.is_none()
                    && self.placed(stream, d).is_some_and(|(alignment, at)| {
                        alignment[..at].iter().all(|(c, _)| c.is_none())
                    })
            }
            Packets::Resplit => self
                .placed(stream, d)
                .is_some_and(|(alignment, at)| self.resplit(stream, alignment, at)),
        }
    }
}

/// The disposition of each difference that exactly one of `dispositions` covers, and
/// the counts of those none or several cover.
fn attribute(
    comparison: &Comparison<'_>,
    found: &[Difference],
    dispositions: &[&Disposition],
    seek: Option<i128>,
) -> (Vec<Option<String>>, usize, usize) {
    let mut each = Vec::with_capacity(found.len());
    let (mut none, mut several) = (0, 0);
    for d in found {
        let covering: Vec<&str> = dispositions
            .iter()
            .filter(|disposition| {
                disposition
                    .scopes
                    .iter()
                    .any(|scope| comparison.covers(scope, d, found, seek))
            })
            .map(|disposition| disposition.id.as_str())
            .collect();
        match covering.as_slice() {
            [one] => each.push(Some((*one).to_owned())),
            [] => {
                none += 1;
                each.push(None);
            }
            _ => {
                several += 1;
                each.push(None);
            }
        }
    }
    (each, none, several)
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

/// A case's row fields, status and review proposal.
struct Graded {
    fields: Vec<(String, Value)>,
    status: &'static str,
    proposal: Option<Value>,
}

fn grade(rules: &Rules, comparison: &Comparison<'_>, found: &[Difference], case: &Case) -> Graded {
    let rows: Vec<Value> = found.iter().map(Difference::row).collect();
    let digest = list_sha256(&rows);
    let mut fields = vec![
        ("differences".to_owned(), Value::int(found.len() as i128)),
        ("difference_fields".to_owned(), field_counts(found)),
        (
            "first_differences".to_owned(),
            Value::Array(rows.iter().take(SHOWN).cloned().collect()),
        ),
        ("difference_sha256".to_owned(), Value::str(digest.as_str())),
    ];
    if found.is_empty() {
        let refused = comparison.candidate.tracks.is_empty()
            && comparison.candidate.error.is_some()
            && comparison.reference.error.is_some();
        return Graded {
            fields,
            status: if refused { "refused-by-both" } else { "pass" },
            proposal: None,
        };
    }
    let seek = case.seek.map(|(track, _)| i128::from(track));
    let reviewed = rules.reviewed.iter().find(|r| r.name == case.name);
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
    let (mut proposed, _, _) = attribute(comparison, found, &considered, seek);
    // A difference several scopes cover is proposed under the one disposition among
    // those the other differences need.
    let needed: Vec<&Disposition> = considered
        .iter()
        .copied()
        .filter(|d| proposed.iter().flatten().any(|id| *id == d.id))
        .collect();
    if !needed.is_empty() && proposed.iter().any(Option::is_none) {
        let (narrowed, _, _) = attribute(comparison, found, &needed, seek);
        for (each, narrowed) in proposed.iter_mut().zip(narrowed) {
            if each.is_none() {
                *each = narrowed;
            }
        }
    }
    let mut ids: Vec<&str> = proposed.iter().flatten().map(String::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    let proposal = Value::object([
        ("name", Value::str(case.name.as_str())),
        ("sha256", Value::str(case.sha256.as_str())),
        ("dispositions", Value::strings(ids)),
        ("difference_sha256", Value::str(digest.as_str())),
        (
            "differences",
            Value::Array(
                rows.iter()
                    .zip(&proposed)
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
        Some(reviewed) if reviewed.sha256 != case.sha256 => {
            problems.push("reviewed for another input".to_owned());
        }
        Some(reviewed) if reviewed.difference_sha256 != digest => {
            fields.push((
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
            let (each, none, several) = attribute(comparison, found, &listed, seek);
            if none > 0 {
                problems.push(format!(
                    "{none} differences in no listed disposition's scope"
                ));
            }
            if several > 0 {
                problems.push(format!(
                    "{several} differences in more than one listed scope"
                ));
            }
            for id in &reviewed.dispositions {
                if !each.iter().flatten().any(|d| d == id) {
                    problems.push(format!("{id} covers none of the differences"));
                }
            }
            if none + several == 0
                && each
                    .iter()
                    .zip(&reviewed.attributed)
                    .any(|(found, reviewed)| found.as_deref() != Some(reviewed.as_str()))
            {
                problems.push("a difference is attributed other than reviewed".to_owned());
            }
            fields.push((
                "dispositions".into(),
                Value::strings(reviewed.dispositions.clone()),
            ));
        }
    }
    let status = if problems.is_empty() {
        "disposition"
    } else {
        fields.push(("problems".into(), Value::strings(problems)));
        "fail"
    };
    Graded {
        fields,
        status,
        proposal: Some(proposal),
    }
}

/// The arguments of `isobmff-reader`.
pub struct Options {
    pub root: PathBuf,
    pub ffprobe: String,
    pub codec_worker: PathBuf,
    pub out: PathBuf,
    pub only: Option<String>,
}

fn compare_case(options: &Options, rules: &Rules, case: &Case, path: &str) -> Result<Graded, Stop> {
    let Referenced {
        reading: theirs,
        summary,
        bases,
    } = reference(
        &options.ffprobe,
        &rules.ffprobe_options,
        path,
        case.seek,
        case.first_entry_colour,
    )?;
    let mut fields = vec![("ffprobe".to_owned(), summary)];
    let (ours, receipt) = match candidate(
        &options.codec_worker,
        path,
        case.seek,
        &bases,
        case.first_entry_colour,
    )? {
        Ok(reading) => reading,
        Err(error) => {
            fields.push(("candidate_error".into(), Value::str(error)));
            return Ok(Graded {
                fields,
                status: "fail",
                proposal: None,
            });
        }
    };
    if case.seek.is_some() {
        fields.push(("receipt".into(), receipt));
    }
    let counts = |reading: &Reading| {
        Value::Array(
            reading
                .streams
                .iter()
                .map(|(t, p)| Value::Array(vec![Value::int(*t), Value::int(p.len() as i128)]))
                .collect(),
        )
    };
    fields.push(("candidate_packets".into(), counts(&ours)));
    fields.push(("reference_packets".into(), counts(&theirs)));
    fields.push((
        "candidate_end".into(),
        ours.error.clone().map_or(Value::str("eof"), Value::str),
    ));
    let comparison = Comparison::new(&ours, &theirs);
    let found = comparison.differences(&rules.compare);
    let mut graded = grade(rules, &comparison, &found, case);
    if case.first_entry_colour {
        let rows = |values: &[(i128, Value)]| {
            Value::Array(
                values
                    .iter()
                    .map(|(id, value)| Value::Array(vec![Value::int(*id), value.clone()]))
                    .collect(),
            )
        };
        fields.push(("candidate_colour".into(), rows(&ours.colours)));
        fields.push(("reference_colour".into(), rows(&theirs.colours)));
        if ours.colours.is_empty() || ours.colours != theirs.colours {
            graded.status = "fail";
            fields.push((
                "colour_error".into(),
                Value::str("first-entry colour differs or is missing"),
            ));
        }
    }
    fields.append(&mut graded.fields);
    graded.fields = fields;
    Ok(graded)
}

/// Compare every case of the corpus (or those `--only` selects), appending one JSON row
/// per case to `--out` and printing one line each, then the status counts; the
/// unexplained differences go to `OUT.differences.json`. `Ok(1)` when a case fails.
pub fn run(options: &Options) -> Result<i32, String> {
    let rules = Rules::load(&options.root)?;
    let cases = corpus(&options.root)?;
    let only: Option<Vec<&str>> = options.only.as_deref().map(|o| o.split(',').collect());
    if let Some(parent) = options.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.out)
        .map_err(|e| format!("{}: {e}", options.out.display()))?;
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut failed = Vec::new();
    let mut proposals = Vec::new();
    for case in &cases {
        if let Some(only) = &only
            && !only.iter().any(|o| case.name.contains(o))
        {
            continue;
        }
        let path = python::path(&options.root.join(&case.path));
        let mut row = vec![
            ("name".to_owned(), Value::str(case.name.as_str())),
            ("path".to_owned(), Value::str(case.path.as_str())),
            ("sha256".to_owned(), Value::str(case.sha256.as_str())),
        ];
        if let Some((track, micros)) = case.seek {
            row.push((
                "seek".into(),
                Value::object([
                    ("track", Value::int(track)),
                    ("microseconds", Value::int(micros)),
                ]),
            ));
        }
        let status = if !Path::new(&path).is_file() {
            "input-missing"
        } else if file_sha(Path::new(&path))? != case.sha256 {
            "input-hash-mismatch"
        } else {
            match compare_case(options, &rules, case, &path) {
                Ok(graded) => {
                    row.extend(graded.fields);
                    proposals.extend(graded.proposal);
                    graded.status
                }
                Err(Stop::Timeout(argv)) => {
                    row.push(("argv".into(), Value::strings(argv)));
                    "timeout"
                }
                Err(Stop::Fatal(message)) => return Err(message),
            }
        };
        row.push(("status".into(), Value::str(status)));
        let row = Value::Object(row);
        writeln!(out, "{}", json::to_python(&row, None))
            .and_then(|()| out.flush())
            .map_err(|e| format!("{}: {e}", options.out.display()))?;
        let ids = row
            .get("dispositions")
            .and_then(Value::as_array)
            .map(|d| d.iter().map(python::str).collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        println!(
            "{} {status}{}{ids}",
            case.name,
            if ids.is_empty() { "" } else { " " }
        );
        if !matches!(status, "pass" | "refused-by-both" | "disposition") {
            failed.push(case.name.clone());
        }
        match counts.iter_mut().find(|(k, _)| *k == status) {
            Some((_, n)) => *n += 1,
            None => counts.push((status, 1)),
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
    counts.sort_unstable();
    for (status, n) in &counts {
        println!("COUNT {status} {n}");
    }
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    println!(
        "isobmff-reader: {} of {total} cases pass",
        total - failed.len()
    );
    if failed.is_empty() {
        Ok(0)
    } else {
        println!("isobmff-reader: failing: {}", failed.join(", "));
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_corpus_matches_the_contract_counts_and_its_pins() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let cases = corpus(root).unwrap();
        assert!(cases.iter().any(|c| c.seek.is_some()));
        // Inputs a prepared host generates or fetches are under .research; every other
        // one is committed, with the bytes its pin names.
        for case in cases.iter().filter(|c| !c.path.starts_with(".research/")) {
            assert_eq!(
                file_sha(&root.join(&case.path)).unwrap(),
                case.sha256,
                "{}",
                case.name
            );
        }
        let rules = Rules::load(root).unwrap();
        for reviewed in &rules.reviewed {
            assert!(
                cases
                    .iter()
                    .any(|c| c.name == reviewed.name && c.sha256 == reviewed.sha256),
                "{} is reviewed but not a case",
                reviewed.name
            );
        }
    }

    #[test]
    fn times_project_exactly_or_as_a_fraction() {
        assert_eq!(project(3, (1, 1000), (1, 90000)), Value::int(270));
        assert_eq!(project(1, (1, 3), (1, 1000)), Value::str("1000/3"));
        assert_eq!(project(-6, (1, 48000), (1, 24000)), Value::int(-3));
    }

    /// Packets as (place in the delivery, position, size).
    type Placed<'a> = &'a [(usize, i128, i128)];

    fn reading(tracks: &[(i128, Placed<'_>)]) -> Reading {
        let mut reading = Reading::default();
        for (track, packets) in tracks {
            reading.tracks.push(*track);
            for &(at, position, size) in *packets {
                let mut fields: Fields = std::array::from_fn(|_| Value::Null);
                fields[POSITION] = Value::int(position);
                fields[SIZE] = Value::int(size);
                push(&mut reading, *track, at, fields);
            }
        }
        reading
    }

    #[test]
    fn a_run_of_packets_in_another_order_is_one_difference() {
        let ours = reading(&[
            (1, &[(0, 0, 10), (2, 20, 10)]),
            (2, &[(1, 10, 10), (3, 30, 10)]),
        ]);
        let theirs = reading(&[
            (1, &[(0, 0, 10), (1, 20, 10)]),
            (2, &[(2, 10, 10), (3, 30, 10)]),
        ]);
        let comparison = Comparison::new(&ours, &theirs);
        let found = comparison.differences(&[SIZE, POSITION]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].field, "order");
        assert_eq!((found[0].candidate, found[0].reference), (Some(1), Some(1)));
        assert_eq!(
            (found[0].ours.clone(), found[0].theirs.clone()),
            (Value::int(2), Value::int(1))
        );
    }

    #[test]
    fn packets_cut_differently_from_the_same_bytes_are_a_resplit() {
        let ours = reading(&[(1, &[(0, 0, 4), (1, 4, 4), (2, 8, 4)])]);
        let theirs = reading(&[(1, &[(0, 0, 8), (1, 8, 4)])]);
        let comparison = Comparison::new(&ours, &theirs);
        let found = comparison.differences(&[SIZE, POSITION]);
        let scope = Scope {
            fields: vec!["size".into(), "unmatched".into()],
            packets: Packets::Resplit,
            reference: Relation::Any,
            seeks: false,
        };
        assert!(!found.is_empty());
        assert!(
            found
                .iter()
                .all(|d| comparison.covers(&scope, d, &found, None))
        );
        let shifted = reading(&[(1, &[(0, 0, 8), (1, 9, 4)])]);
        let comparison = Comparison::new(&ours, &shifted);
        let found = comparison.differences(&[SIZE, POSITION]);
        assert!(
            !found
                .iter()
                .all(|d| comparison.covers(&scope, d, &found, None))
        );
    }
}
