//! Pin-bound native header outcomes over immutable source sample entries.
//! This is neither codec validation nor an ISO/QuickTime validity rule.
use crate::{BoxChildren, BoxStep, BoxView, BuildStatus, EntryLayout, Limits, SampleDescription};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// D32 public MOV open observations, not a general native profile.
#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32SampleEntryHeaderV7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9HeaderCauseV7 {
    OpusBodyLength { declared: usize },
    OpusVersion { raw: u8 },
    OpusConfigurationCollision { earlier: [u8; 4] },
    DtsSampleRate { raw: u32 },
}

#[derive(Debug)]
pub struct Mov9HeaderRejectionV7 {
    pub raw: BoxView,
    pub cause: Mov9HeaderCauseV7,
}

#[derive(Debug)]
pub struct Mov9SampleEntryHeaderV7 {
    /// Complete source entry, including every opaque/private child and reserved bit.
    pub description: SampleDescription,
    pub rejection: Option<Mov9HeaderRejectionV7>,
    pub inspected_children: u32,
    /// Receiver-selected opaque owner in original child order. A nontrivial
    /// configuration persists; all original ordered entry bytes stay retained.
    pub installed_configuration_source: Option<BoxView>,
}
impl Mov9SampleEntryHeaderV7 {
    /// None means no failure established by this bounded domain. It does not
    /// admit the rest of native open or assert codec/configuration conformance.
    pub fn native_open_result(&self) -> Option<i32> {
        self.rejection.as_ref().map(|_| -1_094_995_529)
    }
}

/// Scans all source child headers before publishing a native-domain outcome.
/// Header/count/range errors remain source errors, even after a native rejection.
/// One child header unit plus one fixed-field interpretation unit; a pending
/// child permits Work1 progress. No child vector, body copy or new owner is made.
///
/// This job consumes an already owned metadata snapshot, not a live I/O cursor.
/// It cannot seek, consume input, acknowledge failure or change a source token.
/// Resetting an unrelated/live cursor cannot invalidate retained source bytes.
pub struct Mov9SampleEntryHeaderJobV7 {
    description: Option<SampleDescription>,
    children: Option<BoxChildren>,
    pending: Option<BoxView>,
    installed: Option<[u8; 4]>,
    installed_source: Option<BoxView>,
    rejection: Option<Mov9HeaderRejectionV7>,
    inspected: u32,
    ready: bool,
    finished: bool,
    failed: Option<Error>,
}
impl Mov9SampleEntryHeaderJobV7 {
    /// The caller supplies the sourced layout; unknown entries are never
    /// silently assigned a visual/audio layout. Configuration state starts at
    /// each description boundary, never at an I/O transaction/config epoch.
    pub fn new(
        description: &SampleDescription,
        layout: EntryLayout,
        limits: Limits,
        _profile: PinnedMovD32SampleEntryHeaderV7,
    ) -> Result<Self> {
        let children = description.extensions(layout, limits)?;
        Ok(Self {
            description: Some(description.clone()),
            children: Some(children),
            pending: None,
            installed: None,
            installed_source: None,
            rejection: None,
            inspected: 0,
            ready: false,
            finished: false,
            failed: None,
        })
    }

    fn poison(&mut self, error: Error) -> Error {
        self.description = None;
        self.children = None;
        self.pending = None;
        self.rejection = None;
        self.installed_source = None;
        self.failed = Some(error);
        error
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(work, cancel);
        result.map_err(|error| self.poison(error))
    }

    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.finished {
            return Err(Error::Invalid("v7 header already taken"));
        }
        if self.ready {
            return Ok(BuildStatus::Ready);
        }
        loop {
            if work.remaining() == 0 {
                return Ok(BuildStatus::Yield);
            }
            if self.pending.is_none() {
                match self
                    .children
                    .as_mut()
                    .ok_or(Error::Invalid("v7 child cursor absent"))?
                    .poll(work)?
                {
                    BoxStep::Yield => return Ok(BuildStatus::Yield),
                    BoxStep::Box(raw) => self.pending = Some(raw),
                    BoxStep::End => {
                        self.children = None;
                        self.ready = true;
                        return Ok(BuildStatus::Ready);
                    }
                }
            }
            if !work.try_charge(1) {
                return Ok(BuildStatus::Yield);
            }
            let raw = self
                .pending
                .take()
                .ok_or(Error::Invalid("v7 pending child absent"))?;
            self.inspected = self.inspected.checked_add(1).ok_or(Error::Overflow)?;
            // A failed native header still does not skip later source syntax.
            if self.rejection.is_none()
                && let Some(cause) = self.inspect(&raw)
            {
                self.rejection = Some(Mov9HeaderRejectionV7 { raw, cause });
            }
        }
    }

    fn inspect(&mut self, raw: &BoxView) -> Option<Mov9HeaderCauseV7> {
        let body = raw.payload();
        if body.is_empty() {
            // The pinned generic native dispatcher does not call handlers for
            // empty bodies. This is not the Opus grammar's minimum-size rule.
            return None;
        }
        match raw.header().kind {
            [b'd', b'O', b'p', b's'] => {
                if !(11..=1 << 30).contains(&body.len()) {
                    return Some(Mov9HeaderCauseV7::OpusBodyLength {
                        declared: body.len(),
                    });
                }
                if let Some(earlier) = self.installed {
                    return Some(Mov9HeaderCauseV7::OpusConfigurationCollision { earlier });
                }
                if body[0] != 0 {
                    return Some(Mov9HeaderCauseV7::OpusVersion { raw: body[0] });
                }
                self.installed = Some(*b"dOps");
                self.installed_source = Some(raw.clone());
            }
            [b'a', b'v', b'c', b'C'] | [b'h', b'v', b'c', b'C'] | [b'a', b'v', b'1', b'C'] => {
                // These direct configuration handlers also dispatch a whole
                // legacy field-only envelope. Such a body installs no codec
                // bytes and must not overwrite an earlier genuine owner.
                let field_only = body.len() >= 10
                    && &body[4..8] == b"fiel"
                    && usize::try_from(u32::from_be_bytes([body[0], body[1], body[2], body[3]]))
                        .ok()
                        == Some(body.len());
                if !field_only {
                    self.installed = Some(raw.header().kind);
                    // Independent source-order receiver reduction: preserve an
                    // installed body longer than one byte on later glbl input.
                    if self
                        .installed_source
                        .as_ref()
                        .is_none_or(|prior| prior.payload().len() <= 1)
                    {
                        self.installed_source = Some(raw.clone());
                    }
                }
            }
            [b'd', b'd', b't', b's'] if body.len() >= 20 => {
                let rate = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                if rate == 0 || rate > i32::MAX as u32 {
                    return Some(Mov9HeaderCauseV7::DtsSampleRate { raw: rate });
                }
            }
            _ => {}
        }
        // Short ddts may cause a native cross-box read and warning. This job
        // never reads outside a source child or infers its unowned trailing data.
        None
    }

    /// Checks cancellation again at the publication boundary. A result keeps
    /// its complete original owner charged after the job/caller aliases drop.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<Mov9SampleEntryHeaderV7> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(Error::Invalid("v7 header not ready or already taken"));
        }
        let description = self
            .description
            .take()
            .ok_or(Error::Invalid("v7 entry absent"))?;
        self.finished = true;
        Ok(Mov9SampleEntryHeaderV7 {
            description,
            rejection: self.rejection.take(),
            inspected_children: self.inspected,
            installed_configuration_source: self.installed_source.take(),
        })
    }
}

#[cfg(test)]
mod tests;
