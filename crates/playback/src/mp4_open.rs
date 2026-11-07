//! The transport pump around the ISO BMFF movie opener.
use crate::{
    Format, Result,
    input::Source,
    io::{SelectedInput, SourceOwner, provide},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, WorkBudget};
pub(crate) use triergon_isobmff::open::{
    CachedMedia, PendingMedia, RETAINED_MEDIA_BYTES, box_reader,
};
pub use triergon_isobmff::open::{MetadataReadScope, Movie, MovieTrack, OpenLimits};
use triergon_isobmff::{
    TimingRunPolicy,
    open::{MovieOpen, OpenStep},
};

pub struct OpenedMp4<S: Source> {
    pub movie: Movie,
    pub input: InputCursor,
    pub source: SourceOwner<S>,
}
#[derive(Debug)]
pub enum OpenProgress {
    Yield,
    NeedInput(triergon_core::InputRequest),
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
pub struct Mp4Open<S: Source> {
    input: Option<InputCursor>,
    source: Option<SourceOwner<S>>,
    opener: MovieOpen,
    request: Option<triergon_core::InputRequest>,
    failed: bool,
}
impl<S: Source> Mp4Open<S> {
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
            if provide(
                self.input.as_mut().ok_or(Error::Invalid("input missing"))?,
                &reply,
            )? {
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
