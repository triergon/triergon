//! Raw AAC-LC decoding for the 48 kHz mono 960-sample program profile.

use crate::decoder::{ID_DSE, ID_END, ID_FIL, ID_SCE};
use crate::filterbank_960::{Filterbank960, Sequence, Shape};
use crate::ics::{AOT_LC, ChannelData, CoreCfg, TNS_SYNTHESIS, parse_ics_for, tns_filter_for};
use crate::{AudioFrame, ChannelLayout, DecodeError, Feature, Speaker, pce};
use triergon_bitstream::CachedBitReader;
use triergon_codec::{AudioDecoder, Budget, Charged, DecodeFailure, DownmixLevels, Reservation};

const INITIAL_RANDOM: u32 = 0x1f2e3d4c;

/// Decoder for the 48 kHz mono AAC-LC program with 960 samples per access unit.
#[derive(Debug)]
pub struct RawCompactLcDecoder {
    filterbank: Filterbank960,
    channel: Charged<Box<ChannelData>>,
    overlap: [f32; 960],
    previous_shape: u8,
    random: u32,
    tag: u8,
    output_charge: Reservation,
}

impl RawCompactLcDecoder {
    /// Configure from the complete byte-aligned program configuration element.
    pub fn with_program_config_in(
        frequency_index: u8,
        program_config: &[u8],
        budget: &Budget,
    ) -> Result<Self, DecodeError> {
        let mut reader = CachedBitReader::new(program_config);
        let program = pce::parse_pce(&mut reader, 0)?;
        if reader.left() != 0
            || frequency_index != 3
            || program.object_type != 1
            || program.sf_index != 3
            || program.front.len() != 1
            || program.front[0].is_cpe
            || program.front[0].height != 0
            || !program.side.is_empty()
            || !program.back.is_empty()
            || !program.lfe.is_empty()
            || !program.cc.is_empty()
        {
            return Err(DecodeError::Unsupported(Feature::FrameLength960));
        }
        Ok(Self {
            filterbank: Filterbank960::new(budget)?,
            channel: Charged::boxed(budget, ChannelData::new())?,
            overlap: [0.0; 960],
            previous_shape: 0,
            random: INITIAL_RANDOM,
            tag: program.front[0].tag,
            output_charge: Reservation::empty(budget),
        })
    }

    /// Decode one complete raw data block. On error the output is empty and history is preserved.
    pub fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        out.samples_per_channel = 0;
        let result = self.decode_inner(au, out);
        if result.is_err() {
            out.clear();
        }
        result
    }

    fn decode_inner(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<(), DecodeError> {
        if au.len() > crate::RawLcDecoder::MAX_ACCESS_UNIT_BYTES {
            return Err(DecodeError::ResourceLimit("raw AAC access unit"));
        }
        let mut br = CachedBitReader::new(au);
        let cfg = CoreCfg {
            aot: AOT_LC,
            sf_index: 3,
        };
        let mut random = self.random;
        let mut decoded = false;
        loop {
            if br.left() < 3 {
                return Err(DecodeError::InvalidData("missing END element"));
            }
            match br.read(3) as u8 {
                ID_SCE => {
                    let tag = br.read(4) as u8;
                    if decoded || tag != self.tag {
                        return Err(DecodeError::InvalidData(
                            "element not in channel configuration",
                        ));
                    }
                    parse_ics_for::<960>(
                        &mut br,
                        &cfg,
                        false,
                        false,
                        &mut self.channel,
                        &mut random,
                    )?;
                    decoded = true;
                }
                ID_DSE => {
                    br.skip(4);
                    let align = br.bit();
                    let mut count = br.read(8) as usize;
                    if count == 255 {
                        count += br.read(8) as usize;
                    }
                    if align {
                        let pos = br.position();
                        br.skip((8 - (pos & 7)) & 7);
                    }
                    br.skip(count * 8);
                }
                ID_FIL => {
                    let mut count = br.read(4) as usize;
                    if count == 15 {
                        count += br.read(8) as usize;
                        count -= 1;
                    }
                    if count > 0 {
                        if br.left() < (count * 8) as isize {
                            return Err(DecodeError::InvalidData("fill element overrun"));
                        }
                        let ext = br.read(4);
                        if matches!(ext, 13 | 14) {
                            return Err(DecodeError::Unsupported(Feature::RawSbr));
                        }
                        br.skip(count * 8 - 4);
                    }
                }
                ID_END => break,
                _ => return Err(DecodeError::Unsupported(Feature::FrameLength960)),
            }
            if br.overrun() {
                return Err(DecodeError::InvalidData("raw_data_block overrun"));
            }
        }
        let pos = br.position();
        br.skip((8 - (pos & 7)) & 7);
        if br.overrun() || br.left() != 0 {
            return Err(DecodeError::InvalidData("AAC access unit extent"));
        }
        if !decoded {
            return Err(DecodeError::InvalidData(
                "incomplete raw AAC channel layout",
            ));
        }
        out.try_resize(1, 960, &mut self.output_charge)?;
        let cd: &mut ChannelData = &mut self.channel;
        if cd.tns.present {
            tns_filter_for::<960>(&cd.tns, &cd.info, &cfg, &mut cd.coef, TNS_SYNTHESIS);
        }
        let seq = match cd.info.window_sequence {
            0 => Sequence::OnlyLong,
            1 => Sequence::LongStart,
            2 => Sequence::EightShort,
            _ => Sequence::LongStop,
        };
        let shape = |value| if value == 0 { Shape::Sine } else { Shape::Kbd };
        self.filterbank.synth(
            (&cd.coef[..960]).try_into().unwrap(),
            seq,
            shape(cd.info.window_shape),
            shape(self.previous_shape),
            &mut self.overlap,
            (&mut out.planes[0][..960]).try_into().unwrap(),
        );
        self.previous_shape = cd.info.window_shape;
        self.random = random;
        out.sample_rate = 48_000;
        out.layout = ChannelLayout::new([Speaker::FrontCenter]).expect("mono layout");
        out.samples_per_channel = 960;
        Ok(())
    }

    /// Clear signal history after a discontinuity.
    pub fn reset(&mut self) {
        self.overlap.fill(0.0);
        self.previous_shape = 0;
        self.random = INITIAL_RANDOM;
    }
}

impl AudioDecoder for RawCompactLcDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, DecodeFailure> {
        Self::decode(self, au, out).map_err(DecodeFailure::from_error)?;
        Ok(None)
    }
    fn reset(&mut self) {
        Self::reset(self);
    }
}
