//! A pin-bound regular open/index observer; never turns source faults into
//! validated groups. It first proves all enclosing box envelopes and refuses
//! grouping kinds that have native key/config effects outside this operation.
use crate::native_encoded_recovery_v5::RecoveryWalkV5;
use crate::{
    BoxChildren, BoxStep, BoxView, Limits, Mov9RecoveryFactsV5, Mov9SourceProfileFaultV7,
    SampleTable, fields,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};
#[derive(Clone, Debug)]
pub struct Mov9RecoveryObservationV7 {
    raw: BoxView,
    groups: u32,
    validated: Option<Mov9RecoveryFactsV5>,
    fault: Option<Mov9SourceProfileFaultV7>,
}
impl Mov9RecoveryObservationV7 {
    pub fn raw_source_table(&self) -> &BoxView {
        &self.raw
    }
    pub fn ignored_group_boxes(&self) -> u32 {
        self.groups
    }
    pub fn source_result(
        &self,
    ) -> std::result::Result<&Mov9RecoveryFactsV5, &Mov9SourceProfileFaultV7> {
        match (&self.validated, &self.fault) {
            (Some(v), None) => Ok(v),
            (_, Some(e)) => Err(e),
            _ => unreachable!("constructed source outcome"),
        }
    }
}
pub(crate) struct RecoveryReceiverV7 {
    raw: BoxView,
    scan: Option<BoxChildren>,
    strict: Option<RecoveryWalkV5>,
    groups: u32,
    ready: Option<Mov9RecoveryObservationV7>,
}
impl RecoveryReceiverV7 {
    pub(crate) fn new(table: &SampleTable, maximum: u64) -> Result<Self> {
        Ok(Self {
            raw: table.raw().clone(),
            scan: Some(table.raw().children(0, Limits::default())?),
            strict: Some(RecoveryWalkV5::new_v7(table, maximum)?),
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
                    BoxStep::Yield => return Ok(false),
                    BoxStep::End => self.scan = None,
                    BoxStep::Box(raw) => {
                        let atom = raw.header().kind;
                        if atom == *b"sgpd" || atom == *b"sbgp" {
                            let kind = fields::fourcc(raw.payload(), 4)?;
                            if kind == *b"sync" || (atom == *b"sbgp" && kind == *b"rap ") {
                                return Err(Error::Unsupported(
                                    "V7 native-effect sample group not implemented",
                                ));
                            }
                            self.groups = self.groups.checked_add(1).ok_or(Error::Overflow)?;
                        }
                    }
                }
                continue;
            }
            let strict = self
                .strict
                .as_mut()
                .ok_or(Error::Invalid("V7 strict recovery invariant"))?;
            match strict.poll(work, cancel) {
                Ok(false) => return Ok(false),
                Ok(true) => {
                    let validated = self
                        .strict
                        .take()
                        .ok_or(Error::Invalid("V7 recovery commit invariant"))?
                        .finish()?;
                    self.ready = Some(Mov9RecoveryObservationV7 {
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
                    let Some(raw) = strict.error_owner().cloned() else {
                        return Err(error);
                    };
                    self.ready = Some(Mov9RecoveryObservationV7 {
                        raw: self.raw.clone(),
                        groups: self.groups,
                        validated: None,
                        fault: Some(Mov9SourceProfileFaultV7 {
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
    pub(crate) fn finish(mut self) -> Result<Mov9RecoveryObservationV7> {
        self.ready
            .take()
            .ok_or(Error::Invalid("V7 recovery not ready"))
    }
}
