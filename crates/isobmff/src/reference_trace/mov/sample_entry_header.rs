//! Sample entry outcomes of FFmpeg's open.
//!
//! [`SampleEntryHeaderJob`] walks one sample entry's child boxes and reports whether FFmpeg's
//! open rejects the entry, with its return code, and which configuration box it installs.
//! [`RawVisualEntriesJob`] applies that to the raw entries of a video track. Neither
//! validates a codec configuration or applies an ISO or QuickTime validity rule.
use crate::{
    BuildStatus, Limits, Next,
    boxes::{BoxChildren, BoxView},
    movie::{EntryLayout, SampleDescription, TrackHeader},
    table::SampleTable,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, Result, WorkBudget};

/// Selects FFmpeg's sample entry checks during open; pass it to
/// [`SampleEntryHeaderJob::new`] or [`RawVisualEntriesJob::new`].
#[derive(Clone, Copy, Debug)]
pub struct SampleEntryHeaderProfile;

/// Why FFmpeg's open rejects a sample entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderCause {
    /// A `dOps` payload shorter than 11 bytes or longer than 2^30 bytes.
    OpusBodyLength {
        /// Payload length in bytes.
        declared: usize,
    },
    /// A `dOps` version other than 0.
    OpusVersion {
        /// The version byte.
        raw: u8,
    },
    /// A `dOps` after another configuration box was installed.
    OpusConfigurationCollision {
        /// Code of that box: `dOps`, `avcC`, `hvcC` or `av1C`.
        earlier: [u8; 4],
    },
    /// A `ddts` of at least 20 payload bytes whose sampling frequency is 0 or above
    /// `i32::MAX`.
    DtsSampleRate {
        /// The 32-bit sampling frequency field.
        raw: u32,
    },
}

/// The child box FFmpeg's open rejects a sample entry for.
#[derive(Debug)]
pub struct HeaderRejection {
    /// The first rejected child box, as it is in the source.
    pub raw: BoxView,
    /// Why FFmpeg rejects it.
    pub cause: HeaderCause,
}

/// FFmpeg's open outcome for one sample entry, with the entry itself.
#[derive(Debug)]
pub struct SampleEntryHeader {
    /// Complete source entry, including every opaque/private child and reserved bit.
    pub description: SampleDescription,
    rejection: Option<HeaderRejection>,
    /// Number of child boxes walked; every child is walked, also after a rejection.
    pub inspected_children: u32,
    /// The configuration box whose payload FFmpeg installs: the first `dOps`, or an `avcC`,
    /// `hvcC` or `av1C`, which replaces an earlier box only when that box's payload is at
    /// most one byte. A field-only body (a whole `fiel` box) installs nothing, nor does a
    /// child after a rejection. `None` when no child installs a box.
    pub installed_configuration_source: Option<BoxView>,
}
impl SampleEntryHeader {
    /// FFmpeg's open return code for the entry: `Some(-1094995529)` (`AVERROR_INVALIDDATA`)
    /// when it rejects a child, `None` otherwise. `None` says nothing about the rest of the
    /// open or about the configuration's validity.
    pub fn demux_open_result(&self) -> Option<i32> {
        self.rejection.as_ref().map(|_| -1_094_995_529)
    }
}

/// Walks one sample entry's child boxes and works out FFmpeg's open outcome for them.
///
/// Every child header is read before the outcome is ready, and a malformed child fails the
/// job even after a rejection. The job reads the entry's retained bytes, never the input,
/// so resetting an input cursor does not affect it. It copies and allocates nothing.
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
    /// Starts a walk over `description`'s child boxes, which start where `layout` says,
    /// with no configuration installed. Fails as
    /// [`SampleDescription::extensions`](crate::movie::SampleDescription::extensions) does
    /// for that layout.
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

    /// Walks further child boxes; a malformed child fails the job.
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
            return Err(Error::Invalid("sample entry header already taken"));
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
                    .ok_or(Error::Invalid("sample entry child cursor absent"))?
                    .poll(work)?
                {
                    Next::Yield => return Ok(BuildStatus::Yield),
                    Next::Item(raw) => self.pending = Some(raw),
                    Next::End => {
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
                .ok_or(Error::Invalid("sample entry pending child absent"))?;
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
        match &raw.header().kind {
            b"dOps" => {
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
            b"avcC" | b"hvcC" | b"av1C" => {
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
            b"ddts" if body.len() >= 20 => {
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

    /// Takes the outcome after [`poll`](Self::poll) returned `Ready`. Fails with
    /// `Error::Invalid` before then or when called again, and with `Error::Cancelled` when
    /// `cancel` is set. The outcome keeps the entry's bytes charged to their pool.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<SampleEntryHeader> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        if !self.ready || self.finished {
            return Err(Error::Invalid(
                "sample entry header not ready or already taken",
            ));
        }
        let description = self
            .description
            .take()
            .ok_or(Error::Invalid("sample entry absent"))?;
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
    /// Starts a check of `table`'s `raw ` entries longer than the 78 bytes of visual
    /// fields. A track whose handler is not `vide` has none to check.
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

    /// Checks further entries. Fails with `Error::Invalid` on the first entry FFmpeg's open
    /// rejects.
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
