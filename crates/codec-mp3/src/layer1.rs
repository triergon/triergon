//! Layer I (ISO/IEC 11172-3 2.4.1.5 and 2.4.3.2): 12 slots of 32 subbands per frame.

use triergon_bitstream::CachedBitReader;
use triergon_framing::MpegAudioHeader;

use crate::subband::{dequantize, joint_bound, scalefactor};
use crate::synthesis::Synthesis;

/// Decode one whole frame into `planes` (384 samples each).
pub(crate) fn decode(
    header: &MpegAudioHeader,
    frame: &[u8],
    synthesis: &mut [Synthesis; 2],
    planes: &mut [Vec<f32>],
) {
    let channels = usize::from(header.channels());
    let bound = joint_bound(header, 32);
    let mut reader = CachedBitReader::at(frame, if header.crc { 48 } else { 32 });
    // Bits per sample of each subband, 0 when it carries none. The forbidden
    // allocation 15 reads as 16 bits, as FFmpeg decodes it.
    let mut bits = [[0u32; 32]; 2];
    for sb in 0..32 {
        let coded = if sb < bound { channels } else { 1 };
        for ch in 0..coded {
            let allocation = reader.read(4);
            bits[ch][sb] = if allocation == 0 { 0 } else { allocation + 1 };
        }
        if coded < channels {
            bits[1][sb] = bits[0][sb];
        }
    }
    let mut scales = [[0f32; 32]; 2];
    for sb in 0..32 {
        for ch in 0..channels {
            if bits[ch][sb] != 0 {
                scales[ch][sb] = scalefactor(reader.read(6));
            }
        }
    }
    let mut subbands = [[0f32; 32]; 2];
    for slot in 0..12 {
        for sb in 0..32 {
            let coded = if sb < bound { channels } else { 1 };
            let mut fraction = 0.0;
            for ch in 0..channels {
                let width = bits[ch][sb];
                if ch < coded {
                    fraction = if width == 0 {
                        0.0
                    } else {
                        dequantize(reader.read(width), (1 << width) - 1)
                    };
                }
                subbands[ch][sb] = fraction * scales[ch][sb];
            }
        }
        for ((filter, subbands), plane) in
            synthesis.iter_mut().zip(&subbands).zip(planes.iter_mut())
        {
            filter.run(subbands, &mut plane.as_chunks_mut::<32>().0[slot]);
        }
    }
}
