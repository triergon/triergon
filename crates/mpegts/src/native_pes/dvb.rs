//! Original explicit DVB-signalled finite-PES admission. No inner decoder/AU
//! parsing or source namespace inference. The native packet keeps the complete
//! PES data field, including identifier/stream ID/tail bytes and opaque faults.
use super::{NativePesObserver, NativePesScope};
use crate::clock::NativeClockProfile;
use crate::psi::{DescriptorNamespace, Descriptors};
use crate::{ParseLimits, checked_pid, limit};
use triergon_core::{Budget, Error, Result};

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeDvbSubtitlesDescriptors<'a> {
    pub program: &'a [u8],
    pub elementary: &'a [u8],
}

fn preflight(
    scope: NativePesScope,
    namespace: DescriptorNamespace,
    source: NativeDvbSubtitlesDescriptors<'_>,
    limits: ParseLimits,
) -> Result<()> {
    if scope.program == 0 {
        return Err(Error::Invalid("native PES selected program"));
    }
    checked_pid(scope.pid)?;
    limits.validate()?;
    if scope.stream_type != 6 || scope.stream_id != 0xbd {
        return Err(Error::Unsupported(
            "native DVB PES stream context unobserved",
        ));
    }
    if !matches!(namespace, DescriptorNamespace::Dvb(_)) {
        return Err(Error::Unsupported("native DVB namespace not declared"));
    }
    let bytes = source
        .program
        .len()
        .checked_add(source.elementary.len())
        .ok_or(Error::Overflow)?;
    if bytes > limits.section_bytes {
        return Err(limit(bytes, limits.section_bytes));
    }
    // Allocation-free bounded syntax actions, <=4096 bytes / configured entries.
    // No stream body is scanned, copied or retained by this preflight.
    let mut count = 0_usize;
    let mut subtitle_entries = 0;
    let mut subtitle_descriptors = 0;
    for (program, raw) in [(true, source.program), (false, source.elementary)] {
        for entry in Descriptors::new(raw) {
            let descriptor = entry?;
            count = count.checked_add(1).ok_or(Error::Overflow)?;
            if count > limits.descriptors {
                return Err(limit(count, limits.descriptors));
            }
            descriptor.validate_selected_syntax()?;
            if descriptor.tag == 0x59 {
                let view = descriptor.dvb_subtitling()?;
                if !program {
                    subtitle_descriptors += 1;
                    subtitle_entries += view.service_count();
                }
            }
        }
    }
    if !source.program.is_empty()
        || source.elementary.len() != 10
        || subtitle_descriptors != 1
        || subtitle_entries != 1
    {
        return Err(Error::Unsupported(
            "native DVB single-service PMT context unobserved",
        ));
    }
    // The exact ordered TLV remains the caller's source. No service/language
    // selection, codec mapping, namespace backfill or inner segment validation.
    Ok(())
}

impl NativePesObserver {
    pub fn new_dvb_subtitles_v1(
        profile: NativeClockProfile,
        scope: NativePesScope,
        namespace: DescriptorNamespace,
        descriptors: NativeDvbSubtitlesDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        preflight(scope, namespace, descriptors, limits)?;
        let mut observer = Self::admitted(profile, scope, budget, limits)?;
        observer.dvb_subtitles = true;
        Ok(observer)
    }
}
