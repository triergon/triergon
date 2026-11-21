//! EBML bytes for the Matroska tests, shared by the unit and integration tests.
// Each test binary uses part of this file.
#![allow(dead_code)]

/// An element with its size in the fewest bytes.
pub(crate) fn element(id: u64, data: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (data.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | data.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(data);
    out
}

/// An unsigned integer element, always eight bytes wide.
pub(crate) fn uint(id: u64, value: u64) -> Vec<u8> {
    element(id, &value.to_be_bytes())
}

/// An EBML header followed by a Segment holding `segment`.
pub(crate) fn mkv(segment: &[u8]) -> Vec<u8> {
    [
        element(0x1a45dfa3, &element(0x4282, b"matroska")),
        element(0x18538067, segment),
    ]
    .concat()
}

/// The fields of a TrackEntry: number, UID and type, then an authored codec and `extra`.
pub(crate) fn track_entry(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    [
        uint(0xd7, number),
        uint(0x73c5, number),
        uint(0x83, kind),
        element(0x86, b"V_AUTHORED/OPAQUE"),
        extra.to_vec(),
    ]
    .concat()
}

/// A TrackEntry.
pub(crate) fn track(number: u64, kind: u64, extra: &[u8]) -> Vec<u8> {
    element(0xae, &track_entry(number, kind, extra))
}

/// A SimpleBlock on a track numbered below 127.
pub(crate) fn simple(track: u8, ticks: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    let [high, low] = ticks.to_be_bytes();
    element(0xa3, &[&[0x80 | track, high, low, flags], data].concat())
}

/// A Cluster with an eight-byte Timestamp.
pub(crate) fn cluster(time: u64, blocks: impl AsRef<[u8]>) -> Vec<u8> {
    element(
        0x1f43b675,
        &[uint(0xe7, time), blocks.as_ref().to_vec()].concat(),
    )
}

/// The file shape of the BlockAdditions tests: one authored track, one Cluster.
pub(crate) mod side_data {
    use super::{cluster, element, mkv};

    pub(crate) fn track(extra: &[u8]) -> Vec<u8> {
        let private = element(0x63a2, b"initial");
        super::track(1, 1, &[private, extra.to_vec()].concat())
    }

    pub(crate) fn file(extra: &[u8], groups: &[Vec<u8>]) -> Vec<u8> {
        let tracks = element(0x1654ae6b, &track(extra));
        mkv(&[tracks, cluster(7, groups.concat())].concat())
    }

    pub(crate) fn block() -> Vec<u8> {
        element(0xa1, &[0x81, 0, 0, 0, b'V'])
    }

    pub(crate) fn additions(mores: &[Vec<u8>]) -> Vec<u8> {
        element(0x75a1, &mores.concat())
    }

    pub(crate) fn more(id: Option<&[u8]>, bytes: &[u8]) -> Vec<u8> {
        let id = id.map_or_else(Vec::new, |id| element(0xee, id));
        element(0xa6, &[id, element(0xa5, bytes)].concat())
    }
}
