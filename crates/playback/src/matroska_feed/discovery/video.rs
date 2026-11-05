//! Packet-clock readiness for startup with one active AVC or HEVC chain.
use super::super::video_clock::{avc_chain, picture_nal};
use crate::{Output, Result, raw_video::hevc_syntax as hevc};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon::{matroska::Track, typed::Packet};
use triergon_core::{Error, WorkBudget};
use triergon_framing::{
    AvcConfigLimits, AvcConfigSource, AvcConfigViewParser, BuildStep, Codec, HvcConfigLimits,
    HvcConfigSource, HvcConfigViewParser, LengthSize,
};

#[derive(Clone, Copy)]
enum TrackProbe {
    Unseen,
    Unresolved,
    Watching {
        chain: Chain,
        pictures: u8,
        last_pts: i64,
    },
}

#[derive(Clone, Copy)]
struct Chain {
    codec: Codec,
    length: LengthSize,
    pps: u8,
    minimum_pictures: u8,
    reorders: bool,
}

#[derive(Default)]
struct Sample {
    cursor: usize,
    nals: u8,
    picture: bool,
}

#[allow(clippy::large_enum_variant, reason = "one bounded setup at a time")]
enum Setup {
    Avc(AvcConfigViewParser),
    Hevc(HvcConfigViewParser),
}

pub(super) struct VideoProbe {
    tracks: [TrackProbe; 64],
    setup: Option<Setup>,
    sample: Sample,
}

impl Default for VideoProbe {
    fn default() -> Self {
        Self {
            tracks: [TrackProbe::Unseen; 64],
            setup: None,
            sample: Sample::default(),
        }
    }
}

impl VideoProbe {
    pub(super) fn poll(
        &mut self,
        output: &Output,
        packet: &Packet,
        track: &Track,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<Option<bool>> {
        let index = packet.packet.track;
        if matches!(self.tracks[index], TrackProbe::Unseen) {
            if self.setup.is_none() {
                if !work.try_charge(1) {
                    return Ok(None);
                }
                self.setup = match Setup::new(output, packet, track) {
                    Ok(setup) => setup,
                    Err(error) if propagate(error) => return Err(error.into()),
                    Err(_) => None,
                };
            }
            let chain = if let Some(setup) = &mut self.setup {
                match setup.poll(work, cancel) {
                    Ok(BuildStep::Yield) => return Ok(None),
                    Ok(BuildStep::Ready(chain)) => Some(chain),
                    Ok(BuildStep::Complete) => None,
                    Err(error) if propagate(error) => return Err(error.into()),
                    Err(_) => None,
                }
            } else {
                None
            };
            self.setup = None;
            self.tracks[index] =
                chain.map_or(TrackProbe::Unresolved, |chain| TrackProbe::Watching {
                    chain,
                    pictures: 0,
                    last_pts: 0,
                });
        }
        let TrackProbe::Watching {
            chain,
            pictures,
            last_pts,
        } = self.tracks[index]
        else {
            return Ok(Some(false));
        };
        let pts = packet
            .packet
            .timing
            .ffmpeg9()?
            .pts
            .and_then(|pts| i64::try_from(pts.numerator()).ok());
        let Some(pts) = pts.filter(|pts| {
            packet.packet.timing.frames == 1 && (pictures == 0 || chain.reorders || *pts > last_pts)
        }) else {
            self.tracks[index] = TrackProbe::Unresolved;
            self.sample = Sample::default();
            return Ok(Some(false));
        };
        let ready = match self.sample.poll(
            packet.packet.data.as_ref(),
            chain,
            pictures == 0,
            work,
            cancel,
        ) {
            Ok(None) => return Ok(None),
            Ok(Some(true)) => {
                self.tracks[index] = TrackProbe::Watching {
                    chain,
                    pictures: pictures + 1,
                    last_pts: pts,
                };
                pictures + 1 >= chain.minimum_pictures
            }
            Ok(Some(false)) => {
                self.tracks[index] = TrackProbe::Unresolved;
                false
            }
            Err(error) if propagate(error) => return Err(error.into()),
            Err(_) => {
                self.tracks[index] = TrackProbe::Unresolved;
                false
            }
        };
        self.sample = Sample::default();
        Ok(Some(ready))
    }
}

fn propagate(error: Error) -> bool {
    matches!(
        error,
        Error::Cancelled | Error::Allocation | Error::Backpressure { .. } | Error::ForeignPool
    )
}

impl Setup {
    fn new(output: &Output, packet: &Packet, track: &Track) -> triergon_core::Result<Option<Self>> {
        // The native Matroska probe has both frame rates at open for this range.
        // Keep away from the rounded endpoints of its rational reduction.
        if !track
            .default_duration_ns
            .is_some_and(|ns| (1_001_000..=199_000_000).contains(&ns))
        {
            return Ok(None);
        }
        let codec = match track.codec.as_str() {
            "V_MPEG4/ISO/AVC" => Codec::Avc,
            "V_MPEGH/ISO/HEVC" => Codec::Hevc,
            _ => return Ok(None),
        };
        let Some(raw) = packet
            .configuration
            .initialization_bytes(packet.packet.track)
        else {
            return Ok(None);
        };
        let pool = output.pool();
        let source = output.source_id();
        let generation = packet.configuration.generation();
        Ok(Some(match codec {
            Codec::Avc => Self::Avc(AvcConfigViewParser::new(
                pool,
                AvcConfigSource {
                    source,
                    generation,
                    offset: 0,
                },
                raw,
                AvcConfigLimits {
                    maximum_record_bytes: 8192,
                    maximum_nals: 2,
                    maximum_nal_bytes: 4096,
                },
            )?),
            Codec::Hevc => Self::Hevc(HvcConfigViewParser::new(
                pool,
                HvcConfigSource {
                    source,
                    generation,
                    offset: 0,
                },
                raw,
                HvcConfigLimits {
                    maximum_record_bytes: 8192,
                    maximum_arrays: 16,
                    maximum_nals: 16,
                    maximum_nal_bytes: 4096,
                },
            )?),
        }))
    }

    fn poll(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<BuildStep<Chain>> {
        match self {
            Self::Avc(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(view) => {
                    let (pps, reorder) = avc_chain(view.nals().map(|nal| nal.source_bytes()))?;
                    // Without delay, seven progressive pictures and a further
                    // timed packet establish the first decode timestamp.
                    let minimum_pictures = if reorder == 0 { 8 } else { reorder + 2 };
                    Ok(BuildStep::Ready(Chain {
                        codec: Codec::Avc,
                        length: view.sample_length_size(),
                        pps,
                        minimum_pictures,
                        reorders: reorder != 0,
                    }))
                }
                BuildStep::Yield => Ok(BuildStep::Yield),
                BuildStep::Complete => Ok(BuildStep::Complete),
            },
            Self::Hevc(parser) => match parser.step(work, cancel)? {
                BuildStep::Ready(view) => {
                    let chain = hevc_chain(
                        view.length_size(),
                        view.nals().map(|nal| nal.source_bytes()),
                    )?;
                    Ok(BuildStep::Ready(chain))
                }
                BuildStep::Yield => Ok(BuildStep::Yield),
                BuildStep::Complete => Ok(BuildStep::Complete),
            },
        }
    }
}

fn hevc_chain<'a>(
    length: LengthSize,
    nals: impl Iterator<Item = &'a [u8]>,
) -> triergon_core::Result<Chain> {
    let chain = hevc::startup_chain(nals)?;
    Ok(Chain {
        codec: Codec::Hevc,
        length,
        pps: chain.pps_id,
        minimum_pictures: chain.reorder + 1,
        reorders: chain.reorder != 0,
    })
}

impl Sample {
    fn poll(
        &mut self,
        raw: &[u8],
        chain: Chain,
        first_packet: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<Option<bool>> {
        while self.cursor != raw.len() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.nals == 128 {
                return Ok(Some(false));
            }
            if !work.try_charge(1) {
                return Ok(None);
            }
            let (nal, consumed) = split_nal(&raw[self.cursor..], chain.length)?;
            if let Some((first, random_access, pps)) =
                picture_nal(chain.codec, nal, chain.reorders)?
            {
                if pps != chain.pps || first == self.picture || (first_packet && !random_access) {
                    return Ok(Some(false));
                }
                self.picture = true;
            }
            self.cursor += consumed;
            self.nals += 1;
        }
        Ok(Some(self.picture))
    }
}

fn split_nal(raw: &[u8], width: LengthSize) -> triergon_core::Result<(&[u8], usize)> {
    let width = usize::from(width.bytes());
    let (prefix, rest) = raw.split_at_checked(width).ok_or(Error::Truncated)?;
    let size = prefix
        .iter()
        .fold(0, |size, &byte| (size << 8) | usize::from(byte));
    let nal = rest.get(..size).ok_or(Error::Truncated)?;
    Ok((nal, width + size))
}
