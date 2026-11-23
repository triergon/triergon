//! Bounded Colour/MasteringMetadata fields over the original TrackEntry owner.
use crate::ebml::ChildCursor;
use crate::step::{Required, check};
use crate::{
    Error, Result,
    colour::{Colour, MasteringMetadata},
    ebml,
    step::ParseStep,
    storage::SharedBytes,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;

pub(crate) const MATRIX_COEFFICIENTS: u64 = 0x55b1;
pub(crate) const BITS_PER_CHANNEL: u64 = 0x55b2;
pub(crate) const CHROMA_SUBSAMPLING_HORZ: u64 = 0x55b3;
pub(crate) const CHROMA_SUBSAMPLING_VERT: u64 = 0x55b4;
pub(crate) const CB_SUBSAMPLING_HORZ: u64 = 0x55b5;
pub(crate) const CB_SUBSAMPLING_VERT: u64 = 0x55b6;
pub(crate) const CHROMA_SITING_HORZ: u64 = 0x55b7;
pub(crate) const CHROMA_SITING_VERT: u64 = 0x55b8;
pub(crate) const RANGE: u64 = 0x55b9;
pub(crate) const TRANSFER_CHARACTERISTICS: u64 = 0x55ba;
pub(crate) const PRIMARIES: u64 = 0x55bb;
pub(crate) const MAX_CLL: u64 = 0x55bc;
pub(crate) const MAX_FALL: u64 = 0x55bd;

#[derive(Debug)]
pub(crate) struct ColourJob {
    source: SharedBytes,
    cursor: ChildCursor,
    mastering: Option<ChildCursor>,
    out: Colour,
    seen: u16,
    master_seen: u16,
    done: bool,
    failed: bool,
}
impl ColourJob {
    pub(crate) fn new(source: SharedBytes, range: Range<usize>) -> Self {
        Self {
            source,
            cursor: ChildCursor::new(range),
            mastering: None,
            out: Colour::default(),
            seen: 0,
            master_seen: 0,
            done: false,
            failed: false,
        }
    }
    pub(crate) fn step(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Colour>> {
        if self.failed {
            return Err(Error::Invalid(
                "Colour job requires replacement after error",
            ));
        }
        let result = self.advance(work, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    /// Marks a child as seen: `false` for a repeat, which RFC 8794 §12 lets a reader
    /// ignore (only the first occurrence of these maxOccurs-1 children counts).
    fn first(seen: &mut u16, bit: u32) -> bool {
        let mask = 1 << bit;
        let first = *seen & mask == 0;
        *seen |= mask;
        first
    }
    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<ParseStep<Colour>> {
        loop {
            check(cancel)?;
            if self.done {
                return Ok(ParseStep::Event(self.out));
            }
            if let Some(mastering) = &mut self.mastering {
                match mastering.step(&self.source, work, cancel)? {
                    ParseStep::Yield => return Ok(ParseStep::Yield),
                    ParseStep::Event(None) => {
                        self.mastering = None;
                        continue;
                    }
                    ParseStep::Event(Some(child)) => {
                        if !(0x55d1..=0x55da).contains(&child.id) {
                            continue;
                        }
                        let index = (child.id - 0x55d1) as u32;
                        if !Self::first(&mut self.master_seen, index) {
                            continue;
                        }
                        let bytes = &self.source[child.payload];
                        let value = match bytes.len() {
                            0 => 0.,
                            4 => f64::from(f32::from_be_bytes(
                                bytes
                                    .try_into()
                                    .map_err(|_| Error::Invalid("HDR float width"))?,
                            )),
                            8 => f64::from_be_bytes(
                                bytes
                                    .try_into()
                                    .map_err(|_| Error::Invalid("HDR float width"))?,
                            ),
                            _ => return Err(Error::Invalid("HDR float width")),
                        };
                        let out = self
                            .out
                            .mastering
                            .as_mut()
                            .required("missing mastering builder")?;
                        match index {
                            0..=7 => out.chromaticity[index as usize] = Some(value),
                            8 => out.luminance_max = Some(value),
                            _ => out.luminance_min = Some(value),
                        }
                    }
                    _ => return Err(Error::Invalid("invalid mastering transition")),
                }
                continue;
            }
            match self.cursor.step(&self.source, work, cancel)? {
                ParseStep::Yield => return Ok(ParseStep::Yield),
                ParseStep::Event(None) => self.done = true,
                ParseStep::Event(Some(child)) => {
                    if child.id == 0x55d0 {
                        if !Self::first(&mut self.seen, 13) {
                            continue;
                        }
                        self.out.mastering = Some(MasteringMetadata::default());
                        self.master_seen = 0;
                        self.mastering = Some(ChildCursor::new(child.payload));
                        continue;
                    }
                    if !(MATRIX_COEFFICIENTS..=MAX_FALL).contains(&child.id) {
                        continue;
                    }
                    if !Self::first(&mut self.seen, (child.id - MATRIX_COEFFICIENTS) as u32) {
                        continue;
                    }
                    let bytes = &self.source[child.payload];
                    let v = if bytes.is_empty()
                        && matches!(
                            child.id,
                            MATRIX_COEFFICIENTS | TRANSFER_CHARACTERISTICS | PRIMARIES
                        ) {
                        2
                    } else {
                        ebml::uint(bytes)?
                    };
                    match child.id {
                        MATRIX_COEFFICIENTS => self.out.matrix_coefficients = v,
                        BITS_PER_CHANNEL => self.out.bits_per_channel = v,
                        CHROMA_SUBSAMPLING_HORZ..=CHROMA_SUBSAMPLING_VERT => {
                            self.out.chroma_subsampling
                                [(child.id - CHROMA_SUBSAMPLING_HORZ) as usize] = Some(v);
                        }
                        CB_SUBSAMPLING_HORZ..=CB_SUBSAMPLING_VERT => {
                            self.out.cb_subsampling[(child.id - CB_SUBSAMPLING_HORZ) as usize] =
                                Some(v);
                        }
                        CHROMA_SITING_HORZ..=CHROMA_SITING_VERT => {
                            self.out.chroma_siting[(child.id - CHROMA_SITING_HORZ) as usize] = v
                        }
                        RANGE => self.out.range = v,
                        TRANSFER_CHARACTERISTICS => self.out.transfer_characteristics = v,
                        PRIMARIES => self.out.primaries = v,
                        MAX_CLL => self.out.max_cll = Some(v),
                        _ => self.out.max_fall = Some(v),
                    }
                }
                _ => return Err(Error::Invalid("invalid Colour transition")),
            }
        }
    }
}

crate::storage::usage::storage!(ColourJob, |s| [s.source]);
