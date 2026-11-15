//! Authored ISO BMFF movies for the seek and range-plan suite.
//!
//! Each movie is built here from its box structure (ISO/IEC 14496-12 §§8.2–8.8 and
//! the QuickTime File Format for the `qt  ` brand and `raw ` sample entry), with
//! two-by-two 24-bit `raw ` samples of 12 bytes whose bytes are a function of the
//! track and sample number. They are longer than the open and index corpus's
//! movies, so that a seek searches a real index: up to 1,800 samples per track,
//! with sync samples every 30 or fewer, regular and fragmented, with composition
//! offsets and edit lists. The committed files are these bytes; the suite's test
//! regenerates them.

/// One sample in decode order.
#[derive(Clone, Copy)]
struct Sample {
    duration: u32,
    composition_offset: u32,
    sync: bool,
}

/// One edit: a segment duration in the movie clock and a media time, or an empty
/// edit at media time -1.
#[derive(Clone, Copy)]
struct Edit {
    duration: u32,
    media_time: i32,
}

struct Track {
    id: u32,
    timescale: u32,
    samples: Vec<Sample>,
    edits: Vec<Edit>,
    /// Samples per chunk of a regular movie.
    chunk: usize,
}

/// How a regular movie lays out its boxes.
#[derive(Clone, Copy)]
struct Layout {
    brand: [u8; 4],
    movie_first: bool,
    co64: bool,
    /// A sample size table rather than one constant size.
    size_table: bool,
}

const MOVIE_TIMESCALE: u32 = 1000;
const SAMPLE_BYTES: u32 = 12;

/// Every authored movie, by file name.
pub fn all() -> Vec<(&'static str, Vec<u8>)> {
    let qt = Layout {
        brand: *b"qt  ",
        movie_first: true,
        co64: false,
        size_table: true,
    };
    vec![
        (
            "regular-front-1800.mov",
            regular(&[gop_track(1, 1800, 30)], qt),
        ),
        (
            "regular-end-co64-1800.mov",
            regular(
                &[gop_track(1, 1800, 30)],
                Layout {
                    movie_first: false,
                    co64: true,
                    size_table: false,
                    ..qt
                },
            ),
        ),
        (
            "regular-two-tracks.mov",
            regular(&[gop_track(1, 600, 30), half_rate_track(2, 1200, 48)], qt),
        ),
        (
            "regular-composition-600.mov",
            regular(&[reordered_track(1, 600)], qt),
        ),
        (
            "regular-edit-media-offset.mov",
            regular(
                &[Track {
                    edits: vec![Edit {
                        duration: 19_900,
                        media_time: 9000,
                    }],
                    ..gop_track(1, 600, 30)
                }],
                qt,
            ),
        ),
        (
            "regular-leading-empty-edit.mov",
            regular(
                &[Track {
                    edits: vec![
                        Edit {
                            duration: 500,
                            media_time: -1,
                        },
                        Edit {
                            duration: 20_000,
                            media_time: 0,
                        },
                    ],
                    ..gop_track(1, 600, 30)
                }],
                qt,
            ),
        ),
        (
            "fragmented-1200.mp4",
            fragmented(&gop_track(1, 1200, 30), 30),
        ),
        (
            "fragmented-two-gops.mp4",
            fragmented(&gop_track(1, 1200, 30), 60),
        ),
    ]
}

/// A 90 kHz track at 30 frames per second, a sync sample every `gop`.
fn gop_track(id: u32, count: usize, gop: usize) -> Track {
    Track {
        id,
        timescale: 90_000,
        samples: (0..count)
            .map(|i| Sample {
                duration: 3000,
                composition_offset: 0,
                sync: i % gop == 0,
            })
            .collect(),
        edits: Vec::new(),
        chunk: 15,
    }
}

/// A 90 kHz track at 60 samples per second, a sync sample every `gop`.
fn half_rate_track(id: u32, count: usize, gop: usize) -> Track {
    let mut track = gop_track(id, count, gop);
    for sample in &mut track.samples {
        sample.duration = 1500;
    }
    track.chunk = 30;
    track
}

/// A 90 kHz track in groups of 28: a sync sample presented one frame late, then
/// repeated P, B, B triples, the P presented three frames late.
fn reordered_track(id: u32, count: usize) -> Track {
    let mut track = gop_track(id, count, 28);
    for (i, sample) in track.samples.iter_mut().enumerate() {
        sample.composition_offset = match i % 28 {
            0 => 3000,
            n if n % 3 == 1 => 9000,
            _ => 0,
        };
    }
    track
}

fn payload(track: u32, ordinal: usize) -> [u8; SAMPLE_BYTES as usize] {
    std::array::from_fn(|i| (ordinal as u32 * 13 + i as u32 + track * 101) as u8)
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(body.len() + 4);
    data.extend_from_slice(&((u32::from(version) << 24) | flags).to_be_bytes());
    data.extend_from_slice(body);
    boxed(kind, &data)
}

fn words(values: impl IntoIterator<Item = u32>) -> Vec<u8> {
    values.into_iter().flat_map(u32::to_be_bytes).collect()
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn file_type(brand: [u8; 4]) -> Vec<u8> {
    let compatible: &[[u8; 4]] = if brand == *b"qt  " {
        &[*b"qt  "]
    } else {
        &[*b"isom", brand]
    };
    let mut body = brand.to_vec();
    body.extend_from_slice(&[0; 4]);
    for code in compatible {
        body.extend_from_slice(code);
    }
    boxed(b"ftyp", &body)
}

fn media_duration(track: &Track) -> u64 {
    track.samples.iter().map(|s| u64::from(s.duration)).sum()
}

/// The track's duration in the movie clock: its edits, or its media.
fn movie_duration(track: &Track) -> u32 {
    if track.edits.is_empty() {
        (media_duration(track) * u64::from(MOVIE_TIMESCALE) / u64::from(track.timescale)) as u32
    } else {
        track.edits.iter().map(|e| e.duration).sum()
    }
}

fn movie_header(tracks: &[Track]) -> Vec<u8> {
    let duration = tracks.iter().map(movie_duration).max().unwrap_or(0);
    let mut body = words([0, 0, MOVIE_TIMESCALE, duration, 0x0001_0000]);
    body.extend_from_slice(&[0x01, 0x00]);
    body.extend_from_slice(&[0; 10]);
    body.extend(words(MATRIX));
    body.extend_from_slice(&[0; 24]);
    body.extend(words([tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1]));
    full(b"mvhd", 0, 0, &body)
}

fn track_header(track: &Track) -> Vec<u8> {
    let mut body = words([0, 0, track.id, 0, movie_duration(track), 0, 0]);
    body.extend_from_slice(&[0; 8]);
    body.extend(words(MATRIX));
    body.extend(words([0x0002_0000, 0x0002_0000]));
    full(b"tkhd", 0, 7, &body)
}

fn edits(track: &Track) -> Vec<u8> {
    if track.edits.is_empty() {
        return Vec::new();
    }
    let mut body = words([track.edits.len() as u32]);
    for edit in &track.edits {
        body.extend(words([edit.duration, edit.media_time as u32, 0x0001_0000]));
    }
    boxed(b"edts", &full(b"elst", 0, 0, &body))
}

fn sample_description() -> Vec<u8> {
    let mut entry = vec![0; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 16]);
    entry.extend_from_slice(&[0, 2, 0, 2]);
    entry.extend(words([0x0048_0000, 0x0048_0000, 0]));
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0; 32]);
    entry.extend_from_slice(&[0x00, 0x18, 0xff, 0xff]);
    let mut body = words([1]);
    body.extend(boxed(b"raw ", &entry));
    full(b"stsd", 0, 0, &body)
}

/// The media box around a sample table.
fn media(track: &Track, table: &[Vec<u8>]) -> Vec<u8> {
    let mut header = words([0, 0, track.timescale, media_duration(track) as u32]);
    header.extend_from_slice(&[0x55, 0xc4, 0, 0]);
    let mut handler = words([0]);
    handler.extend_from_slice(b"vide");
    handler.extend_from_slice(&[0; 12]);
    handler.extend_from_slice(b"Authored seek\0");
    let mut information = full(b"vmhd", 0, 1, &[0; 8]);
    information.extend(boxed(
        b"dinf",
        &full(
            b"dref",
            0,
            0,
            &concat(&[words([1]), full(b"url ", 0, 1, &[])]),
        ),
    ));
    information.extend(boxed(b"stbl", &concat(table)));
    boxed(
        b"mdia",
        &concat(&[
            full(b"mdhd", 0, 0, &header),
            full(b"hdlr", 0, 0, &handler),
            boxed(b"minf", &information),
        ]),
    )
}

/// Run-length entries of `(count, value)`.
fn runs(values: impl IntoIterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for value in values {
        match runs.last_mut() {
            Some((count, last)) if *last == value => *count += 1,
            _ => runs.push((1, value)),
        }
    }
    runs
}

fn run_table(kind: &[u8; 4], runs: &[(u32, u32)]) -> Vec<u8> {
    let mut body = words([runs.len() as u32]);
    for &(count, value) in runs {
        body.extend(words([count, value]));
    }
    full(kind, 0, 0, &body)
}

/// A regular track's sample table with its chunk offsets.
fn sample_table(track: &Track, layout: Layout, offsets: &[u64]) -> Vec<Vec<u8>> {
    let count = track.samples.len() as u32;
    let mut table = vec![
        sample_description(),
        run_table(b"stts", &runs(track.samples.iter().map(|s| s.duration))),
    ];
    if track.samples.iter().any(|s| s.composition_offset != 0) {
        table.push(run_table(
            b"ctts",
            &runs(track.samples.iter().map(|s| s.composition_offset)),
        ));
    }
    let last = track.samples.len() % track.chunk;
    let mut chunks = vec![(1, track.chunk as u32, 1)];
    if last != 0 {
        chunks.push((offsets.len() as u32, last as u32, 1));
    }
    let mut body = words([chunks.len() as u32]);
    for (first, samples, description) in chunks {
        body.extend(words([first, samples, description]));
    }
    table.push(full(b"stsc", 0, 0, &body));
    table.push(if layout.size_table {
        full(
            b"stsz",
            0,
            0,
            &concat(&[words([0, count]), words((0..count).map(|_| SAMPLE_BYTES))]),
        )
    } else {
        full(b"stsz", 0, 0, &words([SAMPLE_BYTES, count]))
    });
    let mut body = words([offsets.len() as u32]);
    if layout.co64 {
        body.extend(offsets.iter().flat_map(|o| o.to_be_bytes()));
        table.push(full(b"co64", 0, 0, &body));
    } else {
        body.extend(words(offsets.iter().map(|&o| o as u32)));
        table.push(full(b"stco", 0, 0, &body));
    }
    let sync: Vec<u32> = (1..=count)
        .filter(|&n| track.samples[n as usize - 1].sync)
        .collect();
    if sync.len() != track.samples.len() {
        table.push(full(
            b"stss",
            0,
            0,
            &concat(&[words([sync.len() as u32]), words(sync)]),
        ));
    }
    table
}

fn regular_movie(tracks: &[Track], layout: Layout, offsets: &[Vec<u64>]) -> Vec<u8> {
    let mut body = movie_header(tracks);
    for (track, offsets) in tracks.iter().zip(offsets) {
        body.extend(boxed(
            b"trak",
            &concat(&[
                track_header(track),
                edits(track),
                media(track, &sample_table(track, layout, offsets)),
            ]),
        ));
    }
    boxed(b"moov", &body)
}

/// A regular movie whose chunks interleave the tracks by decode time.
fn regular(tracks: &[Track], layout: Layout) -> Vec<u8> {
    // Chunks by start time, then track: (start on the grid of every clock, track, first sample).
    let grid: u128 = tracks.iter().map(|t| u128::from(t.timescale)).product();
    let mut chunks: Vec<(u128, usize, usize)> = Vec::new();
    for (t, track) in tracks.iter().enumerate() {
        let mut time = 0u128;
        for (i, sample) in track.samples.iter().enumerate() {
            if i % track.chunk == 0 {
                chunks.push((time * grid / u128::from(track.timescale), t, i));
            }
            time += u128::from(sample.duration);
        }
    }
    chunks.sort_unstable();
    let mut media = Vec::new();
    let mut relative: Vec<Vec<u64>> = vec![Vec::new(); tracks.len()];
    for &(_, t, first) in &chunks {
        relative[t].push(media.len() as u64);
        let track = &tracks[t];
        for ordinal in first..(first + track.chunk).min(track.samples.len()) {
            media.extend_from_slice(&payload(track.id, ordinal));
        }
    }
    let file_type = file_type(layout.brand);
    let placed = |base: u64| -> Vec<Vec<u64>> {
        relative
            .iter()
            .map(|offsets| offsets.iter().map(|o| o + base).collect())
            .collect()
    };
    let media_box = boxed(b"mdat", &media);
    if layout.movie_first {
        let size = regular_movie(tracks, layout, &placed(0)).len() as u64;
        let base = file_type.len() as u64 + size + 8;
        concat(&[
            file_type,
            regular_movie(tracks, layout, &placed(base)),
            media_box,
        ])
    } else {
        let base = file_type.len() as u64 + 8;
        let movie = regular_movie(tracks, layout, &placed(base));
        concat(&[file_type, media_box, movie])
    }
}

/// A fragmented movie of one track: an empty sample table, a `trex` and one
/// fragment of `per_fragment` samples after another, each with a `tfdt`.
fn fragmented(track: &Track, per_fragment: usize) -> Vec<u8> {
    let empty = [
        sample_description(),
        full(b"stts", 0, 0, &words([0])),
        full(b"stsc", 0, 0, &words([0])),
        full(b"stsz", 0, 0, &words([0, 0])),
        full(b"stco", 0, 0, &words([0])),
    ];
    let extends = boxed(
        b"mvex",
        &full(
            b"trex",
            0,
            0,
            &words([track.id, 1, 3000, SAMPLE_BYTES, 0x0101_0000]),
        ),
    );
    let movie = Track {
        id: track.id,
        timescale: track.timescale,
        samples: Vec::new(),
        edits: Vec::new(),
        chunk: 1,
    };
    let mut out = file_type(*b"iso6");
    out.extend(boxed(
        b"moov",
        &concat(&[
            movie_header(std::slice::from_ref(&movie)),
            boxed(
                b"trak",
                &concat(&[track_header(&movie), media(&movie, &empty)]),
            ),
            extends,
        ]),
    ));
    let mut decode = 0u64;
    for (n, samples) in track.samples.chunks(per_fragment).enumerate() {
        let first = n * per_fragment;
        let run = |data_offset: u32| {
            let mut body = words([samples.len() as u32, data_offset]);
            for sample in samples {
                let flags = if sample.sync {
                    0x0200_0000
                } else {
                    0x0101_0000
                };
                body.extend(words([sample.duration, SAMPLE_BYTES, flags]));
            }
            full(b"trun", 0, 0x0701, &body)
        };
        let fragment = |data_offset: u32| {
            let mut time = vec![];
            time.extend_from_slice(&decode.to_be_bytes());
            boxed(
                b"moof",
                &concat(&[
                    full(b"mfhd", 0, 0, &words([n as u32 + 1])),
                    boxed(
                        b"traf",
                        &concat(&[
                            full(b"tfhd", 0, 0x02_0002, &words([track.id, 1])),
                            full(b"tfdt", 1, 0, &time),
                            run(data_offset),
                        ]),
                    ),
                ]),
            )
        };
        let size = fragment(0).len() as u32;
        out.extend(fragment(size + 8));
        let media: Vec<u8> = (first..first + samples.len())
            .flat_map(|ordinal| payload(track.id, ordinal))
            .collect();
        out.extend(boxed(b"mdat", &media));
        decode += samples.iter().map(|s| u64::from(s.duration)).sum::<u64>();
    }
    out
}
