//! Streaming planar and RGBA video comparisons used by the codec grader.

use crate::json::{self, Value};
use crate::process::{FrameExit, Frames, Stop};
use crate::python;

use super::{RUN_TIMEOUT, json_lines, probe};

struct Compared {
    ours: FrameExit,
    reference: FrameExit,
    frames_ours: i64,
    frames_ref: i64,
    equal: i64,
}

impl Compared {
    fn exact(&self) -> bool {
        self.equal > 0
            && self.frames_ours == self.equal
            && self.frames_ref == self.equal
            && self.ours.code == 0
            && self.reference.code == 0
            && self.ours.partial_bytes == 0
            && self.reference.partial_bytes == 0
    }
}

fn compare(
    mut ours: Frames,
    mut reference: Frames,
    mut on_difference: impl FnMut(i64, &[u8], &[u8]),
) -> Result<Compared, Stop> {
    let (mut frames_ours, mut frames_ref, mut equal) = (0, 0, 0);
    loop {
        let (a, b) = (ours.next()?, reference.next()?);
        match (a, b) {
            (None, None) => break,
            (Some(a), Some(b)) if a == b => equal += 1,
            (Some(a), Some(b)) => on_difference(frames_ours, a, b),
            _ => {}
        }
        frames_ours += i64::from(a.is_some());
        frames_ref += i64::from(b.is_some());
    }
    Ok(Compared {
        ours: ours.close()?,
        reference: reference.close()?,
        frames_ours,
        frames_ref,
        equal,
    })
}

/// Locate the first differing sample within the frame. HEVC reports the
/// containing 64x64 luma region, including for subsampled chroma planes.
fn first_diff(
    a: &[u8],
    b: &[u8],
    width: usize,
    height: usize,
    depth: usize,
    hevc: bool,
) -> Vec<(String, Value)> {
    fn clip(data: &[u8], from: usize, to: usize) -> &[u8] {
        &data[from.min(data.len())..to.min(data.len())]
    }

    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let mut from = 0;
    for (name, pw, ph, scale) in [("Y", width, height, 1), ("U", cw, ch, 2), ("V", cw, ch, 2)] {
        let to = from + pw * ph * depth;
        let (pa, pb) = (clip(a, from, to), clip(b, from, to));
        if pa != pb {
            let k = pa
                .chunks(depth)
                .zip(pb.chunks(depth).chain(std::iter::repeat(&[][..])))
                .position(|(ours, reference)| ours != reference)
                .unwrap_or(0);
            let (x, y) = (k % pw, k / pw);
            let mut fields = vec![
                ("plane".into(), Value::str(name)),
                ("x".into(), Value::int(x as i64)),
                ("y".into(), Value::int(y as i64)),
            ];
            if hevc {
                fields.push((
                    "ctb64".into(),
                    Value::Array(vec![
                        Value::int((x * scale / 64) as i64),
                        Value::int((y * scale / 64) as i64),
                    ]),
                ));
            }
            return fields;
        }
        from = to;
    }
    Vec::new()
}

fn has_error(line: &Value) -> bool {
    line.get("error").is_some()
}

fn mentions_unsupported(value: &Value) -> bool {
    json::to_python(value, None).contains("nsupported")
}

fn errors_head(errors: &[Value]) -> Value {
    Value::Array(errors.iter().take(3).cloned().collect())
}

fn reference_command(ffmpeg: &str, path: &str, out_format: &str) -> Vec<String> {
    [
        ffmpeg,
        "-nostdin",
        "-v",
        "error",
        "-threads",
        "1",
        "-i",
        path,
        "-f",
        "rawvideo",
        "-pix_fmt",
        out_format,
        "-fps_mode",
        "passthrough",
        "-",
    ]
    .map(String::from)
    .to_vec()
}

/// Without a pixel format there is no frame boundary. Drain and count the actual
/// output from both decoders before classifying a reference refusal.
fn unframed(ours_argv: &[String], ref_argv: &[String], probe_rc: i64) -> Result<Value, Stop> {
    let (ours_bytes, ours) = Frames::count_stdout(ours_argv, RUN_TIMEOUT)?;
    let (reference_bytes, reference) = Frames::count_stdout(ref_argv, RUN_TIMEOUT)?;
    let lines = json_lines(&ours.stderr);
    let errors: Vec<_> = lines.iter().filter(|j| has_error(j)).cloned().collect();
    let status = if reference.code > 0
        && reference_bytes == 0
        && ours_bytes == 0
        && matches!(ours.code, 0 | 1)
        && probe_rc >= 0
    {
        "ffmpeg-refused"
    } else {
        "unframed-output"
    };
    let partial = |bytes| {
        if bytes == 0 {
            Value::int(0)
        } else {
            Value::Null
        }
    };
    Ok(Value::object([
        ("status", Value::str(status)),
        ("ffmpeg", Value::str("ffprobe found no decodable stream")),
        ("ffprobe_rc", Value::int(probe_rc)),
        ("ffmpeg_rc", Value::int(reference.code)),
        ("ours_rc", Value::int(ours.code)),
        (
            "ours_frames",
            Value::int(lines.iter().filter(|j| !has_error(j)).count() as i64),
        ),
        ("ours_bytes", Value::int(ours_bytes)),
        ("ffmpeg_bytes", Value::int(reference_bytes)),
        ("ours_partial_bytes", partial(ours_bytes)),
        ("ffmpeg_partial_bytes", partial(reference_bytes)),
        ("ours_errors", errors_head(&errors)),
        (
            "ours_stderr_tail",
            Value::str(python::tail(&ours.stderr, 300)),
        ),
        (
            "ffmpeg_stderr_tail",
            Value::str(python::tail(&reference.stderr, 300)),
        ),
    ]))
}

pub(super) fn decoder(
    ffprobe: &str,
    ffmpeg: &str,
    example: &str,
    lane: &str,
    path: &str,
) -> Result<Value, Stop> {
    let (probe_rc, stream) = probe(ffprobe, path, None, false)?;
    let ours_argv = vec![
        example.to_owned(),
        path.to_owned(),
        if lane == "h264" { "/dev/stdout" } else { "-" }.to_owned(),
    ];
    let pix_fmt = stream.as_ref().and_then(|s| s.get("pix_fmt"));
    let (Some(stream), Some(pix_fmt)) = (stream.as_ref(), pix_fmt) else {
        return unframed(
            &ours_argv,
            &reference_command(ffmpeg, path, "yuv420p"),
            probe_rc,
        );
    };
    let pix_name = pix_fmt
        .as_str()
        .ok_or_else(|| Stop::Fatal("pix_fmt is not a string".into()))?;
    let depth = if pix_name.contains("10") { 2 } else { 1 };
    let out_format = if depth == 2 { "yuv420p10le" } else { "yuv420p" };
    let dimension = |key: &str| -> Result<(usize, Value), Stop> {
        let value = stream
            .get(key)
            .cloned()
            .ok_or_else(|| Stop::Fatal(format!("ffprobe reported no {key}")))?;
        let number = python::int(&value)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| Stop::Fatal(format!("ffprobe {key} is not a size")))?;
        Ok((number, value))
    };
    let ((w, width), (h, height)) = (dimension("width")?, dimension("height")?);
    let size = w
        .div_ceil(2)
        .checked_mul(h.div_ceil(2))
        .and_then(|chroma| chroma.checked_mul(2))
        .and_then(|chroma| w.checked_mul(h)?.checked_add(chroma))
        .and_then(|samples| samples.checked_mul(depth))
        .filter(|_| w > 0 && h > 0)
        .ok_or_else(|| Stop::Fatal(format!("invalid video dimensions {w}x{h}")))?;
    let reference_argv = reference_command(ffmpeg, path, out_format);
    let reference = Frames::spawn(&reference_argv, size, false, RUN_TIMEOUT)?;
    let ours = Frames::spawn(&ours_argv, size, false, RUN_TIMEOUT)?;
    let mut first = None;
    let compared = compare(ours, reference, |n, fa, fb| {
        first.get_or_insert_with(|| (n, first_diff(fa, fb, w, h, depth, lane == "hevc")));
    })?;
    let lines = json_lines(&compared.ours.stderr);
    let first = first.map(|(frame, position)| {
        let mut fields = vec![("frame".into(), Value::int(frame))];
        fields.extend(position);
        let metadata = lines.iter().find(|line| {
            line.get("frame")
                .or_else(|| line.get("n"))
                .and_then(python::int)
                == Some(i128::from(frame))
        });
        if let Some(poc) = metadata.and_then(|line| line.get("poc")) {
            fields.push(("poc".into(), poc.clone()));
        }
        Value::Object(fields)
    });
    let size_mismatch = lines.iter().find(|line| {
        line.get("width").is_some_and(|v| v != &width)
            || line.get("height").is_some_and(|v| v != &height)
    });
    let errors: Vec<_> = lines.iter().filter(|j| has_error(j)).cloned().collect();
    let no_tails = compared.ours.partial_bytes == 0 && compared.reference.partial_bytes == 0;
    let status = if probe_rc != 0 {
        "failed-probe"
    } else if !no_tails {
        "partial-frame"
    } else if compared.frames_ref == 0
        && compared.frames_ours == 0
        && compared.reference.code > 0
        && matches!(compared.ours.code, 0 | 1)
    {
        "ffmpeg-refused"
    } else if compared.frames_ref > 0
        && compared.reference.code == 0
        && matches!(compared.ours.code, 0 | 1)
        && errors.iter().any(mentions_unsupported)
        && compared.frames_ours == 0
    {
        "refused-unsupported"
    } else if compared.ours.code != 0 || compared.reference.code != 0 {
        "failed-exit"
    } else if !errors.is_empty() {
        "decoder-error"
    } else if compared.exact() && size_mismatch.is_none() {
        "pass"
    } else {
        "fail"
    };
    Ok(Value::object([
        ("status", Value::str(status)),
        ("frames_ref", Value::int(compared.frames_ref)),
        ("frames_ours", Value::int(compared.frames_ours)),
        ("frames_equal", Value::int(compared.equal)),
        ("first_diff", first.unwrap_or(Value::Null)),
        ("ffprobe_rc", Value::int(probe_rc)),
        ("width", width),
        ("height", height),
        ("pix_fmt", pix_fmt.clone()),
        (
            "size_mismatch",
            size_mismatch.cloned().unwrap_or(Value::Null),
        ),
        ("ours_rc", Value::int(compared.ours.code)),
        ("ffmpeg_rc", Value::int(compared.reference.code)),
        ("ours_errors", errors_head(&errors)),
        (
            "ours_partial_bytes",
            Value::int(compared.ours.partial_bytes as i64),
        ),
        (
            "ffmpeg_partial_bytes",
            Value::int(compared.reference.partial_bytes as i64),
        ),
        (
            "ffmpeg_stderr_tail",
            Value::str(python::tail(&compared.reference.stderr, 300)),
        ),
    ]))
}

/// Compare the fixed 1920x1080 RGBA worker output. Count differing bytes and
/// track the largest delta directly in the borrowed frames.
pub(super) fn output(ours: Frames, reference: Frames) -> Result<Value, Stop> {
    rgba(ours, reference, false)
}

pub(super) fn rgba(ours: Frames, reference: Frames, timeline: bool) -> Result<Value, Stop> {
    let (mut first, mut differing, mut worst) = (None, 0, 0u8);
    let compared = compare(ours, reference, |n, a, b| {
        differing += 1;
        if first.is_some() {
            worst = worst.max(
                a.iter()
                    .zip(b)
                    .map(|(x, y)| x.abs_diff(*y))
                    .max()
                    .unwrap_or(0),
            );
            return;
        }
        let mut first_byte = None;
        let mut count = 0;
        for (k, (&x, &y)) in a.iter().zip(b).enumerate() {
            if x != y {
                first_byte.get_or_insert(k);
                count += 1;
                worst = worst.max(x.abs_diff(y));
            }
        }
        let k = first_byte.expect("unequal frames of the same size");
        first = Some(Value::object([
            ("frame", Value::int(n)),
            ("x", Value::int(((k / 4) % 1920) as i64)),
            ("y", Value::int(((k / 4) / 1920) as i64)),
            ("channel", Value::str(&"RGBA"[k % 4..k % 4 + 1])),
            ("differing_bytes", Value::int(count)),
        ]));
    })?;
    let status = if compared.ours.partial_bytes != 0 || compared.reference.partial_bytes != 0 {
        "partial-frame"
    } else if compared.exact() {
        "pass"
    } else if compared.frames_ref == 0
        && compared.frames_ours == 0
        && compared.reference.code >= 0
        && matches!(compared.ours.code, 0 | 1)
    {
        "reference-produced-nothing"
    } else if compared.ours.code != 0 || compared.reference.code != 0 {
        "failed-exit"
    } else {
        "fail"
    };
    let mut row = vec![
        ("status", Value::str(status)),
        ("pictures_ref", Value::int(compared.frames_ref)),
        ("pictures_ours", Value::int(compared.frames_ours)),
        ("pictures_equal", Value::int(compared.equal)),
        ("differing_pictures", Value::int(differing)),
        ("max_abs", Value::int(worst)),
        ("first_diff", first.unwrap_or(Value::Null)),
        ("ours_rc", Value::int(compared.ours.code)),
        ("reference_rc", Value::int(compared.reference.code)),
        (
            "ours_partial_bytes",
            Value::int(compared.ours.partial_bytes as i64),
        ),
        (
            "reference_partial_bytes",
            Value::int(compared.reference.partial_bytes as i64),
        ),
        (
            "ours_stderr_tail",
            Value::str(python::tail(&compared.ours.stderr, 300)),
        ),
        (
            "reference_stderr_tail",
            Value::str(python::tail(&compared.reference.stderr, 300)),
        ),
    ];
    if timeline {
        row.push(("timeline", Value::Array(json_lines(&compared.ours.stderr))));
        row.push((
            "reference_timeline",
            Value::Array(json_lines(&compared.reference.stderr)),
        ));
    }
    Ok(Value::object(row))
}

#[cfg(test)]
mod tests;
