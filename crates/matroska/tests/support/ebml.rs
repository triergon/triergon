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

/// A file with one AVC track (CodecPrivate, name, BCP 47 language), one attachment
/// and a Cluster of two keyframe SimpleBlocks.
pub(crate) fn avc_file_with_attachment() -> Vec<u8> {
    let track = [
        element(0xd7, &[1]),
        element(0x73c5, &[1]),
        element(0x83, &[1]),
        element(0x86, b"V_MPEG4/ISO/AVC"),
        element(0x63a2, b"private"),
        element(0x536e, b"authored track"),
        element(0x22b59d, b"en-GB"),
    ]
    .concat();
    let attachment = element(
        0x61a7,
        &[
            element(0x46ae, &[1]),
            element(0x466e, b"a.bin"),
            element(0x4660, b"application/octet-stream"),
            element(0x465c, b"opaque"),
        ]
        .concat(),
    );
    let cluster = element(
        0x1f43b675,
        &[
            element(0xe7, &[0]),
            element(0xa3, &[0x81, 0, 0, 0x80, 1, 2, 3]),
            element(0xa3, &[0x81, 0, 1, 0x80, 4, 5, 6]),
        ]
        .concat(),
    );
    mkv(&[
        element(0x1654ae6b, &element(0xae, &track)),
        element(0x1941a469, &attachment),
        cluster,
    ]
    .concat())
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

/// A zlib stream of `abc` in one stored block.
pub(crate) const ABC: [u8; 14] = [120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];

/// ContentEncodings with one zlib ContentCompression of `scope`.
pub(crate) fn zlib_encoding(scope: u64) -> Vec<u8> {
    element(
        0x6d80,
        &element(
            0x6240,
            &[uint(0x5032, scope), element(0x5034, &uint(0x4254, 0))].concat(),
        ),
    )
}

/// One ContentEncoding of frames and private data (scope 3): ContentCompression
/// with `algorithm` and `settings`, at `order`.
pub(crate) fn content_encoding(order: u64, algorithm: u64, settings: &[u8]) -> Vec<u8> {
    element(
        0x6240,
        &[
            uint(0x5031, order),
            uint(0x5032, 3),
            element(
                0x5034,
                &[uint(0x4254, algorithm), element(0x4255, settings)].concat(),
            ),
        ]
        .concat(),
    )
}

/// A SimpleBlock body on track 1 at tick 0 with two fixed-laced frames, each
/// [`ABC`]; `bad_last_crc` flips the last Adler-32 byte.
pub(crate) fn abc_laces(bad_last_crc: bool) -> Vec<u8> {
    let mut raw = vec![0x81, 0, 0, 0x84, 1];
    raw.extend(ABC);
    raw.extend(ABC);
    if bad_last_crc {
        *raw.last_mut().unwrap() ^= 1;
    }
    raw
}

/// A file with one encoded TrackEntry (name, language, [`ABC`] as CodecPrivate,
/// two content encodings), a Chapters and a Tags document, a Cluster of two
/// fixed-laced [`ABC`] frames, and Cues.
pub(crate) fn catalogue_file() -> Vec<u8> {
    let track = element(
        0xae,
        &[
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            element(0x86, b"V_AUTHORED/OPAQUE"),
            element(0x536e, "épisode".as_bytes()),
            element(0x22b59d, b"fr-CA"),
            element(0x63a2, &ABC),
            element(
                0x6d80,
                &[content_encoding(1, 0, &[]), content_encoding(0, 3, b"P")].concat(),
            ),
        ]
        .concat(),
    );
    let tracks = element(0x1654ae6b, &track);
    let chapter = element(
        0x1043a770,
        &element(
            0x45b9,
            &element(
                0xb6,
                &[
                    uint(0x73c4, 1),
                    uint(0x91, 0),
                    element(0x80, &element(0x85, "épisode".as_bytes())),
                ]
                .concat(),
            ),
        ),
    );
    let tags = element(
        0x1254c367,
        &element(
            0x7373,
            &element(
                0x67c8,
                &[element(0x45a3, b"TITLE"), element(0x4487, b"source")].concat(),
            ),
        ),
    );
    let block = abc_laces(false);
    let cluster = element(0x1f43b675, &[uint(0xe7, 0), element(0xa3, &block)].concat());
    let cue = element(
        0x1c53bb6b,
        &element(
            0xbb,
            &[
                uint(0xb3, 0),
                element(
                    0xb7,
                    &[
                        uint(0xf7, 1),
                        uint(0xf1, (tracks.len() + chapter.len() + tags.len()) as u64),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    );
    mkv(&[tracks, chapter, tags, cluster, cue].concat())
}

/// The CRC-32 (IEEE 802.3, reflected) that an EBML CRC-32 element stores.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for &byte in bytes {
        value ^= u32::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(value & 1));
        }
    }
    !value
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
