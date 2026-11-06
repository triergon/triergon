//! Original RFC2781/RFC3629 scalar decoder for the native document frontdoor.
//! Exact source bytes stay in Input; line marks preserve physical positions.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, BudgetVec, Bytes, Error, Result, WorkBudget};

#[derive(Clone, Copy, Debug)]
pub(crate) struct LineMark {
    pub utf8: usize,
    pub source: usize,
}

#[derive(Debug)]
pub(crate) struct Decoder {
    pool: Budget,
    little: bool,
    at: usize,
    first: Option<u8>,
    high: Option<u16>,
    pending: [u8; 4],
    pending_len: usize,
    pending_at: usize,
    line_after: Option<usize>,
    initial_mark: bool,
    bytes: BudgetVec<u8>,
    marks: BudgetVec<LineMark>,
    stopped: bool,
    done: bool,
    maximum: usize,
}

impl Decoder {
    pub(crate) fn new(pool: &Budget, little: bool, maximum: usize) -> Self {
        Self {
            pool: pool.clone(),
            little,
            at: 0,
            first: None,
            high: None,
            pending: [0; 4],
            pending_len: 0,
            pending_at: 0,
            line_after: None,
            initial_mark: false,
            bytes: BudgetVec::new(pool),
            marks: BudgetVec::new(pool),
            stopped: false,
            done: false,
            maximum,
        }
    }
    fn scalar(&mut self, value: u32) -> Result<()> {
        let scalar = char::from_u32(value).ok_or(Error::Invalid("UTF16 scalar"))?;
        self.pending_len = scalar.encode_utf8(&mut self.pending).len();
        self.pending_at = 0;
        if value == 10 {
            self.line_after = Some(self.at);
        }
        Ok(())
    }
    pub(crate) fn advance(
        &mut self,
        source: &[u8],
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        while !self.done {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !self.initial_mark {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.marks
                    .try_push(LineMark { utf8: 0, source: 2 })
                    .map_err(|error| error.error)?;
                self.initial_mark = true;
                continue;
            }
            if self.pending_at < self.pending_len {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                let requested = self.bytes.len().checked_add(1).ok_or(Error::Overflow)?;
                if requested > self.maximum {
                    return Err(Error::Limit {
                        requested,
                        maximum: self.maximum,
                    });
                }
                self.bytes
                    .try_push(self.pending[self.pending_at])
                    .map_err(|error| error.error)?;
                self.pending_at += 1;
                continue;
            }
            if let Some(source) = self.line_after {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.marks
                    .try_push(LineMark {
                        utf8: self.bytes.len(),
                        source,
                    })
                    .map_err(|error| error.error)?;
                self.line_after = None;
                continue;
            }
            if self.stopped || self.at == source.len() {
                self.stopped |= self.first.is_some() || self.high.is_some();
                self.done = true;
                continue;
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            let byte = source[self.at];
            self.at += 1;
            if self.at <= 2 {
                continue;
            } // BOM bytes are still charged input work.
            let Some(first) = self.first.take() else {
                self.first = Some(byte);
                continue;
            };
            let value = if self.little {
                u16::from_le_bytes([first, byte])
            } else {
                u16::from_be_bytes([first, byte])
            };
            if let Some(high) = self.high.take() {
                if !(0xdc00..=0xdfff).contains(&value) {
                    self.stopped = true;
                    continue;
                }
                self.scalar(
                    0x10000 + (u32::from(high) - 0xd800) * 1024 + u32::from(value) - 0xdc00,
                )?;
            } else if (0xd800..=0xdbff).contains(&value) {
                self.high = Some(value);
            } else if (0xdc00..=0xdfff).contains(&value) {
                self.stopped = true;
            } else {
                self.scalar(u32::from(value))?;
            }
        }
        Ok(true)
    }
    pub(crate) fn finish(&mut self) -> Result<(Bytes, BudgetVec<LineMark>, bool)> {
        if !self.done {
            return Err(Error::Invalid("unfinished UTF16 carriage decode"));
        }
        let bytes = std::mem::replace(&mut self.bytes, BudgetVec::new(&self.pool));
        let bytes = match bytes.freeze() {
            Ok(bytes) => bytes,
            Err(error) => {
                self.bytes = error.buffer;
                return Err(error.error);
            }
        };
        let marks = std::mem::replace(&mut self.marks, BudgetVec::new(&self.pool));
        Ok((bytes, marks, self.stopped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(source: &[u8], little: bool, quantum: u64) -> (Bytes, BudgetVec<LineMark>, bool) {
        let pool = Budget::new(1 << 20).unwrap();
        let mut decoder = Decoder::new(&pool, little, 4096);
        let cancel = AtomicBool::new(false);
        for _ in 0..4096 {
            if decoder
                .advance(source, &mut WorkBudget::new(quantum), &cancel)
                .unwrap()
            {
                return decoder.finish().unwrap();
            }
        }
        panic!("bounded Unicode source control did not complete")
    }
    #[test]
    fn rfc_scalars_and_physical_line_marks_are_exact_at_work_one_seven_and_4096() {
        for quantum in [1, 7, 4096] {
            for little in [false, true] {
                let units = [0xfeff_u16, 0x31, 0xa, 0xc5, 0x6c34, 0xd83d, 0xde00, 0xa];
                let mut source = Vec::new();
                for unit in units {
                    source.extend_from_slice(&if little {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                let (bytes, marks, stopped) = decode(&source, little, quantum);
                assert_eq!(bytes.as_ref(), b"1\n\xc3\x85\xe6\xb0\xb4\xf0\x9f\x98\x80\n");
                assert!(!stopped);
                assert_eq!(marks.len(), 3);
                assert_eq!(
                    (marks.as_slice()[0].utf8, marks.as_slice()[0].source),
                    (0, 2)
                );
                assert_eq!(
                    (marks.as_slice()[1].utf8, marks.as_slice()[1].source),
                    (2, 6)
                );
                assert_eq!(
                    (marks.as_slice()[2].utf8, marks.as_slice()[2].source),
                    (12, 16)
                );
            }
        }
    }
    #[test]
    fn unpaired_surrogates_do_not_become_replacement_characters_or_partial_utf8() {
        for source in [
            &b"\xfe\xff\x00A\xd8\x00\x00\n"[..],
            &b"\xfe\xff\x00A\xdc\x00"[..],
            &b"\xfe\xff\x00A\x00"[..],
        ] {
            let (bytes, _, stopped) = decode(source, false, 1);
            assert_eq!(bytes.as_ref(), b"A");
            assert!(stopped);
        }
    }
    #[test]
    fn cancellation_precedes_work_and_retains_the_resume_state() {
        let pool = Budget::new(4096).unwrap();
        let mut decoder = Decoder::new(&pool, false, 1024);
        let mut work = WorkBudget::new(7);
        assert!(matches!(
            decoder.advance(b"\xfe\xff\x00A", &mut work, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(work.spent(), 0);
        assert!(
            decoder
                .advance(
                    b"\xfe\xff\x00A",
                    &mut WorkBudget::new(4096),
                    &AtomicBool::new(false)
                )
                .unwrap()
        );
        assert_eq!(decoder.finish().unwrap().0.as_ref(), b"A");
    }
}
