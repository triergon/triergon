//! A self-contained picture, without saved parameter tables or decoder history.
//!
//! H.264 §§7.3.2.1–2, 7.3.3 and 7.4.1.2.4; H.265 §§7.3.2.1–3,
//! 7.3.6.1 and 7.4.2.4.4. Only one progressive AVC IDR picture or a base-layer
//! HEVC IDR_N_LP slice is admitted, preceded by its referenced parameter sets.
//! The remaining parameter/slice bodies stay opaque, as in the ordinary reader;
//! this proves available setup and independence, not successful decoding.
use triergon_bitstream::{BitSource, Rbsp};
use triergon_framing::{
    avc::{self, AvcReading, AvcSliceDepth},
    hevc,
    nal::{Codec, NalHeader, RawAccessUnit, skip_hevc_profile_tier_level},
};

pub(super) fn nals(unit: &RawAccessUnit) -> Option<usize> {
    if !unit.has_vcl {
        return None;
    }
    let mut nals = unit.nals();
    let aud = match unit.codec {
        Codec::Avc => avc::AUD,
        Codec::Hevc => hevc::AUD,
    };
    if nals.first()?.facts().header.nal_unit_type() == aud {
        nals = &nals[1..];
    }
    match unit.codec {
        Codec::Avc => {
            let [sps, pps, slices @ ..] = nals else {
                return None;
            };
            if slices.is_empty() || slices.len() > 8 {
                return None;
            }
            for (nal, kind) in [(sps, avc::SPS), (pps, avc::PPS)] {
                if !matches!(nal.facts().header, NalHeader::Avc {
                    nal_unit_type, nal_ref_idc: 1..=3,
                } if nal_unit_type == kind)
                {
                    return None;
                }
            }
            let sps =
                avc::read_sps(&sps.bytes()[1..], AvcReading::Strict, false, |_, _| {}).ok()?;
            let pps = avc::read_pps(&pps.bytes()[1..], AvcReading::Strict).ok()?;
            if !sps.frame_mbs_only
                || sps.separate_colour_plane
                || pps.sps_id != sps.id
                || pps.redundant_pic_cnt_present
            {
                return None;
            }
            let mut previous_mb = None;
            let mut picture = None;
            for slice in slices {
                if !matches!(
                    slice.facts().header,
                    NalHeader::Avc {
                        nal_unit_type: avc::IDR,
                        nal_ref_idc: 1..=3,
                    }
                ) {
                    return None;
                }
                let payload = &slice.bytes()[1..];
                let first_mb = Rbsp::new(payload).exp_golomb(32).ok()?;
                let header = avc::read_slice_header(
                    payload,
                    true,
                    true,
                    &sps,
                    &pps,
                    AvcReading::Strict,
                    AvcSliceDepth::Order,
                )
                .ok()?;
                if first_mb >= u64::from(sps.frame_mbs)
                    || previous_mb.map_or(first_mb != 0, |old| first_mb <= old)
                    || picture.is_some_and(|old| header != old)
                    || header.pps_id != pps.id
                    || header.slice_type != 2
                    || header.frame_num != 0
                {
                    return None;
                }
                previous_mb = Some(first_mb);
                picture = Some(header);
            }
        }
        Codec::Hevc => {
            let [vps, sps, pps, slice] = nals else {
                return None;
            };
            // One layer and one temporal sublayer; no RADL/RASL or CRA inference.
            for nal in unit.nals() {
                if !matches!(
                    nal.facts().header,
                    NalHeader::Hevc {
                        layer_id: 0,
                        temporal_id_plus_one: 1,
                        ..
                    }
                ) {
                    return None;
                }
            }
            for (nal, kind) in [
                (vps, hevc::VPS),
                (sps, hevc::SPS),
                (pps, hevc::PPS),
                (slice, hevc::IDR_N_LP),
            ] {
                if nal.facts().header.nal_unit_type() != kind {
                    return None;
                }
            }
            let mut vps = Rbsp::new(&vps.bytes()[2..]);
            let vps_id = vps.read(4).ok()?;
            if vps.read(2).ok()? != 3 // base-layer internal and available
                || vps.read(6).ok()? != 0 // vps_max_layers_minus1
                || vps.read(4).ok()? != 1 // zero sublayers, temporal nesting
                || vps.read(16).ok()? != 0xffff
            {
                return None;
            }
            let mut sps = Rbsp::new(&sps.bytes()[2..]);
            if sps.read(4).ok()? != vps_id
                || sps.read(4).ok()? != 1
                || skip_hevc_profile_tier_level(&mut sps, 0).ok()? != 0
            {
                return None;
            }
            let sps_id = sps.exp_golomb(4).ok()?;
            let mut pps = Rbsp::new(&pps.bytes()[2..]);
            let pps_id = pps.exp_golomb(6).ok()?;
            if sps_id > 15 || pps_id > 63 || pps.exp_golomb(4).ok()? != sps_id {
                return None;
            }
            let mut slice = Rbsp::new(&slice.bytes()[2..]);
            if slice.read(1).ok()? != 1 {
                // first_slice_segment_in_pic_flag
                return None;
            }
            slice.read(1).ok()?; // no_output_of_prior_pics_flag, unchanged
            if slice.exp_golomb(6).ok()? != pps_id {
                return None;
            }
        }
    }
    Some(unit.nals().len())
}
