use crate::ebml::{element as e, header, uint as u};

pub(super) type Cases = Vec<(String, Vec<u8>)>;
const FRAME: &[u8] = &[0x82, 0x49, 0x83, 0x42];

pub(super) fn track(number: u64, codec: &[u8], extensions: &[u8]) -> Vec<u8> {
    e(
        0xae,
        &[
            u(0xd7, number),
            u(0x73c5, 100 + number),
            u(0x83, 1),
            e(0x86, codec),
            e(0xe0, &[u(0xb0, 64), u(0xba, 64)].concat()),
            extensions.to_vec(),
        ]
        .concat(),
    )
}

pub(super) fn file(webm: bool, tracks: &[Vec<u8>], metadata: &[u8], blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut header = if webm {
        e(
            0x1a45dfa3,
            &[
                u(0x4286, 1),
                u(0x42f7, 1),
                u(0x42f2, 4),
                u(0x42f3, 8),
                e(0x4282, b"webm"),
                u(0x4287, 4),
                u(0x4285, 2),
            ]
            .concat(),
        )
    } else {
        header()
    };
    let info = e(
        0x1549a966,
        &[e(0x4d80, b"fixture muxer"), e(0x5741, b"fixture writer")].concat(),
    );
    header.extend(e(
        0x18538067,
        &[
            info,
            e(0x1654ae6b, &tracks.concat()),
            metadata.to_vec(),
            e(0x1f43b675, &[u(0xe7, 0), blocks.concat()].concat()),
        ]
        .concat(),
    ));
    header
}

fn block(track: u8, tick: u8, flags: u8) -> Vec<u8> {
    [&[0x80 | track, 0, tick, flags][..], FRAME].concat()
}

pub(super) fn simple(track: u8, tick: u8) -> Vec<u8> {
    e(0xa3, &block(track, tick, 0x80))
}

fn group(track: u8, tick: u8, laced: bool, other: &[u8]) -> Vec<u8> {
    let mut body = block(track, tick, 0);
    if laced {
        body[3] = 4;
        body.insert(4, 1); // Two equal frames.
        body.extend(FRAME);
    }
    e(0xa0, &[e(0xa1, &body), other.to_vec()].concat())
}

fn more(id: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let id = id.map_or_else(Vec::new, |id| u(0xee, id));
    e(0xa6, &[id, e(0xa5, payload)].concat())
}

fn additions(items: &[Vec<u8>]) -> Vec<u8> {
    e(0x75a1, &items.concat())
}

fn mapping(value: Option<u64>, kind: Option<u64>, extra: Option<&[u8]>, names: bool) -> Vec<u8> {
    e(
        0x41e4,
        &[
            value.map_or_else(Vec::new, |value| u(0x41f0, value)),
            kind.map_or_else(Vec::new, |kind| u(0x41e7, kind)),
            extra.map_or_else(Vec::new, |extra| e(0x41ed, extra)),
            if names {
                [e(0x41a4, b"mapping\0\0"), e(0x4fff, b"future")].concat()
            } else {
                Vec::new()
            },
        ]
        .concat(),
    )
}

fn fourcc(name: &[u8; 4]) -> u64 {
    u64::from(u32::from_be_bytes(*name))
}

#[derive(Default)]
struct Bits {
    data: Vec<u8>,
    count: usize,
}

impl Bits {
    fn put(&mut self, value: u32, width: usize) {
        for pos in (0..width).rev() {
            if self.count.is_multiple_of(8) {
                self.data.push(0);
            }
            *self.data.last_mut().unwrap() |= ((value >> pos) as u8 & 1) << (7 - self.count % 8);
            self.count += 1;
        }
    }
}

fn grid(bits: &mut Bits, dimensions: Option<(u32, u32)>, base: u32) {
    bits.put(u32::from(dimensions.is_some()), 1);
    if let Some((rows, columns)) = dimensions {
        bits.put(rows, 5);
        bits.put(columns, 5);
        for i in 0..rows * columns {
            bits.put((i + base) % 16, 4);
        }
    }
}

fn hdr(windows: u32, count: u32, tone_mapping: bool, peak_grids: bool) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(1, 8);
    bits.put(windows, 2);
    for i in 1..windows {
        for field in [2, 3, 60, 61, 30, 31] {
            bits.put(field + i, 16);
        }
        bits.put(12 + i, 8);
        for axis in [12, 20, 18] {
            bits.put(axis + i, 16);
        }
        bits.put(1, 1);
    }
    bits.put(1000, 27);
    grid(&mut bits, peak_grids.then_some((2, 3)), 0);
    for window in 0..windows {
        for value in [10001, 22222, 50000, 9876] {
            bits.put(value + window, 17);
        }
        bits.put(count, 4);
        for i in 0..count {
            bits.put(6 * (i + 1), 7);
            bits.put(100 * i + 5, 17);
        }
        bits.put(93 + window, 10);
    }
    grid(&mut bits, peak_grids.then_some((3, 2)), 7);
    for window in 0..windows {
        let tone = tone_mapping && window != 1;
        bits.put(u32::from(tone), 1);
        if tone {
            bits.put(123, 12);
            bits.put(2345, 12);
            bits.put(count, 4);
            for i in 0..count {
                bits.put((i + 1) * 60, 10);
            }
        }
        let saturation = tone_mapping && window != 2;
        bits.put(u32::from(saturation), 1);
        if saturation {
            bits.put(8 + window, 6);
        }
    }
    [&[0xb5, 0, 0x3c, 0, 1, 4][..], &bits.data].concat()
}

pub(super) fn packet_cases() -> Cases {
    let mut cases = Cases::new();
    let make = |webm, mapping: &[u8], additions: Vec<u8>, laced| {
        let extension = if webm {
            Vec::new()
        } else {
            [u(0x55ee, 12), mapping.to_vec()].concat()
        };
        file(
            webm,
            &[track(1, b"V_VP9", &extension)],
            &[],
            &[simple(1, 0), group(1, 2, laced, &additions), simple(1, 4)],
        )
    };
    for count in 0_u32..=15 {
        // Every percentile count in one, two or three windows.
        let webm = count.is_multiple_of(2);
        let payload = hdr(
            1 + count % 3,
            count,
            count % 3 != 0,
            count.is_multiple_of(4),
        );
        let (kind, id, map) = if webm {
            ("webm", 4, Vec::new())
        } else {
            ("matroska", 5, mapping(Some(5), Some(4), None, false))
        };
        cases.push((
            format!("hdr-{kind}-{count:02}"),
            make(webm, &map, additions(&[more(Some(id), &payload)]), false),
        ));
    }
    let lcevc = [0xb4, 0, 0x50, 0, 0x12, 0x34];
    let t35 = mapping(Some(4), Some(4), None, false);
    cases.extend([
        (
            "hdr-truncated".into(),
            make(
                false,
                &t35,
                additions(&[more(Some(4), &hdr(1, 4, false, false)[..13])]),
                false,
            ),
        ),
        (
            "lcevc".into(),
            make(false, &t35, additions(&[more(Some(4), &lcevc)]), false),
        ),
        (
            "mixed-hdr-opaque".into(),
            make(
                false,
                &t35,
                additions(&[
                    more(None, b"opaque"),
                    more(Some(4), &hdr(1, 0, false, false)),
                ]),
                false,
            ),
        ),
        (
            "laced-opaque-lcevc".into(),
            make(
                false,
                &t35,
                additions(&[
                    more(None, b"first"),
                    more(Some(4), &lcevc),
                    more(Some(1), b"last"),
                ]),
                true,
            ),
        ),
        (
            "laced-valid-opaque-lcevc".into(),
            make(
                false,
                &t35,
                additions(&[more(None, b"opaque"), more(Some(4), &lcevc)]),
                true,
            ),
        ),
        (
            "opaque-mapped".into(),
            make(
                false,
                &mapping(Some(3), Some(fourcc(b"priv")), None, true),
                additions(&[more(Some(3), b"mapped")]),
                false,
            ),
        ),
        (
            "opaque-identity".into(),
            make(
                false,
                &mapping(Some(2), Some(0), None, false),
                additions(&[more(Some(2), b"mapped-identity")]),
                false,
            ),
        ),
        (
            "opaque-default".into(),
            make(false, &[], additions(&[more(None, b"default")]), false),
        ),
        (
            "opaque-empty".into(),
            make(false, &[], additions(&[more(Some(1), b"")]), false),
        ),
        (
            "opaque-webm".into(),
            make(
                true,
                &[],
                additions(&[
                    more(Some(1), b"one"),
                    more(Some(2), b"two"),
                    more(Some(3), b"three"),
                    more(Some(8), b"eight"),
                ]),
                false,
            ),
        ),
        (
            "opaque-unmapped".into(),
            make(
                false,
                &[],
                additions(&[
                    more(Some(2), b"two"),
                    more(Some(3), b"three"),
                    more(Some(4), b"four"),
                    more(Some(8), b"eight"),
                ]),
                false,
            ),
        ),
    ]);
    cases
}

pub(super) fn extension_cases() -> Cases {
    let make = |maximum: &[u8], entries: &[Vec<u8>]| {
        file(
            false,
            &[track(
                1,
                b"V_MPEGH/ISO/HEVC",
                &[maximum, &entries.concat()].concat(),
            )],
            &[],
            &[simple(1, 0)],
        )
    };
    let max = u(0x55ee, 12);
    let dolby = [1, 0, 0x10, 0x2f, 0x20];
    let hvce = [
        0x01, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xf0, 0, 0xfc, 0xfc, 0xf8, 0xf8, 0, 0, 0x0f, 0,
    ];
    [
        ("mapping-absent", make(&[], &[])),
        (
            "mapping-empty-fields",
            make(
                &e(0x55ee, &[]),
                &[e(
                    0x41e4,
                    &[e(0x41e7, &[]), e(0x41a4, &[]), e(0x41ed, &[])].concat(),
                )],
            ),
        ),
        (
            "mapping-value-one",
            make(&max, &[mapping(Some(1), Some(4), None, false)]),
        ),
        (
            "mapping-t35",
            make(&max, &[mapping(Some(4), Some(4), None, true)]),
        ),
        (
            "mapping-dvcc",
            make(
                &max,
                &[mapping(Some(2), Some(fourcc(b"dvcC")), Some(&dolby), true)],
            ),
        ),
        (
            "mapping-dvvc",
            make(
                &max,
                &[mapping(
                    Some(3),
                    Some(fourcc(b"dvvC")),
                    Some(&[2, 1, 0x11, 0x2d, 0x3c]),
                    true,
                )],
            ),
        ),
        (
            "mapping-mvcc",
            make(
                &max,
                &[mapping(Some(4), Some(fourcc(b"mvcC")), Some(b"mvc"), true)],
            ),
        ),
        (
            "mapping-hvce",
            make(
                &max,
                &[mapping(Some(5), Some(fourcc(b"hvcE")), Some(&hvce), true)],
            ),
        ),
        (
            "mapping-short-hvce",
            make(
                &max,
                &[mapping(
                    Some(5),
                    Some(fourcc(b"hvcE")),
                    Some(b"short"),
                    true,
                )],
            ),
        ),
        (
            "mapping-unknown",
            make(
                &max,
                &[mapping(
                    Some(11),
                    Some(fourcc(b"zzzz")),
                    Some(b"private"),
                    true,
                )],
            ),
        ),
        (
            "mapping-duplicate-value",
            make(
                &max,
                &[
                    mapping(Some(4), Some(4), None, false),
                    mapping(Some(4), Some(fourcc(b"zzzz")), Some(b"later"), true),
                ],
            ),
        ),
    ]
    .into_iter()
    .map(|(name, data)| (name.to_owned(), data))
    .collect()
}

pub(super) fn state_cases() -> Cases {
    let make = |tracks, blocks: &[Vec<u8>]| {
        let tracks: Vec<_> = (1..=tracks)
            .map(|i| track(i, b"V_VP9", &e(0x63a2, b"initial")))
            .collect();
        file(false, &tracks, &[], blocks)
    };
    let state = |track, tick, laced, data| group(track, tick, laced, &e(0xa4, data));
    [
        (
            "state-first",
            make(1, &[state(1, 0, false, b"first"), simple(1, 1)]),
        ),
        (
            "state-late",
            make(
                1,
                &[simple(1, 0), state(1, 1, false, b"later"), simple(1, 2)],
            ),
        ),
        (
            "state-two",
            make(
                2,
                &[
                    state(1, 0, false, b"first"),
                    state(2, 1, false, b"other"),
                    state(1, 2, true, b"second"),
                    simple(2, 3),
                ],
            ),
        ),
        (
            "state-empty",
            make(
                1,
                &[
                    state(1, 0, false, b"first"),
                    state(1, 1, false, b""),
                    simple(1, 2),
                ],
            ),
        ),
        (
            "state-limit",
            make(
                1,
                &[simple(1, 0), state(1, 1, false, &vec![0x55; (1 << 20) + 1])],
            ),
        ),
    ]
    .into_iter()
    .map(|(name, data)| (name.to_owned(), data))
    .collect()
}
