//! Authored H.222.0 PSI and EN 300 468 subtitling descriptor controls.
use std::path::Path;

use super::write;

pub(super) fn fixtures(dir: &Path) -> Result<(), String> {
    let mut two_programs = Vec::new();
    section_packets(
        &mut two_programs,
        0,
        &mut 0,
        &pat(&[(1, 0x1000), (2, 0x1001)]),
    );
    section_packets(
        &mut two_programs,
        0x1000,
        &mut 0,
        &pmt(1, 0, 0x100, &[(0x1b, 0x100, vec![])]),
    );
    section_packets(
        &mut two_programs,
        0x1001,
        &mut 0,
        &pmt(2, 0, 0x101, &[(0x24, 0x101, vec![])]),
    );
    write(dir, "two-programs.ts", &two_programs)?;

    for (name, pcr_pid, stream) in [
        ("no-pcr.ts", 0x1fff, (0x1b, 0x100, vec![])),
        (
            "three-languages.ts",
            0x100,
            (0x0f, 0x100, b"\x0a\x0ceng\x00deu\x02fra\x03".to_vec()),
        ),
        (
            "two-dvb-services.ts",
            0x100,
            (
                6,
                0x100,
                b"\x59\x10eng\x14\x00\x01\x00\x02deu\x24\x00\x03\x00\x04".to_vec(),
            ),
        ),
    ] {
        let mut data = Vec::new();
        section_packets(&mut data, 0, &mut 0, &pat(&[(1, 0x1000)]));
        section_packets(&mut data, 0x1000, &mut 0, &pmt(1, 0, pcr_pid, &[stream]));
        write(dir, name, &data)?;
    }

    let mut video = Vec::new();
    section_packets(&mut video, 0, &mut 0, &pat(&[(1, 0x1000)]));
    let mut count = 0;
    // Each of the nine AVC flags, and each of the nine HEVC flags, takes
    // both values with every combination of the other eight in its descriptor.
    // Vary the non-flag fields independently of the presence of the temporal tail.
    for version in 0..32 {
        let mut streams = Vec::new();
        for index in 0..16 {
            let flags = u16::from(version) * 16 + index;
            let avc = vec![
                0x28,
                4,
                66 + (flags % 59) as u8,
                ((flags >> 3) as u8) << 2 | (flags % 4) as u8,
                10 + (flags % 52) as u8,
                ((flags & 7) as u8) << 5 | 0x1f,
            ];
            let hevc = hevc(flags);
            streams.push((0x1b, 0x100 + index * 2, avc));
            streams.push((0x24, 0x101 + index * 2, hevc));
        }
        section_packets(
            &mut video,
            0x1000,
            &mut count,
            &pmt(1, version, 0x100, &streams),
        );
    }
    write(dir, "video-descriptor-flags.ts", &video)
}

fn hevc(flags: u16) -> Vec<u8> {
    let subset = flags & 8 != 0;
    let mut descriptor = vec![0x38, if subset { 15 } else { 13 }];
    let profile_space = ((flags >> 3) % 4) as u8;
    let tier = (flags >> 8) as u8;
    descriptor.push(profile_space << 6 | tier << 5 | (flags % 32) as u8);
    descriptor.extend((u32::from(flags).wrapping_mul(0x10204081)).to_be_bytes());
    let copied = u64::from(flags).wrapping_mul(0x13579bdf) & ((1 << 44) - 1);
    let constraints = u64::from((flags >> 4) & 15) << 44 | copied;
    descriptor.extend(&constraints.to_be_bytes()[2..]);
    descriptor.push(30 + (flags % 156) as u8);
    descriptor.push((flags as u8 & 15) << 4 | 0x0c | ((flags >> 5) % 4) as u8);
    if subset {
        let min = (flags >> 6) as u8 % 7;
        let max = min + flags as u8 % (7 - min);
        descriptor.extend([min << 5 | 31, max << 5 | 31]);
    }
    descriptor
}

fn pat(programs: &[(u16, u16)]) -> Vec<u8> {
    let mut body = Vec::new();
    for &(program, pid) in programs {
        body.extend(program.to_be_bytes());
        body.extend((0xe000 | pid).to_be_bytes());
    }
    section(0, 1, 0, &body)
}

fn pmt(program: u16, version: u8, pcr_pid: u16, streams: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = (0xe000 | pcr_pid).to_be_bytes().to_vec();
    body.extend([0xf0, 0]);
    for (kind, pid, descriptors) in streams {
        body.push(*kind);
        body.extend((0xe000 | pid).to_be_bytes());
        body.extend((0xf000 | descriptors.len() as u16).to_be_bytes());
        body.extend(descriptors);
    }
    section(2, program, version, &body)
}

fn section(tag: u8, identity: u16, version: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![tag];
    bytes.extend((0xb000 | (body.len() + 9) as u16).to_be_bytes());
    bytes.extend(identity.to_be_bytes());
    bytes.extend([0xc1 | version << 1, 0, 0]);
    bytes.extend(body);
    let mut crc = u32::MAX;
    for &byte in &bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc >> 31 == 1 { 0x04c11db7 } else { 0 };
        }
    }
    bytes.extend(crc.to_be_bytes());
    bytes
}

fn section_packets(out: &mut Vec<u8>, pid: u16, counter: &mut u8, section: &[u8]) {
    let [high, low] = pid.to_be_bytes();
    let mut payload = vec![0];
    payload.extend(section);
    for (index, chunk) in payload.chunks(184).enumerate() {
        let mut carrier = [0xff; 188];
        carrier[..4].copy_from_slice(&[
            0x47,
            high | if index == 0 { 0x40 } else { 0 },
            low,
            0x10 | *counter,
        ]);
        carrier[4..4 + chunk.len()].copy_from_slice(chunk);
        out.extend(carrier);
        *counter = (*counter + 1) & 15;
    }
}
