//! MOV packet order and edit clocks using the
//! [ISO BMFF MOV policy](triergon_isobmff::compat::mov).

use crate::{
    Engine, Result,
    engine::mp4::progressive::ProgressiveLimits,
    mp4::open::Movie,
    mp4::plan::{Mp4IndexJob, PlanLimits, PreparedMp4},
};
use triergon_core::InputCursor;
use triergon_isobmff::compat::mov::edits::EncodedEditPlan;

/// Index regular edited packets from front metadata and one finite media
/// extent. Packets follow source byte order and carry the encoded trim.
/// Unpresented source samples remain in the physical index.
pub fn forward_encoded_edits<'a>(
    movie: &'a Movie,
    plans: &'a [Option<EncodedEditPlan>],
    limits: PlanLimits,
) -> Result<Mp4IndexJob<'a>> {
    Mp4IndexJob::for_forward_encoded_edits(movie, plans, limits)
}

/// Index `movie` in the MOV demux packet view: FFmpeg's packet order and clocks,
/// with the recovery of an opener made by `Mp4Open::new_recovery`.
pub fn index(movie: &Movie, limits: PlanLimits) -> Result<Mp4IndexJob<'_>> {
    Mp4IndexJob::demux(movie, limits)
}

/// Index metadata read before its media (`MetadataReadScope::FrontMetadataBeforeMdat`)
/// in the demux view, for [`open_front`].
///
/// # Errors
///
/// `Invalid` when the opener read more than the front metadata.
pub fn index_front(movie: &Movie, limits: PlanLimits) -> Result<Mp4IndexJob<'_>> {
    Mp4IndexJob::front(movie, limits)
}

/// Index a complete source in the demux view with encoded edit plans, one per
/// track in source order (`None` for a track without one).
///
/// # Errors
///
/// `Unsupported` for a recovering opener or a source read only in part.
pub fn index_encoded_range<'a>(
    movie: &'a Movie,
    plans: &'a [Option<EncodedEditPlan>],
    limits: PlanLimits,
) -> Result<Mp4IndexJob<'a>> {
    Mp4IndexJob::encoded_range(movie, plans, limits)
}

/// Play a nonseekable source from a plan made by [`index_front`], continuing
/// through the later roots forward. A terminal `mdat` finishes only on a
/// matching `SourceReply::End`.
///
/// # Errors
///
/// `Invalid` for another plan or a seekable input; the refusals of `Engine::mp4`.
pub fn open_front(
    input: InputCursor,
    plan: PreparedMp4,
    maximum_packet: usize,
    limits: ProgressiveLimits,
) -> Result<Engine> {
    Engine::mp4_front(input, plan, maximum_packet, limits)
}
