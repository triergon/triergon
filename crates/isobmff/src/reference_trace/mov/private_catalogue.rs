//! The private configuration bytes FFmpeg publishes for a raw-video entry.
//!
//! [`PrivateConfigurationJob`] works out the bytes from the configuration box FFmpeg installs
//! for the entry, and [`StreamView::for_source_private`] puts them on the track's stream view
//! as `extradata`. The track keeps its raw-video codec whatever the bytes hold, and nothing
//! is decoded.
use crate::{
    BuildStatus, Limits,
    compat::mov::view::StreamView,
    movie::{EntryLayout, SampleDescription, TrackHeader},
    reference_trace::mov::sample_entry_header::{
        SampleEntryHeader, SampleEntryHeaderJob, SampleEntryHeaderProfile,
    },
    table::SampleTable,
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};
/// Selects FFmpeg's private configuration for raw video; pass it to
/// [`PrivateConfigurationJob::new`] or [`StreamView::for_source_private`].
#[derive(Clone, Copy, Debug)]
pub struct PrivateCatalogueProfile;
/// Where the private bytes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateConfigCause {
    /// No bytes: no child installs a configuration box, or FFmpeg's open rejects the entry.
    Absent,
    /// The payload of the installed `avcC`, `hvcC` or `av1C` box, unchanged.
    OpaqueDirectBody,
    /// A 19-byte Opus identification header (`OpusHead`, RFC 7845) built from an 11-byte
    /// `dOps` of channel mapping family 0.
    OpusIdentificationMappingZero,
}
/// The private bytes FFmpeg publishes for a raw-video entry, with the entry's open outcome.
#[derive(Debug)]
pub struct PrivateConfiguration {
    header: SampleEntryHeader,
    demux: Option<Bytes>,
    cause: PrivateConfigCause,
}
impl PrivateConfiguration {
    /// FFmpeg's open outcome for the entry, with the entry itself.
    pub fn source(&self) -> &SampleEntryHeader {
        &self.header
    }
    /// The bytes FFmpeg publishes as the stream's `extradata`, or `None` for none.
    pub fn demux_bytes(&self) -> Option<&Bytes> {
        self.demux.as_ref()
    }
    /// Where [`demux_bytes`](Self::demux_bytes) come from.
    pub fn cause(&self) -> PrivateConfigCause {
        self.cause
    }
}
/// Works out the private bytes FFmpeg publishes for one `raw ` entry.
///
/// Like [`SampleEntryHeaderJob`], it reads the entry's retained bytes, never the input.
/// Pass the result to [`StreamView::for_source_private`] with the same entry's track.
#[derive(Debug)]
pub struct PrivateConfigurationJob {
    pool: Budget,
    header_job: Option<SampleEntryHeaderJob>,
    pending: Option<SampleEntryHeader>,
    ready: Option<PrivateConfiguration>,
    maximum: usize,
    failed: Option<Error>,
    finished: bool,
}
impl PrivateConfigurationJob {
    /// Starts the job for `description`, allowing at most `maximum` private bytes. Fails
    /// with `Error::ForeignPool` when the entry is not in `pool` and with
    /// `Error::Unsupported` unless its format is `raw `.
    pub fn new(
        description: &SampleDescription,
        pool: &Budget,
        maximum: usize,
        limits: Limits,
        _profile: PrivateCatalogueProfile,
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
            header_job: Some(SampleEntryHeaderJob::new(
                description,
                EntryLayout::Visual,
                limits,
                SampleEntryHeaderProfile,
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
    /// Walks the entry and builds the bytes. Fails with `Error::Limit` when they exceed
    /// `maximum`, and with `Error::Unsupported` for a `dOps` other than an 11-byte record
    /// of version 0, one or two channels and channel mapping family 0.
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
        // A demux header rejection remains a rejection with original source
        // cause. Do not synthesize usable configuration from the failed entry.
        let (demux, cause) = if header.demux_open_result().is_some() {
            (None, PrivateConfigCause::Absent)
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
                    PrivateConfigCause::OpusIdentificationMappingZero,
                )
            } else {
                let start = usize::from(raw.header().header_size);
                (
                    Some(raw.bytes().slice(start..raw.bytes().len())?),
                    PrivateConfigCause::OpaqueDirectBody,
                )
            }
        } else {
            (None, PrivateConfigCause::Absent)
        };
        self.ready = Some(PrivateConfiguration {
            header: self
                .pending
                .take()
                .ok_or(Error::Invalid("private header commit absent"))?,
            demux,
            cause,
        });
        Ok(BuildStatus::Ready)
    }
    /// Takes the result after [`poll`](Self::poll) returned `Ready`. Fails with
    /// `Error::Invalid` before then or when called again, and with `Error::Cancelled` when
    /// `cancel` is set.
    pub fn try_finish(&mut self, cancel: &AtomicBool) -> Result<PrivateConfiguration> {
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
impl StreamView {
    /// A regular raw-video track's stream view as [`for_source`](Self::for_source) builds
    /// it, with `configuration`'s bytes as `extradata`; the codec and table stay those of the
    /// source. Fails with `Error::Unsupported` unless the track is an unfragmented `vide`
    /// track with one `raw ` entry, and with `Error::Invalid` when `configuration` belongs
    /// to another entry or FFmpeg's open rejects the entry.
    pub fn for_source_private(
        header: &TrackHeader,
        table: &SampleTable,
        has_fragments: bool,
        major: Option<[u8; 4]>,
        configuration: &PrivateConfiguration,
        _profile: PrivateCatalogueProfile,
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
        if configuration.source().demux_open_result().is_some() {
            return Err(Error::Invalid("private catalogue rejected source header"));
        }
        let mut stream = Self::for_source(header, table, false, major)?;
        if let Some(bytes) = configuration.demux_bytes() {
            stream.extradata = bytes.clone();
        }
        Ok(stream)
    }
}
