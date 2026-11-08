//! One frame (RFC 9639 section 9): header, subframes, padding and CRC-16.
use crate::output::Output;
use crate::predict::Restore;
use crate::scratch::Scratch;
use crate::stereo::Stereo;
use crate::stream::Stream;
use crate::{CrcPolicy, DecodeError, FrameInfo, subframe};
use triergon_bitstream::CachedBitReader;
use triergon_bitstream::crc::crc16_a52;
use triergon_codec::{Budget, StorageError};
use triergon_framing::FlacFrameHeader;

/// Everything a frame decodes against: the stream parameters, the channel buffers and
/// the CRC policy.
#[derive(Debug)]
pub(crate) struct FrameDecoder {
    stream: Stream,
    pub(crate) crc: CrcPolicy,
    scratch: Scratch,
}

impl FrameDecoder {
    pub(crate) fn new(stream: Stream, budget: &Budget) -> Self {
        FrameDecoder {
            stream,
            crc: CrcPolicy::default(),
            scratch: Scratch::new(budget),
        }
    }

    /// Decodes the frame `au` into `out`, leaving it empty on failure.
    pub(crate) fn decode(
        &mut self,
        au: &[u8],
        out: &mut impl Output,
    ) -> Result<FrameInfo, DecodeError> {
        let result = self.decode_into(au, out);
        if result.is_err() {
            out.abandon();
        }
        result
    }

    fn decode_into(&mut self, au: &[u8], out: &mut impl Output) -> Result<FrameInfo, DecodeError> {
        let header = FlacFrameHeader::parse(au).map_err(|error| match error {
            StorageError::Invalid(what) => DecodeError::Invalid(what),
            _ => DecodeError::Invalid("truncated frame header"),
        })?;
        let frame = self.stream.frame(&header)?;
        out.begin(&frame)?;
        let len = frame.samples_per_channel;
        let bits = u32::from(frame.bits_per_sample);
        let mut reader = CachedBitReader::at(au, header.len * 8);
        match Stereo::of(header.channels) {
            Some(stereo) if bits < 32 => {
                let (plain, side) = self.scratch.pair(len)?;
                decode_stereo(&mut reader, stereo, bits, plain, side, out)?;
            }
            Some(stereo) => {
                let (plain, side) = self.scratch.wide_pair(len)?;
                decode_stereo(&mut reader, stereo, bits, plain, side, out)?;
            }
            None => {
                let samples = self.scratch.plain(len)?;
                for plane in 0..usize::from(header.channels.channels()) {
                    subframe::decode(&mut reader, bits, samples)?;
                    out.put(plane, samples.iter().copied());
                }
            }
        }
        // Zero padding to a byte, then the CRC-16 of everything before it.
        reader.align(8);
        let frame_bytes = au
            .get(..reader.position() / 8 + 2)
            .ok_or(DecodeError::Invalid("frame without its CRC-16"))?;
        if self.crc == CrcPolicy::Reject && crc16_a52(0, frame_bytes) != 0 {
            return Err(DecodeError::CrcMismatch);
        }
        Ok(frame)
    }
}

/// Decodes a decorrelated pair in coded order, then writes left and right.
fn decode_stereo<S: Restore>(
    reader: &mut CachedBitReader<'_>,
    stereo: Stereo,
    bits: u32,
    plain: &mut [i32],
    side: &mut [S],
    out: &mut impl Output,
) -> Result<(), DecodeError> {
    if stereo.side_first() {
        subframe::decode(reader, bits + 1, side)?;
        subframe::decode(reader, bits, plain)?;
    } else {
        subframe::decode(reader, bits, plain)?;
        subframe::decode(reader, bits + 1, side)?;
    }
    stereo.emit(plain, side, out);
    Ok(())
}
