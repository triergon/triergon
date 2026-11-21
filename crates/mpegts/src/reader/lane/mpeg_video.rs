//! One picture with sequence setup in one aligned, finite, single-span PES.
//!
//! H.262 §§6.2.1–6.2.4 and Table 6-1 define the start codes and header
//! fields. H.222.0 Table 2-34 identifies stream types 1 and 2; PES alignment
//! alone never proves a picture boundary. Slice payloads remain coded bytes.
use super::Piece;
use crate::reader::{MpegPictureCoding, MpegVideoPicture};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_bitstream::bits_at;
use triergon_core::{BudgetVec, Error, Result, Time, WorkBudget};

const PROFILE: Error = Error::Unsupported(
    "MPEG video requires one supported picture in an aligned finite single-span PES",
);

#[derive(Default)]
enum Phase {
    #[default]
    Sequence,
    AfterSequence,
    Setup,
    Group,
    Coding,
    Slices,
    End,
}

#[derive(Default)]
pub(super) struct Reader {
    scan: usize,
    start: usize,
    phase: Phase,
    width: u16,
    height: u16,
    rate: u8,
    picture_offset: usize,
    temporal_reference: u16,
    coding: MpegPictureCoding,
    slices: u8,
    last_slice: u8,
    mpeg2: bool,
}

impl Reader {
    pub(super) fn step(
        &mut self,
        pieces: &BudgetVec<Piece>,
        stream_type: u8,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<MpegVideoPicture>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let piece = pieces.as_slice().last().expect("pending MPEG video PES");
        let pes = piece.source.pes();
        if !(0xe0..=0xef).contains(&pes.header.stream_id)
            || pes.header.declared_packet_length == 0
            || !pes.header.data_alignment_indicator
            || pes.spans().len() != 1
        {
            return Err(PROFILE);
        }
        let span = &pes.spans()[0];
        let bytes = span.as_ref();
        if bytes.len() < 4 || bytes[..4] != [0, 0, 1, 0xb3] {
            return Err(PROFILE);
        }
        // Each byte inspection consumes work, including prefixes. Header validation
        // below has a fixed maximum of eight bytes and never scans coded payload.
        while self.scan < bytes.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            let at = self.scan;
            self.scan += 1;
            if at >= self.start + 6 && bytes[at - 2..=at] == [0, 0, 1] {
                let next = at - 2;
                self.unit(
                    bytes[self.start + 3],
                    &bytes[self.start + 4..next],
                    stream_type,
                )?;
                self.start = next;
            }
        }
        if !work.try_charge(1) {
            return Ok(None);
        }
        let code = *bytes.get(self.start + 3).ok_or(Error::Truncated)?;
        self.unit(code, &bytes[self.start + 4..], stream_type)?;
        if self.slices == 0 || !matches!(self.phase, Phase::End) {
            return Err(PROFILE);
        }
        let (numerator, denominator) = match self.rate {
            1 => (1001, 24_000),
            2 => (1, 24),
            3 => (1, 25),
            4 => (1001, 30_000),
            5 => (1, 30),
            6 => (1, 50),
            7 => (1001, 60_000),
            8 => (1, 60),
            _ => return Err(PROFILE),
        };
        let picture = MpegVideoPicture {
            bytes: span.payload_bytes()?,
            range: piece.range.clone(),
            width: self.width,
            height: self.height,
            temporal_reference: self.temporal_reference,
            coding: self.coding,
            picture_offset: piece.range.start + self.picture_offset as u64,
            slice_count: self.slices,
            mpeg2: self.mpeg2,
            duration: Time::new(numerator, denominator)?,
        };
        *self = Self::default();
        Ok(Some(picture))
    }

    fn unit(&mut self, code: u8, data: &[u8], stream_type: u8) -> Result<()> {
        let bits = |at, width| bits_at(data, at, width).ok_or(Error::Truncated);
        match (&self.phase, code) {
            (Phase::Sequence, 0xb3) => {
                if data.len() != 8 || bits(50, 1)? != 1 || bits(62, 2)? != 0 {
                    return Err(PROFILE);
                }
                self.width = bits(0, 12)? as u16;
                self.height = bits(12, 12)? as u16;
                self.rate = bits(28, 4)? as u8;
                if self.width == 0
                    || self.height == 0
                    || self.height > 2800
                    || !(1..=8).contains(&self.rate)
                    || !(1..=if stream_type == 2 { 4 } else { 14 }).contains(&bits(24, 4)?)
                    || bits(32, 18)? == 0
                {
                    return Err(PROFILE);
                }
                self.phase = Phase::AfterSequence;
            }
            (Phase::AfterSequence, 0xb5) if stream_type == 2 => {
                // Main Profile, progressive 4:2:0, base dimensions and frame rate.
                if data.len() != 6
                    || bits(0, 4)? != 1
                    || bits(4, 8)? != 0x48
                    || bits(12, 1)? != 1
                    || bits(13, 2)? != 1
                    || bits(15, 4)? != 0
                    || bits(31, 1)? != 1
                    || bits(41, 7)? != 0
                {
                    return Err(PROFILE);
                }
                self.mpeg2 = true;
                self.phase = Phase::Setup;
            }
            (Phase::AfterSequence | Phase::Setup, 0xb8) => {
                if data.len() != 4 || bits(12, 1)? != 1 || bits(27, 5)? != 0 {
                    return Err(PROFILE);
                }
                self.phase = Phase::Group;
            }
            (Phase::AfterSequence | Phase::Setup | Phase::Group, 0x00)
                if stream_type == 1 || self.mpeg2 =>
            {
                // H.262 §6.2.3: predictive headers carry forward and, for B,
                // backward vector fields before extra_bit_picture. MPEG-2
                // uses zero full-pel flags and legacy f_codes set to seven.
                self.coding = match bits(10, 3)? {
                    1 if data.len() == 4 && bits(29, 3)? == 0 => MpegPictureCoding::Intra,
                    2 if self.mpeg2 && data.len() == 5 && bits(29, 11)? == 0b011_1000_0000 => {
                        MpegPictureCoding::Predictive
                    }
                    3 if self.mpeg2 && data.len() == 5 && bits(29, 11)? == 0b011_1011_1000 => {
                        MpegPictureCoding::Bidirectional
                    }
                    _ => return Err(PROFILE),
                };
                self.picture_offset = self.start;
                self.temporal_reference = bits(0, 10)? as u16;
                self.phase = if self.mpeg2 {
                    Phase::Coding
                } else {
                    Phase::Slices
                };
            }
            (Phase::Coding, 0xb5) => {
                if data.len() != 5
                    || bits(0, 4)? != 8
                    || bits(22, 2)? != 3
                    || bits(24, 1)? != 0
                    || bits(25, 1)? != 1
                    || bits(30, 1)? != 0
                    || bits(31, 2)? != 3
                    || bits(33, 7)? != 0
                {
                    return Err(PROFILE);
                }
                self.phase = Phase::Slices;
            }
            (Phase::Slices, 1..=0xaf) => {
                if code <= self.last_slice || data.len() < 2 || bits(0, 5)? == 0 {
                    return Err(PROFILE);
                }
                self.last_slice = code;
                self.slices += 1;
            }
            (Phase::Slices, 0xb7) if self.slices != 0 && data.is_empty() => self.phase = Phase::End,
            _ => return Err(PROFILE),
        }
        Ok(())
    }
}
