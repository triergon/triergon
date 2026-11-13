//! Audio parameter observations for the admitted Matroska startup profiles.
use crate::compat::matroska::legacy_aac_lc_parameters;
use crate::{Output, Result};
use std::sync::atomic::AtomicBool;
use triergon_core::{Error, WorkBudget};
use triergon_framing::{
    AacAdtsParameters, AacAscParser, AacConfigLimits, AacSource, Ac3Family, Ac3FrameDetails,
    BuildStep, probe_ac3_header,
};
use triergon_matroska::{
    matroska::{LegacyAacProfile, Track},
    typed::Packet,
};

#[derive(Default)]
pub(super) struct AudioProbe {
    aac: Option<AacAscParser>,
    unavailable_aac: u64,
}

impl AudioProbe {
    /// Observe one pending packet. An unsupported configuration leaves its
    /// track unresolved, so discovery can fall through to its usual bounds.
    pub fn poll(
        &mut self,
        output: &Output,
        packet: &Packet,
        track: &Track,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<bool>> {
        if self.aac.is_some() {
            return self.poll_aac(packet, work, cancel);
        }
        let dolby = match track.codec.as_str() {
            "A_AAC" => None,
            "A_AC3" => Some(Ac3Family::Ac3),
            "A_EAC3" => Some(Ac3Family::Eac3),
            _ if track.legacy_aac_profile() == Some(LegacyAacProfile::LowComplexity) => None,
            _ => return Ok(Some(false)),
        };
        if !work.try_charge(1) {
            return Ok(None);
        }
        let payload = packet.packet.data.as_ref();
        if payload.is_empty()
            || triergon_matroska::compat::clock::packet_timing(packet.packet.timing)?
                .pts
                .is_none()
        {
            return Ok(Some(false));
        }
        if let Some(family) = dolby {
            // AC-3 decoder initialization already supplies a sample format.
            // A damaged first syncframe can leave the declared rate and
            // channel count available for discovery, independent of decoding.
            let declared = (1.0..=f64::from(i32::MAX)).contains(&track.sample_rate)
                && (1..=i32::MAX as u64).contains(&track.channels);
            return Ok(Some(
                declared
                    || probe_ac3_header(payload).is_ok_and(|header| {
                        header.family == family
                            && header.frame_bytes <= payload.len()
                            && !matches!(
                                header.details,
                                Ac3FrameDetails::Eac3 {
                                    stream_type: 1 | 3,
                                    ..
                                }
                            )
                    }),
            ));
        }
        let bit = 1_u64 << packet.packet.track;
        if self.unavailable_aac & bit != 0 {
            return Ok(Some(false));
        }
        let configuration = &packet.configuration;
        let Some(raw) = configuration.initialization_bytes(packet.packet.track) else {
            let ready = matches!(
                legacy_aac_lc_parameters(configuration, packet.packet.track),
                Ok(Some(_))
            );
            if !ready {
                self.unavailable_aac |= bit;
            }
            return Ok(Some(ready));
        };
        let source = AacSource {
            source: output.source_id(),
            generation: packet.configuration.generation(),
            offset: 0,
        };
        match AacAscParser::new(
            output.pool(),
            source,
            raw,
            AacConfigLimits {
                maximum_record_bytes: 4096,
                maximum_pce_elements: 0,
                maximum_comment_bytes: 0,
            },
        ) {
            Ok(parser) => self.aac = Some(parser),
            Err(error @ (Error::ForeignPool | Error::Overflow)) => {
                return Err(error);
            }
            Err(_) => {
                self.unavailable_aac |= bit;
                return Ok(Some(false));
            }
        }
        self.poll_aac(packet, work, cancel)
    }

    fn poll_aac(
        &mut self,
        packet: &Packet,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<bool>> {
        let parser = self
            .aac
            .as_mut()
            .ok_or(Error::Invalid("missing Matroska AAC probe"))?;
        let ready = match parser.step(work, cancel) {
            Ok(BuildStep::Yield) => return Ok(None),
            Ok(BuildStep::Ready(config)) => {
                let facts = config.facts();
                !facts.sbr.is_present()
                    && !facts.ps.is_present()
                    && AacAdtsParameters::from_asc(facts)
                        .is_ok_and(|adts| (1..=7).contains(&adts.channel_configuration))
            }
            Ok(BuildStep::Complete) => false,
            Err(error @ (Error::Cancelled | Error::Allocation | Error::Backpressure { .. })) => {
                return Err(error);
            }
            Err(_) => false,
        };
        self.aac = None;
        if !ready {
            self.unavailable_aac |= 1_u64 << packet.packet.track;
        }
        Ok(Some(ready))
    }
}
