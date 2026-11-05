//! Original deterministic fixture bytes. Materialize only on the parent's CPU lease.
//! H.222.0 section/CRC/PES grammar, no production/native parser import.
#[derive(Clone, Copy, Debug)]
pub enum Case {
    MultiOne,
    MultiTwo,
    MultiNetwork,
    MultiSections,
    Retype188,
    Retype192,
    RetypeWrap,
    Repeat,
    NextMap,
    PendingBody,
    PendingHeader,
    ChangedPid,
    IncompletePat,
    NextPat,
    NetworkOnly,
    BadMapCrc,
}
pub const CASES: &[(Case, &str)] = &[
    (Case::MultiOne, "multi-program-selected-one"),
    (Case::MultiTwo, "multi-program-selected-two"),
    (Case::MultiNetwork, "multi-program-network-entry"),
    (Case::MultiSections, "multi-section-reversed-arrival"),
    (Case::Retype188, "drained-avc-hevc-188"),
    (Case::Retype192, "drained-avc-hevc-192"),
    (Case::RetypeWrap, "drained-avc-hevc-raw33-wrap"),
    (Case::Repeat, "identical-current-map-repeat"),
    (Case::NextMap, "not-current-map-keeps-avc"),
    (Case::PendingBody, "partial-body-refuses-rebind"),
    (Case::PendingHeader, "partial-header-refuses-rebind"),
    (Case::ChangedPid, "changed-elementary-pid-refused"),
    (Case::IncompletePat, "incomplete-pat-stays-inactive"),
    (Case::NextPat, "not-current-pat-stays-inactive"),
    (Case::NetworkOnly, "network-only-pat-stays-unbound"),
    (Case::BadMapCrc, "bad-new-map-crc-keeps-source-avc"),
];
pub fn selected(case: Case) -> u16 {
    if matches!(case, Case::MultiTwo | Case::MultiSections) {
        2
    } else {
        1
    }
}
pub fn width(case: Case) -> usize {
    if matches!(case, Case::Retype192) {
        192
    } else {
        188
    }
}
pub fn id(case: Case) -> &'static str {
    CASES
        .iter()
        .find(|(c, _)| std::mem::discriminant(c) == std::mem::discriminant(&case))
        .unwrap()
        .1
}

// Bitwise MPEG-2 CRC recurrence over the authored section, before appending CRC.
fn section(mut data: Vec<u8>) -> Vec<u8> {
    let n = data.len() + 4 - 3;
    data[1] = 0xb0 | ((n >> 8) as u8);
    data[2] = n as u8;
    let mut remainder = 0xffff_ffffu32;
    for byte in &data {
        remainder ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            remainder = if remainder & 0x8000_0000 != 0 {
                (remainder << 1) ^ 0x04c1_1db7
            } else {
                remainder << 1
            };
        }
    }
    data.extend_from_slice(&remainder.to_be_bytes());
    data
}
fn pat(entries: &[(u16, u16)], number: u8, last: u8, current: bool) -> Vec<u8> {
    let mut data = vec![0, 0, 0, 0, 1, 0xc0 | u8::from(current), number, last];
    for (program, pid) in entries {
        data.extend_from_slice(&program.to_be_bytes());
        data.extend_from_slice(&(0xe000 | pid).to_be_bytes());
    }
    section(data)
}
fn pmt(program: u16, pid: u16, kind: u8, version: u8, current: bool) -> Vec<u8> {
    let mut data = vec![
        2,
        0,
        0,
        (program >> 8) as u8,
        program as u8,
        0xc0 | (version << 1) | u8::from(current),
        0,
        0,
        0xe1,
        0,
        0xf0,
        0,
        kind,
    ];
    data.extend_from_slice(&(0xe000 | pid).to_be_bytes());
    data.extend_from_slice(&[0xf0, 0]);
    section(data)
}
fn time5(t: u64) -> [u8; 5] {
    assert!(t < 1 << 33);
    [
        0x21 | ((t >> 29) as u8 & 14),
        (t >> 22) as u8,
        1 | ((t >> 14) as u8 & 254),
        (t >> 7) as u8,
        1 | ((t << 1) as u8 & 254),
    ]
}
fn pes(t: u64, body: &[u8], unbounded: bool) -> Vec<u8> {
    let length = if unbounded { 0 } else { 8 + body.len() };
    let mut data = vec![
        0,
        0,
        1,
        0xe0,
        (length >> 8) as u8,
        length as u8,
        0x80,
        0x80,
        5,
    ];
    data.extend_from_slice(&time5(t));
    data.extend_from_slice(body);
    data
}
fn packet(pid: u16, cc: u8, payload: &[u8]) -> Vec<u8> {
    assert!(!payload.is_empty() && payload.len() <= 182);
    let length = 183 - payload.len();
    let mut data = vec![
        0x47,
        0x40 | ((pid >> 8) as u8),
        pid as u8,
        0x30 | cc,
        length as u8,
        0,
    ];
    data.resize(5 + length, 0xff);
    data.extend_from_slice(payload);
    assert_eq!(data.len(), 188);
    data
}
fn psi(pid: u16, cc: u8, section: Vec<u8>) -> Vec<u8> {
    let mut data = vec![0];
    data.extend(section);
    packet(pid, cc, &data)
}

pub fn bytes(case: Case) -> Vec<u8> {
    let program = selected(case);
    let pmt_pid = 4095 + program;
    let elementary = 255 + program;
    let mut packets = Vec::new();
    match case {
        Case::MultiOne => packets.push(psi(0, 0, pat(&[(2, 4097), (1, 4096)], 0, 0, true))),
        Case::MultiTwo => packets.push(psi(0, 0, pat(&[(1, 4096), (2, 4097)], 0, 0, true))),
        Case::MultiNetwork => {
            packets.push(psi(0, 0, pat(&[(0, 16), (2, 4097), (1, 4096)], 0, 0, true)))
        }
        Case::MultiSections => {
            packets.push(psi(0, 0, pat(&[(2, 4097)], 1, 1, true)));
            packets.push(psi(0, 1, pat(&[(1, 4096)], 0, 1, true)));
        }
        Case::IncompletePat => packets.push(psi(0, 0, pat(&[(1, 4096)], 0, 1, true))),
        Case::NextPat => packets.push(psi(0, 0, pat(&[(1, 4096)], 0, 0, false))),
        Case::NetworkOnly => {
            packets.push(psi(0, 0, pat(&[(0, 16)], 0, 0, true)));
        }
        _ => packets.push(psi(0, 0, pat(&[(1, 4096)], 0, 0, true))),
    }
    let lifecycle = matches!(
        case,
        Case::Retype188
            | Case::Retype192
            | Case::RetypeWrap
            | Case::Repeat
            | Case::NextMap
            | Case::PendingBody
            | Case::PendingHeader
            | Case::ChangedPid
            | Case::BadMapCrc
    );
    if !matches!(case, Case::NetworkOnly) {
        packets.push(psi(
            pmt_pid,
            0,
            pmt(
                program,
                elementary,
                0x1b,
                if lifecycle { 31 } else { 0 },
                true,
            ),
        ));
    }
    let first = if matches!(case, Case::RetypeWrap) {
        (1u64 << 33) - 90_000
    } else if program == 2 {
        180_000
    } else {
        90_000
    };
    let first_body = if program == 2 {
        b"SEL2" as &[u8]
    } else if matches!(case, Case::MultiSections) {
        b"ATMC"
    } else {
        b"AVC0"
    };
    let mut before = pes(first, first_body, matches!(case, Case::PendingBody));
    if matches!(case, Case::PendingHeader) {
        before.truncate(5);
    }
    packets.push(packet(elementary, 0, &before));
    if lifecycle {
        let next_pid = if matches!(case, Case::ChangedPid) {
            elementary + 1
        } else {
            elementary
        };
        let mut next = if matches!(case, Case::Repeat) {
            pmt(program, elementary, 0x1b, 31, true)
        } else {
            pmt(program, next_pid, 0x24, 0, !matches!(case, Case::NextMap))
        };
        if matches!(case, Case::BadMapCrc) {
            let end = next.len() - 1;
            next[end] ^= 1;
        }
        packets.push(psi(pmt_pid, 1, next));
        let after = if matches!(case, Case::RetypeWrap) {
            45_000
        } else {
            180_000
        };
        let body = if matches!(case, Case::Repeat | Case::NextMap | Case::BadMapCrc) {
            b"AVC1" as &[u8]
        } else {
            b"HEV1"
        };
        packets.push(packet(next_pid, 1, &pes(after, body, false)));
    }
    let mut result = Vec::new();
    for p in packets {
        if width(case) == 192 {
            result.extend_from_slice(&[0x93, 0xab, 0, 0x7f]);
        }
        result.extend(p);
    }
    result
}
