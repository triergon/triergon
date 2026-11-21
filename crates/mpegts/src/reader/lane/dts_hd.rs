//! One core-bearing DTS-HD access unit per aligned DVB PES (TS 102 114 F.4.3).
use super::{DtsAudioFrame, aligned_audio};
use crate::{Pes, psi::DvbDtsHd};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Error, InputRequest, InputWindow, Result, SourceCapabilities, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{
    DtsCoreHeader, DtsFrameExtentLimits, probe_dts_core_header,
    probe_dts_extension_substream_extent,
};

struct HeaderCrc {
    at: usize,
    end: usize,
    remainder: u16,
}

struct Validation {
    frame: aligned_audio::Frame<DtsCoreHeader>,
    offset: usize,
    remaining: u8,
    crc: Option<HeaderCrc>,
}

pub(super) struct Reader {
    input: aligned_audio::Reader<15, DtsCoreHeader>,
    extensions: u8,
    pes_bytes: usize,
    validation: Option<Validation>,
    incomplete: bool,
}

impl Reader {
    pub(super) fn new(
        pool: &Budget,
        source: SourceId,
        caps: SourceCapabilities,
        maximum: usize,
        window: usize,
        descriptor: DvbDtsHd<'_>,
    ) -> Result<Self> {
        let extensions = descriptor
            .extensions
            .iter()
            .enumerate()
            .fold(0_u8, |mask, (index, stream)| {
                mask | (u8::from(stream.is_some()) << index)
            });
        if descriptor.core.is_none() || extensions == 0 {
            return Err(Error::Unsupported("DVB DTS-HD core and extension layout"));
        }
        Ok(Self {
            input: aligned_audio::Reader::new(pool, source, caps, maximum, window)?,
            extensions,
            pes_bytes: 0,
            validation: None,
            incomplete: false,
        })
    }

    pub(super) fn check_pes(&self, pes: &Pes) -> Result<()> {
        if self.incomplete {
            return Err(Error::Truncated);
        }
        if pes.header.stream_id != 0xbd
            || pes.header.declared_packet_length == 0
            || !pes.header.data_alignment_indicator
            || pes.payload_len < 15
        {
            return Err(Error::Invalid("DTS-HD aligned PES"));
        }
        if pes.payload_len > self.input.maximum() {
            return Err(crate::limit(pes.payload_len, self.input.maximum()));
        }
        Ok(())
    }

    pub(super) fn begin_pes(&mut self, bytes: usize) {
        self.pes_bytes = bytes;
    }

    pub(super) fn input_wait_buffer_capacity(&self) -> usize {
        self.input.input_wait_buffer_capacity()
    }

    pub(super) fn provide(&mut self, window: InputWindow) -> Result<()> {
        self.input.provide(window)
    }

    pub(super) fn finish(&mut self, request: InputRequest) -> Result<()> {
        self.input.finish(request)
    }

    pub(super) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsAudioFrame>> {
        if self.incomplete {
            return self.await_logical_end(work, cancel);
        }
        if self.validation.is_none() {
            let size = self.pes_bytes;
            let frame = match self.input.step(work, cancel, |prefix| {
                Ok((probe_dts_core_header(prefix)?, size))
            })? {
                Step::Ready(frame) => frame,
                Step::Yield => return Ok(Step::Yield),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::End(end) => return Ok(Step::End(end)),
                Step::Backpressure(never) => match never {},
            };
            self.validation = Some(Validation {
                offset: frame.header.frame_bytes,
                frame,
                remaining: self.extensions,
                crc: None,
            });
        }
        let validation = self.validation.as_mut().expect("complete DTS-HD PES bytes");
        match validation.check(self.input.maximum(), work, cancel) {
            Ok(false) => return Ok(Step::Yield),
            Err(Error::Truncated) => {
                // Confirm logical EOF so the lane can report an incomplete
                // audio fragment. A further PES cannot extend an aligned AU.
                self.validation = None;
                self.incomplete = true;
                return self.await_logical_end(work, cancel);
            }
            result => {
                result?;
            }
        }
        let frame = self.validation.take().expect("validated DTS-HD PES").frame;
        Ok(Step::Ready(DtsAudioFrame {
            core_bytes: frame.header.frame_bytes,
            bytes: frame.bytes,
            offset: frame.offset,
            duration: frame.header.window,
        }))
    }

    fn await_logical_end(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Step<DtsAudioFrame>> {
        match self.input.input().read_discard(1, work, cancel)? {
            Step::NeedInput(request) => Ok(Step::NeedInput(request)),
            Step::Yield => Ok(Step::Yield),
            Step::End(_) => Err(Error::Truncated),
            Step::Ready(_) => Err(Error::Invalid("DTS-HD bytes beyond aligned PES")),
            Step::Backpressure(never) => match never {},
        }
    }
}

impl Validation {
    fn check(
        &mut self,
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        let bytes = self.frame.bytes.as_ref();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(crc) = &mut self.crc {
                let end = crc.at + work.take((crc.end - crc.at).min(256) as u64) as usize;
                for &byte in &bytes[crc.at..end] {
                    // Fold the high byte through x^16 + x^12 + x^5 + 1.
                    let high = (crc.remainder >> 8) ^ u16::from(byte);
                    let folded = high ^ (high >> 4);
                    crc.remainder = (crc.remainder << 8) ^ (folded << 12) ^ (folded << 5) ^ folded;
                }
                crc.at = end;
                if end < crc.end {
                    if work.remaining() == 0 {
                        return Ok(false);
                    }
                    continue;
                }
                if crc.remainder != 0 {
                    return Err(Error::Invalid("DTS-HD extension header CRC"));
                }
                self.crc = None;
            }
            if self.remaining == 0 {
                return if self.offset == bytes.len() {
                    Ok(true)
                } else {
                    Err(Error::Invalid("DTS-HD PES access unit extent"))
                };
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let aligned = self.offset.next_multiple_of(4);
            if bytes
                .get(self.offset..aligned)
                .ok_or(Error::Truncated)?
                .iter()
                .any(|&byte| byte != 0)
            {
                return Err(Error::Invalid("DTS-HD substream alignment"));
            }
            let tail = bytes.get(aligned..).ok_or(Error::Truncated)?;
            let ext = probe_dts_extension_substream_extent(
                tail,
                DtsFrameExtentLimits {
                    maximum_header_bytes: 4096,
                    maximum_frame_bytes: maximum,
                },
            )?;
            if ext.frame_bytes > tail.len() {
                return Err(Error::Truncated);
            }
            if u32::from(ext.substream_index) != self.remaining.trailing_zeros() {
                return Err(Error::Invalid("DTS-HD extension substream order"));
            }
            self.remaining &= !(1 << ext.substream_index);
            // §7.5.2 and Annex B cover the index through byte alignment.
            // The stored 16-bit checksum reduces this range to residue zero.
            self.crc = Some(HeaderCrc {
                at: aligned + 5,
                end: aligned + ext.header_bytes,
                remainder: u16::MAX,
            });
            self.offset = aligned + ext.frame_bytes;
        }
    }
}
