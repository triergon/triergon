//! Explicit D32 opaque-private catalogue view. Source raw-video codec identity
//! never becomes Opus/AVC/HEVC from private bytes. No decoded-AU claim.
use crate::{
    BuildStatus, EntryLayout, Limits, Mov9SampleEntryHeaderJobV7, Mov9SampleEntryHeaderV7,
    Mov9StreamView, PinnedMovD32SampleEntryHeaderV7, SampleDescription, SampleTable, TrackHeader,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};
#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32OpaquePrivateCatalogueV8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mov9PrivateConfigCauseV8 {
    Absent,
    OpaqueDirectBody,
    OpusIdentificationMappingZero,
}
#[derive(Debug)]
pub struct Mov9PrivateConfigurationV8 {
    header: Mov9SampleEntryHeaderV7,
    native: Option<Bytes>,
    cause: Mov9PrivateConfigCauseV8,
}
impl Mov9PrivateConfigurationV8 {
    pub fn source(&self) -> &Mov9SampleEntryHeaderV7 {
        &self.header
    }
    pub fn native_bytes(&self) -> Option<&Bytes> {
        self.native.as_ref()
    }
    pub fn cause(&self) -> Mov9PrivateConfigCauseV8 {
        self.cause
    }
}
/// This is an owned metadata operation like V7. It neither reads a live cursor
/// nor implies that reset invalidates retained source metadata. The caller must
/// bind the result to the exact source description before publishing a stream.
#[derive(Debug)]
pub struct Mov9PrivateConfigurationJobV8 {
    pool: Budget,
    header_job: Option<Mov9SampleEntryHeaderJobV7>,
    pending: Option<Mov9SampleEntryHeaderV7>,
    ready: Option<Mov9PrivateConfigurationV8>,
    maximum: usize,
    failed: Option<Error>,
    finished: bool,
}
impl Mov9PrivateConfigurationJobV8 {
    pub fn new(
        description: &SampleDescription,
        pool: &Budget,
        maximum: usize,
        limits: Limits,
        _profile: PinnedMovD32OpaquePrivateCatalogueV8,
    ) -> Result<Self> {
        if !description.raw().bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        if description.format != *b"raw " {
            return Err(Error::Unsupported(
                "private configuration raw-visual source",
            ));
        }
        Ok(Self {
            pool: pool.clone(),
            header_job: Some(Mov9SampleEntryHeaderJobV7::new(
                description,
                EntryLayout::Visual,
                limits,
                PinnedMovD32SampleEntryHeaderV7,
            )?),
            pending: None,
            ready: None,
            maximum,
            failed: None,
            finished: false,
        })
    }
    fn poison(&mut self, error: Error) -> Error {
        self.header_job = None;
        self.pending = None;
        self.ready = None;
        self.failed = Some(error);
        error
    }
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.inner(work, cancel);
        match result {
            Err(error @ Error::Backpressure { .. }) => Err(error),
            Err(error) => Err(self.poison(error)),
            value => value,
        }
    }
    fn inner(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if self.finished {
            return Err(Error::Invalid("private configuration already taken"));
        }
        if self.ready.is_some() {
            return Ok(BuildStatus::Ready);
        }
        if let Some(job) = &mut self.header_job {
            if job.poll(work, cancel)? == BuildStatus::Yield {
                return Ok(BuildStatus::Yield);
            }
            self.pending = Some(job.try_finish(cancel)?);
            self.header_job = None;
        }
        if !work.try_charge(1) {
            return Ok(BuildStatus::Yield);
        }
        let header = self
            .pending
            .as_ref()
            .ok_or(Error::Invalid("private header pending absent"))?;
        // A native header rejection remains a rejection with original source
        // cause. Do not synthesize usable configuration from the failed entry.
        let (native, cause) = if header.native_open_result().is_some() {
            (None, Mov9PrivateConfigCauseV8::Absent)
        } else if let Some(raw) = &header.installed_configuration_source {
            let body = raw.payload();
            if body.len() > self.maximum {
                return Err(Error::Limit {
                    requested: body.len(),
                    maximum: self.maximum,
                });
            }
            if raw.header().kind == *b"dOps" {
                if body.len() != 11 || body[0] != 0 || !matches!(body[1], 1 | 2) || body[10] != 0 {
                    return Err(Error::Unsupported("private Opus mapping0 fixed11 profile"));
                }
                if self.maximum < 19 {
                    return Err(Error::Limit {
                        requested: 19,
                        maximum: self.maximum,
                    });
                }
                // dOps fields are big endian; RFC7845 identification fields are
                // little endian. No timestamp/sample-rate rescaling is implied.
                let mut out = [0u8; 19];
                out[..8].copy_from_slice(b"OpusHead");
                out[8] = 1;
                out[9] = body[1];
                out[10] = body[3];
                out[11] = body[2];
                out[12] = body[7];
                out[13] = body[6];
                out[14] = body[5];
                out[15] = body[4];
                out[16] = body[9];
                out[17] = body[8];
                out[18] = body[10];
                (
                    Some(Bytes::copy_from_slice(&self.pool, &out)?),
                    Mov9PrivateConfigCauseV8::OpusIdentificationMappingZero,
                )
            } else {
                let start = usize::from(raw.header().header_size);
                (
                    Some(raw.bytes().slice(start..raw.bytes().len())?),
                    Mov9PrivateConfigCauseV8::OpaqueDirectBody,
                )
            }
        } else {
            (None, Mov9PrivateConfigCauseV8::Absent)
        };
        self.ready = Some(Mov9PrivateConfigurationV8 {
            header: self
                .pending
                .take()
                .ok_or(Error::Invalid("private header commit absent"))?,
            native,
            cause,
        });
        Ok(BuildStatus::Ready)
    }
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<Mov9PrivateConfigurationV8> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(self.poison(Error::Cancelled));
        }
        let result = self
            .ready
            .take()
            .ok_or(Error::Invalid("private configuration not ready"))?;
        self.finished = true;
        Ok(result)
    }
}
impl Mov9StreamView {
    /// Limited raw-visual private-control receiver. It preserves the actual
    /// source codec and table, and constructs the stream before publication.
    pub fn for_source_private_v8(
        header: &TrackHeader,
        table: &SampleTable,
        has_fragments: bool,
        major: Option<[u8; 4]>,
        configuration: &Mov9PrivateConfigurationV8,
        _profile: PinnedMovD32OpaquePrivateCatalogueV8,
    ) -> Result<Self> {
        if has_fragments || header.handler != *b"vide" || table.descriptions().len() != 1 {
            return Err(Error::Unsupported(
                "private catalogue regular raw-visual context",
            ));
        }
        let entry = &table.descriptions()[0];
        let owned = &configuration.source().description;
        if entry.format != *b"raw " {
            return Err(Error::Unsupported("private catalogue raw source coding"));
        }
        if entry.index != owned.index
            || entry.format != owned.format
            || entry.data_reference_index != owned.data_reference_index
            || entry.raw().header() != owned.raw().header()
            || entry.raw().bytes().as_ref().as_ptr() != owned.raw().bytes().as_ref().as_ptr()
            || entry.raw().bytes().len() != owned.raw().bytes().len()
        {
            return Err(Error::Invalid("private catalogue source association"));
        }
        if configuration.source().native_open_result().is_some() {
            return Err(Error::Invalid("private catalogue rejected source header"));
        }
        let mut stream = Self::for_source(header, table, false, major)?;
        if let Some(bytes) = configuration.native_bytes() {
            stream.extradata = bytes.clone();
        }
        Ok(stream)
    }
}
