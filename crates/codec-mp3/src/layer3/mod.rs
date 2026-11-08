//! Layer III (ISO/IEC 11172-3 2.4.3.4, ISO/IEC 13818-3 2.4.3): the bit reservoir and the
//! per-granule pipeline from side information to subband samples.

mod bands;
mod huffman;
mod huffman_tables;
mod hybrid;
mod requantize;
mod scalefactors;
mod side_info;
mod stereo;

use triergon_bitstream::CachedBitReader;
use triergon_framing::{MpegAudioHeader, MpegVersion};

use crate::DecodeError;
use crate::synthesis::Synthesis;
use bands::Bands;
use hybrid::Hybrid;
use scalefactors::Scalefactors;
use side_info::{GranuleChannel, SideInfo};
use stereo::{Intensity, JointStereo};

/// The furthest back `main_data_begin` reaches (9 bits).
const MAX_BACKSTEP: usize = 511;

/// The longest Layer III frame: 320 kbit/s at 32 kHz, or 160 kbit/s at 8 kHz, padded.
const MAX_FRAME: usize = 1441;

/// Main data of earlier frames, the last [`MAX_BACKSTEP`] bytes of the stream's
/// concatenated main data, followed by the current frame's.
#[derive(Debug)]
struct Reservoir {
    bytes: [u8; MAX_BACKSTEP + MAX_FRAME],
    /// Bytes of earlier frames at the start of `bytes`.
    held: usize,
}

impl Reservoir {
    fn new() -> Reservoir {
        Reservoir {
            bytes: [0; MAX_BACKSTEP + MAX_FRAME],
            held: 0,
        }
    }

    /// Append the current frame's main data, which starts `backstep` bytes before it.
    /// Returns where the main data lies in [`Reservoir::bytes`] and how many of its
    /// first bits the stream has not supplied: at its start, or after a reset, a frame
    /// may point back to bytes that were never received.
    fn append(&mut self, main_data: &[u8], backstep: usize) -> (core::ops::Range<usize>, usize) {
        let end = self.held + main_data.len();
        self.bytes[self.held..end].copy_from_slice(main_data);
        let available = backstep.min(self.held);
        let range = self.held - available..end;
        self.held = end;
        (range, 8 * (backstep - available))
    }

    /// Keep the tail of the main data for the frames that follow.
    fn retire(&mut self) {
        let keep = self.held.min(MAX_BACKSTEP);
        self.bytes.copy_within(self.held - keep..self.held, 0);
        self.held = keep;
    }
}

/// Signal state and scratch of one channel.
#[derive(Debug)]
struct Channel {
    scalefactors: Scalefactors,
    hybrid: Hybrid,
    /// The spectrum of the current granule, requantized then transformed in place.
    xr: [f32; 576],
    /// Lines of `xr` that may be nonzero; the rest are +0.
    lines: usize,
}

/// Layer III decoding state (about 14 KiB, kept inside the decoder's boxed state).
#[derive(Debug)]
pub(crate) struct Layer3 {
    reservoir: Reservoir,
    channels: [Channel; 2],
    /// The 18 samples of each subband of the current granule and channel.
    subbands: [[f32; 18]; 32],
}

impl Layer3 {
    pub(crate) fn new() -> Layer3 {
        let channel = || Channel {
            scalefactors: Scalefactors::default(),
            hybrid: Hybrid::default(),
            xr: [0.0; 576],
            lines: 0,
        };
        Layer3 {
            reservoir: Reservoir::new(),
            channels: [channel(), channel()],
            subbands: [[0.0; 18]; 32],
        }
    }

    /// Drop the reservoir and the overlap. Scalefactors stay, as FFmpeg keeps them for
    /// a granule that reuses them (`scfsi`) after a flush.
    pub(crate) fn reset(&mut self) {
        self.reservoir.held = 0;
        for channel in &mut self.channels {
            channel.hybrid.reset();
        }
    }

    /// Decode one whole frame into `planes` (one per channel, 576 samples per granule).
    pub(crate) fn decode(
        &mut self,
        header: &MpegAudioHeader,
        frame: &[u8],
        synthesis: &mut [Synthesis; 2],
        planes: &mut [Vec<f32>],
    ) -> Result<(), DecodeError> {
        let lsf = header.version != MpegVersion::Mpeg1;
        let channels = usize::from(header.channels());
        let bands =
            Bands::for_rate(header.sample_rate).ok_or(DecodeError::Invalid("sample rate"))?;
        let side_start = 4 + if header.crc { 2 } else { 0 };
        let main_data = frame
            .get(side_start + header.side_info_bytes()..)
            .ok_or(DecodeError::Invalid("truncated side information"))?;
        let side = SideInfo::read(
            &mut CachedBitReader::at(frame, side_start * 8),
            lsf,
            channels,
        );
        let backstep = side.map_or_else(
            |invalid| invalid.main_data_begin,
            |side| side.main_data_begin,
        );
        let (range, missing) = self.reservoir.append(main_data, backstep);
        let mut side = match side {
            Ok(side) => side,
            Err(invalid) => {
                self.reservoir.retire();
                return Err(DecodeError::Invalid(invalid.reason));
            }
        };
        let mut main = MainData {
            main: &self.reservoir.bytes[range],
            missing,
            position: 0,
            bands,
            lsf,
        };
        let joint = JointStereo::of_frame(header);
        for (gr, plane_part) in (0..if lsf { 1 } else { 2 }).zip((0..).step_by(576)) {
            let reuse = if gr == 1 { side.scfsi } else { [[false; 4]; 2] };
            let mut joint = joint;
            // A granule that starts in bytes the stream never supplied decodes as silence.
            let supplied = main.position >= main.missing;
            for (ch, channel) in self.channels[..channels].iter_mut().enumerate() {
                let g = &mut side.granules[gr][ch];
                if !supplied {
                    main.position += g.part2_3_length;
                    channel.xr.fill(0.0);
                    channel.lines = 0;
                    continue;
                }
                let intensity_right = ch == 1 && joint.is_some_and(|j| j.intensity.is_some());
                let scale = main.read_channel(channel, g, reuse[ch], intensity_right);
                if let (Some(joint), Some(scale)) =
                    (joint.as_mut(), scale.filter(|_| intensity_right))
                {
                    joint.intensity = Some(Intensity::Lsf { scale });
                }
            }
            if let (Some(joint), [left, right]) = (joint, &mut self.channels[..channels]) {
                joint.apply(
                    &mut left.xr,
                    &mut right.xr,
                    &side.granules[gr][1],
                    &right.scalefactors,
                    bands,
                );
                let lines = left.lines.max(right.lines);
                (left.lines, right.lines) = (lines, lines);
            }
            for (ch, (channel, plane)) in
                self.channels.iter_mut().zip(planes.iter_mut()).enumerate()
            {
                let g = &side.granules[gr][ch];
                channel.hybrid.run(
                    &mut channel.xr,
                    channel.lines,
                    g.block_type,
                    g.mixed,
                    bands,
                    &mut self.subbands,
                );
                let (slots, _) = plane[plane_part..plane_part + 576].as_chunks_mut::<32>();
                for (t, pcm) in slots.iter_mut().enumerate() {
                    let slot = core::array::from_fn(|sb| self.subbands[sb][t]);
                    synthesis[ch].run(&slot, pcm);
                }
            }
        }
        self.reservoir.retire();
        Ok(())
    }
}

/// The main data of one frame as its granules read it.
struct MainData<'a> {
    main: &'a [u8],
    /// Leading bits of the main data the stream never supplied.
    missing: usize,
    /// Start of the next part (scalefactors and Huffman data) in the main data, in bits
    /// from its true start.
    position: usize,
    bands: &'static Bands,
    lsf: bool,
}

impl MainData<'_> {
    /// Read the scalefactors and spectrum of one channel into `channel.xr`, from a part
    /// that starts in supplied bytes. Returns `intensity_scale` for the low sampling
    /// frequencies.
    fn read_channel(
        &mut self,
        channel: &mut Channel,
        g: &mut GranuleChannel,
        reuse: [bool; 4],
        intensity_right: bool,
    ) -> Option<bool> {
        let start = self.position - self.missing;
        self.position += g.part2_3_length;
        let end = self.position - self.missing;
        let mut reader = CachedBitReader::at(self.main, start);
        let scale = if self.lsf {
            Some(
                channel
                    .scalefactors
                    .read_lsf(&mut reader, g, intensity_right),
            )
        } else {
            channel.scalefactors.read_mpeg1(&mut reader, g, reuse);
            None
        };
        let lines = huffman::read_spectrum(&mut reader, end, g, self.bands, &mut channel.xr);
        channel.lines = lines;
        requantize::requantize(&mut channel.xr, lines, g, &channel.scalefactors, self.bands);
        scale
    }
}
