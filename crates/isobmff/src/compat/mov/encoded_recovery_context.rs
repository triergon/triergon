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
    validated: Option<RecoveryFacts>,
    fault: Option<SourceProfileFault>,
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
        match (&self.validated, &self.fault) {
            (Some(v), None) => Ok(v),
            (_, Some(e)) => Err(e),
            _ => unreachable!("constructed source outcome"),
        }
    }
}
#[derive(Debug)]
pub(crate) struct RecoveryContextReceiver {
    raw: BoxView,
    scan: Option<BoxChildren>,
    strict: Option<RecoveryWalk>,
    groups: u32,
    ready: Option<RecoveryObservation>,
}
impl RecoveryContextReceiver {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            raw: table.raw().clone(),
            scan: Some(table.raw().children(0, Limits::default())?),
            strict: Some(RecoveryWalk::new_with_recovery_contexts(table, maximum)?),
            groups: 0,
            ready: None,
        })
    }
    pub(crate) fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.ready.is_some() {
                return Ok(true);
            }
            if let Some(scan) = &mut self.scan {
                match scan.poll(work)? {
                    Next::Yield => return Ok(false),
                    Next::End => self.scan = None,
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
            let strict = self.strict.as_mut().ok_or(Error::Invalid(
                "encoded context view recovery walk invariant",
            ))?;
            match strict.poll(work, cancel) {
                Ok(false) => return Ok(false),
                Ok(true) => {
                    let validated = self
                        .strict
                        .take()
                        .ok_or(Error::Invalid(
                            "encoded context view recovery commit invariant",
                        ))?
                        .finish()?;
                    self.ready = Some(RecoveryObservation {
                        raw: self.raw.clone(),
                        groups: self.groups,
                        validated: Some(validated),
                        fault: None,
                    });
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
                    self.ready = Some(RecoveryObservation {
                        raw: self.raw.clone(),
                        groups: self.groups,
                        validated: None,
                        fault: Some(SourceProfileFault {
                            error,
                            raw,
                            ordinal: None,
                        }),
                    });
                    self.strict = None;
                }
            }
        }
    }
    pub(crate) fn finish(mut self) -> Result<RecoveryObservation> {
        self.ready
            .take()
            .ok_or(Error::Invalid("encoded context view recovery not ready"))
    }
}
