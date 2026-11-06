//! Both timing workers digest exactly the work their trace shows: a scalar
//! recomputation of the ordered checksum over every trace field.

use super::{Context, Outcome, argv, hex_bytes, items, output};
use crate::json::Value;

/// The ordered work checksum of a trace: every field folded in trace order as
/// `value = value * 31 + field (mod 2^64)`, byte strings one byte at a time.
pub fn scalar_trace_checksum(trace: &Value) -> Result<u64, String> {
    let mut sum = Sum(0);
    let schema = trace.get("schema").ok_or("trace without schema")?;
    let schema_one = crate::harness::diff::equal(schema, &Value::int(1));
    for track in items(trace, "tracks")? {
        sum.text(track.get("codec"))?;
        sum.payload(&hex_bytes(track.get("private"))?);
        if schema_one {
            sum.text(track.get("language"))?;
            sum.text(track.get("name"))?;
        }
        for key in [
            "kind",
            "width",
            "height",
            "channels",
            "sample_rate",
            "default",
            "forced",
        ] {
            sum.field(track.get(key))?;
        }
        if let Some(colour) = track.get("colour") {
            for n in colour.as_array().ok_or("colour is not a list")? {
                sum.field(Some(n))?;
            }
        }
    }
    for packet in items(trace, "packets")? {
        sum.payload(&hex_bytes(packet.get("payload"))?);
        for key in [
            "track",
            "pts_ns",
            "dts_ns",
            "duration_ns",
            "key",
            "trim_start_samples",
            "trim_end_samples",
        ] {
            match packet.get(key) {
                Some(Value::Null) if key == "duration_ns" => sum.add(0),
                Some(Value::Null) => sum.add(i128::from(i64::MIN)),
                n => sum.field(n)?,
            }
        }
    }
    if let Some(catalogue) = trace.get("catalogue") {
        sum.dictionary(catalogue.get("global"))?;
        let tracks = items(catalogue, "tracks")?;
        sum.add(tracks.len() as i128);
        for track in tracks {
            sum.field(track.get("disposition"))?;
            sum.dictionary(track.get("tags"))?;
        }
        let chapters = items(catalogue, "chapters")?;
        sum.add(chapters.len() as i128);
        for chapter in chapters {
            for key in ["id", "start_ns", "end_ns"] {
                sum.field(chapter.get(key))?;
            }
            sum.dictionary(chapter.get("tags"))?;
        }
        let attachments = items(catalogue, "attachments")?;
        sum.add(attachments.len() as i128);
        for attachment in attachments {
            sum.dictionary(attachment.get("tags"))?;
            let data = hex_bytes(attachment.get("data"))?;
            sum.add(data.len() as i128);
            sum.payload(&data);
        }
    }
    Ok(sum.0)
}

struct Sum(u64);

impl Sum {
    /// Fold the low 64 bits (two's complement) of `n`.
    fn add(&mut self, n: i128) {
        self.0 = self.0.wrapping_mul(31).wrapping_add(n as u64);
    }

    /// Fold an integer field (`int(n)`: a boolean is 0 or 1, a float truncates).
    fn field(&mut self, value: Option<&Value>) -> Result<(), String> {
        let n = match value {
            Some(Value::Bool(b)) => i128::from(*b),
            Some(Value::Number(n)) => match n.as_i128() {
                Some(n) => n,
                None if n.as_f64().is_finite() => n.as_f64().trunc() as i128,
                None => return Err(format!("not an integer field: {}", n.text())),
            },
            other => return Err(format!("not an integer field: {other:?}")),
        };
        self.add(n);
        Ok(())
    }

    fn payload(&mut self, data: &[u8]) {
        for byte in data {
            self.add(i128::from(*byte));
        }
    }

    /// Fold the UTF-8 bytes of a string field.
    fn text(&mut self, value: Option<&Value>) -> Result<(), String> {
        let text = value.and_then(Value::as_str).ok_or("not a string field")?;
        self.payload(text.as_bytes());
        Ok(())
    }

    /// Fold a list of `[key, value]` string pairs, led by its length.
    fn dictionary(&mut self, rows: Option<&Value>) -> Result<(), String> {
        let rows = rows
            .and_then(Value::as_array)
            .ok_or("tags are not a list")?;
        self.add(rows.len() as i128);
        for row in rows {
            match row.as_array() {
                Some([key, value]) => {
                    self.text(Some(key))?;
                    self.text(Some(value))?;
                }
                _ => return Err("a tag is not a [key, value] pair".into()),
            }
        }
        Ok(())
    }
}

/// Packet count and payload bytes of a trace.
pub fn work(trace: &Value) -> Result<(usize, usize), String> {
    let packets = items(trace, "packets")?;
    let bytes = packets
        .iter()
        .map(|p| {
            p.get("payload")
                .and_then(Value::as_str)
                .map(|s| s.len() / 2)
        })
        .sum::<Option<usize>>()
        .ok_or("packet without payload")?;
    Ok((packets.len(), bytes))
}

/// The measured run's digest, packet count and byte count equal the trace's.
pub fn check_work(cx: &mut Context<'_>, measured: &Value, trace: &Value, what: &str) -> Outcome {
    let expected = scalar_trace_checksum(trace).map_err(|e| format!("{what}: {e}"))?;
    let (packets, bytes) = work(trace).map_err(|e| format!("{what}: {e}"))?;
    let at = |key: &str| measured.get(key).cloned().unwrap_or(Value::Null);
    cx.equal(
        &at("digest"),
        &Value::int(expected),
        format!("{what}: digest"),
    )?;
    cx.equal(
        &at("packets"),
        &Value::int(packets as i128),
        format!("{what}: packets"),
    )?;
    cx.equal(
        &at("bytes"),
        &Value::int(bytes as i128),
        format!("{what}: bytes"),
    )
}

pub fn both_workers_match_the_original_ordered_checksum(cx: &mut Context<'_>) -> Outcome {
    let workers = [cx.workers.candidate.clone(), cx.workers.reference.clone()];
    for directory in [
        "fixtures",
        "header-fixtures",
        "zlib-fixtures",
        "seek-fixtures",
    ] {
        let folder = cx.root.join(".research").join(directory);
        let mut paths: Vec<_> = std::fs::read_dir(&folder)
            .map_err(|e| format!("{}: {e}", folder.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|e| e == "mkv"))
            .collect();
        paths.sort();
        for path in &paths {
            for worker in &workers {
                let what = format!(
                    "{} {}",
                    worker.file_name().unwrap_or_default().to_string_lossy(),
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                let trace = output(&argv(worker, "trace", path, &[]))?;
                let measured = output(&argv(worker, "bench", path, &[&1]))?;
                check_work(cx, &measured, &trace, &what)?;
            }
        }
    }
    Ok(())
}

pub fn reviewed_packet_views_checksum_all_their_selected_work(cx: &mut Context<'_>) -> Outcome {
    for (case, reference) in [
        ("zlib-empty", cx.workers.reference.clone()),
        ("zlib-mixed", cx.workers.combined.clone()),
    ] {
        let path = cx
            .root
            .join(".research/zlib-fixtures")
            .join(format!("{case}.mkv"));
        for worker in [cx.workers.candidate.clone(), reference] {
            let what = format!(
                "{} {case}",
                worker.file_name().unwrap_or_default().to_string_lossy()
            );
            let trace = output(&argv(&worker, "codec-trace", &path, &[]))?;
            let measured = output(&argv(&worker, "codec-bench", &path, &[&1]))?;
            check_work(cx, &measured, &trace, &what)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_fields_in_order_with_wrapping_and_absent_times() {
        // The value Python's integer arithmetic gives for this trace.
        let trace = crate::json::parse(
            r#"{"schema": 2, "tracks": [{"codec": "A", "private": "ff", "kind": 1,
                "width": 0, "height": 0, "channels": 2, "sample_rate": 48000,
                "default": true, "forced": false}],
                "packets": [{"payload": "0102", "track": 0, "pts_ns": -5, "dts_ns": null,
                "duration_ns": null, "key": true, "trim_start_samples": 0,
                "trim_end_samples": 0}]}"#,
        )
        .unwrap();
        assert_eq!(scalar_trace_checksum(&trace).unwrap(), 18200677638695604325);
    }
}
