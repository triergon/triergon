//! Authored transport carrier geometry and adaptation controls.
use std::path::Path;

use super::write;

pub(super) fn fixtures(dir: &Path) -> Result<(), String> {
    let mut plain = [0xff; 188];
    plain[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    let mut baseline = plain.repeat(32);
    // Keep the raw demuxer's clock anchors ahead of each damage site.
    for (index, carrier) in baseline.chunks_exact_mut(188).enumerate() {
        carrier[3] |= index as u8 & 15;
        carrier[187] = index as u8;
        if index < 2 {
            carrier[1..6].copy_from_slice(&[1, 0, 0x30 | index as u8, 7, 0x10]);
            let pcr = ((90_000 + index as u64 * 90) << 15) | (63 << 9);
            carrier[6..12].copy_from_slice(&pcr.to_be_bytes()[2..]);
        }
    }
    let mut m2ts = Vec::new();
    for (index, carrier) in baseline.chunks_exact(188).enumerate() {
        m2ts.extend((0xa0c00000 + index as u32).to_be_bytes());
        m2ts.extend(carrier);
    }
    write(dir, "pcr-anchored.m2ts", &m2ts)?;
    let offset = 12 * plain.len();
    for (name, change) in [
        ("lost-sync.ts", &[(0, 0x46)][..]),
        ("adaptation-183.ts", &[(3, 0x20), (4, 183), (5, 0)]),
        ("adaptation-184.ts", &[(3, 0x20), (4, 184), (5, 0)]),
        ("reserved-afc.ts", &[(3, 0)]),
        (
            "pcr-reserved.ts",
            &[
                (3, 0x30),
                (4, 7),
                (5, 0x10),
                (6, 0),
                (7, 0),
                (8, 0),
                (9, 0),
                (10, 0),
                (11, 1),
            ],
        ),
        ("adaptation-183-payload.ts", &[(3, 0x30), (4, 183), (5, 0)]),
    ] {
        let mut bytes = baseline.clone();
        for &(index, value) in change {
            bytes[offset + index] = value;
        }
        if name == "adaptation-183" {
            bytes[offset + 187] = 0xff;
        }
        write(dir, name, &bytes)?;
    }
    write(dir, "tail-187.ts", &baseline[..baseline.len() - 1])?;
    write(dir, "null-run.ts", &plain.repeat(32))?;
    let mut fields = baseline;
    // Wrap the six optional adaptation subfields in a valid payload carrier.
    let field = [
        23, 0xff, // length and flags
        0xff, 0xff, 0xff, 0xff, 0xff, 0x2b, // PCR at the largest base, extension 299
        0, 0, 0, 1, 0x7e, 0,    // OPCR
        0xf7, // signed splice countdown -9
        2, 0x23, 0x47, // private data
        3, 0x1f, 0x9c, 0x00, // opaque adaptation extension
        0xff, 0xff, // stuffing
    ];
    fields[offset..offset + 4].copy_from_slice(&[0x47, 0xe1, 0, 0xbd]);
    fields[offset + 4..offset + 4 + field.len()].copy_from_slice(&field);
    write(dir, "adaptation-fields.ts", &fields)
}
