//! The selected and discovered PES tracks of a TS engine and the observers
//! built for them.

use crate::Result;
use crate::compat::pes::{AdmittedCarriage, Carriage, PidSelection};
use crate::engine::ts::{Delivery, TsEngine};
use triergon_core::{BudgetVec, Error};
use triergon_mpegts::{self as ts};
use ts::compat::pes::{
    DemuxObserver, DemuxScope, DvbSubtitlesDescriptors, PrivateAudioCarriage,
    PrivateAudioDescriptors, RegisteredCarriage,
};
use ts::pes::stream_id;
use ts::psi::{DescriptorNamespace, ProgramMap, stream_type};
#[derive(Debug)]
pub(crate) enum TrackUpdate {
    Configuration(AdmittedCarriage),
    Refusal(Error),
}

#[derive(Debug)]
pub(crate) struct PesTrack {
    pub(crate) selection: PidSelection,
    pub(crate) observer: Option<DemuxObserver>,
    pub(crate) update: Option<TrackUpdate>,
}

impl PesTrack {
    pub(crate) fn new(selection: PidSelection) -> Self {
        Self {
            selection,
            observer: None,
            update: None,
        }
    }

    pub(crate) fn pid(&self) -> u16 {
        self.selection.pid
    }

    pub(crate) fn refuse(&mut self, error: Error) {
        self.observer = None;
        self.update = Some(TrackUpdate::Refusal(error));
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "one selected track holds its observer inline, so the common case allocates no list"
)]
#[derive(Debug)]
pub(crate) enum PesTracks {
    Inline(Option<PesTrack>),
    Multiple(BudgetVec<PesTrack>),
}

impl PesTracks {
    pub(crate) fn as_slice(&self) -> &[PesTrack] {
        match self {
            Self::Inline(track) => track.as_slice(),
            Self::Multiple(tracks) => tracks.as_slice(),
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [PesTrack] {
        match self {
            Self::Inline(track) => track.as_mut_slice(),
            Self::Multiple(tracks) => tracks.as_mut_slice(),
        }
    }

    pub(crate) fn index(&self, pid: u16) -> Option<usize> {
        self.as_slice()
            .binary_search_by_key(&pid, PesTrack::pid)
            .ok()
    }
}

impl TsEngine {
    pub(crate) fn discover_tracks(&self, map: &ProgramMap) -> Result<PesTracks> {
        let mut selections = map.streams.as_slice().iter().filter_map(|stream| {
            let carriage = match stream.stream_type {
                stream_type::H264 => Carriage::Registered(RegisteredCarriage::Avc),
                stream_type::HEVC => Carriage::Registered(RegisteredCarriage::Hevc),
                stream_type::AAC_ADTS
                | stream_type::AAC_LATM
                | stream_type::ATSC_AC3
                | stream_type::ATSC_EAC3 => Carriage::AudioFromPmt,
                stream_type::PES_PRIVATE_DATA
                    if (matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_))
                        || self.delivery == Delivery::MediaFeed)
                        && PrivateAudioCarriage::is_signalled_by_dvb_descriptor(stream) =>
                {
                    Carriage::AudioFromPmt
                }
                _ => return None,
            };
            Some(PidSelection {
                pid: stream.pid,
                carriage,
            })
        });
        let count = selections.clone().count();
        if count <= 1 {
            return Ok(PesTracks::Inline(selections.next().map(PesTrack::new)));
        }
        let mut tracks = BudgetVec::new(self.input.budget());
        tracks.try_reserve_exact(count)?;
        for selection in selections {
            tracks
                .try_push(PesTrack::new(selection))
                .map_err(|error| error.error)?;
        }
        tracks.as_mut_slice().sort_unstable_by_key(PesTrack::pid);
        Ok(PesTracks::Multiple(tracks))
    }

    pub(crate) fn observer_for(
        &self,
        spec: PidSelection,
        map: &ProgramMap,
        configuration_epoch: u64,
    ) -> Result<(DemuxObserver, Carriage)> {
        let mismatch = Error::Unsupported("selected PES carriage disagrees with the active PMT");
        let stream = map
            .streams
            .as_slice()
            .iter()
            .find(|entry| entry.pid == spec.pid)
            .ok_or(mismatch)?;
        let mut scope = DemuxScope {
            program: self.program,
            pid: spec.pid,
            stream_type: stream.stream_type,
            stream_id: stream_id::PRIVATE_STREAM_1,
            entity: self.input.source(),
            epochs: ts::SourceEpochs {
                clock: self.stream.source_epochs().clock,
                configuration: configuration_epoch,
            },
        };
        let private_audio = PrivateAudioDescriptors {
            program: map.program_descriptors.as_ref(),
            elementary: stream.descriptor_bytes.as_ref(),
        };
        let hls_audio = self.delivery == Delivery::MediaFeed;
        let carriage = match spec.carriage {
            Carriage::AudioFromPmt => match stream.stream_type {
                stream_type::AAC_ADTS => Carriage::Registered(RegisteredCarriage::AacAdts),
                stream_type::AAC_LATM => Carriage::Registered(RegisteredCarriage::AacLatm),
                stream_type::PES_PRIVATE_DATA
                    if !hls_audio
                        && !matches!(map.descriptor_namespace, DescriptorNamespace::Dvb(_)) =>
                {
                    return Err(Error::Unsupported(
                        "private PMT audio needs an explicit DVB namespace",
                    ));
                }
                stream_type::PES_PRIVATE_DATA | stream_type::ATSC_AC3 | stream_type::ATSC_EAC3 => {
                    Carriage::PrivateAudio(if hls_audio {
                        PrivateAudioCarriage::from_hls_pmt(
                            stream.stream_type,
                            private_audio,
                            self.limits,
                        )?
                    } else {
                        PrivateAudioCarriage::from_observed_pmt(
                            stream.stream_type,
                            private_audio,
                            self.limits,
                        )?
                    })
                }
                _ => return Err(mismatch),
            },
            fixed => fixed,
        };
        let pool = self.input.budget().clone();
        let mut observer = match carriage {
            Carriage::Registered(carriage) => {
                if stream.stream_type != carriage.stream_type() {
                    return Err(mismatch);
                }
                scope.stream_id = carriage.stream_id();
                DemuxObserver::new_registered(scope, carriage, pool, self.limits)
            }
            Carriage::PrivateAudio(carriage) if hls_audio => DemuxObserver::new_hls_private_audio(
                scope,
                carriage,
                private_audio,
                pool,
                self.limits,
            ),
            Carriage::PrivateAudio(carriage) => {
                DemuxObserver::new_private_audio(scope, carriage, private_audio, pool, self.limits)
            }
            Carriage::DvbSubtitles => DemuxObserver::new_dvb_subtitles(
                scope,
                map.descriptor_namespace,
                DvbSubtitlesDescriptors {
                    program: map.program_descriptors.as_ref(),
                    elementary: stream.descriptor_bytes.as_ref(),
                },
                pool,
                self.limits,
            ),
            Carriage::AudioFromPmt => Err(mismatch),
        }?;
        if self.delivery == Delivery::Progressive && matches!(carriage, Carriage::Registered(_)) {
            observer.enable_compact_payloads()?;
        }
        Ok((observer, carriage))
    }
}
