//! One ADTS frame per PES for exact, independent timestamp controls.
use std::fs;
use std::path::Path;

use super::write;

const PERIOD: u64 = 1 << 33;
const CARRIER: usize = 188;
const AUDIO: &str = "crates/matroska/tests/fixtures/cue-ties/aac-packet.bin";
const TABLES: &str = "crates/mpegts/tests/data/carriage-aac-adts-direct.ts";

pub(super) fn fixtures(root: &Path, dir: &Path) -> Result<(), String> {
    let read = |path| fs::read(root.join(path)).map_err(|error| format!("{path}: {error}"));
    let frame = read(AUDIO)?;
    let tables = read(TABLES)?;
    let tables = tables.get(..2 * CARRIER).ok_or("missing PAT and PMT")?;
    let length = frame.len() + 7;
    if length > 0x1fff {
        return Err("AAC packet exceeds one ADTS frame".into());
    }
    let mut adts = vec![
        0xff,
        0xf1,
        0x4c,
        0x80 | (length >> 11) as u8,
        (length >> 3) as u8,
        ((length & 7) << 5) as u8 | 0x1f,
        0xfc,
    ];
    adts.extend(frame);
    for (name, pts_dts) in [
        ("clock-crossing.ts", &[(PERIOD - 7, PERIOD - 9), (4, 2)][..]),
        ("clock-half-period.ts", &[(PERIOD / 2, PERIOD / 2 - 1)][..]),
        ("clock-linear.ts", &[(90_002, 90_000), (91_922, 91_920)][..]),
    ] {
        let mut ts = tables.to_vec();
        let mut counter = 0;
        for &(pts, dts) in pts_dts {
            let mut pes = vec![0, 0, 1, 0xc0];
            pes.extend(((adts.len() + 13) as u16).to_be_bytes());
            pes.extend([0x84, 0xc0, 10]);
            pes.extend(timestamp(3, pts));
            pes.extend(timestamp(1, dts));
            pes.extend_from_slice(&adts);
            for (index, chunk) in pes.chunks(184).enumerate() {
                let mut packet = [0xff; CARRIER];
                let at = CARRIER - chunk.len();
                let start = if index == 0 { 0x40 } else { 0 };
                let adaptation = if at == 4 { 0x10 } else { 0x30 };
                packet[..4].copy_from_slice(&[0x47, start | 1, 1, adaptation | counter]);
                if at > 4 {
                    packet[4] = (at - 5) as u8;
                }
                if at > 5 {
                    packet[5] = 0;
                }
                packet[at..].copy_from_slice(chunk);
                counter = (counter + 1) & 15;
                ts.extend(packet);
            }
        }
        write(dir, name, &ts)?;
    }
    Ok(())
}

fn timestamp(prefix: u8, ticks: u64) -> [u8; 5] {
    let value = (u64::from(prefix) << 36)
        | ((ticks >> 30) << 33)
        | (((ticks >> 15) & 0x7fff) << 17)
        | ((ticks & 0x7fff) << 1)
        | 0x1_0001_0001;
    let [_, _, _, a, b, c, d, e] = value.to_be_bytes();
    [a, b, c, d, e]
}
