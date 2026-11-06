//! Packet clock projection shared by the selected AVC and HEVC feeds.
use super::{Association, FeedCodec, MatroskaFeedPacket, MatroskaFeedPayload, MatroskaTrackFeed};
use crate::{Result, hls_avc::syntax as avc, raw_video::hevc_syntax as hevc};
use triergon_core::Error;
use triergon_framing::{AvcAnnexBConfig, Codec, NalHeader};
use triergon_matroska::typed::Packet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatroskaVideoTiming {
    pub pts_ns: Option<i64>,
    pub dts_ns: Option<i64>,
    pub duration_ns: Option<u64>,
}

/// One timing projection for an ordered progressive AVC feed, beginning at
/// an admitted IDR. Observe each checked sample once.
pub struct MatroskaAvcClock<'feed> {
    association: &'feed Association,
    config: &'feed AvcAnnexBConfig,
    clock: DecodeClock,
}

impl MatroskaTrackFeed {
    /// Bind a native packet clock to this feed's single AVC parameter-set
    /// chain. An AAC feed returns `None`.
    pub fn native_avc_clock(&self) -> Result<Option<MatroskaAvcClock<'_>>> {
        let FeedCodec::Avc(config) = &self.codec else {
            return Ok(None);
        };
        let (pps, reorder) = avc_chain(config.record().nals().map(|nal| nal.source_bytes()))?;
        Ok(Some(MatroskaAvcClock {
            association: &self.association,
            config,
            clock: DecodeClock::new(Codec::Avc, pps, reorder),
        }))
    }
}

impl MatroskaAvcClock<'_> {
    /// Project the packet's native presentation and decode clocks. Source
    /// timing remains available on the supplied packet.
    pub fn observe(&mut self, packet: &MatroskaFeedPacket<'_>) -> Result<MatroskaVideoTiming> {
        let MatroskaFeedPayload::Avc(unit) = &packet.payload else {
            return Err(Error::Invalid("native AVC clock packet codec"));
        };
        if !self.association.matches(packet.source)
            || !std::ptr::eq(self.config, unit.configuration())
        {
            return Err(Error::Invalid("native AVC clock feed association"));
        }
        self.clock
            .observe(packet.source, unit.source_nals(), |_| None)
    }
}

pub(super) struct DecodeClock {
    codec: Codec,
    pps: u8,
    reorder: usize,
    pts: [Option<i64>; 17],
    started: bool,
    valid: bool,
}

impl DecodeClock {
    pub(super) fn new(codec: Codec, pps: u8, reorder: u8) -> Self {
        Self {
            codec,
            pps,
            reorder: usize::from(reorder),
            pts: [None; 17],
            started: false,
            valid: true,
        }
    }

    pub(super) fn observe<'a>(
        &mut self,
        source: &Packet,
        nals: impl Iterator<Item = &'a [u8]>,
        fallback_duration: impl FnOnce(u64) -> Option<u64>,
    ) -> Result<MatroskaVideoTiming> {
        if !self.valid {
            return Err(Error::Invalid("native video clock continuity"));
        }
        let timing = source.packet.timing;
        if timing.frames != 1 {
            self.valid = false;
            return Err(Error::Unsupported("native video clock laced sample"));
        }
        if let Err(error) = self.validate_picture(nals) {
            self.valid = false;
            return Err(error);
        }
        let native = timing.ffmpeg9()?;
        let pts_ns = native.pts.map(|pts| pts.rounded_ns()).transpose()?;
        let duration_ns = match native.duration {
            Some(duration) => Some(duration.rounded_duration_ns()?),
            None => fallback_duration(timing.scale_ns),
        };
        self.started = true;
        let dts_ns = pts_ns.and_then(|pts| self.push_pts(pts));
        Ok(MatroskaVideoTiming {
            pts_ns,
            dts_ns,
            duration_ns,
        })
    }

    fn validate_picture<'a>(
        &self,
        nals: impl Iterator<Item = &'a [u8]>,
    ) -> triergon_core::Result<()> {
        let mut picture = false;
        for (index, nal) in nals.enumerate() {
            if index == 128 {
                return Err(Error::Limit {
                    requested: index + 1,
                    maximum: 128,
                });
            }
            let Some((first, random_access, pps)) =
                picture_nal(self.codec, nal, self.reorder != 0)?
            else {
                continue;
            };
            if pps != self.pps || first == picture || (!self.started && !random_access) {
                return Err(Error::Unsupported("native video clock picture continuity"));
            }
            picture = true;
        }
        if !picture {
            return Err(Error::Unsupported(
                "native video clock sample has no picture",
            ));
        }
        Ok(())
    }

    fn push_pts(&mut self, pts: i64) -> Option<i64> {
        self.pts[0] = Some(pts);
        for index in 0..self.reorder {
            if self.pts[index] <= self.pts[index + 1] {
                break;
            }
            self.pts.swap(index, index + 1);
        }
        self.pts[0]
    }
}

pub(super) fn avc_chain<'a>(
    nals: impl Iterator<Item = &'a [u8]>,
) -> triergon_core::Result<(u8, u8)> {
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        if nal.len() > 4096 {
            return Err(Error::Unsupported("AVC clock parameter-set extent"));
        }
        match NalHeader::parse(Codec::Avc, nal)? {
            NalHeader::Avc {
                nal_unit_type: 7, ..
            } if sps.is_none() => sps = Some(avc::startup_sps(&nal[1..])?),
            NalHeader::Avc {
                nal_unit_type: 8, ..
            } if pps.is_none() => pps = Some(avc::pps(&nal[1..])?),
            _ => return Err(Error::Unsupported("AVC clock parameter-set chain")),
        }
    }
    let (pps, reference) = pps.ok_or(Error::Invalid("AVC clock PPS"))?;
    let (sps, reorder) = sps.ok_or(Error::Invalid("AVC clock SPS"))?;
    if sps != reference {
        return Err(Error::Invalid("AVC clock SPS reference"));
    }
    Ok((pps, reorder))
}

pub(super) fn picture_nal(
    codec: Codec,
    nal: &[u8],
    reorders: bool,
) -> triergon_core::Result<Option<(bool, bool, u8)>> {
    match NalHeader::parse(codec, nal)? {
        NalHeader::Avc {
            nal_unit_type: kind @ (1 | 5),
            ..
        } => {
            let (address, slice_type, pps) = avc::slice_picture(&nal[1..])?;
            if !matches!(slice_type, 0 | 2) && !(reorders && slice_type == 1) {
                return Err(Error::Unsupported("AVC clock slice type"));
            }
            Ok(Some((address == 0, kind == 5, pps)))
        }
        NalHeader::Avc {
            nal_unit_type: 6 | 9 | 12,
            ..
        } => Ok(None),
        header @ NalHeader::Hevc { .. } => match hevc::startup_nal(nal, header)? {
            hevc::StartupNal::Picture {
                first,
                random_access,
                pps,
            } => Ok(Some((first, random_access, pps))),
            hevc::StartupNal::Supplemental => Ok(None),
        },
        _ => Err(Error::Unsupported("AVC clock sample NAL")),
    }
}
