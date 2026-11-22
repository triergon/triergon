//! ADTS search inputs from the framing and reader corpora.

use std::collections::BTreeSet;
use std::path::Path;

use crate::json::{self, Value};

use super::blocks::{Cases, string};

pub(super) fn adts_fixtures(root: &Path, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, "adts-sync");
    let framing = json::read(&root.join("harness/features/aac-adts-frames/corpus.json"))?;
    let inputs = string(&framing, "directory")?;
    for row in framing
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("missing AAC framing cases")?
    {
        cases.pinned(
            string(row, "id")?,
            string(row, "profile")?,
            &format!("{inputs}/{}", string(row, "file")?),
            string(row, "sha256")?,
            string(row, "rights")?,
            &[],
        );
    }
    let readers = json::read(&root.join("harness/codecs/reader-manifest.json"))?;
    for row in readers
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("missing reader inputs")?
    {
        if string(row, "lane")? == "reader-aac" {
            cases.pinned(
                string(row, "name")?,
                "reader-aac",
                string(row, "path")?,
                string(row, "sha256")?,
                "authored-synthetic",
                &[],
            );
        }
    }

    let frame = [
        0xff, 0xf1, 0x50, 0x80, 0x01, 0x7f, 0xfc, 0xff, 0xf1, 0x50, 0xaa,
    ];
    let protected = [
        0xff, 0xf0, 0x50, 0x80, 0x01, 0x7f, 0xfc, 0x12, 0x34, 0xbb, 0xcc,
    ];
    let bad_length = [0xff, 0xf1, 0x50, 0x80, 0, 0x7f, 0xfc];
    let junk = [0xde, 0xff, 0xf7, 0xbe, 0xef];
    for (id, data) in [
        (
            "junk-prefix-and-sync-payload",
            [&junk[..], &frame, &frame].concat(),
        ),
        ("junk-between-frames", [&frame[..], &junk, &frame].concat()),
        (
            "bad-length-then-frames",
            [&bad_length[..], &frame, &frame].concat(),
        ),
        ("short-final-frame", [&frame[..], &frame[..8]].concat()),
        (
            "protected-after-junk",
            [&junk[..], &protected, &protected].concat(),
        ),
    ] {
        let (path, pin) = cases.authored(&format!("{id}.adts"), &data, write)?;
        cases.pinned(id, "search", &path, &pin, "authored-synthetic", &[]);
    }
    cases.finish(write)
}

pub(super) fn fixtures(root: &Path, id: &str, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, id);
    let mut names = BTreeSet::new();
    let workloads = json::read(&root.join("harness/codecs/workloads.json"))?;
    let copied_workloads = workloads
        .get("workloads")
        .and_then(Value::as_array)
        .ok_or("missing workload inputs")?
        .iter()
        .filter(|row| string(row, "origin").is_ok_and(|origin| origin.starts_with("FATE")))
        .filter_map(|row| string(row, "path").ok())
        .collect::<BTreeSet<_>>();
    let grade = json::read(&root.join("harness/codecs/correctness-manifest.json"))?;
    for row in grade
        .get("vectors")
        .and_then(Value::as_array)
        .ok_or("missing correctness vectors")?
    {
        let lane = string(row, "lane")?;
        let path = string(row, "path")?;
        let selected = if id == "codec-layouts" {
            matches!(
                lane,
                "aac" | "ac3" | "eac3" | "dts" | "flac" | "mp1" | "mp2" | "mp3" | "vorbis" | "opus"
            )
        } else {
            matches!(lane, "ac3" | "eac3")
        };
        if !selected {
            continue;
        }
        let rights = if path.starts_with(".research/fate-samples/")
            || path.starts_with(".research/codec-derived/")
            || path.starts_with(".research/flac-test-files/")
            || copied_workloads.contains(path)
        {
            "fetched-not-redistributed"
        } else {
            "authored-synthetic"
        };
        let args: &[&str] = if id == "ac3-frame-header" {
            &[]
        } else {
            &[lane]
        };
        let mut name = format!("{lane}-{}", string(row, "name")?);
        if !names.insert(name.clone()) {
            let source = path
                .strip_prefix(".research/")
                .and_then(|path| path.split_once('/'))
                .map_or("committed", |(directory, _)| directory);
            name = format!("{name}-{source}");
            if !names.insert(name.clone()) {
                return Err(format!("duplicate audio view vector {name}"));
            }
        }
        cases.pinned(&name, lane, path, string(row, "sha256")?, rights, args);
    }
    if id == "codec-layouts" {
        let index = json::read(&root.join("harness/evidence/sets/codec-layout-inputs.json"))?;
        for row in index
            .get("files")
            .and_then(Value::as_array)
            .ok_or("missing layout files")?
        {
            let path = string(row, "path")?;
            let Some(name) = path.strip_prefix(".research/codec-layout-inputs/") else {
                continue;
            };
            let (lane, _) = name.split_once('-').ok_or("missing layout codec")?;
            cases.pinned(
                &format!("wide-{name}"),
                "wide",
                path,
                string(row, "sha256")?,
                "authored-synthetic",
                &[lane],
            );
        }
    } else if id == "ac3-frame-header" {
        let mut matrix = Vec::new();
        for fscod in 0..=2 {
            matrix.push((format!("fscod-{fscod}"), fscod, 12, 2, 8, false));
        }
        for frmsizecod in 0..38 {
            matrix.push((
                format!("frmsizecod-{frmsizecod:02}"),
                1,
                frmsizecod,
                2,
                8,
                false,
            ));
        }
        for acmod in 0..8 {
            for lfe in [false, true] {
                matrix.push((
                    format!("acmod-{acmod}-lfe-{}", u8::from(lfe)),
                    0,
                    12,
                    acmod,
                    8,
                    lfe,
                ));
            }
        }
        for bsid in [9, 10] {
            matrix.push((format!("bsid-{bsid}"), 0, 12, 2, bsid, false));
        }
        for (name, fscod, frmsizecod, acmod, bsid, lfe) in matrix {
            let frame = dolby_header(fscod, frmsizecod, acmod, bsid, lfe);
            let data = [&frame[..], &frame].concat();
            let (path, pin) = cases.authored(&format!("{name}.ac3"), &data, write)?;
            cases.pinned(&name, "authored", &path, &pin, "authored-synthetic", &[]);
        }
    }
    cases.finish(write)
}

fn dolby_header(fscod: u8, frmsizecod: u8, acmod: u8, bsid: u8, lfe: bool) -> Vec<u8> {
    // A/52 Table 5.18. For 44.1 kHz the high code rounds up by one word.
    let rates = [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640,
    ];
    let rate = rates[usize::from(frmsizecod / 2)];
    let size = 2 * match fscod {
        0 => 2 * rate,
        1 => 320 * rate / 147 + usize::from(frmsizecod % 2),
        _ => 3 * rate,
    };
    let mut frame = vec![0; size];
    frame[0..2].copy_from_slice(&[0x0b, 0x77]);
    frame[4] = (fscod << 6) | frmsizecod;
    frame[5] = bsid << 3;
    let mut metadata = u16::from(acmod) << 13;
    let optional = 2 * u16::from(acmod == 2)
        + 2 * u16::from(acmod >= 4)
        + 2 * u16::from(acmod > 1 && acmod % 2 == 1);
    metadata |= u16::from(lfe) << (12 - optional);
    metadata |= 31 << (7 - optional);
    frame[6..8].copy_from_slice(&metadata.to_be_bytes());
    frame
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: usize,
}

impl Bits {
    fn push(&mut self, value: u32, width: usize) {
        for shift in (0..width).rev() {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            self.bytes[self.used / 8] |= ((value >> shift) as u8 & 1) << (7 - self.used % 8);
            self.used += 1;
        }
    }

    fn flag(&mut self, set: bool) {
        self.push(u32::from(set), 1);
    }

    fn flat_exponents(&mut self, absolute: u32, groups: usize) {
        self.push(absolute, 4);
        for _ in 0..groups {
            self.push(62, 7);
        }
    }
}

fn backwards(mut register: u16) -> u16 {
    for _ in 0..8 {
        let feedback = register & 1;
        register = ((register ^ (feedback * 0x8005)) >> 1) | (feedback << 15);
    }
    register
}

fn close_dolby_crcs(mut frame: Vec<u8>) -> Vec<u8> {
    let words = frame.len() / 2;
    let first_end = 2 * (words / 2 + words / 8);
    let mut needed = 0;
    for &byte in frame[4..first_end].iter().rev() {
        needed = backwards(needed) ^ (u16::from(byte) << 8);
    }
    frame[2..4].copy_from_slice(&backwards(backwards(needed)).to_be_bytes());
    let last = frame.len() - 2;
    let mut residue = 0;
    for &byte in &frame[2..last] {
        residue ^= u16::from(byte) << 8;
        for _ in 0..8 {
            residue = (residue << 1) ^ (0x8005 * (residue >> 15));
        }
    }
    frame[last..].copy_from_slice(&residue.to_be_bytes());
    frame
}

fn zero_width_frame(control: u8, observe: usize, coupled: bool, lfe: bool) -> Vec<u8> {
    let mut bits = Bits::default();
    for (value, width) in [
        (0x0b77, 16),
        (0, 16),
        (16, 8),
        (8, 5),
        (0, 3),
        (2, 3),
        (0, 2),
        (u32::from(lfe), 1),
        (31, 5),
        (0, 8),
    ] {
        bits.push(value, width);
    }
    for block in 0..6 {
        let selected = |tag: u32| block == observe && control & (1_u8 << tag) != 0;
        bits.push(u32::from(selected(0)) * 2, 2);
        bits.push(u32::from(selected(1)), 2);
        bits.flag(selected(2));
        if selected(2) {
            bits.push(17, 8);
        }
        bits.flag(block == 0); // coupling strategy
        if block == 0 {
            bits.flag(coupled);
            if coupled {
                bits.push(3, 2);
                bits.flag(control & 32 != 0);
                bits.push(0x11, 8); // first bin 49, end bin 85, 3 subbands
                bits.push(1, 2); // join the last two subbands
            }
        }
        if coupled {
            for _channel in 0..2 {
                bits.flag(block == 0);
                if block == 0 {
                    bits.push(0x0770, 18);
                }
            }
            if block == 0 && control & 32 != 0 {
                bits.push(2, 2);
            }
        }
        bits.flag(block == 0 || selected(3));
        if block == 0 || selected(3) {
            bits.push(u32::from(selected(3)), if coupled { 3 } else { 4 });
        }
        if coupled {
            bits.push(u32::from(block == 0), 2);
        }
        bits.push(if block == 0 { 5 } else { 0 }, 4);
        if lfe {
            bits.flag(block == 0);
        }
        if block == 0 {
            if coupled {
                bits.flat_exponents(12, 12);
            } else {
                bits.push(0, 12); // two 73-bin channels
            }
            for _channel in 0..2 {
                bits.flat_exponents(15, if coupled { 16 } else { 24 });
                bits.push(0, 2);
            }
            if lfe {
                bits.flat_exponents(15, 2);
            }
        }
        bits.flag(block == 0);
        if block == 0 {
            bits.push(0, 11);
        }
        bits.flag(block == 0);
        if block == 0 {
            bits.push(0, 6);
            for _channel in 0..2 + usize::from(coupled) + usize::from(lfe) {
                bits.push(0, 7);
            }
        }
        if coupled {
            bits.flag(block == 0);
            if block == 0 {
                bits.push(0, 6);
            }
        }
        bits.flag(selected(4));
        if selected(4) {
            if coupled {
                bits.push(2, 2);
            }
            bits.push(6, 4); // new segment on left, none on right
            bits.push(0, 3); // one segment
            bits.push(0x00c, 12); // first band length one, +6 dB
        }
        bits.flag(selected(6));
        if selected(6) {
            bits.push(2, 9);
            bits.push(0x015a, 16);
        }
    }
    bits.bytes.resize(512, 0);
    close_dolby_crcs(bits.bytes)
}

pub(super) fn audblk_fixtures(root: &Path, write: bool) -> Result<(), String> {
    let mut cases = Cases::new(root, "ac3-features");
    for (id, control, block, coupled, lfe) in [
        ("no-flags", 0, 0, false, false),
        ("switch-first", 1, 0, false, true),
        ("switch-last", 1, 5, false, false),
        ("dither-last", 2, 5, false, false),
        ("range-last", 4, 5, false, false),
        ("rematrix-last", 8, 5, false, false),
        ("delta-first", 16, 0, false, true),
        ("delta-last", 16, 5, true, false),
        ("plain-coupling", 0, 0, true, false),
        ("phase-flags", 32, 0, true, false),
        ("skip-first", 64, 0, false, false),
        ("skip-last", 64, 5, false, false),
        ("mixed", 127, 3, true, true),
    ] {
        let data = zero_width_frame(control, block, coupled, lfe);
        let (path, pin) = cases.authored(&format!("{id}.ac3"), &data, write)?;
        cases.pinned(id, "zero-width", &path, &pin, "authored-synthetic", &[]);
    }
    // Two independent frames ensure the union of tools starts afresh.
    let data = [
        zero_width_frame(127, 5, true, true),
        zero_width_frame(0, 0, false, false),
    ]
    .concat();
    let (path, pin) = cases.authored("feature-reset.ac3", &data, write)?;
    cases.pinned(
        "feature-reset",
        "zero-width",
        &path,
        &pin,
        "authored-synthetic",
        &[],
    );
    cases.finish(write)
}
