//! Materialize decoder-policy vectors from pinned source streams.

use std::fs;
use std::path::Path;

use crate::json::{self, Value};
use crate::process::{Stop, run_text};
use crate::python;

use super::correctness::{PROBE_TIMEOUT, RUN_TIMEOUT, file_sha};

pub(super) const CRC: &str = "harness/codecs/crc-damage.json";
pub(super) const PACKED: &str = "harness/codecs/packed-dts.json";
pub(super) const NODITHER: &str = "harness/codecs/nodither-streams.json";

pub(super) struct FixtureTools<'a> {
    pub root: &'a Path,
    pub ffprobe: &'a str,
    pub strip_dither: &'a Path,
}

pub(super) fn recipe_manifest(lane: &str) -> Option<&'static str> {
    Some(match lane {
        "flac-crc-reject" | "ac3-crc-reject" | "eac3-crc-reject" => CRC,
        "dts-packed" => PACKED,
        "ac3-nodither" => NODITHER,
        _ => return None,
    })
}

fn field<'a>(row: &'a Value, key: &str) -> Result<&'a str, String> {
    row.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("variant recipe is missing {key}"))
}

pub(super) fn damage(bytes: &mut [u8], packets: &[Value], seed: u64) -> Result<(), String> {
    let mut state = seed;
    let mut previous_end = 0;
    for packet in packets {
        let offset = field(packet, "pos")?
            .parse::<usize>()
            .map_err(|e| e.to_string())?;
        let size = field(packet, "size")?
            .parse::<usize>()
            .map_err(|e| e.to_string())?;
        let end = offset.checked_add(size).ok_or("packet range overflow")?;
        // Leave the sync and fixed header intact. The probe supplies only
        // boundaries; the deterministic mutation uses no decoder output.
        if offset < previous_end || size <= 8 || end > bytes.len() {
            return Err(format!("invalid packet at {offset} with size {size}"));
        }
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let bits = (size - 8) * 8;
        let bit = ((state >> 32) % bits as u64) as usize;
        bytes[offset + 8 + bit / 8] ^= 1 << (bit % 8);
        previous_end = end;
    }
    if previous_end == 0 {
        return Err("damage recipe found no input frames".into());
    }
    Ok(())
}

fn opus_preskip(bytes: &mut [u8], preskip: u16) -> Result<(), String> {
    let Some(header) = bytes.get(..27) else {
        return Err("truncated Ogg header".into());
    };
    if !header.starts_with(b"OggS\0") || header[5] != 2 {
        return Err("expected an Ogg beginning-of-stream page".into());
    }
    let payload = 27 + usize::from(header[26]);
    let lengths = bytes.get(27..payload).ok_or("truncated lacing table")?;
    let size: usize = lengths.iter().map(|&len| usize::from(len)).sum();
    let page = bytes
        .get_mut(..payload + size)
        .ok_or("truncated Ogg page")?;
    if page
        .get(payload..payload + 12)
        .is_none_or(|head| !head.starts_with(b"OpusHead"))
    {
        return Err("Ogg page has no OpusHead".into());
    }
    page[payload + 10..payload + 12].copy_from_slice(&preskip.to_le_bytes());
    page[22..26].fill(0);
    let mut crc = 0_u32;
    for &byte in &*page {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            let top = crc >> 31;
            crc = (crc << 1) ^ (top * 0x04c1_1db7);
        }
    }
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn pinned_source(
    root: &Path,
    vector: &Value,
    key: &str,
    pin: &str,
) -> Result<std::path::PathBuf, String> {
    let source = root.join(field(vector, key)?);
    if file_sha(&source)? != field(vector, pin)? {
        return Err(format!("{}: source SHA-256 mismatch", source.display()));
    }
    Ok(source)
}

fn generated(
    tools: &FixtureTools<'_>,
    vector: &Value,
    source: &Path,
    output: &Path,
) -> Result<(), Stop> {
    let bytes = fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let result = if let Some(samples) = vector.get("preskip").and_then(python::int) {
        let mut result = bytes;
        opus_preskip(
            &mut result,
            u16::try_from(samples).map_err(|e| e.to_string())?,
        )?;
        result
    } else if vector.get("tail_source").is_some() {
        let tail = pinned_source(tools.root, vector, "tail_source", "tail_source_sha256")?;
        let mut result = bytes;
        result.extend(fs::read(&tail).map_err(|e| format!("{}: {e}", tail.display()))?);
        result
    } else if let Some(packing) = vector.get("packing").and_then(Value::as_str) {
        crate::framing_views::dts::pack_frames(&bytes, packing)
    } else if let Some(seed) = vector
        .get("seed")
        .and_then(python::int)
        .and_then(|value| u64::try_from(value).ok())
    {
        let format = field(vector, "format")?;
        let args = [
            tools.ffprobe,
            "-v",
            "error",
            "-f",
            format,
            "-select_streams",
            "a:0",
            "-show_packets",
            "-show_entries",
            "packet=pos,size",
            "-of",
            "json",
            source
                .to_str()
                .ok_or_else(|| "non-UTF8 source path".to_owned())?,
        ]
        .map(String::from);
        let captured = run_text(&args, PROBE_TIMEOUT)?;
        if captured.code != 0 {
            return Err(format!("{}: {}", source.display(), captured.stderr).into());
        }
        let parsed = json::parse(&captured.stdout).map_err(|e| e.to_string())?;
        let packets = parsed
            .get("packets")
            .and_then(Value::as_array)
            .ok_or_else(|| "no packets".to_owned())?;
        let mut result = bytes;
        damage(&mut result, packets, seed)?;
        result
    } else {
        let args = [
            python::path(tools.strip_dither),
            python::path(source),
            python::path(output),
        ];
        let captured = run_text(&args, RUN_TIMEOUT)?;
        if captured.code != 0 {
            return Err(format!("{}: {}", source.display(), captured.stderr).into());
        }
        return Ok(());
    };
    fs::write(output, result).map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(())
}

/// Prepare into an isolated directory so two comparisons can regenerate the same vector.
pub(super) fn materialize(
    tools: &FixtureTools<'_>,
    vector: &Value,
    output: &Path,
) -> Result<(), Stop> {
    let source = pinned_source(tools.root, vector, "source", "source_sha256")?;
    generated(tools, vector, &source, output)
}

/// Record the derived byte pins from the same generator the grade uses.
pub(super) fn record(
    tools: &FixtureTools<'_>,
    manifest: &Path,
    scratch: Option<&Path>,
) -> Result<(), String> {
    let mut doc = json::read(manifest)?;
    let Value::Object(fields) = &mut doc else {
        return Err("expected variant manifest".into());
    };
    let vectors = fields
        .iter_mut()
        .find_map(|(key, value)| {
            if key == "vectors"
                && let Value::Array(vectors) = value
            {
                Some(vectors)
            } else {
                None
            }
        })
        .ok_or("variant manifest has no vectors")?;
    let scratch = crate::process::TempDir::new(scratch)?;
    for vector in vectors {
        if vector.get("source").is_none() {
            continue;
        }
        let output = scratch.0.join(field(vector, "name")?);
        materialize(tools, vector, &output).map_err(|error| match error {
            Stop::Timeout(argv) => format!("generator timed out: {argv:?}"),
            Stop::Fatal(message) => message,
        })?;
        let pin = file_sha(&output)?;
        fs::remove_file(&output).map_err(|e| e.to_string())?;
        let Value::Object(fields) = vector else {
            return Err("expected variant vector".into());
        };
        let (_, value) = fields
            .iter_mut()
            .find(|(key, _)| key == "sha256")
            .ok_or("vector lacks sha256 field")?;
        *value = Value::str(pin);
    }
    json::write(manifest, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_preserves_headers_and_flips_one_bit_per_packet() {
        let mut bytes = [0; 40];
        let packets = [
            Value::object([("pos", Value::str("0")), ("size", Value::str("20"))]),
            Value::object([("pos", Value::str("20")), ("size", Value::str("20"))]),
        ];
        damage(&mut bytes, &packets, 280).unwrap();
        for packet in bytes.chunks_exact(20) {
            assert_eq!(packet[..8], [0; 8]);
            assert_eq!(packet.iter().map(|b| b.count_ones()).sum::<u32>(), 1);
        }
    }
}
