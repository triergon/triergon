//! Bounded Colour/MasteringMetadata fields over the original TrackEntry owner.
use super::syntax::{ChildCursor, check};
use crate::matroska::typed::syntax::Required;
use crate::{
    Error, Result,
    colour::{Colour, MasteringMetadata},
    ebml,
    incremental::ParseStep,
    storage::SharedBytes,
};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::WorkBudget;
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
    fn mark(seen: &mut u16, bit: u32) -> Result<()> {
        let mask = 1 << bit;
        if *seen & mask != 0 {
            return Err(Error::Invalid("duplicate colour metadata"));
        }
        *seen |= mask;
        Ok(())
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
                        Self::mark(&mut self.master_seen, index)?;
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
                        if !value.is_finite() || value < 0. || (index < 8 && value > 1.) {
                            return Err(Error::Invalid("HDR float range"));
                        }
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
                        Self::mark(&mut self.seen, 13)?;
                        self.out.mastering = Some(MasteringMetadata::default());
                        self.master_seen = 0;
                        self.mastering = Some(ChildCursor::new(child.payload));
                        continue;
                    }
                    if !(0x55b1..=0x55bd).contains(&child.id) {
                        continue;
                    }
                    Self::mark(&mut self.seen, (child.id - 0x55b1) as u32)?;
                    let bytes = &self.source[child.payload];
                    let v = if bytes.is_empty() && matches!(child.id, 0x55b1 | 0x55ba | 0x55bb) {
                        2
                    } else {
                        ebml::uint(bytes)?
                    };
                    match child.id {
                        0x55b1 => self.out.matrix_coefficients = v,
                        0x55b2 => self.out.bits_per_channel = v,
                        0x55b3..=0x55b4 => {
                            self.out.chroma_subsampling[(child.id - 0x55b3) as usize] = Some(v);
                        }
                        0x55b5..=0x55b6 => {
                            self.out.cb_subsampling[(child.id - 0x55b5) as usize] = Some(v);
                        }
                        0x55b7..=0x55b8 => self.out.chroma_siting[(child.id - 0x55b7) as usize] = v,
                        0x55b9 => self.out.range = v,
                        0x55ba => self.out.transfer_characteristics = v,
                        0x55bb => self.out.primaries = v,
                        0x55bc => self.out.max_cll = Some(v),
                        _ => self.out.max_fall = Some(v),
                    }
                }
                _ => return Err(Error::Invalid("invalid Colour transition")),
            }
        }
    }
}

super::usage::storage!(ColourJob, |s| [s.source]);
