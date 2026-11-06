//! Private functional helper. Not exported or compiled.
//! Reuses the existing independent primary-derived packing detector; no hint.
#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};
use triergon_framing::DtsPacking;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub packing: DtsPacking,
    pub source_offset: u64,
    pub inspected_end: u64,
    pub prefix: [u8; 6],
    pub prefix_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeStep {
    NeedInput,
    Yield,
    Found(Discovered),
    SourceEnd { inspected_bytes: u64 },
    End,
}

/// The caller retains source identity, input owners and original chunk boundaries.
/// Each feed returns its exact consumed prefix; an unconsumed tail is not EOF.
pub struct PackingProbe {
    prefix: [u8; 6],
    length: usize,
    start: u64,
    seen: u64,
    maximum: u64,
    check: bool,
    advance: bool,
    terminal: bool,
    failure: Option<Error>,
}

impl PackingProbe {
    pub const fn inspected_bytes(&self) -> u64 {
        self.seen
    }

    pub fn new(maximum_scan_bytes: u64) -> Result<Self> {
        if maximum_scan_bytes == 0 {
            return Err(Error::Invalid("zero DTS discovery scan policy"));
        }
        Ok(Self {
            prefix: [0; 6],
            length: 0,
            start: 0,
            seen: 0,
            maximum: maximum_scan_bytes,
            check: false,
            advance: false,
            terminal: false,
            failure: None,
        })
    }

    pub fn feed(
        &mut self,
        input: &[u8],
        end_after_input: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<(usize, ProbeStep)> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.advance_input(input, end_after_input, work, cancel);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }

    fn advance_input(
        &mut self,
        input: &[u8],
        end_after_input: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<(usize, ProbeStep)> {
        let mut consumed = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.terminal {
                return Ok((0, ProbeStep::End));
            }
            if self.advance {
                if !work.try_charge(1) {
                    return Ok((consumed, ProbeStep::Yield));
                }
                // At most five inline bytes. No dynamic prefix copy or allocation.
                self.prefix.copy_within(1..self.length, 0);
                self.length -= 1;
                self.start = self.start.checked_add(1).ok_or(Error::Overflow)?;
                self.advance = false;
                self.check = self.length >= 4;
                continue;
            }
            if self.check {
                if !work.try_charge(1) {
                    return Ok((consumed, ProbeStep::Yield));
                }
                // Existing fixed probe: <=6 source bytes, four fixed32-bit views.
                // This is a bounded primitive, not a dynamic scan under one charge.
                self.check = false;
                match DtsPacking::detect(&self.prefix[..self.length]) {
                    Ok(packing) => {
                        self.terminal = true;
                        return Ok((
                            consumed,
                            ProbeStep::Found(Discovered {
                                packing,
                                source_offset: self.start,
                                inspected_end: self.seen,
                                prefix: self.prefix,
                                prefix_bytes: self.length,
                            }),
                        ));
                    }
                    Err(Error::Truncated) => {
                        // Never obtain the missing bytes from physical zero padding.
                        // At EOF inspect later suffixes too, without inventing bytes.
                        if consumed == input.len() && end_after_input {
                            self.advance = true;
                        }
                    }
                    Err(Error::Invalid(_)) => self.advance = true,
                    Err(error) => return Err(error),
                }
                continue;
            }
            if consumed == input.len() {
                if !end_after_input {
                    return Ok((consumed, ProbeStep::NeedInput));
                }
                if !work.try_charge(1) {
                    return Ok((consumed, ProbeStep::Yield));
                }
                self.terminal = true;
                return Ok((
                    consumed,
                    ProbeStep::SourceEnd {
                        inspected_bytes: self.seen,
                    },
                ));
            }
            if self.seen == self.maximum {
                let requested = self.seen.checked_add(1).ok_or(Error::Overflow)?;
                return Err(Error::Limit {
                    requested: usize::try_from(requested).map_err(|_| Error::Overflow)?,
                    maximum: usize::try_from(self.maximum).map_err(|_| Error::Overflow)?,
                });
            }
            if !work.try_charge(1) {
                return Ok((consumed, ProbeStep::Yield));
            }
            if self.length == self.prefix.len() {
                return Err(Error::Invalid("DTS discovery fixed prefix invariant"));
            }
            self.prefix[self.length] = input[consumed];
            self.length += 1;
            consumed += 1;
            self.seen = self.seen.checked_add(1).ok_or(Error::Overflow)?;
            self.check = self.length >= 4;
        }
    }
}
