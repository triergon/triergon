//! The HEVC parameter-set chain a feed starts from, over the prefix readers of
//! `triergon_framing::hevc`.

use super::Sps;
use triergon_core::{Error, Result};
pub(super) use triergon_framing::hevc::{
    read_pps_ids, read_slice_prefix, read_sps_timing, read_vps_id,
};
use triergon_framing::hevc::{check_parameter_sets, read_sps_prefix, read_sps_reorder};
use triergon_framing::nal::{Codec, NalHeader};

pub(super) fn sps(payload: &[u8]) -> Result<(u8, Sps)> {
    let sps = read_sps_prefix(payload)?;
    Ok((
        sps.id,
        Sps {
            vps: Some(sps.vps_id),
            width: sps.width,
            height: sps.height,
            avc_timing: None,
        },
    ))
}

/// One active single-layer VPS, SPS and PPS, as NAL payloads after their headers.
#[derive(Debug)]
pub struct StartupChain<'a> {
    /// The VPS payload.
    pub vps: &'a [u8],
    /// The SPS payload.
    pub sps: &'a [u8],
    /// The PPS payload.
    pub pps: &'a [u8],
    /// The PPS ID.
    pub pps_id: u8,
    /// The SPS's highest-sublayer reorder depth.
    pub reorder: u8,
}

/// The chain of a configuration record's parameter-set NALs: one distinct set
/// of each type (exact repeats allowed, SEI skipped), at most 16 NALs of at
/// most 4,096 bytes; `Unsupported` otherwise.
pub fn startup_chain<'a>(nals: impl Iterator<Item = &'a [u8]>) -> Result<StartupChain<'a>> {
    let mut vps_set = None;
    let mut sps_set = None;
    let mut pps_set = None;
    let mut picture_parameter_set_ids = None;
    let mut repeats = false;
    for (index, nal) in nals.enumerate() {
        if index == 16 || nal.len() > 4096 {
            return Err(Error::Unsupported("HEVC clock parameter-set extent"));
        }
        let NalHeader::Hevc {
            nal_unit_type,
            layer_id: 0,
            temporal_id_plus_one: 1,
        } = NalHeader::parse(Codec::Hevc, nal)?
        else {
            return Err(Error::Unsupported("HEVC discovery NAL layer"));
        };
        let payload = &nal[2..];
        let parameter_set = match nal_unit_type {
            32 => &mut vps_set,
            33 => &mut sps_set,
            34 => &mut pps_set,
            39 | 40 => continue,
            _ => return Err(Error::Unsupported("HEVC discovery parameter-set chain")),
        };
        // An exact repeat identifies the same active chain (H.265 §7.4.2.4.2).
        match *parameter_set {
            None => {
                if nal_unit_type == 34 {
                    picture_parameter_set_ids = Some(read_pps_ids(payload)?);
                }
                *parameter_set = Some(payload);
            }
            Some(previous) if previous == payload => repeats = true,
            Some(_) => return Err(Error::Unsupported("HEVC discovery parameter-set chain")),
        }
    }
    let vps_set = vps_set.ok_or(Error::Invalid("HEVC discovery VPS"))?;
    let sps_set = sps_set.ok_or(Error::Invalid("HEVC discovery SPS"))?;
    let (sps_id, sps_vps, reorder) = read_sps_reorder(sps_set)?;
    let (pps_set, (pps_id, pps_sps)) = pps_set
        .zip(picture_parameter_set_ids)
        .ok_or(Error::Invalid("HEVC discovery PPS"))?;
    if read_vps_id(vps_set)? != sps_vps || sps_id != pps_sps {
        return Err(Error::Invalid("HEVC discovery parameter-set reference"));
    }
    if repeats {
        check_parameter_sets(vps_set, sps_set, pps_set)?;
    }
    Ok(StartupChain {
        vps: vps_set,
        sps: sps_set,
        pps: pps_set,
        pps_id,
        reorder,
    })
}

/// Identify the active chain's three supported parameter-set headers.
pub fn parameter_set_payload(nal: &[u8]) -> Option<(usize, &[u8])> {
    let (header, payload) = nal.split_first_chunk::<2>()?;
    // The chain admits layer zero and temporal ID zero.
    let [kind @ (0x40 | 0x42 | 0x44), 1] = *header else {
        return None;
    };
    Some((usize::from((kind - 0x40) >> 1), payload))
}

/// What a startup clock needs of one NAL of a sample.
#[derive(Debug)]
pub enum StartupNal {
    /// A slice of a picture.
    Picture {
        /// Whether it is the picture's first slice.
        first: bool,
        /// Whether the picture is an IRAP picture.
        random_access: bool,
        /// The slice's PPS ID.
        pps: u8,
    },
    /// A parameter set, AUD, end of sequence or SEI NAL.
    Supplemental,
}

/// Classifies a base-layer NAL of a sample; `Unsupported` for other NALs.
pub fn startup_nal(nal: &[u8], header: NalHeader) -> Result<StartupNal> {
    match header {
        NalHeader::Hevc {
            nal_unit_type: kind @ (0..=9 | 16..=21),
            layer_id: 0,
            temporal_id_plus_one: 1,
        } => {
            let (first, pps) = read_slice_prefix(&nal[2..], kind)?;
            Ok(StartupNal::Picture {
                first,
                random_access: kind >= 16,
                pps,
            })
        }
        NalHeader::Hevc {
            nal_unit_type: 35 | 38..=40,
            layer_id: 0,
            ..
        } => Ok(StartupNal::Supplemental),
        _ => Err(Error::Unsupported("HEVC clock sample NAL")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_accepts_only_identical_parameter_set_repeats() {
        // The three configuration NALs from hevc-repeated-parameter-sets.mkv.
        let vps = [
            0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x1e, 0xba, 0x02, 0x40,
        ];
        let sps = [
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00,
            0x00, 0x03, 0x00, 0x1e, 0xa0, 0x20, 0x83, 0x16, 0x5b, 0xa4, 0xa4, 0xc2, 0xf0, 0x16,
            0x80, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x0c, 0x04,
        ];
        let pps = [0x44, 0x01, 0xc0, 0x73, 0xc0, 0x89];
        let nals = [vps.as_slice(), sps.as_slice(), pps.as_slice()];
        let single = startup_chain(nals.into_iter()).unwrap();
        let repeated = startup_chain(nals.into_iter().flat_map(|nal| [nal, nal])).unwrap();
        assert_eq!(repeated.vps, single.vps);
        assert_eq!(repeated.sps, single.sps);
        assert_eq!(repeated.pps_id, single.pps_id);
        assert_eq!(repeated.reorder, single.reorder);

        for (index, nal) in nals.into_iter().enumerate() {
            let mut truncated = nals;
            truncated[index] = &nal[..nal.len() - 1];
            let repeated = truncated.into_iter().flat_map(|nal| [nal, nal]);
            assert!(matches!(startup_chain(repeated), Err(Error::Truncated)));
        }

        for nal in nals {
            let mut changed = nal.to_vec();
            *changed.last_mut().unwrap() ^= 1;
            let distinct = nals.into_iter().chain([changed.as_slice()]);
            assert!(matches!(
                startup_chain(distinct),
                Err(Error::Unsupported("HEVC discovery parameter-set chain"))
            ));
        }

        let malformed_pps = [0x44, 0x01, 0x00];
        assert!(matches!(
            startup_chain([vps.as_slice(), malformed_pps.as_slice()].into_iter()),
            Err(Error::Truncated)
        ));
    }
}
