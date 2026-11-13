//! Compatibility policies for playback.

pub mod matroska;

pub mod mov {
    //! MOV packet order and edit clocks using the
    //! [ISO BMFF MOV policy](triergon_isobmff::compat::mov).

    use crate::{
        Result,
        mp4_open::Movie,
        mp4_plan::{Mp4IndexJob, PlanLimits},
    };
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
}
