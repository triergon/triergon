//! Seek cases from the registered seek suite and authored ISO BMFF tables.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::harness::linux::file_sha;
use crate::isobmff_bench::{strings, text};
use crate::json::{self, Value};

type R<T> = Result<T, String>;

const TRACK: &str = "harness/features/mov-track-seek";
const SOURCE: &str = "harness/features/isobmff-source-seek";
const TESTS: &str = "crates/isobmff/tests/fixtures";
const MAX_LOOKUPS: usize = 64;
const PLAIN_AND_INDEX: &[&str] = &[
    "r-front-stco.mov",
    "r-end-co64.mov",
    "r-constant-stsz.mov",
    "r-ctts-v0.mov",
    "r-ctts-v1.mov",
    "r-negative-cts.mov",
    "r-multitrack.mov",
    "r-multiple-mdats.mov",
    "r-stss-absent.mov",
    "r-stss-empty.mov",
    "r-stz2-4.mov",
    "r-stz2-8.mov",
    "r-stz2-16.mov",
    "r-vfr.mov",
    "r-edits-empty-trim.mov",
    "r-edits-repeat.mov",
    "r-elst-v1.mov",
    "edit-leading-empty-one.mov",
    "edit-single-late.mov",
    "edit-single-one.mov",
    "edit-single-trim.mov",
    "edit-single-two.mov",
    "edit-two-repeat-one.mov",
    "f-explicit.mp4",
    "f-tfhd-defaults.mp4",
    "f-trex-defaults.mp4",
    "f-first-flags.mp4",
    "f-multitrack.mp4",
    "f-ctts-v0.mp4",
    "f-ctts-v1.mp4",
    "f-tfdt64.mp4",
    "f-tfra-mfra.mp4",
    "r-description-change.mov",
    "f-no-second-tfdt.mp4",
];
const SOURCE_TABLES: &[&str] = &[
    "r-ctts-v0.mov",
    "r-ctts-v1.mov",
    "r-negative-cts.mov",
    "r-stss-absent.mov",
    "r-stss-empty.mov",
    "r-vfr.mov",
    "r-multitrack.mov",
    "r-edits-empty-trim.mov",
    "r-edits-repeat.mov",
    "r-elst-v1.mov",
];

#[derive(Clone, PartialOrd, Ord, PartialEq, Eq)]
struct Input {
    file: String,
    sha: String,
}

#[derive(Default)]
struct Cases {
    rows: Vec<Value>,
    seen: BTreeSet<(String, Vec<String>)>,
}

impl Cases {
    fn add(&mut self, input: &Input, id: &str, profile: &str, args: Vec<String>) {
        if self.seen.insert((input.sha.clone(), args.clone())) {
            self.rows.push(Value::object([
                ("id", Value::str(id)),
                ("profile", Value::str(profile)),
                ("file", Value::str(&input.file)),
                ("sha256", Value::str(&input.sha)),
                ("rights", Value::str("authored-synthetic")),
                ("arguments", Value::strings(args)),
            ]));
        }
    }

    fn save(self, out: &Path, path: &str) -> R<()> {
        let dir = out.join(path);
        fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        json::write(
            &dir.join("corpus.json"),
            &Value::object([
                ("schema", Value::int(1)),
                ("directory", Value::str(".")),
                ("cases", Value::Array(self.rows)),
            ]),
        )
    }
}

pub(super) fn author(root: &Path, out: &Path) -> R<()> {
    let listing = json::read(&root.join("harness/features/isobmff-seek/corpus.json"))?;
    let mut seek = Cases::default();
    let mut source = Cases::default();
    let mut existing = BTreeMap::new();
    for row in listing
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("seek cases absent")?
    {
        let entry = row.get("input").ok_or("seek input absent")?;
        let input = Input {
            file: format!("{}/{}", text(entry, "directory")?, text(entry, "path")?),
            sha: text(entry, "sha256")?.into(),
        };
        let id = text(row, "id")?;
        let label = text(row, "case")?
            .rsplit_once("-t")
            .ok_or("missing seek label")?
            .0;
        let label = label.strip_prefix("plan-").unwrap_or(label);
        existing
            .entry(input.clone())
            .or_insert_with(|| label.to_owned());
        let args = strings(row, "arguments")?;
        let [track, ticks, _, eof, window, _, _, quantum] = args.as_slice() else {
            return Err(format!("{id}: incomplete seek arguments"));
        };
        let options = strings(row, "candidate_leading_arguments")?;
        let recovery = if options.iter().any(|option| option == "--bounded-timing") {
            "recovery"
        } else {
            "strict"
        };
        seek.add(
            &input,
            id,
            "index",
            [track.as_str(), ticks, eof, window, quantum, recovery]
                .map(str::to_owned)
                .to_vec(),
        );
    }

    for (input, label) in existing {
        let data = read(root, &input.file)?;
        let Some(layout) = layout(&data)? else {
            continue;
        };
        for track in &layout.tracks {
            // The original suite sometimes makes only one track its seek target.
            // The one-track comparison also exercises every other stream.
            if !seek
                .seen
                .iter()
                .any(|(sha, args)| *sha == input.sha && args[0] == track.id.to_string())
            {
                add_track(&mut seek, &input, &label, track, false);
            }
            if !layout.fragmented {
                add_source(&mut source, &input, &label, track);
            }
        }
    }

    for name in PLAIN_AND_INDEX {
        let file = format!("{TESTS}/{name}");
        let input = Input {
            sha: file_sha(&root.join(&file))?,
            file,
        };
        let data = read(root, &input.file)?;
        let layout = layout(&data)?.ok_or_else(|| format!("{name}: no movie box"))?;
        let label = name.split('.').next().ok_or("missing fixture label")?;
        for track in &layout.tracks {
            add_track(&mut seek, &input, label, track, true);
            if SOURCE_TABLES.contains(name) {
                add_source(&mut source, &input, label, track);
            }
        }
    }
    for (input, label) in variants(root, out)? {
        let track = Track {
            id: 1,
            count: 0,
            sync: Vec::new(),
            dts: Vec::new(),
            pts: Vec::new(),
        };
        add_track(&mut seek, &input, label, &track, true);
    }
    tied_composition(root, out, &mut source)?;
    seek.save(out, TRACK)?;
    source.save(out, SOURCE)
}

fn add_track(cases: &mut Cases, input: &Input, label: &str, track: &Track, tiny: bool) {
    let mut ticks = BTreeSet::from([
        i64::MIN + 1,
        -1,
        0,
        1000,
        2000,
        2999,
        3000,
        3500,
        5000,
        5500,
        1 << 40,
    ]);
    let entries = track.dts.len();
    for &time in track
        .dts
        .iter()
        .step_by(entries.div_ceil(16).max(1))
        .chain(track.dts.last())
    {
        ticks.extend([time.saturating_sub(1), time, time.saturating_add(1)]);
    }
    for time in ticks {
        for (eof, window, quantum) in [
            ("0", "65536", "4096"),
            ("1", "65536", "4096"),
            ("0", "1", "3"),
        ] {
            if !tiny && window == "1" {
                continue;
            }
            let id = format!("unit-{label}-t{}-at{time}-eof{eof}-w{window}", track.id);
            cases.add(
                input,
                &id,
                "index",
                [
                    track.id.to_string(),
                    time.to_string(),
                    eof.into(),
                    window.into(),
                    quantum.into(),
                    "strict".into(),
                ]
                .to_vec(),
            );
        }
    }
}

fn add_source(cases: &mut Cases, input: &Input, label: &str, track: &Track) {
    let mut targets = BTreeSet::from([1, track.count.max(1), track.count + 1]);
    for &sync in &track.sync {
        targets.extend((sync.saturating_sub(1).max(1)..=sync + 1).filter(|&n| n <= track.count));
    }
    let key = format!("{label}-t{}", track.id);
    for target in targets {
        for preroll in BTreeSet::from([0, 1, 7, target + 1]) {
            let id = format!("{key}-plan-n{target}-p{preroll}");
            cases.add(
                input,
                &id,
                "plan",
                [
                    "plan".into(),
                    track.id.to_string(),
                    target.to_string(),
                    preroll.to_string(),
                    "65536".into(),
                    "4096".into(),
                ]
                .to_vec(),
            );
        }
    }

    add_lookup(cases, input, &key, track);
}

fn add_lookup(cases: &mut Cases, input: &Input, key: &str, track: &Track) {
    let distinct: Vec<_> = track
        .pts
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut queries = BTreeSet::from([
        distinct
            .first()
            .map_or(-1, |&first| first.saturating_sub(1)),
        distinct.last().map_or(1, |&last| last.saturating_add(1)),
    ]);
    queries.extend(distinct.iter().copied());
    queries.extend(distinct.windows(2).filter_map(|w| {
        let midway = i64::midpoint(w[0], w[1]);
        (midway > w[0]).then_some(midway)
    }));
    let queries: Vec<_> = queries.into_iter().collect();
    // Individual small cases isolate a failed lookup. Large fixtures share one
    // reference packet scan per bounded group and fold every query's result.
    let batch = if queries.len() > MAX_LOOKUPS {
        MAX_LOOKUPS
    } else {
        1
    };
    for group in queries.chunks(batch) {
        let id = format!("{key}-lookup-{}", group[0]);
        let mut args = vec![
            "lookup".into(),
            track.id.to_string(),
            "65536".into(),
            "4096".into(),
        ];
        args.extend(group.iter().map(i64::to_string));
        cases.add(input, &id, "lookup", args);
    }
}

struct Track {
    id: u32,
    count: u32,
    sync: Vec<u32>,
    dts: Vec<i64>,
    pts: Vec<i64>,
}

struct Layout {
    fragmented: bool,
    tracks: Vec<Track>,
}

fn layout(data: &[u8]) -> R<Option<Layout>> {
    // Two existing seek inputs have invalid root lengths and no movie to describe.
    let Ok(roots) = boxes(data) else {
        return Ok(None);
    };
    let Some(movie) = find(&roots, b"moov") else {
        return Ok(None);
    };
    let children = boxes(movie)?;
    let fragmented = find(&roots, b"moof").is_some();
    let tracks = children
        .iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, track)| track_layout(track, fragmented))
        .collect::<R<Vec<_>>>()?;
    Ok(Some(Layout { fragmented, tracks }))
}

fn track_layout(track: &[u8], fragmented: bool) -> R<Track> {
    let header = locate(track, &[*b"tkhd"])?;
    let id = number(header, if header[0] == 1 { 20 } else { 12 })?;
    let mut result = Track {
        id,
        count: 0,
        sync: Vec::new(),
        dts: Vec::new(),
        pts: Vec::new(),
    };
    if fragmented {
        return Ok(result);
    }
    let table = boxes(locate(track, &[*b"mdia", *b"minf", *b"stbl"])?)?;
    let size = find(&table, b"stsz")
        .or_else(|| find(&table, b"stz2"))
        .ok_or("missing sample size table")?;
    result.count = number(size, 8)?;
    if find(&table, b"stsz").is_some() && number(size, 4)? == 0 {
        // Retain the malformed size-table case. Derive its requested ordinals
        // from the entries carried, even when the declared count is impossible.
        let stored = u32::try_from(size.len().saturating_sub(12) / 4)
            .map_err(|_| "sample size count overflow")?;
        result.count = result.count.min(stored);
    }
    if result.count > 10_000 {
        return Err(format!(
            "seek fixture track {id} exceeds the authoring sample limit"
        ));
    }
    result.sync = if let Some(sync) = find(&table, b"stss") {
        words(sync, 4)?
            .map(|row| number(row, 0))
            .collect::<R<_>>()?
    } else {
        (1..=result.count).collect()
    };
    let delta = find(&table, b"stts").ok_or("missing sample decode times")?;
    let durations = expand(delta, result.count, false)?;
    let offsets = find(&table, b"ctts")
        .map(|table| expand(table, result.count, table[0] == 1))
        .transpose()?;
    let mut time = 0;
    for (index, duration) in durations.into_iter().enumerate() {
        result.dts.push(time);
        if let Some(offset) = offsets
            .as_ref()
            .map_or(Some(0), |runs| runs.get(index).copied())
        {
            result.pts.push(time + offset);
        }
        time += duration;
    }
    Ok(result)
}

fn expand(table: &[u8], count: u32, signed: bool) -> R<Vec<i64>> {
    let mut samples = Vec::new();
    for row in words(table, 8)? {
        let run = number(row, 0)? as usize;
        let value = number(row, 4)?;
        let value = if signed {
            i64::from(value as i32)
        } else {
            i64::from(value)
        };
        samples.extend(std::iter::repeat_n(
            value,
            run.min(count as usize - samples.len()),
        ));
    }
    Ok(samples)
}

fn words(data: &[u8], width: usize) -> R<impl Iterator<Item = &[u8]>> {
    let count = number(data, 4)? as usize;
    let rows = data.get(8..).ok_or("missing sample table entries")?;
    let end = count
        .checked_mul(width)
        .ok_or("sample table size overflow")?;
    Ok(rows
        .get(..end)
        .ok_or("truncated sample table entries")?
        .chunks_exact(width))
}

fn read(root: &Path, file: &str) -> R<Vec<u8>> {
    fs::read(root.join(file)).map_err(|err| format!("{file}: {err}"))
}

fn tied_composition(root: &Path, out: &Path, cases: &mut Cases) -> R<()> {
    let mut data = read(root, &format!("{TESTS}/r-ctts-v0.mov"))?;
    // Samples one and two both present at tick 1000 after this offset change.
    patch(&mut data, b"ctts", 12, &1000_u32.to_be_bytes())?;
    let layout = layout(&data)?.ok_or("missing composition variant movie")?;
    let file = format!("{SOURCE}/fixtures/tied-composition.mov");
    let path = out.join(&file);
    let dir = path
        .parent()
        .ok_or("missing composition fixture directory")?;
    fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    fs::write(&path, data).map_err(|err| err.to_string())?;
    let input = Input {
        file,
        sha: file_sha(&path)?,
    };
    for track in &layout.tracks {
        add_lookup(
            cases,
            &input,
            &format!("tied-composition-t{}", track.id),
            track,
        );
    }
    Ok(())
}

fn number(data: &[u8], at: usize) -> R<u32> {
    let word = data.get(at..at + 4).ok_or("short ISO BMFF word")?;
    Ok(u32::from_be_bytes(
        word.try_into().map_err(|_| "short ISO BMFF word")?,
    ))
}

type BoxSlice<'a> = ([u8; 4], &'a [u8]);

fn boxes(mut data: &[u8]) -> R<Vec<BoxSlice<'_>>> {
    let mut boxes = Vec::new();
    while !data.is_empty() {
        let size = number(data, 0)?;
        let kind = data.get(4..8).ok_or("short ISO BMFF box header")?;
        let (size, header) = match size {
            0 => (data.len(), 8),
            1 => {
                let size = (u64::from(number(data, 8)?) << 32) | u64::from(number(data, 12)?);
                (usize::try_from(size).map_err(|_| "box size overflow")?, 16)
            }
            size => (size as usize, 8),
        };
        let payload = data
            .get(header..size)
            .ok_or("invalid ISO BMFF box length")?;
        boxes.push((
            kind.try_into().map_err(|_| "short ISO BMFF box kind")?,
            payload,
        ));
        data = &data[size..];
    }
    Ok(boxes)
}

fn find<'a>(boxes: &[BoxSlice<'a>], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes
        .iter()
        .find(|(tag, _)| tag == kind)
        .map(|(_, data)| *data)
}

fn locate<'a>(mut data: &'a [u8], path: &[[u8; 4]]) -> R<&'a [u8]> {
    for kind in path {
        data = find(&boxes(data)?, kind)
            .ok_or_else(|| format!("missing {}", String::from_utf8_lossy(kind)))?;
    }
    Ok(data)
}

fn variants(root: &Path, out: &Path) -> R<Vec<(Input, &'static str)>> {
    let dir = out.join(TRACK).join("fixtures");
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let end = read(root, &format!("{TESTS}/r-end-co64.mov"))?;
    let front = read(root, &format!("{TESTS}/r-front-stco.mov"))?;
    let mut overflow = end.clone();
    patch(
        &mut overflow,
        b"co64",
        24,
        &(u64::MAX - 0x1000).to_be_bytes(),
    )?;
    patch(&mut overflow, b"stsz", 32, &u32::MAX.to_be_bytes())?;
    let mut position = end;
    patch(&mut position, b"co64", 24, &(1_u64 << 63).to_be_bytes())?;
    let mut size = front;
    patch(&mut size, b"stsz", 32, &(1_u32 << 29).to_be_bytes())?;
    let mut null = read(root, &format!("{TESTS}/f-tfdt64.mp4"))?;
    let offsets: Vec<_> = null
        .windows(4)
        .enumerate()
        .filter_map(|(at, bytes)| (bytes == b"tfdt").then_some(at + 8))
        .collect();
    for (index, at) in offsets.into_iter().enumerate() {
        null[at..at + 8].copy_from_slice(&((1_u64 << 63) + 3000 * index as u64).to_be_bytes());
    }
    [
        ("sample-end-overflow", overflow),
        ("signed-position", position),
        ("signed-size", size),
        ("null-index-clock", null),
    ]
    .into_iter()
    .map(|(label, bytes)| {
        let file = format!("{TRACK}/fixtures/{label}.mov");
        let path = out.join(&file);
        fs::write(&path, bytes).map_err(|err| err.to_string())?;
        Ok((
            Input {
                file,
                sha: file_sha(&path)?,
            },
            label,
        ))
    })
    .collect()
}

fn patch(data: &mut [u8], kind: &[u8; 4], offset: usize, value: &[u8]) -> R<()> {
    let tag = data
        .windows(4)
        .position(|bytes| bytes == kind)
        .ok_or("missing variant box")?;
    let at = tag + 4 + offset;
    data.get_mut(at..at + value.len())
        .ok_or("short variant table")?
        .copy_from_slice(value);
    Ok(())
}
