//! Scalefactors: ISO/IEC 11172-3 2.4.2.7 (`scalefac_compress`, `scfsi`) and the scheme
//! of the low sampling frequencies in ISO/IEC 13818-3 2.4.3.2, intensity stereo included.

use triergon_bitstream::CachedBitReader;

use super::side_info::{BlockType, GranuleChannel};

/// The scalefactors of one channel. They persist between the granules of a frame,
/// which `scfsi` relies on.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Scalefactors {
    /// Long band `b`; band 21 carries none and stays 0.
    pub(crate) long: [u8; 22],
    /// Short band `b`, window `w`; band 12 carries none and stays 0.
    pub(crate) short: [[u8; 3]; 13],
}

/// `slen1` and `slen2` of each `scalefac_compress` (ISO/IEC 11172-3 2.4.2.7).
const MPEG1_SLEN: [(u32, u32); 16] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (3, 0),
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 1),
    (2, 2),
    (2, 3),
    (3, 1),
    (3, 2),
    (3, 3),
    (4, 2),
    (4, 3),
];

/// The long bands each `scfsi` bit covers.
const SCFSI_GROUPS: [core::ops::Range<usize>; 4] = [0..6, 6..11, 11..16, 16..21];

/// `nr_of_sfb` of ISO/IEC 13818-3 Table B.2: scalefactors in each of the four `slen`
/// groups, for long, short and mixed blocks (short ones count every window).
const LSF_GROUP_SIZES: [[[u8; 4]; 3]; 6] = [
    [[6, 5, 5, 5], [9, 9, 9, 9], [6, 9, 9, 9]],
    [[6, 5, 7, 3], [9, 9, 12, 6], [6, 9, 12, 6]],
    [[11, 10, 0, 0], [18, 18, 0, 0], [15, 18, 0, 0]],
    [[7, 7, 7, 0], [12, 12, 12, 0], [6, 15, 12, 0]],
    [[6, 6, 6, 3], [12, 9, 9, 6], [6, 12, 9, 6]],
    [[8, 8, 5, 0], [15, 12, 9, 0], [6, 18, 9, 0]],
];

impl Scalefactors {
    /// Read the MPEG-1 scalefactors of granule `g`; groups with `reuse` set keep the
    /// values of the previous granule.
    pub(crate) fn read_mpeg1(
        &mut self,
        reader: &mut CachedBitReader<'_>,
        g: &GranuleChannel,
        reuse: [bool; 4],
    ) {
        let (slen1, slen2) = MPEG1_SLEN[usize::from(g.scalefac_compress & 15)];
        match g.block_type {
            BlockType::Short => {
                let first_short = if g.mixed {
                    for band in 0..8 {
                        self.long[band] = read(reader, slen1);
                    }
                    3
                } else {
                    0
                };
                for band in first_short..12 {
                    let slen = if band < 6 { slen1 } else { slen2 };
                    for window in &mut self.short[band] {
                        *window = read(reader, slen);
                    }
                }
            }
            _ => {
                for (group, bands) in SCFSI_GROUPS.into_iter().enumerate() {
                    if reuse[group] {
                        continue;
                    }
                    let slen = if group < 2 { slen1 } else { slen2 };
                    for band in bands {
                        self.long[band] = read(reader, slen);
                    }
                }
            }
        }
    }

    /// Read the scalefactors of the low sampling frequencies and set `g.preflag`. For
    /// the right channel of an intensity-stereo frame (`intensity_right`) the fields
    /// are intensity positions; the result is then `intensity_scale`.
    pub(crate) fn read_lsf(
        &mut self,
        reader: &mut CachedBitReader<'_>,
        g: &mut GranuleChannel,
        intensity_right: bool,
    ) -> bool {
        let (slen, table) = lsf_slen(u32::from(g.scalefac_compress), intensity_right);
        g.preflag = table == 2;
        let (shape, long_bands, short_bands) = match (g.block_type, g.mixed) {
            (BlockType::Short, false) => (1, 0..0, 0..12),
            (BlockType::Short, true) => (2, 0..6, 3..12),
            _ => (0, 0..21, 0..0),
        };
        let mut slots = long_bands.map(Slot::Long).chain(
            short_bands.flat_map(|band| (0..3).map(move |window| Slot::Short(band, window))),
        );
        for (&count, slen) in LSF_GROUP_SIZES[table][shape].iter().zip(slen) {
            for slot in slots.by_ref().take(usize::from(count)) {
                let value = read(reader, slen);
                match slot {
                    Slot::Long(band) => self.long[band] = value,
                    Slot::Short(band, window) => self.short[band][window] = value,
                }
            }
        }
        g.scalefac_compress & 1 == 1
    }
}

/// The four `slen` widths and the `nr_of_sfb` table of a `scalefac_compress` value
/// (ISO/IEC 13818-3 2.4.3.2), for an intensity-coded right channel or not.
fn lsf_slen(compress: u32, intensity_right: bool) -> ([u32; 4], usize) {
    if intensity_right {
        match compress >> 1 {
            c @ 0..180 => ([c / 36, (c % 36) / 6, c % 6, 0], 3),
            c @ 180..244 => {
                let c = c - 180;
                ([(c % 64) >> 4, (c % 16) >> 2, c % 4, 0], 4)
            }
            c => {
                let c = c - 244;
                ([c / 3, c % 3, 0, 0], 5)
            }
        }
    } else {
        match compress {
            c @ 0..400 => ([(c >> 4) / 5, (c >> 4) % 5, (c & 15) >> 2, c & 3], 0),
            c @ 400..500 => {
                let c = c - 400;
                ([(c >> 2) / 5, (c >> 2) % 5, c & 3, 0], 1)
            }
            c => {
                let c = c - 500;
                ([c / 3, c % 3, 0, 0], 2)
            }
        }
    }
}

/// Where one scalefactor of the low-sampling-frequency scheme goes.
#[derive(Clone, Copy)]
enum Slot {
    Long(usize),
    Short(usize, usize),
}

fn read(reader: &mut CachedBitReader<'_>, bits: u32) -> u8 {
    if bits == 0 {
        0
    } else {
        reader.read(bits) as u8
    }
}
