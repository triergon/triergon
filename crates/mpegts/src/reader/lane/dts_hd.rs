//! DVB DTS-HD access units in aligned PES (TS 102 114 F.4.3).
use super::{DtsAudioFrame, DtsExtensionFrame, aligned_audio};
use crate::{Pes, psi::DvbDtsHd};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::{BitCursor, BitSource};
use triergon_core::{
    Budget, Error, InputCursor, InputRequest, InputWindow, Result, SourceCapabilities, SourceId,
    Step, Time, WorkBudget,
};
use triergon_framing::{
    DtsCoreHeader, DtsExtensionSubstreamExtent, DtsFrameExtentLimits, probe_dts_core_header,
    probe_dts_extension_substream_extent,
};

struct HeaderCrc {
    at: usize,
    end: usize,
    remainder: u16,
}

impl HeaderCrc {
    // §7.5.2 and Annex B cover the index through byte alignment.
    // The stored 16-bit checksum reduces this range to residue zero.
    fn new(start: usize, header_bytes: usize) -> Self {
        Self {
            at: start + 5,
            end: start + header_bytes,
            remainder: u16::MAX,
        }
    }

    fn step(&mut self, bytes: &[u8], work: &mut WorkBudget) -> Result<bool> {
        let end = self.at + work.take((self.end - self.at).min(256) as u64) as usize;
        for &byte in &bytes[self.at..end] {
            // Fold the high byte through x^16 + x^12 + x^5 + 1.
            let high = (self.remainder >> 8) ^ u16::from(byte);
            let folded = high ^ (high >> 4);
            self.remainder = (self.remainder << 8) ^ (folded << 12) ^ (folded << 5) ^ folded;
        }
        self.at = end;
        if end < self.end {
            Ok(false)
        } else if self.remainder != 0 {
            Err(Error::Invalid("DTS-HD extension header CRC"))
        } else {
            Ok(true)
        }
    }
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
        check_pes(pes, 15, self.input.maximum())
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
            return await_logical_end(self.input.input(), work, cancel);
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
                self.validation = None;
                self.incomplete = true;
                return await_logical_end(self.input.input(), work, cancel);
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
                if !crc.step(bytes, work)? {
                    if work.remaining() == 0 {
                        return Ok(false);
                    }
                    continue;
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
            self.crc = Some(HeaderCrc::new(aligned, ext.header_bytes));
            self.offset = aligned + ext.frame_bytes;
        }
    }
}

// Confirm logical EOF so the lane can report an incomplete audio fragment.
fn await_logical_end<T>(
    input: &mut InputCursor,
    work: &mut WorkBudget,
    cancel: &AtomicBool,
) -> Result<Step<T>> {
    match input.read_discard(1, work, cancel)? {
        Step::NeedInput(request) => Ok(Step::NeedInput(request)),
        Step::Yield => Ok(Step::Yield),
        Step::End(_) => Err(Error::Truncated),
        Step::Ready(_) => Err(Error::Invalid("DTS-HD bytes beyond aligned PES")),
        Step::Backpressure(never) => match never {},
    }
}

fn check_pes(pes: &Pes, minimum: usize, maximum: usize) -> Result<()> {
    if pes.header.stream_id != 0xbd
        || pes.header.declared_packet_length == 0
        || !pes.header.data_alignment_indicator
        || pes.payload_len < minimum
    {
        return Err(Error::Invalid("DTS-HD aligned PES"));
    }
    if pes.payload_len > maximum {
        return Err(crate::limit(pes.payload_len, maximum));
    }
    Ok(())
}

struct ExtensionPacket {
    crc: Option<HeaderCrc>,
    end: usize,
    duration: Option<Time>,
    remaining: u8,
}

struct Batch {
    frame: aligned_audio::Frame<()>,
    cursor: usize,
    current: Option<ExtensionPacket>,
}

// Annex F.4.3 allows multiple AUs in one PES only for a single substream.
pub(super) struct ExtensionReader {
    input: aligned_audio::Reader<10, ()>,
    extensions: u8,
    pes_bytes: usize,
    batch: Option<Batch>,
    incomplete: bool,
    duration: Option<Time>,
}

impl ExtensionReader {
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
        if extensions == 0 {
            return Err(Error::Invalid("DTS-HD extension substream"));
        }
        Ok(Self {
            input: aligned_audio::Reader::new(pool, source, caps, maximum, window)?,
            extensions,
            pes_bytes: 0,
            batch: None,
            incomplete: false,
            duration: None,
        })
    }

    pub(super) fn check_pes(&self, pes: &Pes) -> Result<()> {
        if self.incomplete {
            return Err(Error::Truncated);
        }
        check_pes(pes, 12, self.input.maximum())
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
    ) -> Result<Step<DtsExtensionFrame>> {
        if self.incomplete {
            return await_logical_end(self.input.input(), work, cancel);
        }
        if self.batch.is_none() {
            let size = self.pes_bytes;
            let frame = match self.input.step(work, cancel, |_| Ok(((), size)))? {
                Step::Ready(frame) => frame,
                Step::Yield => return Ok(Step::Yield),
                Step::NeedInput(request) => return Ok(Step::NeedInput(request)),
                Step::End(end) => return Ok(Step::End(end)),
                Step::Backpressure(never) => match never {},
            };
            self.batch = Some(Batch {
                frame,
                cursor: 0,
                current: None,
            });
        }
        let batch = self.batch.as_mut().expect("complete DTS-HD PES");
        let bytes = batch.frame.bytes.as_ref();
        if batch.current.is_none() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(Step::Yield);
            }
            match next_extension(bytes, batch.cursor, self.input.maximum(), self.extensions) {
                Ok(pending) => batch.current = Some(pending),
                Err(Error::Truncated) => {
                    self.batch = None;
                    self.incomplete = true;
                    return await_logical_end(self.input.input(), work, cancel);
                }
                Err(error) => return Err(error),
            }
        }
        let pending = batch.current.as_mut().expect("extension header");
        match pending.check(bytes, self.input.maximum(), work, cancel) {
            Ok(false) => return Ok(Step::Yield),
            Err(Error::Truncated) => {
                self.batch = None;
                self.incomplete = true;
                return await_logical_end(self.input.input(), work, cancel);
            }
            result => {
                result?;
            }
        }
        if self.extensions.count_ones() > 1 && pending.end != bytes.len() {
            return Err(Error::Invalid("DTS-HD PES access unit extent"));
        }
        let pending = batch.current.take().expect("validated extension header");
        self.duration = pending.duration.or(self.duration);
        let unit = DtsExtensionFrame {
            bytes: batch.frame.bytes.slice(batch.cursor..pending.end)?,
            offset: batch.frame.offset + batch.cursor as u64,
            duration: self.duration,
        };
        batch.cursor = pending.end;
        if batch.cursor == bytes.len() {
            self.batch = None;
        }
        Ok(Step::Ready(unit))
    }
}

fn next_extension(
    bytes: &[u8],
    cursor: usize,
    maximum: usize,
    extensions: u8,
) -> Result<ExtensionPacket> {
    let tail = &bytes[cursor..];
    let extent = probe_dts_extension_substream_extent(
        tail,
        DtsFrameExtentLimits {
            maximum_header_bytes: 4096,
            maximum_frame_bytes: maximum,
        },
    )?;
    if u32::from(extent.substream_index) != extensions.trailing_zeros() {
        return Err(Error::Invalid("DTS-HD extension substream index"));
    }
    if extent.frame_bytes > tail.len() {
        return Err(Error::Truncated);
    }
    Ok(ExtensionPacket {
        crc: Some(HeaderCrc::new(cursor, extent.header_bytes)),
        end: cursor + extent.frame_bytes,
        duration: extension_duration(tail, extent)?,
        remaining: extensions & !(1 << extent.substream_index),
    })
}

impl ExtensionPacket {
    fn check(
        &mut self,
        bytes: &[u8],
        maximum: usize,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if let Some(crc) = &mut self.crc {
                if !crc.step(bytes, work)? {
                    if work.remaining() == 0 {
                        return Ok(false);
                    }
                    continue;
                }
                self.crc = None;
            }
            if self.remaining == 0 {
                return Ok(true);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let aligned = self.end.next_multiple_of(4);
            if bytes
                .get(self.end..aligned)
                .ok_or(Error::Truncated)?
                .iter()
                .any(|&byte| byte != 0)
            {
                return Err(Error::Invalid("DTS-HD substream alignment"));
            }
            let tail = bytes.get(aligned..).ok_or(Error::Truncated)?;
            let extent = probe_dts_extension_substream_extent(
                tail,
                DtsFrameExtentLimits {
                    maximum_header_bytes: 4096,
                    maximum_frame_bytes: maximum,
                },
            )?;
            if extent.frame_bytes > tail.len() {
                return Err(Error::Truncated);
            }
            if u32::from(extent.substream_index) != self.remaining.trailing_zeros() {
                return Err(Error::Invalid("DTS-HD extension substream order"));
            }
            self.remaining &= !(1 << extent.substream_index);
            self.crc = Some(HeaderCrc::new(aligned, extent.header_bytes));
            self.end = aligned + extent.frame_bytes;
        }
    }
}

fn extension_duration(bytes: &[u8], extent: DtsExtensionSubstreamExtent) -> Result<Option<Time>> {
    // Table 7-2 gives the fixed prefix, and Table 7-3 the available clocks.
    let header = &bytes[..extent.header_bytes - 2];
    let mut bits = BitCursor::at(header, if extent.long_sizes { 75 } else { 67 });
    let mut read = |width| bits.read(width).map_err(|_| Error::Truncated);
    if read(1)? == 0 {
        return Ok(None);
    }
    let clock = match read(2)? {
        0 => 32_000,
        1 => 44_100,
        2 => 48_000,
        _ => return Err(Error::Invalid("DTS-HD reference clock code")),
    };
    let samples = 512 * (read(3)? + 1);
    Time::new(i128::from(samples), clock).map(Some)
}
