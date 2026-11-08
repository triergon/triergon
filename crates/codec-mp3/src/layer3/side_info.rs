//! Layer III side information (ISO/IEC 11172-3 2.4.1.7, ISO/IEC 13818-3 2.4.1.7).

use triergon_bitstream::CachedBitReader;

use super::bands::Bands;

/// The window sequence of one granule of one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BlockType {
    /// One long block (`block_type` 0).
    #[default]
    Long,
    /// The long block opening a run of short blocks (1).
    Start,
    /// Three short blocks (2).
    Short,
    /// The long block closing a run of short blocks (3).
    Stop,
}

/// Side information of one channel in one granule.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GranuleChannel {
    /// Bits of scalefactors and Huffman data.
    pub(crate) part2_3_length: usize,
    /// Spectral line pairs coded with the big-value tables.
    pub(crate) big_values: usize,
    pub(crate) global_gain: u8,
    pub(crate) scalefac_compress: u16,
    pub(crate) block_type: BlockType,
    /// `mixed_block_flag`: the two lowest subbands are a long block with the normal
    /// window. It changes the scalefactor and band layout of short blocks only.
    pub(crate) mixed: bool,
    /// Big-value table of each region.
    pub(crate) table_select: [u8; 3],
    pub(crate) subblock_gain: [u8; 3],
    pub(crate) region0_count: u8,
    pub(crate) region1_count: u8,
    /// Pre-emphasis of the high long bands; read here for MPEG-1, derived from
    /// `scalefac_compress` for the low sampling frequencies.
    pub(crate) preflag: bool,
    pub(crate) scalefac_scale: bool,
    /// Count1 region coded with table B instead of A.
    pub(crate) count1_table_b: bool,
}

impl GranuleChannel {
    /// End line of each big-value region.
    pub(crate) fn region_ends(&self, bands: &Bands) -> [usize; 3] {
        let end = self.big_values * 2;
        let (region1, region2) = match self.block_type {
            BlockType::Short => (3 * usize::from(bands.short[3]), 576),
            BlockType::Start | BlockType::Stop => (usize::from(bands.long[8]), 576),
            BlockType::Long => {
                let r0 = usize::from(self.region0_count) + 1;
                let r1 = (r0 + usize::from(self.region1_count) + 1).min(22);
                (usize::from(bands.long[r0]), usize::from(bands.long[r1]))
            }
        };
        [region1.min(end), region2.min(end), end]
    }
}

/// Side information of one frame.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SideInfo {
    /// Bytes of main data taken from earlier frames.
    pub(crate) main_data_begin: usize,
    /// Scalefactor selection information: granule 1 reuses group `g` of granule 0.
    pub(crate) scfsi: [[bool; 4]; 2],
    /// `granules[gr][ch]`.
    pub(crate) granules: [[GranuleChannel; 2]; 2],
}

/// Side information FFmpeg refuses: the frame decodes to nothing, but its main data
/// still joins the bit reservoir.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InvalidSideInfo {
    pub(crate) main_data_begin: usize,
    pub(crate) reason: &'static str,
}

impl SideInfo {
    /// Read the side information of a frame with `channels` channels, one granule for
    /// the low sampling frequencies (`lsf`) and two otherwise.
    pub(crate) fn read(
        reader: &mut CachedBitReader<'_>,
        lsf: bool,
        channels: usize,
    ) -> Result<SideInfo, InvalidSideInfo> {
        let mut info = SideInfo::default();
        if lsf {
            info.main_data_begin = reader.read(8) as usize;
            reader.skip(channels);
        } else {
            info.main_data_begin = reader.read(9) as usize;
            reader.skip(if channels == 1 { 5 } else { 3 });
            for scfsi in &mut info.scfsi[..channels] {
                for group in scfsi {
                    *group = reader.bit();
                }
            }
        }
        for granule in &mut info.granules[..if lsf { 1 } else { 2 }] {
            for channel in &mut granule[..channels] {
                *channel = GranuleChannel::read(reader, lsf).map_err(|reason| InvalidSideInfo {
                    main_data_begin: info.main_data_begin,
                    reason,
                })?;
            }
        }
        Ok(info)
    }
}

impl GranuleChannel {
    fn read(reader: &mut CachedBitReader<'_>, lsf: bool) -> Result<GranuleChannel, &'static str> {
        let mut g = GranuleChannel {
            part2_3_length: reader.read(12) as usize,
            big_values: reader.read(9) as usize,
            global_gain: reader.read(8) as u8,
            scalefac_compress: reader.read(if lsf { 9 } else { 4 }) as u16,
            ..GranuleChannel::default()
        };
        if g.big_values > 288 {
            return Err("big_values beyond the granule");
        }
        if reader.bit() {
            g.block_type = match reader.read(2) {
                1 => BlockType::Start,
                2 => BlockType::Short,
                3 => BlockType::Stop,
                _ => return Err("block type 0 with window switching"),
            };
            g.mixed = reader.bit();
            for table in &mut g.table_select[..2] {
                *table = reader.read(5) as u8;
            }
            for gain in &mut g.subblock_gain {
                *gain = reader.read(3) as u8;
            }
        } else {
            for table in &mut g.table_select {
                *table = reader.read(5) as u8;
            }
            g.region0_count = reader.read(4) as u8;
            g.region1_count = reader.read(3) as u8;
        }
        if !lsf {
            g.preflag = reader.bit();
        }
        g.scalefac_scale = reader.bit();
        g.count1_table_b = reader.bit();
        Ok(g)
    }
}
