//! Capture correctness comparison of one required case.
//!
//! Runs the case through the Rust worker (candidate) and the FFmpeg reference
//! worker, then applies the rules of `harness/capture/contract.json`
//! `comparison_rules`:
//! - conversion: every Y/U/V sample within ±1 of the reference conversion;
//! - mux: own TS checker on both outputs (sync, continuity, PCR = DTS*300 on
//!   each PES start, PTS/DTS per frame, table cadence, PSI bytes) plus an
//!   FFmpeg re-demux; PES payload equals the elementary stream;
//! - encoder: error-free independent FFmpeg decode of every frame, identical
//!   picture types, VBV underflow no worse than the reference, and (rule v2)
//!   PSNR-Y ≥ reference − 0.25 dB where the candidate uses ≤ 1.03× the
//!   reference bits or ≤ 1.03× the requested budget; if the reference spent
//!   more, the candidate is re-run at the [`ENCODER_RATE_FACTORS`] and its
//!   PSNR interpolated (log-linear in bits) at the reference's bits. The v1
//!   verdict is kept alongside in every report;
//! - pipeline: all of the above on one complete session.
//!
//! Writes one JSON report keeping every observation, failures included, and
//! prints a one-line summary.

use std::fs;
use std::path::Path;

use super::ts::{self, Facts};
use crate::codecs::correctness::file_sha;
use crate::json::{self, Value};
use crate::process::{check_output, run_text};
use crate::{numpy, python};

/// The requested-rate factors of the encoder rule's equal-bits search, as
/// `harness/capture/contract.json` records them (`encoder_rate_factors`).
pub const ENCODER_RATE_FACTORS: [f64; 6] = [1.25, 1.6, 2.0, 2.6, 3.4, 4.5];

/// The arguments of `capture compare`.
pub struct Options {
    pub candidate: String,
    pub reference: String,
    pub ffmpeg_bin: String,
    pub case: String,
    pub input: String,
    pub out: String,
}

/// `os.path.join(directory, name)`.
fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() || directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{path}: {e}"))
}

/// Mean squared difference over one plane, as numpy computes it (an exact
/// integer sum divided by the count), then PSNR in dB; infinite when equal.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let sum: i64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = i64::from(x) - i64::from(y);
            d * d
        })
        .sum();
    let mse = sum as f64 / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// `10 ** (-p / 10)`: the base stays a run-time value so the compiler keeps
/// the `pow` call Python makes.
fn power(p: f64) -> f64 {
    std::hint::black_box(10.0f64).powf(-p / 10.0)
}

/// Mean PSNR over frames from the mean of their squared-error ratios.
fn mean_psnr(values: &[f64]) -> f64 {
    let ratios: Vec<f64> = values
        .iter()
        .filter(|p| **p != f64::INFINITY)
        .map(|&p| power(p))
        .collect();
    let mse = if ratios.is_empty() {
        0.0
    } else {
        numpy::mean(&ratios)
    };
    if mse == 0.0 {
        f64::INFINITY
    } else {
        -10.0 * mse.log10()
    }
}

/// Y, U and V planes of the first `frames` YUV 4:2:0 pictures.
fn planes(data: &[u8], w: usize, h: usize, frames: usize) -> Result<Vec<[&[u8]; 3]>, String> {
    let size = w * h * 3 / 2;
    (0..frames)
        .map(|i| {
            let f = data
                .get(i * size..(i + 1) * size)
                .ok_or("buffer is smaller than requested size")?;
            Ok([
                &f[..w * h],
                &f[w * h..w * h + w * h / 4],
                &f[w * h + w * h / 4..],
            ])
        })
        .collect()
}

/// Variable-rate VBV (H.262 Annex C): total underflow bits.
pub fn vbv(sizes_bits: &[i64], max_rate: i64, buffer_bits: i64) -> f64 {
    let buffer = buffer_bits as f64;
    let mut fullness = buffer;
    let mut under = 0.0;
    for &b in sizes_bits {
        let b = b as f64;
        if b > fullness {
            under += b - fullness;
            fullness = 0.0;
        } else {
            fullness -= b;
        }
        fullness = buffer.min(fullness + max_rate as f64 / 30.0);
    }
    under
}

/// Python's `s[start:end]` on an ASCII string.
fn slice(s: &str, start: i64, end: Option<i64>) -> &str {
    let len = s.len() as i64;
    let clamp = |i: i64| if i < 0 { (len + i).max(0) } else { i.min(len) } as usize;
    let (a, b) = (clamp(start), end.map_or(len as usize, clamp));
    if a >= b { "" } else { &s[a..b] }
}

fn int_field(case: &Value, key: &str) -> Result<(i64, String), String> {
    let value = case.get(key).ok_or_else(|| format!("case without {key}"))?;
    let number = python::int(value).ok_or_else(|| format!("case {key} is not a number"))? as i64;
    Ok((number, python::str(value)))
}

fn ints(values: &[i64]) -> Value {
    Value::Array(values.iter().map(|&v| Value::int(v)).collect())
}

fn floats(values: &[f64]) -> Value {
    Value::Array(values.iter().map(|&v| Value::float(v)).collect())
}

fn ffprobe_packets(ffprobe: &str, path: &str) -> Result<Vec<Value>, String> {
    let out = check_output(
        &[
            ffprobe,
            "-v",
            "error",
            "-show_packets",
            "-show_entries",
            "packet=pts,dts,size,flags",
            "-of",
            "json",
            path,
        ]
        .map(String::from),
    )?;
    Ok(python::loads(&out)
        .and_then(|v| {
            v.get("packets")
                .and_then(Value::as_array)
                .map(<[Value]>::to_vec)
        })
        .ok_or("ffprobe printed no packets")?)
}

fn decode(ffmpeg: &str, path: &str, out: &str) -> Result<(i64, String), String> {
    let done = run_text(
        &[
            ffmpeg,
            "-v",
            "error",
            "-err_detect",
            "+crccheck+bitstream+buffer+explode",
            "-i",
            path,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-y",
            out,
        ]
        .map(String::from),
        std::time::Duration::from_secs(u64::MAX / 4),
    )
    .map_err(|e| format!("{e:?}"))?;
    Ok((done.code, done.stderr))
}

fn types(facts: &Facts) -> String {
    facts
        .pes
        .iter()
        .map(|p| match ts::picture_type(&p.data) {
            Some(1) => 'I',
            Some(2) => 'P',
            _ => '?',
        })
        .collect()
}

struct Role {
    ts: String,
    facts: Facts,
}

struct Encoded {
    decode_exit: i64,
    decode_stderr: String,
    frames: usize,
    types: String,
    es_bits: Vec<i64>,
    decoded: String,
}

pub fn run(a: &Options) -> Result<(), String> {
    let case = if Path::new(&a.case).exists() {
        crate::checks::read_json(Path::new(&a.case))?
    } else {
        python::loads(&a.case).ok_or("--case is neither a file nor JSON")?
    };
    let (w, w_text) = int_field(&case, "width")?;
    let (h, h_text) = int_field(&case, "height")?;
    let (_, flip_text) = int_field(&case, "flip")?;
    let (n, n_text) = int_field(&case, "frames")?;
    let (rate, rate_text) = int_field(&case, "bitrate")?;
    let (wu, hu, nu) = (w as usize, h as usize, n as usize);
    fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out))?;
    let ffmpeg = join(&a.ffmpeg_bin, "ffmpeg");
    let ffprobe = join(&a.ffmpeg_bin, "ffprobe");
    let mut report = vec![
        ("case".to_owned(), case.clone()),
        (
            "input_sha256".to_owned(),
            Value::str(file_sha(Path::new(&a.input))?),
        ),
        (
            "candidate_sha256".to_owned(),
            Value::str(file_sha(Path::new(&a.candidate))?),
        ),
        (
            "reference_sha256".to_owned(),
            Value::str(file_sha(Path::new(&a.reference))?),
        ),
    ];
    let geometry = [
        w_text.clone(),
        h_text.clone(),
        flip_text.clone(),
        n_text.clone(),
    ];
    let mut roles = Vec::new();
    let mut outputs = Vec::new();
    for (role, worker) in [("candidate", &a.candidate), ("reference", &a.reference)] {
        let yuv = join(&a.out, &format!("{role}.convert.yuv"));
        let mut argv = vec![worker.clone(), "convert".into()];
        argv.extend(geometry.iter().cloned());
        argv.extend([a.input.clone(), yuv.clone()]);
        check_output(&argv)?;
        let ts_path = join(&a.out, &format!("{role}.ts"));
        let mut argv = vec![worker.clone(), "pipeline".into()];
        argv.extend(geometry.iter().cloned());
        argv.extend([rate_text.clone(), a.input.clone(), ts_path.clone()]);
        let out = check_output(&argv)?;
        let result = json::parse(&out).map_err(|e| format!("{role} pipeline output: {e}"))?;
        let bytes = result.get("bytes").and_then(python::int).filter(|&n| n > 0);
        let digest = result
            .get("fnv1a64")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let (Some(bytes), Some(digest)) = (bytes, digest) else {
            return Err(format!(
                "{role} pipeline output has no byte count or digest"
            ));
        };
        outputs.push((
            role.to_owned(),
            Value::object([
                ("bytes", Value::int(bytes)),
                ("fnv1a64", Value::str(digest)),
            ]),
        ));
        roles.push((role, yuv, ts_path));
    }
    // Conversion rule.
    let cand = read(&roles[0].1)?;
    let reference = read(&roles[1].1)?;
    let mut conv = vec![("bytes_equal".to_owned(), Value::Bool(cand == reference))];
    let (conv_pass, conv_max, conv_exact) = if cand.len() != reference.len() {
        conv.push(("error".into(), Value::str("size")));
        (false, None, None)
    } else {
        let d: Vec<u8> = cand
            .iter()
            .zip(&reference)
            .map(|(x, y)| x.abs_diff(*y))
            .collect();
        let max = *d
            .iter()
            .max()
            .ok_or("zero-size array to reduction operation maximum")?;
        #[allow(clippy::naive_bytecount)] // Once per case.
        let equal = d.iter().filter(|&&x| x == 0).count();
        let exact = equal as f64 / d.len() as f64;
        conv.push(("max_abs".into(), Value::int(max)));
        conv.push(("exact_fraction".into(), Value::float(exact)));
        (max <= 1, Some(max), Some(exact))
    };
    conv.push(("pass".into(), Value::Bool(conv_pass)));
    report.push(("conversion".into(), Value::Object(conv)));
    // Mux rule.
    let mut mux: Vec<(String, Value)> = Vec::new();
    let mut parsed: Vec<Role> = Vec::new();
    let mut table_sets = Vec::new();
    for (role, _, ts_path) in &roles {
        let facts = ts::parse(&read(ts_path)?)?;
        let problems = ts::check_mux(&facts)?;
        let mut tables: Vec<(u16, String)> = facts
            .tables
            .iter()
            .map(|t| (t.pid, t.section.clone()))
            .collect();
        tables.sort();
        tables.dedup();
        mux.push((
            (*role).to_owned(),
            Value::object([
                ("packets", Value::int(facts.packets as i64)),
                ("pictures", Value::int(facts.pes.len() as i64)),
                (
                    "problems",
                    Value::strings(problems.iter().take(50).cloned()),
                ),
                ("problem_count", Value::int(problems.len() as i64)),
            ]),
        ));
        table_sets.push(tables);
        parsed.push(Role {
            ts: ts_path.clone(),
            facts,
        });
    }
    let lookup = |tables: &[(u16, String)], pid: u16| -> Option<String> {
        tables
            .iter()
            .rev()
            .find(|(p, _)| *p == pid)
            .map(|(_, s)| s.clone())
    };
    let (ct, rt) = (&table_sets[0], &table_sets[1]);
    let pat_equal = lookup(rt, 0) == lookup(ct, 0);
    let pmt_r = lookup(rt, 0x1000).unwrap_or_default();
    let pmt_c = lookup(ct, 0x1000).unwrap_or_default();
    let pmt_equal = format!(
        "{}{}",
        slice(&pmt_r, 0, Some(24)),
        slice(&pmt_r, 26, Some(-8))
    ) == format!(
        "{}{}",
        slice(&pmt_c, 0, Some(24)),
        slice(&pmt_c, 26, Some(-8))
    );
    mux.push(("pat_equal".into(), Value::Bool(pat_equal)));
    mux.push((
        "pmt_equal_except_stream_type".into(),
        Value::Bool(pmt_equal),
    ));
    mux.push((
        "pmt_stream_types".into(),
        Value::object([
            ("reference", Value::str(slice(&pmt_r, 24, Some(26)))),
            ("candidate", Value::str(slice(&pmt_c, 24, Some(26)))),
        ]),
    ));
    let sdt = |tables| lookup(tables, 0x11).unwrap_or_default();
    mux.push((
        "sdt_names".into(),
        Value::object([
            ("reference", Value::str(slice(&sdt(rt), 38, Some(-8)))),
            ("candidate", Value::str(slice(&sdt(ct), 38, Some(-8)))),
        ]),
    ));
    let mut timing_ok = Vec::new();
    for (i, tables) in table_sets.iter().enumerate() {
        let listed = Value::Array(
            tables
                .iter()
                .map(|(p, s)| {
                    Value::Array(vec![Value::str(format!("{p:#x}")), Value::str(s.as_str())])
                })
                .collect(),
        );
        push(&mut mux[i].1, "tables", listed);
    }
    for (i, role) in parsed.iter().enumerate() {
        let packets = ffprobe_packets(&ffprobe, &role.ts)?;
        let mut ok = true;
        for (k, p) in packets.iter().enumerate() {
            let k = k as i128;
            let field = |key: &str| {
                p.get(key)
                    .and_then(python::int)
                    .ok_or_else(|| format!("ffprobe packet without integral {key}"))
            };
            if field("dts")? != 3000 * k || field("pts")? != 3000 * (k + 1) {
                ok = false;
                break;
            }
        }
        push(
            &mut mux[i].1,
            "ffprobe_packets",
            Value::int(packets.len() as i64),
        );
        push(&mut mux[i].1, "ffprobe_timing_ok", Value::Bool(ok));
        timing_ok.push((ok, packets.len()));
    }
    let candidate_problems = mux[0]
        .1
        .get("problem_count")
        .and_then(python::int)
        .unwrap_or(1);
    let mux_pass = candidate_problems == 0
        && pat_equal
        && pmt_equal
        && timing_ok[0].0
        && timing_ok[0].1 as i64 == n;
    mux.push(("pass".into(), Value::Bool(mux_pass)));
    report.push(("mux".into(), Value::Object(mux)));
    // Encoder rule.
    let frame_size = wu * hu * 3 / 2;
    let mut encoded = Vec::new();
    for (role, r) in ["candidate", "reference"].iter().zip(&parsed) {
        let out = join(&a.out, &format!("{role}.decoded.yuv"));
        let (code, err) = decode(&ffmpeg, &r.ts, &out)?;
        let frames = fs::metadata(&out).map_or(0, |m| m.len() as usize / frame_size);
        encoded.push(Encoded {
            decode_exit: code,
            decode_stderr: python::tail(&err, 2000),
            frames,
            types: types(&r.facts),
            es_bits: r
                .facts
                .pes
                .iter()
                .map(|p| p.data.len() as i64 * 8)
                .collect(),
            decoded: out,
        });
    }
    let common = encoded[0].frames.min(encoded[1].frames);
    let src_ref = planes(&reference, wu, hu, nu)?;
    let src_cand = planes(&cand, wu, hu, nu)?;
    let mut enc: Vec<(String, Value)> = Vec::new();
    let mut means = Vec::new();
    let mut common_bits = Vec::new();
    let mut underflow = Vec::new();
    for (index, e) in encoded.iter().enumerate() {
        let decoded = read(&e.decoded)?;
        let dec = planes(&decoded, wu, hu, e.frames)?;
        let own = if index == 0 { &src_cand } else { &src_ref };
        let series = |plane: usize, src: &[[&[u8]; 3]]| -> Vec<f64> {
            (0..common)
                .map(|i| psnr(dec[i][plane], src[i][plane]))
                .collect()
        };
        let y_own = series(0, own);
        let mean = mean_psnr(&y_own);
        let bits: i64 = e.es_bits[..common.min(e.es_bits.len())].iter().sum();
        let under = vbv(&e.es_bits, rate * 7 / 5, (rate / 7 + 16383) / 16384 * 16384);
        enc.push((
            ["candidate", "reference"][index].to_owned(),
            Value::object([
                ("decode_exit", Value::int(e.decode_exit)),
                ("decode_stderr", Value::str(e.decode_stderr.as_str())),
                ("frames", Value::int(e.frames as i64)),
                ("types", Value::str(e.types.as_str())),
                ("es_bits", ints(&e.es_bits)),
                (
                    "psnr_y_vs_reference_conversion",
                    floats(&series(0, &src_ref)),
                ),
                ("psnr_y_vs_own_conversion", floats(&y_own)),
                ("psnr_u_vs_own_conversion", floats(&series(1, own))),
                ("psnr_v_vs_own_conversion", floats(&series(2, own))),
                ("mean_psnr_y", Value::float(mean)),
                ("common_bits", Value::int(bits)),
                ("vbv_underflow_bits", Value::float(under)),
            ]),
        ));
        means.push(mean);
        common_bits.push(bits);
        underflow.push(under);
    }
    let (c, r) = (&encoded[0], &encoded[1]);
    let (cb, rb) = (common_bits[0], common_bits[1]);
    if rb == 0 {
        return Err("division by zero".into());
    }
    let bits_ratio = cb as f64 / rb as f64;
    let delta = means[0] - means[1];
    let prefix = |t: &str| t.chars().take(common).collect::<String>();
    let types_equal = prefix(&c.types) == prefix(&r.types);
    let base_ok = c.decode_exit == 0
        && c.frames as i64 == n
        && c.decode_stderr.is_empty()
        && types_equal
        && underflow[0] <= underflow[1];
    let pass_v1 = base_ok && delta >= -0.25 && bits_ratio <= 1.03;
    let budget = (rate * common as i64) as f64 / 30.0;
    let (mut mode, mut quality) = if cb as f64 <= 1.03 * rb as f64 {
        ("equal-or-fewer-bits", delta >= -0.25)
    } else if cb as f64 <= 1.03 * budget {
        ("within-requested-budget", delta >= -0.25)
    } else {
        ("over-budget", false)
    };
    let mut points: Vec<(i64, f64, i64)> = vec![(cb, means[0], rate)];
    let mut interpolated = None;
    if !quality && rb as f64 > 1.03 * cb as f64 {
        for factor in ENCODER_RATE_FACTORS {
            let extra = (rate as f64 * factor) as i64;
            let ts_path = join(&a.out, &format!("candidate.rate{extra}.ts"));
            let mut argv = vec![a.candidate.clone(), "pipeline".into()];
            argv.extend(geometry.iter().cloned());
            argv.extend([extra.to_string(), a.input.clone(), ts_path.clone()]);
            check_output(&argv)?;
            let out = format!("{ts_path}.yuv");
            decode(&ffmpeg, &ts_path, &out)?;
            let decoded = read(&out)?;
            let dec = planes(&decoded, wu, hu, common)?;
            let ratios: Vec<f64> = (0..common)
                .map(|i| power(psnr(dec[i][0], src_cand[i][0])))
                .collect();
            let mse = numpy::mean(&ratios);
            if mse <= 0.0 {
                return Err("math domain error".into());
            }
            let bits: i64 = ts::parse(&read(&ts_path)?)?
                .pes
                .iter()
                .take(common)
                .map(|p| p.data.len() as i64 * 8)
                .sum();
            points.push((bits, -10.0 * mse.log10(), extra));
            fs::remove_file(&out).map_err(|e| format!("{out}: {e}"))?;
            if bits >= rb {
                break;
            }
        }
        let mut ordered = points.clone();
        ordered.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)).then(x.2.cmp(&y.2)));
        for pair in ordered.windows(2) {
            let ((b0, p0, _), (b1, p1, _)) = (pair[0], pair[1]);
            if b0 <= rb && rb <= b1 && b1 > b0 {
                let t =
                    ((rb as f64).ln() - (b0 as f64).ln()) / ((b1 as f64).ln() - (b0 as f64).ln());
                interpolated = Some(p0 + t * (p1 - p0));
            }
        }
        if let Some(value) = interpolated {
            mode = "equal-bits-interpolated";
            quality = value >= means[1] - 0.25;
        } else {
            mode = "reference-bits-unreachable";
        }
    }
    let reference_exceeds = underflow[1] > 0.0 || rb as f64 > 1.03 * budget;
    let encoder_pass = base_ok && quality;
    enc.push(("common_frames".into(), Value::int(common as i64)));
    enc.push(("bits_ratio".into(), Value::float(bits_ratio)));
    enc.push(("psnr_y_delta_db".into(), Value::float(delta)));
    enc.push(("types_equal".into(), Value::Bool(types_equal)));
    enc.push(("pass_v1".into(), Value::Bool(pass_v1)));
    enc.push((
        "v2".into(),
        Value::object([
            ("mode", Value::str(mode)),
            ("quality_pass", Value::Bool(quality)),
            ("budget_bits", Value::float(budget)),
            (
                "points",
                Value::Array(
                    points
                        .iter()
                        .map(|&(b, p, r)| {
                            Value::Array(vec![Value::int(b), Value::float(p), Value::int(r)])
                        })
                        .collect(),
                ),
            ),
            (
                "interpolated_psnr_y",
                interpolated.map_or(Value::Null, Value::float),
            ),
            ("reference_exceeds_request", Value::Bool(reference_exceeds)),
        ]),
    ));
    enc.push(("pass".into(), Value::Bool(encoder_pass)));
    report.push(("encoder".into(), Value::Object(enc)));
    let pipeline = conv_pass && mux_pass && encoder_pass;
    report.push(("pipeline_pass".into(), Value::Bool(pipeline)));
    let outputs = Value::Object(outputs);
    report.push(("outputs".into(), outputs.clone()));
    let path = join(&a.out, "report.json");
    fs::write(&path, json::to_python(&Value::Object(report), Some(1)))
        .map_err(|e| format!("{path}: {e}"))?;
    let summary = Value::object([
        ("conversion", Value::Bool(conv_pass)),
        (
            "conversion_max_abs",
            conv_max.map_or(Value::Null, Value::int),
        ),
        (
            "conversion_exact",
            conv_exact.map_or(Value::int(0), |x| Value::float(python::round(x, 5))),
        ),
        ("mux", Value::Bool(mux_pass)),
        ("encoder", Value::Bool(encoder_pass)),
        ("encoder_v1", Value::Bool(pass_v1)),
        ("encoder_mode", Value::str(mode)),
        (
            "interpolated_psnr_y",
            match interpolated {
                None => Value::Null,
                Some(x) if x == 0.0 => Value::float(x),
                Some(x) => Value::float(python::round(x, 3)),
            },
        ),
        ("reference_exceeds_request", Value::Bool(reference_exceeds)),
        ("bits_ratio", Value::float(python::round(bits_ratio, 4))),
        (
            "psnr_y",
            floats(&[python::round(means[0], 3), python::round(means[1], 3)]),
        ),
        ("psnr_delta", Value::float(python::round(delta, 3))),
        ("vbv_underflow", floats(&underflow)),
        ("pipeline", Value::Bool(pipeline)),
        ("outputs", outputs),
    ]);
    println!("{}", json::to_python(&summary, None));
    Ok(())
}

fn push(object: &mut Value, key: &str, value: Value) {
    if let Value::Object(members) = object {
        members.push((key.to_owned(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_matches_numpy() {
        // python3 -c "import numpy as np, math; a=np.arange(64,dtype=np.uint8).reshape(8,8); b=(a+np.arange(64).reshape(8,8)%3).astype(np.uint8); m=np.mean((a.astype(np.int32)-b.astype(np.int32))**2); print(10*math.log10(255*255/m))"
        let a: Vec<u8> = (0..64).collect();
        let b: Vec<u8> = (0..64u8).map(|i| i + i % 3).collect();
        assert_eq!(psnr(&a, &b), 45.980710357818595);
        assert!(psnr(&a, &a).is_infinite());
    }

    #[test]
    fn mean_psnr_matches_numpy() {
        // python3 -c "import numpy as np, math; p=[30+0.37*i for i in range(90)]+[float('inf')]; print(-10*math.log10(np.mean([10**(-x/10) for x in p if x != float('inf')])))"
        let mut p: Vec<f64> = (0..90).map(|i| 30.0 + 0.37 * f64::from(i)).collect();
        p.push(f64::INFINITY);
        assert_eq!(mean_psnr(&p), 38.6649443986595);
        assert!(mean_psnr(&[f64::INFINITY]).is_infinite());
    }

    #[test]
    fn vbv_counts_underflow() {
        // 200 - 100 + 10 leaves 110 bits: 290 short for the second picture, 40 for the third.
        assert_eq!(vbv(&[100, 400, 50], 300, 200), 330.0);
        assert_eq!(slice("abcdefghij", 26, Some(-8)), "");
        assert_eq!(slice("abcdefghij", 2, Some(-3)), "cdefg");
    }

    #[test]
    fn rate_factors_follow_the_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let contract =
            crate::checks::read_json(&root.join("harness/capture/contract.json")).unwrap();
        let recorded: Vec<f64> = contract
            .get("comparison_rules")
            .and_then(|r| r.get("encoder_rate_factors"))
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert_eq!(recorded, ENCODER_RATE_FACTORS);
    }
}
