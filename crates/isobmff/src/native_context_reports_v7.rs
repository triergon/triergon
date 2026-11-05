//! Owned source/default-profile diagnostics alongside explicit receiver views.
//! A source error never becomes validated source metadata through this report.
use crate::BoxView;
use triergon_core::Error;
#[derive(Clone, Debug)]
pub struct Mov9SourceProfileFaultV7 {
    pub error: Error,
    pub raw: BoxView,
    pub ordinal: Option<u32>,
}
#[derive(Clone, Copy, Debug)]
pub struct PinnedMovD32EncodedContextsV7;
#[derive(Clone, Debug)]
pub struct Mov9WaveObservationV7 {
    pub raw: BoxView,
    pub format: Option<BoxView>,
    pub marker: Option<BoxView>,
    pub terminator: Option<BoxView>,
    pub format_count: u32,
    pub configuration_count: u32,
    pub child_count: u32,
    pub fault_count: u32,
    pub first_default_profile_fault: Option<Mov9SourceProfileFaultV7>,
}
impl Mov9WaveObservationV7 {
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
            self.first_default_profile_fault = Some(Mov9SourceProfileFaultV7 {
                error,
                raw,
                ordinal: Some(ordinal),
            });
        }
        Ok(())
    }
}
