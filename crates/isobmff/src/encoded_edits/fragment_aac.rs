use crate::{
    ConfigurationLimits,
    encoded_stream::{AudioDescription, AudioDescriptionStep},
    mov_cursor::fragment_media_start,
    open::{MetadataReadScope, Movie},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, Result, Rounding, TimeBase, WorkBudget};
use triergon_framing::{AacAscParser, AacConfigLimits, AacSource, BuildStep};

enum Phase {
    Description(AudioDescription),
    Audio(AacAscParser),
}

/// Read the leading decoder trim for the first AAC packet of a
/// fragmented track with an open-ended rate-one edit. The source sample and
/// fragment decode times stay in the original media timebase.
#[allow(missing_debug_implementations)] // The AAC parser has no Debug implementation.
pub struct FragmentAacTrimJob {
    phase: Phase,
    pool: Budget,
    source: AacSource,
    media_start: i64,
    media_base: TimeBase,
}

impl FragmentAacTrimJob {
    /// Returns no job when the track has no leading AAC trim to project.
    pub fn new(movie: &Movie, at: usize) -> Result<Option<Self>> {
        let track = movie
            .tracks()
            .get(at)
            .ok_or(Error::Invalid("fragment edit track absent"))?;
        if track.header().handler != *b"soun" || track.table().sample_count() != 0 {
            return Ok(None);
        }
        if track.fragments().is_empty()
            && !(matches!(
                movie.read_scope(),
                MetadataReadScope::FrontMetadataBeforeMdat(_)
            ) && movie
                .trex()
                .iter()
                .any(|trex| trex.track_id == track.header().track_id))
        {
            return Ok(None);
        }
        let Some(edits) = track.edits() else {
            return Ok(None);
        };
        let media_start = fragment_media_start(edits)?;
        if media_start == 0 {
            return Ok(None);
        }
        let descriptions = track.table().descriptions();
        if descriptions
            .iter()
            .all(|description| description.format != *b"mp4a")
        {
            return Ok(None);
        }
        let [description] = descriptions else {
            return Err(Error::Unsupported(
                "fragment audio edit requires one description",
            ));
        };
        Ok(Some(Self {
            phase: Phase::Description(AudioDescription::new(
                description,
                ConfigurationLimits::default(),
            )?),
            pool: movie.pool().clone(),
            source: AacSource {
                source: movie.source(),
                generation: movie.generation(),
                offset: 0,
            },
            media_start,
            media_base: track.header().time_base()?,
        }))
    }

    /// Return the leading sample count, yielding with `None` until the
    /// configuration is ready. A non-AAC description yields zero.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Option<u32>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if let Phase::Description(job) = &mut self.phase {
            let config = match job.poll(work)? {
                AudioDescriptionStep::Yield => return Ok(None),
                AudioDescriptionStep::Aac(config) => config,
                AudioDescriptionStep::Other => return Ok(Some(0)),
            };
            self.phase = Phase::Audio(AacAscParser::new(
                &self.pool,
                AacSource {
                    offset: config.offset,
                    ..self.source
                },
                config.bytes,
                AacConfigLimits {
                    maximum_record_bytes: 4096,
                    maximum_pce_elements: 64,
                    maximum_comment_bytes: 255,
                },
            )?);
        }
        let Phase::Audio(parser) = &mut self.phase else {
            return Err(Error::Invalid("fragment AAC parser absent"));
        };
        match parser.step(work, cancel)? {
            BuildStep::Yield => Ok(None),
            BuildStep::Ready(config) => {
                let rate = config.facts().signalled_render_rate_hz();
                let samples = self.media_base.rescale(
                    self.media_start,
                    TimeBase::new(1, u64::from(rate))?,
                    Rounding::NearestAway,
                )?;
                Ok(Some(u32::try_from(samples).map_err(|_| Error::Overflow)?))
            }
            BuildStep::Complete => Err(Error::Invalid("fragment AAC trim already read")),
        }
    }
}
