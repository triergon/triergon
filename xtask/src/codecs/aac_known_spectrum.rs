//! Independent single-coefficient checks for compact AAC synthesis and band placement.

use std::f64::consts::PI;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::{json, python};

const LONG: [usize; 50] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960,
];
const SHORT: [usize; 15] = [0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 120];
const OP: &str = "audio-aac-access-units-compact-raw";
const TOLERANCE: f64 = 2e-8;

struct Bits(Vec<u8>);
impl Bits {
    fn put(&mut self, value: usize, count: usize) {
        for bit in (0..count).rev() {
            self.0.push(((value >> bit) & 1) as u8);
        }
    }
    fn bytes(mut self) -> Vec<u8> {
        while !self.0.len().is_multiple_of(8) {
            self.0.push(0);
        }
        self.0
            .chunks(8)
            .map(|bits| bits.iter().fold(0, |a, b| (a << 1) | b))
            .collect()
    }
}
fn section(bits: &mut Bits, book: usize, count: usize, short: bool) {
    let n = if short { 3 } else { 5 };
    let escape = (1 << n) - 1;
    bits.put(book, 4);
    let mut remaining = count;
    while remaining >= escape {
        bits.put(escape, n);
        remaining -= escape;
    }
    bits.put(remaining, n);
}
fn packet(band: usize, width: usize, short: bool, window: usize, last: bool) -> Vec<u8> {
    let mut b = Bits(Vec::new());
    b.put(0, 3);
    b.put(0, 4);
    b.put(160, 8);
    b.put(0, 1);
    b.put(if short { 2 } else { 0 }, 2);
    b.put(0, 1);
    b.put(band + 1, if short { 4 } else { 6 });
    if short {
        b.put(0, 7);
    } else {
        b.put(0, 1);
    }
    for group in 0..if short { 8 } else { 1 } {
        if group == window {
            if band > 0 {
                section(&mut b, 0, band, short);
            }
            section(&mut b, 1, 1, short);
        } else {
            section(&mut b, 0, band + 1, short);
        }
    }
    b.put(0, 1); // scalefactor delta zero
    b.put(0, 3); // pulse, TNS and gain control absent
    for tuple in 0..width / 4 {
        if (!last && tuple == 0) || (last && tuple == width / 4 - 1) {
            b.put(if last { 0x14 } else { 0x10 }, 5);
        } else {
            b.put(0, 1);
        }
    }
    b.put(7, 3);
    b.bytes()
}
fn silent(sequence: usize) -> Vec<u8> {
    let mut b = Bits(Vec::new());
    b.put(0, 3);
    b.put(0, 4);
    b.put(100, 8);
    b.put(0, 1);
    b.put(sequence, 2);
    b.put(0, 1);
    b.put(0, 6);
    b.put(0, 1);
    b.put(0, 3);
    b.put(7, 3);
    b.bytes()
}
fn envelope(short: bool, probe: &[u8], config: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    for word in [48_000u32, 1, 3, config.len() as u32] {
        data.extend_from_slice(&word.to_le_bytes());
    }
    data.extend_from_slice(config);
    for au in [
        silent(usize::from(short)),
        probe.to_vec(),
        silent(if short { 3 } else { 0 }),
    ] {
        data.extend_from_slice(&(au.len() as u32).to_le_bytes());
        data.extend_from_slice(&au);
        data.extend_from_slice(&[0; 64]);
    }
    data
}
fn decode(worker: &Path, input: &Path, pcm: &Path) -> Result<Vec<f32>, String> {
    let result = Command::new(worker)
        .arg(OP)
        .arg(input)
        .arg(pcm)
        .output()
        .map_err(|e| e.to_string())?;
    if !result.status.success() || !result.stderr.is_empty() {
        return Err(format!(
            "{}: status {}, diagnostics: {}",
            worker.display(),
            result.status,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let row = json::parse(std::str::from_utf8(&result.stdout).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if row.get("packets").and_then(python::int) != Some(3)
        || row.get("bytes").and_then(python::int) != Some(2880 * 4)
        || row
            .get("speakers")
            .and_then(json::Value::as_array)
            .is_none_or(|speakers| speakers.len() != 1 || python::int(&speakers[0]) != Some(2))
    {
        return Err(format!(
            "{}: unexpected AAC output metadata",
            worker.display()
        ));
    }
    let bytes = fs::read(pcm).map_err(|e| e.to_string())?;
    if bytes.len() != 2880 * 4 {
        return Err(format!("{}: unexpected PCM length", worker.display()));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .collect())
}
fn expected(i: usize, short: bool, window: usize, bin: usize) -> f64 {
    let n = if short { 120 } else { 960 };
    let offset = 960 + if short { 420 + 120 * window } else { 0 };
    if !(offset..offset + 2 * n).contains(&i) {
        return 0.0;
    }
    let j = i - offset;
    let wi = if j < n { j } else { 2 * n - 1 - j };
    let window = (PI * (wi as f64 + 0.5) / (2 * n) as f64).sin();
    (PI / n as f64 * (j as f64 + 0.5 + n as f64 / 2.0) * (bin as f64 + 0.5)).cos() * window
        / n as f64
}
pub fn run(root: &Path, out: &Path) -> Result<(), String> {
    let (candidate, reference) = super::aac_access_units::compact_workers(root)?;
    fs::create_dir(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let input = out.join("probe.aac-access-units");
    let candidate_pcm = out.join("candidate.pcm");
    let reference_pcm = out.join("reference.pcm");
    let mut worst = [0.0f64; 3];
    let mut count = 0;
    for short in [false, true] {
        let edges: &[usize] = if short { &SHORT } else { &LONG };
        for band in 0..edges.len() - 1 {
            for window in 0..if short { 8 } else { 1 } {
                for last in [false, true] {
                    let bin = if last {
                        edges[band + 1] - 1
                    } else {
                        edges[band]
                    };
                    let au = packet(band, edges[band + 1] - edges[band], short, window, last);
                    let candidate_data = envelope(
                        short,
                        &au,
                        &[0x11, 0x84, 0x04, 0xc4, 0x00, 0x20, 0x01, 0xe0, 0x00],
                    );
                    fs::write(&input, &candidate_data).map_err(|e| e.to_string())?;
                    let a = decode(&candidate, &input, &candidate_pcm)?;
                    if fs::read(&input).map_err(|e| e.to_string())? != candidate_data {
                        return Err("AAC candidate input changed during comparison".into());
                    }
                    let data = envelope(short, &au, &[0x11, 0x8c]);
                    fs::write(&input, &data).map_err(|e| e.to_string())?;
                    let b = decode(&reference, &input, &reference_pcm)?;
                    if fs::read(&input).map_err(|e| e.to_string())? != data {
                        return Err("AAC input changed during comparison".into());
                    }
                    let mut errors = [0.0f64; 3];
                    for i in 0..2880 {
                        let (a, b) = (f64::from(a[i]), f64::from(b[i]));
                        if !a.is_finite() || !b.is_finite() {
                            return Err(format!("non-finite PCM at {i}"));
                        }
                        let oracle = expected(i, short, window, bin);
                        for (maximum, error) in errors.iter_mut().zip([
                            (a - oracle).abs(),
                            (b - oracle).abs(),
                            (a - b).abs(),
                        ]) {
                            *maximum = maximum.max(error);
                        }
                    }
                    println!(
                        "{} band={band} window={window} bin={bin} candidate={:.9e} reference={:.9e} cross={:.9e}",
                        if short { "short" } else { "long" },
                        errors[0],
                        errors[1],
                        errors[2]
                    );
                    if errors.iter().any(|&e| e >= TOLERANCE) {
                        return Err(format!(
                            "known-spectrum comparison failed at band {band}, window {window}, bin {bin}"
                        ));
                    }
                    for (maximum, error) in worst.iter_mut().zip(errors) {
                        *maximum = maximum.max(error);
                    }
                    count += 1;
                }
            }
        }
    }
    println!(
        "{count} cases passed; worst candidate={:.9e} reference={:.9e} cross={:.9e}",
        worst[0], worst[1], worst[2]
    );
    Ok(())
}
