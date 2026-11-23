//! FFmpeg's encoded edits for every track of a movie, read once and kept by the movie:
//! the demux view of [`compat::mov`](crate::compat::mov) applies an edit list to an
//! encoded track with the codec's frame clock, trim and dependency flags.
use crate::{
    BuildStatus, CursorStatus,
    compat::mov::{
        encoded_edits::{EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditPlan},
        encoded_metadata::EncodedSelection,
        encoded_stream::{EncodedStream, EncodedStreamJob},
    },
    config::ConfigurationLimits,
    media_kind,
    open::{Movie, MovieTrack},
};
use std::sync::{OnceLock, atomic::AtomicBool};
use triergon_core::{
    BudgetVec, Error, InputCursor, Result, SourceCapabilities, WorkBudget, container::MediaKind,
};

/// What a movie keeps of its encoded edits: the admitted stream of each planned track,
/// then the plans themselves, in track order.
#[derive(Debug, Default)]
pub(crate) struct DemuxEdits {
    streams: OnceLock<BudgetVec<Option<EncodedStream>>>,
    plans: OnceLock<BudgetVec<Option<EncodedEditPlan>>>,
}

enum Stage<'a> {
    /// Admitting the sample entry of track `track`.
    Streams {
        track: usize,
        job: Option<Box<EncodedStreamJob<'a>>>,
        streams: BudgetVec<Option<EncodedStream>>,
    },
    /// Planning the edit of track `track` from its admitted stream.
    Plans {
        streams: &'a [Option<EncodedStream>],
        track: usize,
        job: Option<Box<EncodedEditJob<'a>>>,
        plans: BudgetVec<Option<EncodedEditPlan>>,
    },
    Ready(&'a [Option<EncodedEditPlan>]),
}

/// Reads, within work budgets, the encoded edits FFmpeg's MOV demuxer applies to the
/// tracks of a movie: an [`EncodedEditPlan`] for each track with an edit list, no
/// fragments, audio or video media and one sample entry that the range-playback view
/// of [`EncodedStreamJob`] and [`EncodedEditJob`] admits. Other tracks have none and
/// keep the demuxer's plain edit handling. The movie keeps the plans, so a second job
/// over it finishes at once.
#[derive(Debug)]
pub struct DemuxEditsJob<'a> {
    movie: &'a Movie,
    /// Stands for the movie's source in the jobs' checks. It is never offered a window:
    /// the jobs read only metadata the movie holds, so each step checks cancellation.
    input: InputCursor,
    stage: Stage<'a>,
}

impl std::fmt::Debug for Stage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Streams { .. } => "Streams",
            Self::Plans { .. } => "Plans",
            Self::Ready(_) => "Ready",
        })
    }
}

/// Whether FFmpeg's encoded edit view can apply to `track`.
fn edited_media(track: &MovieTrack) -> bool {
    track.edits().is_some()
        && track.fragments().is_empty()
        && matches!(
            media_kind(&track.header().handler),
            MediaKind::Video | MediaKind::Audio
        )
        && track.table().descriptions().len() == 1
}

/// A refusal of the encoded view that leaves the track to the plain edit handling;
/// backpressure and cancellation stop the job.
fn declined(error: Error) -> Result<()> {
    match error {
        Error::Backpressure { .. } | Error::Cancelled => Err(error),
        _ => Ok(()),
    }
}

impl<'a> DemuxEditsJob<'a> {
    /// A job over `movie`, which must be a complete source opened without recovery.
    /// `Error::Unsupported` otherwise.
    pub fn new(movie: &'a Movie) -> Result<Self> {
        if movie.recovery() || movie.read_scope() != crate::open::MetadataReadScope::CompleteEntity
        {
            return Err(Error::Unsupported(
                "encoded edits require a complete source opened without recovery",
            ));
        }
        let cache = movie.demux_edits.get_or_init(Box::default);
        let stage = if let Some(plans) = cache.plans.get() {
            Stage::Ready(plans.as_slice())
        } else if let Some(streams) = cache.streams.get() {
            Stage::Plans {
                streams: streams.as_slice(),
                track: 0,
                job: None,
                plans: BudgetVec::new(movie.pool()),
            }
        } else {
            Stage::Streams {
                track: 0,
                job: None,
                streams: BudgetVec::new(movie.pool()),
            }
        };
        let input = InputCursor::new(
            movie.pool(),
            movie.source(),
            SourceCapabilities {
                length: movie.source_length(),
                random_access: false,
            },
            1,
        )?;
        Ok(Self {
            movie,
            input,
            stage,
        })
    }

    /// Reads within `work`: `Ready` once every track is planned. A failure is final for
    /// the job; the tracks it has not planned are not kept.
    pub fn poll(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<BuildStatus> {
        let movie = self.movie;
        let tracks = movie.tracks();
        loop {
            match &mut self.stage {
                Stage::Ready(_) => return Ok(BuildStatus::Ready),
                Stage::Streams {
                    track,
                    job,
                    streams,
                } => {
                    let Some(t) = tracks.get(*track) else {
                        let streams = std::mem::replace(streams, BudgetVec::new(movie.pool()));
                        let streams = movie
                            .demux_edits
                            .get_or_init(Box::default)
                            .streams
                            .get_or_init(|| streams);
                        self.stage = Stage::Plans {
                            streams: streams.as_slice(),
                            track: 0,
                            job: None,
                            plans: BudgetVec::new(movie.pool()),
                        };
                        continue;
                    };
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let stream = match job {
                        None if !edited_media(t) => None,
                        None => {
                            match EncodedStreamJob::new_range_playback(
                                t.header(),
                                t.table(),
                                &self.input,
                                EncodedSelection {
                                    major_brand: movie.major_brand(),
                                    description: t.table().descriptions()[0].index,
                                    limits: ConfigurationLimits::default(),
                                },
                            ) {
                                Ok(admission) => *job = Some(Box::new(admission)),
                                Err(error) => {
                                    declined(error)?;
                                    streams.try_reserve_exact(1)?;
                                    streams.try_push(None).map_err(|failure| failure.error)?;
                                    *track += 1;
                                }
                            }
                            continue;
                        }
                        Some(admission) => match admission.poll(&mut self.input, work, cancel) {
                            Ok(CursorStatus::Yield) => return Ok(BuildStatus::Yield),
                            Ok(_) => Some(admission.take(&mut self.input, cancel)?),
                            Err(error) => {
                                declined(error)?;
                                None
                            }
                        },
                    };
                    streams.try_reserve_exact(1)?;
                    streams.try_push(stream).map_err(|failure| failure.error)?;
                    *job = None;
                    *track += 1;
                }
                Stage::Plans {
                    streams,
                    track,
                    job,
                    plans,
                } => {
                    let streams = *streams;
                    let Some(t) = tracks.get(*track) else {
                        let plans = std::mem::replace(plans, BudgetVec::new(movie.pool()));
                        let plans = movie
                            .demux_edits
                            .get_or_init(Box::default)
                            .plans
                            .get_or_init(|| plans);
                        self.stage = Stage::Ready(plans.as_slice());
                        continue;
                    };
                    if !work.try_charge(1) {
                        return Ok(BuildStatus::Yield);
                    }
                    let plan = match (job.as_mut(), streams.get(*track)) {
                        (None, Some(Some(stream))) => {
                            let edits = t
                                .edits()
                                .ok_or(Error::Invalid("admitted stream without an edit list"))?;
                            let context = EncodedEditContext {
                                table: t.table(),
                                edits,
                                stream,
                                media_base: t.header().time_base()?,
                                movie_base: movie.header().time_base()?,
                            };
                            match EncodedEditJob::new_range_playback(
                                context,
                                std::slice::from_ref(stream),
                                &self.input,
                                EncodedEditLimits::default(),
                            ) {
                                Ok(planning) => *job = Some(Box::new(planning)),
                                Err(error) => {
                                    declined(error)?;
                                    plans.try_reserve_exact(1)?;
                                    plans.try_push(None).map_err(|failure| failure.error)?;
                                    *track += 1;
                                }
                            }
                            continue;
                        }
                        (None, _) => None,
                        (Some(planning), _) => match planning.poll(&mut self.input, work, cancel) {
                            Ok(CursorStatus::Yield) => return Ok(BuildStatus::Yield),
                            Ok(_) => Some(planning.take(&mut self.input, cancel)?),
                            Err(error) => {
                                declined(error)?;
                                None
                            }
                        },
                    };
                    plans.try_reserve_exact(1)?;
                    plans.try_push(plan).map_err(|failure| failure.error)?;
                    *job = None;
                    *track += 1;
                }
            }
        }
    }

    /// The plans, one per track in movie order (`None` for a track the encoded view
    /// does not apply to), borrowed from the movie. `Error::Invalid` before `Ready`.
    pub fn finish(self) -> Result<&'a [Option<EncodedEditPlan>]> {
        match self.stage {
            Stage::Ready(plans) => Ok(plans),
            _ => Err(Error::Invalid("encoded edits not ready")),
        }
    }
}
