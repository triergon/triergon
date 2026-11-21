//! The demuxer's private-audio interpretation, not source namespace
//! inference, audio-frame parsing, ATSC service conformance or decoder config.
use super::{DemuxObserver, DemuxScope};
use crate::compat::pes::clock::DemuxProfile;
use crate::psi::Descriptors;
use crate::{ParseLimits, checked_pid, limit};
use triergon_core::{Budget, Error, Result};

/// How a private-audio PID carries AC-3 or E-AC-3, as identified from its PMT entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateAudioCarriage {
    /// Stream type 0x06 with a DVB AC-3 descriptor.
    DvbAc3Tag,
    /// Stream type 0x06 with a DVB enhanced AC-3 descriptor.
    DvbEac3Tag,
    /// Private stream type 0x81, read as AC-3.
    PrivateType81Ac3,
    /// Private stream type 0x87, read as E-AC-3.
    PrivateType87Eac3,
}

/// Borrow exact source loops; neither loop is copied, rewritten or retained by
/// admission. Their source storage and namespaces remain the caller's facts.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrivateAudioDescriptors<'a> {
    /// The program info descriptor loop bytes.
    pub program: &'a [u8],
    /// The elementary stream descriptor loop bytes.
    pub elementary: &'a [u8],
}

impl PrivateAudioCarriage {
    /// The carriage's name in reference traces.
    #[cfg(feature = "reference-trace")]
    pub fn label(self) -> &'static str {
        match self {
            Self::DvbAc3Tag => "dvb-ac3-descriptor",
            Self::DvbEac3Tag => "dvb-eac3-descriptor",
            Self::PrivateType81Ac3 => "private-stream-type-0x81-ac3",
            Self::PrivateType87Eac3 => "private-stream-type-0x87-eac3",
        }
    }
    fn stream_type(self) -> u8 {
        match self {
            Self::DvbAc3Tag | Self::DvbEac3Tag => 6,
            Self::PrivateType81Ac3 => 0x81,
            Self::PrivateType87Eac3 => 0x87,
        }
    }
    /// Allocation-free bounded admission, using existing descriptor views.
    /// At most section_bytes (<=4096) bytes / descriptors entries are inspected.
    /// A private tag is interpreted ONLY in this explicitly named demux view.
    /// This does not backfill DescriptorNamespace or source codec identity.
    pub fn from_observed_pmt(
        stream_type: u8,
        source: PrivateAudioDescriptors<'_>,
        limits: ParseLimits,
    ) -> Result<Self> {
        Self::from_pmt(stream_type, source, limits, false)
    }

    /// Admit the same carriage types in a negotiated independent media
    /// segment. An elementary registration may confirm the stream type or
    /// audio descriptor. The source PMT and its namespace stay intact.
    pub fn from_hls_pmt(
        stream_type: u8,
        source: PrivateAudioDescriptors<'_>,
        limits: ParseLimits,
    ) -> Result<Self> {
        Self::from_pmt(stream_type, source, limits, true)
    }

    fn from_pmt(
        stream_type: u8,
        source: PrivateAudioDescriptors<'_>,
        limits: ParseLimits,
        hls: bool,
    ) -> Result<Self> {
        limits.validate()?;
        if !matches!(stream_type, 6 | 0x81 | 0x87) {
            return Err(Error::Unsupported(
                "private audio requires PMT stream type 0x06, 0x81 or 0x87",
            ));
        }
        let bytes = source
            .program
            .len()
            .checked_add(source.elementary.len())
            .ok_or(Error::Overflow)?;
        if bytes > limits.section_bytes {
            return Err(limit(bytes, limits.section_bytes));
        }
        let mut count = 0_usize;
        let mut ac3 = false;
        let mut eac3 = false;
        let mut registration = None;
        for (program, raw) in [(true, source.program), (false, source.elementary)] {
            for item in Descriptors::new(raw) {
                let descriptor = item?;
                count = count.checked_add(1).ok_or(Error::Overflow)?;
                if count > limits.descriptors {
                    return Err(limit(count, limits.descriptors));
                }
                descriptor.validate_selected_syntax()?;
                match descriptor.tag {
                    0x05 if hls && !program => {
                        let enhanced = match &descriptor.registration()?.format_identifier {
                            b"AC-3" => false,
                            b"EAC3" => true,
                            _ => return Err(Error::Unsupported("HLS private audio registration")),
                        };
                        if registration
                            .replace(enhanced)
                            .is_some_and(|prior| prior != enhanced)
                        {
                            return Err(Error::Invalid("conflicting HLS audio registrations"));
                        }
                    }
                    // These contexts can change private interpretation; they
                    // are not established by the four retained observations.
                    0x05 | 0x5f => {
                        return Err(Error::Unsupported(
                            "registration or private data specifier is unsupported for this private audio carriage",
                        ));
                    }
                    0x59 | 0x28 | 0x38 => {
                        return Err(Error::Unsupported(
                            "conflicting private audio descriptors or stream type",
                        ));
                    }
                    0x6a | 0x7a if program => {
                        return Err(Error::Unsupported(
                            "program-level DVB audio descriptor is unsupported for private audio",
                        ));
                    }
                    0x6a => {
                        descriptor.dvb_ac3()?;
                        ac3 = true;
                    }
                    0x7a => {
                        descriptor.dvb_eac3()?;
                        eac3 = true;
                    }
                    // Ordered duplicates, language and unknown opaque tails
                    // remain in source; they are never canonicalized here.
                    _ => (),
                }
            }
        }
        let carriage = match (stream_type, ac3, eac3) {
            (6, true, false) => Ok(Self::DvbAc3Tag),
            (6, false, true) => Ok(Self::DvbEac3Tag),
            (0x81, _, false) => Ok(Self::PrivateType81Ac3),
            (0x87, false, _) => Ok(Self::PrivateType87Eac3),
            (6, false, false) => Err(Error::Unsupported(
                "private data stream lacks a DVB AC-3 or E-AC-3 descriptor",
            )),
            _ => Err(Error::Unsupported(
                "conflicting private audio descriptors or stream type",
            )),
        }?;
        if hls && stream_type == 6 && registration.is_none() {
            return Err(Error::Unsupported("HLS private audio registration absent"));
        }
        let enhanced = matches!(carriage, Self::DvbEac3Tag | Self::PrivateType87Eac3);
        if registration.is_some_and(|registered| registered != enhanced) {
            return Err(Error::Invalid(
                "HLS audio registration disagrees with the PMT",
            ));
        }
        Ok(carriage)
    }
}

impl DemuxObserver {
    /// Opt-in finite clean private-audio PES, independent of source namespace.
    /// Reuses charged real transport assembly; no codec AU parser or output repair.
    pub fn new_private_audio(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: PrivateAudioCarriage,
        source: PrivateAudioDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Self::with_private_audio(profile, scope, carriage, budget, limits, || {
            PrivateAudioCarriage::from_observed_pmt(scope.stream_type, source, limits)
        })
    }

    /// As [`new_private_audio`](Self::new_private_audio), for an independent media segment: an
    /// elementary registration descriptor (`AC-3` or `EAC3`) may confirm the carriage and is
    /// required for stream type 0x06.
    ///
    /// Fails as [`new_private_audio`](Self::new_private_audio) does, and with `Unsupported` for
    /// another registration or a missing one on stream type 0x06.
    pub fn new_hls_private_audio(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: PrivateAudioCarriage,
        source: PrivateAudioDescriptors<'_>,
        budget: Budget,
        limits: ParseLimits,
    ) -> Result<Self> {
        Self::with_private_audio(profile, scope, carriage, budget, limits, || {
            PrivateAudioCarriage::from_hls_pmt(scope.stream_type, source, limits)
        })
    }

    fn with_private_audio(
        profile: DemuxProfile,
        scope: DemuxScope,
        carriage: PrivateAudioCarriage,
        budget: Budget,
        limits: ParseLimits,
        observe: impl FnOnce() -> Result<PrivateAudioCarriage>,
    ) -> Result<Self> {
        if scope.program == 0 {
            return Err(Error::Invalid(
                "PES observer requires a nonzero program number",
            ));
        }
        checked_pid(scope.pid)?;
        limits.validate()?;
        if scope.stream_type != carriage.stream_type() {
            return Err(Error::Invalid(
                "selected private audio carriage disagrees with the PMT",
            ));
        }
        if scope.stream_id != 0xbd {
            return Err(Error::Unsupported(
                "private audio requires PES stream ID 0xbd",
            ));
        }
        if observe()? != carriage {
            return Err(Error::Invalid(
                "selected private audio carriage disagrees with the PMT",
            ));
        }
        let mut observer = Self::admitted(profile, scope, budget, limits)?;
        observer.private_audio = Some(carriage);
        Ok(observer)
    }
}
