//! Pin-bound demux header outcomes over immutable source sample entries.
//! This is neither codec validation nor an ISO/QuickTime validity rule.
use crate::{
    BoxChildren, BoxStep, BoxView, BuildStatus, EntryLayout, Limits, SampleDescription,
    SampleTable, TrackHeader,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// reference public MOV open observations, not a general demux profile.
#[derive(Clone, Copy, Debug)]
pub struct SampleEntryHeaderProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderCause {
    OpusBodyLength { declared: usize },
    OpusVersion { raw: u8 },
    OpusConfigurationCollision { earlier: [u8; 4] },
    DtsSampleRate { raw: u32 },
}

#[derive(Debug)]
pub struct HeaderRejection {
    pub raw: BoxView,
    pub cause: HeaderCause,
}

#[derive(Debug)]
pub struct SampleEntryHeader {
    /// Complete source entry, including every opaque/private child and reserved bit.
    pub description: SampleDescription,
    pub rejection: Option<HeaderRejection>,
    pub inspected_children: u32,
    /// Receiver-selected opaque storage in original child order. A nontrivial
    /// configuration persists; all original ordered entry bytes stay retained.
    pub installed_configuration_source: Option<BoxView>,
}
impl SampleEntryHeader {
    /// None means no failure established by this bounded domain. It does not
    /// admit the rest of demux open or assert codec/configuration conformance.
    pub fn demux_open_result(&self) -> Option<i32> {
        self.rejection.as_ref().map(|_| -1_094_995_529)
    }
}

/// Scans all source child headers before publishing a demux-domain outcome.
/// Header/count/range errors remain source errors, even after a demux rejection.
/// One child header unit plus one fixed-field interpretation unit; a pending
/// child permits Work1 progress. No child vector, body copy or new storage is made.
///
/// This job consumes an already owned metadata snapshot, not a live I/O cursor.
/// It cannot seek, consume input, acknowledge failure or change a source token.
/// Resetting an unrelated/live cursor cannot invalidate retained source bytes.
#[derive(Debug)]
pub struct SampleEntryHeaderJob {
    description: Option<SampleDescription>,
    children: Option<BoxChildren>,
    pending: Option<BoxView>,
    installed: Option<[u8; 4]>,
    installed_source: Option<BoxView>,
    rejection: Option<HeaderRejection>,
    inspected: u32,
    ready: bool,
    finished: bool,
    failed: Option<Error>,
}
impl SampleEntryHeaderJob {
    /// The caller supplies the sourced layout; unknown entries are never
    /// silently assigned a visual/audio layout. Configuration state starts at
    /// each description boundary, never at an I/O transaction/config epoch.
    pub fn new(
        description: &SampleDescription,
        layout: EntryLayout,
        limits: Limits,
        _profile: SampleEntryHeaderProfile,
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
            // A failed demux header still does not skip later source syntax.
            if self.rejection.is_none()
                && let Some(cause) = self.inspect(&raw)
            {
                self.rejection = Some(HeaderRejection { raw, cause });
            }
        }
    }

    fn inspect(&mut self, raw: &BoxView) -> Option<HeaderCause> {
        let body = raw.payload();
        if body.is_empty() {
            // The reference generic demux dispatcher does not call handlers for
            // empty bodies. This is not the Opus grammar's minimum-size rule.
            return None;
        }
        match raw.header().kind {
            [b'd', b'O', b'p', b's'] => {
                if !(11..=1 << 30).contains(&body.len()) {
                    return Some(HeaderCause::OpusBodyLength {
                        declared: body.len(),
                    });
                }
                if let Some(earlier) = self.installed {
                    return Some(HeaderCause::OpusConfigurationCollision { earlier });
                }
                if body[0] != 0 {
                    return Some(HeaderCause::OpusVersion { raw: body[0] });
                }
                self.installed = Some(*b"dOps");
                self.installed_source = Some(raw.clone());
            }
            [b'a', b'v', b'c', b'C'] | [b'h', b'v', b'c', b'C'] | [b'a', b'v', b'1', b'C'] => {
                // These direct configuration handlers also dispatch a whole
                // legacy field-only envelope. Such a body installs no codec
                // bytes and must not overwrite an earlier genuine storage.
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
                    return Some(HeaderCause::DtsSampleRate { raw: rate });
                }
            }
            _ => {}
        }
        // Short ddts may cause a demux cross-box read and warning. This job
        // never reads outside a source child or infers its unowned trailing data.
        None
    }

    /// Checks cancellation again at the publication boundary. A result keeps
    /// its complete original storage charged after the job/caller aliases drop.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<SampleEntryHeader> {
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
        Ok(SampleEntryHeader {
            description,
            rejection: self.rejection.take(),
            inspected_children: self.inspected,
            installed_configuration_source: self.installed_source.take(),
        })
    }
}

/// Payload bytes of a visual sample entry with no child boxes.
const VISUAL_ENTRY_FIELDS: usize = 78;

/// Refuses a video track whose raw entries FFmpeg's open rejects.
///
/// FFmpeg reads the child boxes of a raw visual entry before it publishes the stream,
/// so every comparison operation on the file fails on one it rejects. Polling ends
/// with `Error::Invalid` on the first such entry. Source inspection keeps the entry
/// and its child boxes.
#[derive(Debug)]
pub struct RawVisualEntriesJob<'a> {
    entries: std::slice::Iter<'a, SampleDescription>,
    limits: Limits,
    current: Option<SampleEntryHeaderJob>,
    failed: Option<Error>,
}
impl<'a> RawVisualEntriesJob<'a> {
    pub fn new(
        header: &TrackHeader,
        table: &'a SampleTable,
        limits: Limits,
        _profile: SampleEntryHeaderProfile,
    ) -> Self {
        let entries: &[SampleDescription] = if header.handler == *b"vide" {
            table.descriptions()
        } else {
            &[]
        };
        Self {
            entries: entries.iter(),
            limits,
            current: None,
            failed: None,
        }
    }

    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let status = self.advance(work, cancel);
        if let Err(error) = status {
            self.current = None;
            self.failed = Some(error);
        }
        status
    }

    fn advance(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        loop {
            let Some(job) = &mut self.current else {
                let entry = self.entries.find(|entry| {
                    entry.format == *b"raw " && entry.raw().payload().len() != VISUAL_ENTRY_FIELDS
                });
                let Some(entry) = entry else {
                    return Ok(BuildStatus::Ready);
                };
                self.current = Some(SampleEntryHeaderJob::new(
                    entry,
                    EntryLayout::Visual,
                    self.limits,
                    SampleEntryHeaderProfile,
                )?);
                continue;
            };
            if job.poll(work, cancel)? == BuildStatus::Yield {
                return Ok(BuildStatus::Yield);
            }
            let outcome = job.try_finish(cancel)?;
            self.current = None;
            if outcome.demux_open_result().is_some() {
                return Err(Error::Invalid("sample entry configuration"));
            }
        }
    }
}

#[cfg(test)]
mod tests;
