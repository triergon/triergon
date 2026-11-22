//! The sample groups of an encoded track as the context view reads them when FFmpeg's
//! MOV demuxer opens and indexes the track. The `roll` and `prol` groups are checked but
//! not applied, and a fault in them is kept beside the result instead of failing; a
//! fault never becomes a validated group. `sync` groups and `rap ` mappings, which
//! affect the demuxer's key frames, are refused.
use crate::compat::mov::encoded_recovery::RecoveryWalk;
use crate::{
    Limits, Next,
    boxes::{BoxChildren, BoxView},
    compat::mov::{encoded_contexts::SourceProfileFault, encoded_recovery::RecoveryFacts},
    fields,
    table::SampleTable,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};
/// A track's sample groups as the context view read them: the checked `roll` and `prol`
/// groups, or the first fault that stopped the check.
#[derive(Clone, Debug)]
pub struct RecoveryObservation {
    raw: BoxView,
    groups: u32,
    outcome: std::result::Result<RecoveryFacts, SourceProfileFault>,
}
impl RecoveryObservation {
    /// The `stbl` box the groups were read from.
    pub fn raw_source_table(&self) -> &BoxView {
        &self.raw
    }
    /// Number of `sgpd` and `sbgp` boxes in the table, of any grouping type; the
    /// demuxer applies none of them.
    pub fn ignored_group_boxes(&self) -> u32 {
        self.groups
    }
    /// The checked groups, or the fault that stopped the check: an `Error::Invalid`,
    /// `Error::Unsupported` or `Error::Truncated` with its `sgpd` or `sbgp` box. A group
    /// of a type other than `roll` or `prol` is such a fault.
    pub fn source_result(&self) -> std::result::Result<&RecoveryFacts, &SourceProfileFault> {
        self.outcome.as_ref()
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Keep the validation walk inline without another allocation.
enum Phase {
    Validate {
        scan: Option<BoxChildren>,
        strict: RecoveryWalk,
    },
    Ready(RecoveryObservation),
    Finished,
}

#[derive(Debug)]
pub(crate) struct RecoveryContextReceiver {
    raw: BoxView,
    groups: u32,
    phase: Phase,
}
impl RecoveryContextReceiver {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            raw: table.raw().clone(),
            groups: 0,
            phase: Phase::Validate {
                scan: Some(table.raw().children(0, Limits::default())?),
                strict: RecoveryWalk::new_with_recovery_contexts(table, maximum)?,
            },
        })
    }
    pub(crate) fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let (scan, strict) = match &mut self.phase {
                Phase::Validate { scan, strict } => (scan, strict),
                Phase::Ready(_) => return Ok(true),
                Phase::Finished => {
                    return Err(Error::Invalid("encoded context view recovery not ready"));
                }
            };
            if let Some(children) = scan {
                match children.poll(work)? {
                    Next::Yield => return Ok(false),
                    Next::End => *scan = None,
                    Next::Item(raw) => {
                        let atom = raw.header().kind;
                        if atom == *b"sgpd" || atom == *b"sbgp" {
                            let kind = fields::fourcc(raw.payload(), 4)?;
                            if kind == *b"sync" || (atom == *b"sbgp" && kind == *b"rap ") {
                                return Err(Error::Unsupported(
                                    "encoded sample group that changes key frames",
                                ));
                            }
                            self.groups = self.groups.checked_add(1).ok_or(Error::Overflow)?;
                        }
                    }
                }
                continue;
            }
            let outcome = match strict.poll(work, cancel) {
                Ok(false) => return Ok(false),
                Ok(true) => {
                    let phase = std::mem::replace(&mut self.phase, Phase::Finished);
                    let Phase::Validate { strict, .. } = phase else {
                        continue;
                    };
                    Ok(strict.finish()?)
                }
                Err(error) => {
                    if !matches!(
                        error,
                        Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated
                    ) {
                        return Err(error);
                    }
                    let Some(raw) = strict.error_backing().cloned() else {
                        return Err(error);
                    };
                    Err(SourceProfileFault {
                        error,
                        raw,
                        ordinal: None,
                    })
                }
            };
            self.phase = Phase::Ready(RecoveryObservation {
                raw: self.raw.clone(),
                groups: self.groups,
                outcome,
            });
        }
    }
    pub(crate) fn finish(self) -> Result<RecoveryObservation> {
        match self.phase {
            Phase::Ready(observation) => Ok(observation),
            _ => Err(Error::Invalid("encoded context view recovery not ready")),
        }
    }
}
