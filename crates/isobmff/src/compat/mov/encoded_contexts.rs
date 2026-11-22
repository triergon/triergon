//! The context view's reports of source faults that FFmpeg's MOV demuxer reads past
//! and the metadata view refuses: [`WaveObservation`] for a QuickTime `wave` box and
//! [`SourceProfileFault`] for one fault. A fault in a report never becomes a validated
//! source value.
use crate::boxes::BoxView;
use triergon_core::Error;
/// A source fault the context view reads past: the error a stricter view returns for
/// it and the box it is in.
#[derive(Clone, Debug)]
pub struct SourceProfileFault {
    /// The error the metadata view, or the strict sample group check, returns.
    pub error: Error,
    /// The box with the fault: the sample entry, its `wave` box or one of the `wave`
    /// children, or an `sgpd` or `sbgp` box.
    pub raw: BoxView,
    /// Number of `wave` children read when the fault was found: a faulty child's 1-based
    /// position, 0 for a fault in the sample entry; `None` for a sample group fault.
    pub ordinal: Option<u32>,
}
/// Opt-in token for the context view, which reports source faults instead of failing:
/// in a `wave` box with `EncodedStreamJob::new_description_with_contexts`, in sample
/// groups with `EncodedEditJob::new_with_recovery_contexts`.
#[derive(Clone, Copy, Debug)]
pub struct EncodedContextProfile;
/// A QuickTime sound entry's `wave` box as the context view read it, with the faults
/// the metadata view refuses.
#[derive(Clone, Debug)]
pub struct WaveObservation {
    /// The `wave` box.
    pub raw: BoxView,
    pub(crate) format: Option<BoxView>,
    pub(crate) marker: Option<BoxView>,
    pub(crate) terminator: Option<BoxView>,
    pub(crate) format_count: u32,
    pub(crate) configuration_count: u32,
    pub(crate) child_count: u32,
    /// Number of faults read past.
    pub fault_count: u32,
    /// The first of them, as the metadata view reports it; `None` when there are none.
    pub first_default_profile_fault: Option<SourceProfileFault>,
}
impl WaveObservation {
    pub(crate) fn new(raw: BoxView) -> Self {
        Self {
            raw,
            format: None,
            marker: None,
            terminator: None,
            format_count: 0,
            configuration_count: 0,
            child_count: 0,
            fault_count: 0,
            first_default_profile_fault: None,
        }
    }
    pub(crate) fn fault(
        &mut self,
        error: Error,
        raw: BoxView,
        ordinal: u32,
    ) -> triergon_core::Result<()> {
        self.fault_count = self.fault_count.checked_add(1).ok_or(Error::Overflow)?;
        if self.first_default_profile_fault.is_none() {
            self.first_default_profile_fault = Some(SourceProfileFault {
                error,
                raw,
                ordinal: Some(ordinal),
            });
        }
        Ok(())
    }
}
