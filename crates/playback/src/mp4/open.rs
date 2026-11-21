//! The transport pump around the ISO BMFF movie opener.
use crate::{Format, Result, Source, SourceOwner, probe::SelectedInput};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, WorkBudget};
pub(crate) use triergon_isobmff::open::{
    CachedMedia, PendingMedia, RETAINED_MEDIA_BYTES, box_reader,
};
pub use triergon_isobmff::open::{MetadataReadScope, Movie, MovieTrack, OpenLimits};
use triergon_isobmff::{
    open::{MovieOpen, OpenStep},
    table::TimingRunPolicy,
};

/// An opened MP4 movie with the input and source it was read from.
#[derive(Debug)]
pub struct OpenedMp4<S: Source> {
    /// The movie's metadata.
    pub movie: Movie,
    /// The input cursor, after the metadata.
    pub input: InputCursor,
    /// The source.
    pub source: SourceOwner<S>,
}
/// One step of an [`Mp4Open`].
#[derive(Debug)]
pub enum OpenProgress {
    /// The work budget ran out: advance again.
    Yield,
    /// The source is pending on this request: advance again once it can reply.
    NeedInput(triergon_core::InputRequest),
    /// The movie is open: call `finish`.
    Ready,
}
pub(crate) fn word(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        b.get(at..at.checked_add(4).ok_or(Error::Overflow)?)
            .ok_or(Error::Truncated)?
            .try_into()
            .map_err(|_| Error::Truncated)?,
    ))
}
fn check_format<S: Source>(selected: &SelectedInput<S>) -> Result<()> {
    if selected.format != Format::Mp4 {
        return Err(Error::Invalid("not selected MP4"));
    }
    Ok(())
}
/// Opens the movie of a selected MP4 input: reads its metadata within work budgets.
#[derive(Debug)]
pub struct Mp4Open<S: Source> {
    input: Option<InputCursor>,
    source: Option<SourceOwner<S>>,
    opener: MovieOpen,
    request: Option<triergon_core::InputRequest>,
    failed: bool,
}
impl<S: Source> Mp4Open<S> {
    /// An opener that refuses out-of-source, truncated and zero-timescale media. Fails
    /// with `Invalid` unless `selected` is an MP4 input.
    pub fn new(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        check_format(&selected)?;
        let opener = MovieOpen::new(&selected.input, limits)?;
        Ok(Self::with_opener(selected, opener))
    }

    /// Choose how the opener validates surplus sample timing.
    /// Inspect each track's sample table for the declared surplus counts.
    pub fn with_timing_policy(
        selected: SelectedInput<S>,
        limits: OpenLimits,
        policy: TimingRunPolicy,
    ) -> Result<Self> {
        let mut opener = Self::new(selected, limits)?;
        opener.opener.set_timing_policy(policy);
        Ok(opener)
    }

    /// Stop at the first media header after a front movie, even when the
    /// transport offers ranges. The progressive engine parses later fragments.
    pub fn new_progressive(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        check_format(&selected)?;
        let opener = MovieOpen::new_progressive(&selected.input, limits)?;
        Ok(Self::with_opener(selected, opener))
    }

    /// Admit terminal media declarations for the explicit pinned native
    /// recovery view. SourcePhysical uses the strict opener.
    pub fn new_recovery(selected: SelectedInput<S>, limits: OpenLimits) -> Result<Self> {
        check_format(&selected)?;
        let opener = MovieOpen::new_recovery(&selected.input, limits)?;
        Ok(Self::with_opener(selected, opener))
    }

    fn with_opener(selected: SelectedInput<S>, opener: MovieOpen) -> Self {
        Self {
            input: Some(selected.input),
            source: Some(selected.source),
            opener,
            request: None,
            failed: false,
        }
    }
    /// Reads metadata within `w`. A failure other than backpressure closes the source
    /// and is final.
    pub fn advance(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<OpenProgress> {
        if self.failed {
            return Err(Error::Closed);
        }
        let result = self.advance_inner(w, c);
        if result.is_err() && !matches!(&result, Err(Error::Backpressure { .. })) {
            self.failed = true;
            if let Some(source) = self.source.as_mut() {
                source.close();
            }
        }
        result
    }
    fn advance_inner(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<OpenProgress> {
        if c.load(Ordering::Relaxed) {
            self.source
                .as_mut()
                .ok_or(Error::Invalid("opener closed"))?
                .close();
            return Err(Error::Cancelled);
        }
        if !self.opener.is_ready()
            && w.remaining() != 0
            && let Some(r) = self.request.clone()
        {
            let reply = self
                .source
                .as_mut()
                .ok_or(Error::Invalid("source missing"))?
                .poll(&r, w, c)?;
            if reply.provide_to(self.input.as_mut().ok_or(Error::Invalid("input missing"))?)? {
                self.request = None;
            }
            return Ok(OpenProgress::Yield);
        }
        let input = self.input.as_mut().ok_or(Error::Invalid("input missing"))?;
        Ok(match self.opener.advance(input, w, c)? {
            OpenStep::Yield | OpenStep::Root(_) => OpenProgress::Yield,
            OpenStep::NeedInput(r) => {
                self.request = Some(r.clone());
                OpenProgress::NeedInput(r)
            }
            OpenStep::Ready => OpenProgress::Ready,
        })
    }
    /// The opened movie. Fails with `Closed` after a failure and as the opener does
    /// before `Ready`.
    pub fn finish(mut self) -> Result<OpenedMp4<S>> {
        if self.failed {
            return Err(Error::Closed);
        }
        let input = self.input.take().ok_or(Error::Invalid("input missing"))?;
        Ok(OpenedMp4 {
            movie: self.opener.finish(&input)?,
            input,
            source: self.source.take().ok_or(Error::Invalid("source missing"))?,
        })
    }
}
