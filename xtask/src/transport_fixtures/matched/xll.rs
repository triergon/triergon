//! Authored lossless-only DTS-HD recovery syntax, without coded audio samples.
use super::{Codec, Input, PID, Path, R, Transport, Value, part};

struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn put(&mut self, width: usize, value: u32) {
        for bit in (0..width).rev() {
            self.bytes[self.at / 8] |= ((value >> bit) as u8 & 1) << (7 - self.at % 8);
            self.at += 1;
        }
    }
}

fn crc(bytes: &mut [u8]) {
    let end = bytes.len() - 2;
    let mut value = u16::MAX;
    for &byte in &bytes[..end] {
        value ^= u16::from(byte) << 8;
        for _ in 0..8 {
            value = (value << 1) ^ if value & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    bytes[end..].copy_from_slice(&value.to_be_bytes());
}

// ETSI TS 102 114 V1.6.1 Tables 7-2, 7-5/6/7 and 8-1. Both headers
// have their own CRC. One complete XLL frame fills the only active asset;
// sync is DWORD-aligned at offset zero, without buffering or decoding delay.
fn frame(long: bool, tag: u8) -> Vec<u8> {
    const XLL_BYTES: u32 = 400;
    let header_bytes = if long { 32 } else { 28 };
    let size_bits = if long { 20 } else { 16 };
    let mut bits = Bits {
        bytes: vec![0; header_bytes + XLL_BYTES as usize],
        at: 0,
    };
    for (width, value) in [
        (32, 0x6458_2025),
        (8, 0),
        (2, 0), // Extension zero.
        (1, u32::from(long)),
        (if long { 12 } else { 8 }, header_bytes as u32 - 1),
        (size_bits, header_bytes as u32 + XLL_BYTES - 1),
        (1, 1), // Static setup in this frame.
        (2, 2), // 48 kHz reference clock.
        (3, 0), // 512 samples.
        (1, 0), // No timecode.
        (3, 0), // One presentation.
        (3, 0), // One asset.
        (1, 1), // Active extension zero.
        (8, 1), // Active asset zero.
        (1, 0), // No mixing.
        (size_bits, XLL_BYTES - 1),
    ] {
        bits.put(width, value);
    }
    let descriptor_end = bits.at + if long { 14 * 8 } else { 12 * 8 };
    for (width, value) in [
        (9, if long { 13 } else { 11 }),
        (3, 0),  // Asset zero.
        (1, 0),  // No descriptive type.
        (1, 0),  // No language.
        (1, 0),  // No text.
        (5, 23), // 24-bit samples.
        (4, 12), // 48 kHz maximum rate.
        (8, 1),  // Two channels.
        (1, 1),  // Speaker representation.
        (1, 0),  // No speaker mask.
        (3, 0),  // No speaker remapping.
        (1, 0),  // No DRC.
        (1, 0),  // No dialogue normalisation.
        (2, 1),  // Lossless coding, without a CBR component.
        (size_bits, XLL_BYTES - 1),
        (1, 1),         // XLL sync present.
        (4, 0),         // No smoothing buffer.
        (5, 3),         // Four bits of initial delay.
        (4, 0),         // Immediate decoding.
        (size_bits, 0), // Sync at the asset start.
        (3, 0),         // DTS-HD stream ID.
        (2, 0),         // Primary decoder; no Rev2 DRC.
    ] {
        bits.put(width, value);
    }
    assert!(bits.at <= descriptor_end);
    bits.at = descriptor_end;
    bits.put(1, 0); // No backward-compatible core.
    assert!(bits.at <= (header_bytes - 2) * 8);
    crc(&mut bits.bytes[5..header_bytes]);
    bits.at = header_bytes * 8;
    for (width, value) in [
        (32, 0x41a2_9547),
        (4, 0),  // XLL version 1.
        (8, 15), // 16-byte common header.
        (5, 9),  // Ten bits for the complete frame size.
        (10, XLL_BYTES - 1),
        (4, 0), // One channel set.
        (4, 0), // One segment.
        (4, 9), // 512 samples per segment.
        (5, 7), // Eight bits for segment sizes.
        (2, 0), // No band CRC.
        (1, 0), // No scalable LSBs.
        (5, 1), // Four-bit channel masks.
    ] {
        bits.put(width, value);
    }
    assert!(bits.at <= (header_bytes + 14) * 8);
    crc(&mut bits.bytes[header_bytes + 4..header_bytes + 16]);
    bits.bytes[header_bytes + 16..].fill(tag);
    bits.bytes
}

pub(super) fn seek_cases(
    root: &Path,
    feature: &str,
    reference: &Path,
    cases: &mut Vec<Value>,
) -> R<()> {
    let codec = Codec {
        name: "dts-xll",
        stream_type: 6,
        stream_id: 0xbd,
        dvb: true,
        bytes: Vec::new(),
        first: 0,
        header: 0,
    };
    let pid = PID.to_string();
    for (carrier, m2ts) in [("ts188", false), ("m2ts192", true)] {
        let mut ts = Transport::new();
        // Annex G.3.1 / Table E-2: one 24-bit stereo construction-17 asset.
        ts.tables(&codec, &[0x7f, 8, 0x0e, 0x40, 5, 2, 0x64, 0x8c, 0, 0])?;
        for i in 0..4 {
            let bytes = frame(m2ts, i + 1);
            let pts = 90_000 + u64::from(i) * 960;
            let mut pes = part(&bytes, Some(pts), (i % 2 == 0).then_some(pts - 120));
            pes.aligned = true;
            ts.pes(codec.stream_id, &pes)?;
        }
        let input = Input {
            name: if m2ts {
                "dts-xll-long"
            } else {
                "dts-xll-short"
            },
            m2ts,
            data: ts.finish(m2ts),
        };
        for policy in ["previous", "preroll"] {
            for (landing, target, repeat) in [
                ("early", "90500", None),
                ("late", "92500", None),
                ("backward-indexed", "92500", Some("90500")),
            ] {
                let mut options = vec!["--dvb", pid.as_str(), target, policy];
                options.extend(repeat);
                cases.push(input.record(
                    root,
                    feature,
                    &format!("dts-xll-{carrier}-{landing}-{policy}"),
                    &options,
                    reference,
                    "ts-audio-seek-fold",
                )?);
            }
        }
    }
    Ok(())
}
