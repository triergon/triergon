//! H.264 parameter sets and slice headers (H.264 08/2024 §§7.3.2.1.1,
//! 7.3.2.2, 7.3.3, E.1.1) as far as primary-picture identity (§7.4.1.2.4) and
//! picture output (§8.2.1 order counts, §8.2.5.4 operation 5, the VUI's timing
//! and bitstream restriction) need them, leaving the rest of each RBSP opaque.
//! [`read_sps`], [`read_pps`] and [`read_slice_header`] serve any
//! caller; `AvcPictures` compares pictures for access-unit boundaries.
use crate::nal::{Codec, NalHeader, SyntaxError, skip_avc_scaling_matrix, skip_vui_to_timing};
use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AvcPictureFact {
    pub vcl: bool,
    pub new_picture: bool,
    pub random_access: bool,
}

/// How a reader treats emulation prevention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcReading {
    /// `00 00 00`, `00 00 01` and `00 00 02` inside an RBSP are refused.
    Strict,
    /// Those bytes are read as data, as decoders tolerate them.
    Lenient,
}

/// How a sequence codes picture order counts (`pic_order_cnt_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcPocSyntax {
    /// Type 0 with `pic_order_cnt_lsb` of `bits` bits.
    Lsb { bits: u8 },
    /// Type 1; the cycle's `offset_for_ref_frame` values go to the caller.
    Cycle {
        always_zero: bool,
        offset_for_non_ref_pic: i32,
        offset_for_top_to_bottom_field: i32,
        len: u8,
    },
    /// Type 2.
    FrameNum,
}

/// The VUI fields of picture output (E.1.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AvcVui {
    /// `num_units_in_tick` and `time_scale`, both above 0.
    pub timing: Option<(u32, u32)>,
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

/// The fields of a sequence parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcSps {
    pub id: u8,
    pub profile_idc: u8,
    pub constraint_set3: bool,
    pub level_idc: u8,
    /// ChromaArrayType: `chroma_format_idc`, or 0 with separate colour planes.
    pub chroma_array_type: u8,
    pub separate_colour_plane: bool,
    /// `log2_max_frame_num_minus4 + 4`.
    pub frame_num_bits: u8,
    pub poc: AvcPocSyntax,
    pub max_num_ref_frames: u32,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub gaps_in_frame_num_allowed: bool,
    pub frame_mbs_only: bool,
    /// PicWidthInMbs * FrameHeightInMbs.
    pub frame_mbs: u32,
    /// Default when the VUI is absent or was not read.
    pub vui: AvcVui,
}

/// The fields of a picture parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcPps {
    pub id: u8,
    pub sps_id: u8,
    pub bottom_field_pic_order_in_frame_present: bool,
    /// `num_ref_idx_l0_default_active_minus1 + 1` and the same for list 1.
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub redundant_pic_cnt_present: bool,
}

/// How far a slice header is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AvcSliceDepth {
    /// Up to `redundant_pic_cnt`: picture identity and order count fields.
    Order,
    /// Through `dec_ref_pic_marking( )`.
    Marking,
}

/// The fields of a slice header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcSliceHeader {
    /// `slice_type % 5`: 0 P, 1 B, 2 I, 3 SP, 4 SI.
    pub slice_type: u8,
    pub pps_id: u8,
    pub frame_num: u16,
    /// `bottom_field_flag` of a field picture.
    pub bottom_field: Option<bool>,
    pub idr_pic_id: Option<u16>,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    /// Read with [`AvcSliceDepth::Marking`]: `memory_management_control_operation`
    /// 5 is present, and `no_output_of_prior_pics_flag` of an IDR picture.
    pub mmco5: bool,
    pub no_output_of_prior_pics: bool,
}

impl AvcSliceHeader {
    /// An I or SI slice.
    #[must_use]
    pub fn intra(&self) -> bool {
        matches!(self.slice_type, 2 | 4)
    }

    /// A B slice.
    #[must_use]
    pub fn bipredictive(&self) -> bool {
        self.slice_type == 1
    }
}

pub(crate) struct AvcPictures {
    sps: [Option<AvcSps>; 32],
    pps: [Option<AvcPps>; 256],
    previous: Option<Picture>,
}

impl Default for AvcPictures {
    fn default() -> Self {
        Self {
            sps: [None; 32],
            pps: [None; 256],
            previous: None,
        }
    }
}

impl AvcPictures {
    /// The caller established an AU boundary from a delimiter or leading NAL.
    /// The first VCL in the next AU will start a picture using the same tables.
    pub(crate) fn reset_picture(&mut self) {
        self.previous = None;
    }

    /// Observe one complete, padding-free NAL in decoding order. Only a
    /// successfully inspected primary slice changes the previous VCL identity.
    pub(crate) fn observe(&mut self, nal: &[u8], header: NalHeader) -> Result<AvcPictureFact> {
        let NalHeader::Avc {
            nal_unit_type,
            nal_ref_idc,
        } = header
        else {
            return Err(Error::Invalid("AVC picture requires AVC NAL"));
        };
        if NalHeader::parse(Codec::Avc, nal)? != header {
            return Err(Error::Invalid("AVC picture NAL header mismatch"));
        }
        let payload = &nal[1..];
        match nal_unit_type {
            1 | 5 => {
                let idr = nal_unit_type == 5;
                let picture = self.slice(payload, idr, nal_ref_idc != 0)?;
                let new_picture = self.previous.is_none_or(|prev| picture.differs(prev));
                self.previous = Some(picture);
                Ok(AvcPictureFact {
                    vcl: true,
                    new_picture,
                    random_access: idr,
                })
            }
            2..=4 => Err(Error::Unsupported("AVC slice data partitioning")),
            7 => {
                if payload.first().is_some_and(|profile| {
                    !matches!(profile, 66 | 77 | 88 | 44 | 100 | 110 | 122 | 244)
                }) {
                    return Err(Error::Unsupported("AVC SPS profile for picture comparison"));
                }
                let sps = read_sps(payload, AvcReading::Strict, false, |_, _| {})?;
                self.sps[usize::from(sps.id)] = Some(sps);
                Ok(AvcPictureFact::default())
            }
            8 => {
                let pps = read_pps(payload, AvcReading::Strict)?;
                self.pps[usize::from(pps.id)] = Some(pps);
                Ok(AvcPictureFact::default())
            }
            13..=16 | 19..=21 => Err(Error::Unsupported("AVC extension NAL")),
            _ => Ok(AvcPictureFact::default()),
        }
    }

    fn slice(&self, payload: &[u8], idr: bool, reference: bool) -> Result<Picture> {
        let pps_id = slice_pps_id(payload, AvcReading::Strict)?;
        let pps = self.pps[usize::from(pps_id)].ok_or(Error::Invalid("AVC slice has no PPS"))?;
        let sps =
            self.sps[usize::from(pps.sps_id)].ok_or(Error::Invalid("AVC slice has no SPS"))?;
        let header = read_slice_header(
            payload,
            reference,
            idr,
            &sps,
            &pps,
            AvcReading::Strict,
            AvcSliceDepth::Order,
        )?;
        if header.redundant_pic_cnt != 0 {
            return Err(Error::Unsupported("AVC redundant coded picture"));
        }
        let order = match sps.poc {
            AvcPocSyntax::Lsb { .. } => PictureOrder::Lsb {
                value: header.pic_order_cnt_lsb,
                bottom_delta: header.delta_pic_order_cnt_bottom,
            },
            AvcPocSyntax::Cycle { .. } => PictureOrder::Delta {
                first: header.delta_pic_order_cnt[0],
                second: header.delta_pic_order_cnt[1],
            },
            AvcPocSyntax::FrameNum => PictureOrder::Inferred,
        };
        Ok(Picture {
            frame_num: header.frame_num,
            pps_id,
            field: match header.bottom_field {
                None => Field::Frame,
                Some(false) => Field::Top,
                Some(true) => Field::Bottom,
            },
            reference,
            idr_pic_id: header.idr_pic_id,
            order,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Frame,
    Top,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PictureOrder {
    Lsb { value: u32, bottom_delta: i32 },
    Delta { first: i32, second: i32 },
    Inferred,
}

#[derive(Clone, Copy)]
struct Picture {
    frame_num: u16,
    pps_id: u8,
    field: Field,
    reference: bool,
    idr_pic_id: Option<u16>,
    order: PictureOrder,
}

impl Picture {
    fn differs(self, other: Self) -> bool {
        self.frame_num != other.frame_num
            || self.pps_id != other.pps_id
            || self.field != other.field
            || self.reference != other.reference
            || self.idr_pic_id != other.idr_pic_id
            || match (self.order, other.order) {
                (PictureOrder::Lsb { .. }, PictureOrder::Lsb { .. })
                | (PictureOrder::Delta { .. }, PictureOrder::Delta { .. }) => {
                    self.order != other.order
                }
                _ => false,
            }
    }
}

/// Reads an SPS payload (after the NAL header). The cycle of a type 1 order
/// count goes to `cycle` (index, `offset_for_ref_frame`); the VUI is read only
/// when `vui` is set.
///
/// # Errors
///
/// When the SPS is cut short or a field is out of range.
pub fn read_sps(
    payload: &[u8],
    reading: AvcReading,
    vui: bool,
    mut cycle: impl FnMut(usize, i32),
) -> Result<AvcSps> {
    let mut bits = Rbsp::new(payload, reading);
    let profile_idc = bits.fixed(8)? as u8;
    let constraints = bits.fixed(8)?;
    if constraints & 3 != 0 {
        return Err(Error::Invalid("AVC SPS reserved flags"));
    }
    let level_idc = bits.fixed(8)? as u8;
    let id = bits.unsigned(31)? as u8;
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = bits.unsigned(3)?;
        separate_colour_plane = chroma_format_idc == 3 && bits.flag()?;
        bits.unsigned(6)?; // bit_depth_luma_minus8
        bits.unsigned(6)?; // bit_depth_chroma_minus8
        bits.flag()?; // qpprime_y_zero_transform_bypass_flag
        skip_avc_scaling_matrix(&mut bits.0, chroma_format_idc).map_err(|error| match error {
            SyntaxError::Bits(error) => rbsp_error(error),
            SyntaxError::OutOfRange => Error::Invalid("AVC scaling list delta"),
        })?;
    }
    let frame_num_bits = bits.unsigned(12)? as u8 + 4;
    let poc = match bits.unsigned(2)? {
        0 => AvcPocSyntax::Lsb {
            bits: bits.unsigned(12)? as u8 + 4,
        },
        1 => {
            let always_zero = bits.flag()?;
            let offset_for_non_ref_pic = bits.signed()?;
            let offset_for_top_to_bottom_field = bits.signed()?;
            let len = bits.unsigned(255)? as u8;
            for at in 0..usize::from(len) {
                cycle(at, bits.signed()?);
            }
            AvcPocSyntax::Cycle {
                always_zero,
                offset_for_non_ref_pic,
                offset_for_top_to_bottom_field,
                len,
            }
        }
        _ => AvcPocSyntax::FrameNum,
    };
    let max_num_ref_frames = bits.unsigned(u32::MAX)?;
    let gaps_in_frame_num_allowed = bits.flag()?;
    let width = bits.unsigned(u32::MAX)?.saturating_add(1);
    let height_units = bits.unsigned(u32::MAX)?.saturating_add(1);
    let frame_mbs_only = bits.flag()?;
    let mut sps = AvcSps {
        id,
        profile_idc,
        constraint_set3: constraints & 0x10 != 0,
        level_idc,
        chroma_array_type: if separate_colour_plane {
            0
        } else {
            chroma_format_idc as u8
        },
        separate_colour_plane,
        frame_num_bits,
        poc,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        frame_mbs_only,
        frame_mbs: width.saturating_mul(height_units.saturating_mul(2 - u32::from(frame_mbs_only))),
        vui: AvcVui::default(),
    };
    if !vui {
        return Ok(sps);
    }
    if !frame_mbs_only {
        bits.flag()?; // mb_adaptive_frame_field_flag
    }
    bits.flag()?; // direct_8x8_inference_flag
    if bits.flag()? {
        for _ in 0..4 {
            bits.unsigned(u32::MAX)?; // frame cropping offsets
        }
    }
    if bits.flag()? {
        sps.vui = read_vui(&mut bits)?;
    }
    Ok(sps)
}

/// The VUI's timing and bitstream restriction (E.1.1).
fn read_vui(bits: &mut Rbsp<'_>) -> Result<AvcVui> {
    let mut vui = AvcVui::default();
    if skip_vui_to_timing(&mut bits.0, Codec::Avc).map_err(rbsp_error)? {
        let num_units_in_tick = bits.fixed(32)?;
        let time_scale = bits.fixed(32)?;
        bits.flag()?; // fixed_frame_rate_flag
        vui.timing =
            (num_units_in_tick > 0 && time_scale > 0).then_some((num_units_in_tick, time_scale));
    }
    let nal_hrd = bits.flag()?;
    if nal_hrd {
        skip_hrd(bits)?;
    }
    let vcl_hrd = bits.flag()?;
    if vcl_hrd {
        skip_hrd(bits)?;
    }
    if nal_hrd || vcl_hrd {
        bits.flag()?; // low_delay_hrd_flag
    }
    bits.flag()?; // pic_struct_present_flag
    if bits.flag()? {
        bits.flag()?; // motion_vectors_over_pic_boundaries_flag
        for _ in 0..4 {
            bits.unsigned(u32::MAX)?; // byte, bit and motion vector limits
        }
        vui.max_num_reorder_frames = Some(bits.unsigned(u32::MAX)?);
        vui.max_dec_frame_buffering = Some(bits.unsigned(u32::MAX)?);
    }
    Ok(vui)
}

/// hrd_parameters( ) (E.1.2).
fn skip_hrd(bits: &mut Rbsp<'_>) -> Result<()> {
    let count = bits.unsigned(31)? + 1;
    bits.fixed(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..count {
        bits.unsigned(u32::MAX)?;
        bits.unsigned(u32::MAX)?;
        bits.flag()?;
    }
    bits.fixed(20)?; // the four delay and length fields
    Ok(())
}

/// Reads a PPS payload (after the NAL header).
///
/// # Errors
///
/// When the PPS is cut short, a field is out of range, or it has slice groups
/// (flexible macroblock ordering, which is not read).
pub fn read_pps(payload: &[u8], reading: AvcReading) -> Result<AvcPps> {
    let mut bits = Rbsp::new(payload, reading);
    let id = bits.unsigned(255)? as u8;
    let sps_id = bits.unsigned(31)? as u8;
    bits.flag()?; // entropy_coding_mode_flag
    let bottom_field_pic_order_in_frame_present = bits.flag()?;
    if bits.unsigned(7)? != 0 {
        return Err(Error::Unsupported("AVC flexible macroblock ordering"));
    }
    let l0 = bits.unsigned(31)? + 1;
    let l1 = bits.unsigned(31)? + 1;
    let weighted_pred = bits.flag()?;
    let weighted_bipred_idc = bits.fixed(2)? as u8;
    bits.signed()?; // pic_init_qp_minus26
    bits.signed()?; // pic_init_qs_minus26
    bits.signed()?; // chroma_qp_index_offset
    bits.fixed(2)?; // deblocking filter and constrained intra prediction flags
    let redundant_pic_cnt_present = bits.flag()?;
    Ok(AvcPps {
        id,
        sps_id,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_default: [l0, l1],
        weighted_pred,
        weighted_bipred_idc,
        redundant_pic_cnt_present,
    })
}

/// The `pic_parameter_set_id` of a slice payload (after the NAL header).
///
/// # Errors
///
/// When the header is cut short or a field is out of range.
pub fn slice_pps_id(payload: &[u8], reading: AvcReading) -> Result<u8> {
    let mut bits = Rbsp::new(payload, reading);
    bits.unsigned(u32::MAX)?; // first_mb_in_slice
    bits.unsigned(9)?; // slice_type
    Ok(bits.unsigned(255)? as u8)
}

/// Reads a slice header payload (after the NAL header) of a picture with
/// the given `nal_ref_idc != 0` and IDR flag, through `depth`.
///
/// # Errors
///
/// When the header is cut short or a field is out of range.
pub fn read_slice_header(
    payload: &[u8],
    reference: bool,
    idr: bool,
    sps: &AvcSps,
    pps: &AvcPps,
    reading: AvcReading,
    depth: AvcSliceDepth,
) -> Result<AvcSliceHeader> {
    let mut bits = Rbsp::new(payload, reading);
    bits.unsigned(u32::MAX)?; // first_mb_in_slice
    let slice_type = (bits.unsigned(9)? % 5) as u8;
    let pps_id = bits.unsigned(255)? as u8;
    if sps.separate_colour_plane && bits.fixed(2)? > 2 {
        return Err(Error::Invalid("AVC colour_plane_id"));
    }
    let frame_num = bits.fixed(sps.frame_num_bits)? as u16;
    let bottom_field = if !sps.frame_mbs_only && bits.flag()? {
        Some(bits.flag()?)
    } else {
        None
    };
    let idr_pic_id = if idr {
        Some(bits.unsigned(65535)? as u16)
    } else {
        None
    };
    let mut header = AvcSliceHeader {
        slice_type,
        pps_id,
        frame_num,
        bottom_field,
        idr_pic_id,
        pic_order_cnt_lsb: 0,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        redundant_pic_cnt: 0,
        mmco5: false,
        no_output_of_prior_pics: false,
    };
    let has_bottom_delta = pps.bottom_field_pic_order_in_frame_present && bottom_field.is_none();
    match sps.poc {
        AvcPocSyntax::Lsb { bits: width } => {
            header.pic_order_cnt_lsb = bits.fixed(width)?;
            if has_bottom_delta {
                header.delta_pic_order_cnt_bottom = bits.signed()?;
            }
        }
        AvcPocSyntax::Cycle {
            always_zero: false, ..
        } => {
            header.delta_pic_order_cnt[0] = bits.signed()?;
            if has_bottom_delta {
                header.delta_pic_order_cnt[1] = bits.signed()?;
            }
        }
        _ => {}
    }
    if pps.redundant_pic_cnt_present {
        header.redundant_pic_cnt = bits.unsigned(127)?;
    }
    if depth == AvcSliceDepth::Marking {
        read_marking(&mut bits, &mut header, reference, idr, sps, pps)?;
    }
    Ok(header)
}

/// From `direct_spatial_mv_pred_flag` through `dec_ref_pic_marking( )`.
fn read_marking(
    bits: &mut Rbsp<'_>,
    header: &mut AvcSliceHeader,
    reference: bool,
    idr: bool,
    sps: &AvcSps,
    pps: &AvcPps,
) -> Result<()> {
    let (p, b) = (matches!(header.slice_type, 0 | 3), header.bipredictive());
    if b {
        bits.flag()?; // direct_spatial_mv_pred_flag
    }
    let [mut l0, mut l1] = pps.num_ref_idx_default;
    if (p || b) && bits.flag()? {
        l0 = bits.unsigned(31)? + 1;
        if b {
            l1 = bits.unsigned(31)? + 1;
        }
    }
    // ref_pic_list_modification( ): one list for P and SP, two for B.
    for _ in 0..u32::from(p || b) + u32::from(b) {
        if bits.flag()? {
            loop {
                match bits.unsigned(5)? {
                    3 => break,
                    0..=2 => {
                        bits.unsigned(u32::MAX)?;
                    }
                    _ => return Err(Error::Invalid("AVC modification_of_pic_nums_idc")),
                }
            }
        }
    }
    if (pps.weighted_pred && p) || (pps.weighted_bipred_idc == 1 && b) {
        // pred_weight_table( )
        let chroma = sps.chroma_array_type != 0;
        bits.unsigned(7)?; // luma_log2_weight_denom
        if chroma {
            bits.unsigned(7)?; // chroma_log2_weight_denom
        }
        for count in [Some(l0), b.then_some(l1)].into_iter().flatten() {
            for _ in 0..count {
                if bits.flag()? {
                    bits.signed()?;
                    bits.signed()?;
                }
                if chroma && bits.flag()? {
                    for _ in 0..4 {
                        bits.signed()?;
                    }
                }
            }
        }
    }
    if !reference {
        return Ok(());
    }
    if idr {
        header.no_output_of_prior_pics = bits.flag()?;
        bits.flag()?; // long_term_reference_flag
    } else if bits.flag()? {
        // adaptive_ref_pic_marking_mode_flag: operations up to 0.
        loop {
            match bits.unsigned(6)? {
                0 => break,
                1 | 2 | 4 | 6 => {
                    bits.unsigned(u32::MAX)?;
                }
                3 => {
                    bits.unsigned(u32::MAX)?; // difference_of_pic_nums_minus1
                    bits.unsigned(u32::MAX)?; // long_term_frame_idx
                }
                _ => header.mmco5 = true,
            }
        }
    }
    Ok(())
}

/// On-demand EBSP unescape. Syntax fields and their Exp-Golomb prefixes have
/// finite bounds, independent of the length of the uninspected slice body.
struct Rbsp<'a>(triergon_bitstream::Rbsp<'a>);

fn rbsp_error(error: BitError) -> Error {
    match error {
        BitError::End => Error::Truncated,
        BitError::LongPrefix => Error::Invalid("AVC Exp-Golomb width"),
        BitError::MissingPrevention => Error::Invalid("AVC missing emulation-prevention byte"),
        BitError::InvalidPrevention => Error::Invalid("AVC emulation-prevention byte"),
    }
}

impl<'a> Rbsp<'a> {
    fn new(tail: &'a [u8], reading: AvcReading) -> Self {
        Self(match reading {
            AvcReading::Strict => triergon_bitstream::Rbsp::new(tail),
            AvcReading::Lenient => triergon_bitstream::Rbsp::lenient(tail),
        })
    }

    fn flag(&mut self) -> Result<bool> {
        self.0.flag().map_err(rbsp_error)
    }

    fn fixed(&mut self, width: u8) -> Result<u32> {
        self.0.read(u32::from(width)).map_err(rbsp_error)
    }

    fn golomb(&mut self) -> Result<u64> {
        self.0.exp_golomb(32).map_err(rbsp_error)
    }

    fn unsigned(&mut self, maximum: u32) -> Result<u32> {
        let value = self.golomb()?;
        if value > u64::from(maximum) {
            return Err(Error::Invalid("AVC Exp-Golomb field out of range"));
        }
        Ok(value as u32)
    }

    fn signed(&mut self) -> Result<i32> {
        let code = self.golomb()?;
        let magnitude = code.div_ceil(2) as i64;
        let value = if code & 1 == 0 { -magnitude } else { magnitude };
        i32::try_from(value).map_err(|_| Error::Invalid("AVC signed Exp-Golomb out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order count codings the test streams use.
    #[derive(Clone, Copy)]
    enum OrderMode {
        Lsb { bits: u8 },
        Delta { always_zero: bool },
        Inferred,
    }

    #[derive(Default)]
    struct Bits(Vec<bool>);

    impl Bits {
        fn fixed(&mut self, value: u32, width: u8) {
            for i in (0..width).rev() {
                self.0.push(value & (1 << i) != 0);
            }
        }

        fn ue(&mut self, value: u64) {
            let code = value + 1;
            let bits = code.ilog2();
            self.0.extend(std::iter::repeat_n(false, bits as usize));
            self.0
                .extend((0..=bits).rev().map(|i| code & (1 << i) != 0));
        }

        fn se(&mut self, value: i32) {
            let magnitude = 2 * u64::from(value.unsigned_abs());
            self.ue(magnitude - u64::from(value > 0));
        }

        fn nal(mut self, header: u8) -> Vec<u8> {
            self.0.push(true);
            self.0.resize(self.0.len().next_multiple_of(8), false);
            let mut nal = vec![header];
            let mut zeroes = 0;
            for octet in self.0.chunks_exact(8) {
                let byte = octet.iter().fold(0, |n, &bit| (n << 1) | u8::from(bit));
                if zeroes == 2 && byte <= 3 {
                    nal.push(3);
                    zeroes = 0;
                }
                nal.push(byte);
                zeroes = if byte == 0 { zeroes + 1 } else { 0 };
            }
            nal
        }
    }

    #[derive(Clone, Copy)]
    struct Syntax {
        order: OrderMode,
        frame_only: bool,
        separate_colour_plane: bool,
        redundant: bool,
    }

    fn sps(syntax: Syntax) -> Vec<u8> {
        let mut b = Bits::default();
        b.fixed(
            if syntax.separate_colour_plane {
                244
            } else {
                66
            },
            8,
        );
        b.fixed(0, 8);
        b.fixed(30, 8);
        b.ue(0);
        if syntax.separate_colour_plane {
            b.ue(3);
            b.fixed(1, 1);
            b.ue(0);
            b.ue(0);
            b.fixed(0, 1);
            b.fixed(1, 1);
            // A 4x4 default list and one explicit 8x8 list.
            for i in 0..12 {
                b.fixed(u32::from(i == 0 || i == 6), 1);
                if i == 0 {
                    b.se(-8);
                }
                if i == 6 {
                    for _ in 0..64 {
                        b.se(0);
                    }
                }
            }
        }
        b.ue(0); // 4-bit frame_num
        match syntax.order {
            OrderMode::Lsb { bits } => {
                b.ue(0);
                b.ue(u64::from(bits - 4));
            }
            OrderMode::Delta { always_zero } => {
                b.ue(1);
                b.fixed(u32::from(always_zero), 1);
                b.se(-1);
                b.se(1);
                b.ue(2);
                b.se(1);
                b.se(-1);
            }
            OrderMode::Inferred => b.ue(2),
        }
        b.ue(1);
        b.fixed(0, 1);
        b.ue(3);
        b.ue(3);
        b.fixed(u32::from(syntax.frame_only), 1);
        if !syntax.frame_only {
            b.fixed(0, 1);
        }
        b.fixed(4, 3); // direct_8x8, crop, VUI
        b.nal(0x67)
    }

    fn pps(id: u8, sps_id: u8, redundant: bool, groups_minus1: u64) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(u64::from(id));
        b.ue(u64::from(sps_id));
        b.fixed(1, 2); // entropy coding off, bottom-field POC flag on
        b.ue(groups_minus1);
        b.ue(0);
        b.ue(0);
        b.fixed(0, 3);
        b.se(0);
        b.se(0);
        b.se(0);
        b.fixed(0, 2);
        b.fixed(u32::from(redundant), 1);
        b.nal(0x68)
    }

    #[derive(Clone, Copy)]
    struct Slice {
        first_mb: u32,
        pps: u8,
        frame: u32,
        field: Field,
        reference_idc: u8,
        idr: Option<u16>,
        lsb: u32,
        delta: [i32; 2],
        colour: u32,
        redundant_count: u64,
    }

    const BASE: Slice = Slice {
        first_mb: 11,
        pps: 0,
        frame: 0,
        field: Field::Frame,
        reference_idc: 1,
        idr: None,
        lsb: 0,
        delta: [0, 0],
        colour: 0,
        redundant_count: 0,
    };

    fn slice(spec: Slice, syntax: Syntax) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(u64::from(spec.first_mb));
        b.ue(2); // I slice
        b.ue(u64::from(spec.pps));
        if syntax.separate_colour_plane {
            b.fixed(spec.colour, 2);
        }
        b.fixed(spec.frame, 4);
        if !syntax.frame_only {
            b.fixed(u32::from(spec.field != Field::Frame), 1);
            if spec.field != Field::Frame {
                b.fixed(u32::from(spec.field == Field::Bottom), 1);
            }
        }
        if let Some(idr) = spec.idr {
            b.ue(u64::from(idr));
        }
        match syntax.order {
            OrderMode::Lsb { bits } => {
                b.fixed(spec.lsb, bits);
                if spec.field == Field::Frame {
                    b.se(spec.delta[1]);
                }
            }
            OrderMode::Delta { always_zero: false } => {
                b.se(spec.delta[0]);
                if spec.field == Field::Frame {
                    b.se(spec.delta[1]);
                }
            }
            _ => {}
        }
        if syntax.redundant {
            b.ue(spec.redundant_count);
        }
        let kind = if spec.idr.is_some() { 5 } else { 1 };
        b.nal(spec.reference_idc << 5 | kind)
    }

    fn observe(pictures: &mut AvcPictures, nal: &[u8]) -> Result<AvcPictureFact> {
        pictures.observe(nal, NalHeader::parse(Codec::Avc, nal)?)
    }

    fn configured(syntax: Syntax) -> AvcPictures {
        let mut tracker = AvcPictures::default();
        assert!(!observe(&mut tracker, &sps(syntax)).unwrap().vcl);
        for id in [0, 255] {
            assert!(
                !observe(&mut tracker, &pps(id, 0, syntax.redundant, 0))
                    .unwrap()
                    .vcl
            );
        }
        tracker
    }

    #[test]
    fn first_picture_comparison_uses_full_identity_without_aud() {
        let syntax = Syntax {
            order: OrderMode::Lsb { bits: 4 },
            frame_only: false,
            separate_colour_plane: false,
            redundant: false,
        };
        let mut tracker = configured(syntax);
        let cases = [
            (
                BASE,
                Slice {
                    first_mb: 0,
                    ..BASE
                },
                false,
            ),
            (BASE, Slice { frame: 1, ..BASE }, true),
            (BASE, Slice { pps: 255, ..BASE }, true),
            (
                BASE,
                Slice {
                    field: Field::Top,
                    ..BASE
                },
                true,
            ),
            (
                Slice {
                    field: Field::Top,
                    ..BASE
                },
                Slice {
                    field: Field::Bottom,
                    ..BASE
                },
                true,
            ),
            (
                BASE,
                Slice {
                    reference_idc: 3,
                    ..BASE
                },
                false,
            ),
            (
                BASE,
                Slice {
                    reference_idc: 0,
                    ..BASE
                },
                true,
            ),
            (BASE, Slice { lsb: 1, ..BASE }, true),
            (
                BASE,
                Slice {
                    delta: [0, -1],
                    ..BASE
                },
                true,
            ),
            (
                BASE,
                Slice {
                    idr: Some(1),
                    ..BASE
                },
                true,
            ),
            (
                Slice {
                    idr: Some(1),
                    ..BASE
                },
                Slice {
                    idr: Some(2),
                    ..BASE
                },
                true,
            ),
            (
                Slice {
                    idr: Some(1),
                    ..BASE
                },
                Slice {
                    idr: Some(1),
                    first_mb: 17,
                    ..BASE
                },
                false,
            ),
        ];
        for (first, second, new_picture) in cases {
            tracker.reset_picture();
            assert!(
                observe(&mut tracker, &slice(first, syntax))
                    .unwrap()
                    .new_picture
            );
            assert_eq!(
                observe(&mut tracker, &slice(second, syntax)).unwrap(),
                AvcPictureFact {
                    vcl: true,
                    new_picture,
                    random_access: second.idr.is_some()
                }
            );
        }
    }

    #[test]
    fn poc_type_one_and_separate_colour_planes() {
        let syntax = Syntax {
            order: OrderMode::Delta { always_zero: false },
            frame_only: true,
            separate_colour_plane: true,
            redundant: false,
        };
        let mut tracker = configured(syntax);
        for (delta, colour, boundary) in [
            ([0, 0], 0, true),
            ([0, 0], 2, false),
            ([-12345678, 0], 1, true),
            ([-12345678, -1], 0, true),
            ([-12345678, -1], 1, false),
        ] {
            let unit = slice(
                Slice {
                    delta,
                    colour,
                    ..BASE
                },
                syntax,
            );
            assert_eq!(observe(&mut tracker, &unit).unwrap().new_picture, boundary);
        }
        let invalid = slice(Slice { colour: 3, ..BASE }, syntax);
        assert_eq!(
            observe(&mut tracker, &invalid),
            Err(Error::Invalid("AVC colour_plane_id"))
        );

        for order in [OrderMode::Delta { always_zero: true }, OrderMode::Inferred] {
            let syntax = Syntax {
                order,
                separate_colour_plane: false,
                ..syntax
            };
            let mut tracker = configured(syntax);
            assert!(
                observe(&mut tracker, &slice(BASE, syntax))
                    .unwrap()
                    .new_picture
            );
            assert!(
                !observe(&mut tracker, &slice(BASE, syntax))
                    .unwrap()
                    .new_picture
            );
        }
    }

    #[test]
    fn explicit_au_reset_preserves_parameter_sets_and_refusals_keep_identity() {
        let syntax = Syntax {
            order: OrderMode::Lsb { bits: 16 },
            frame_only: true,
            separate_colour_plane: false,
            redundant: true,
        };
        let unit = slice(BASE, syntax);
        let mut tracker = configured(syntax);
        assert!(observe(&mut tracker, &unit).unwrap().new_picture);
        assert_eq!(
            observe(&mut tracker, &[0x69, 0xf0]).unwrap(),
            AvcPictureFact::default()
        );
        tracker.reset_picture();
        assert!(observe(&mut tracker, &unit).unwrap().new_picture);
        assert_eq!(observe(&mut tracker, &[0x61, 0x80]), Err(Error::Truncated));
        assert_eq!(
            observe(
                &mut tracker,
                &slice(
                    Slice {
                        redundant_count: 1,
                        ..BASE
                    },
                    syntax
                )
            ),
            Err(Error::Unsupported("AVC redundant coded picture"))
        );
        assert!(!observe(&mut tracker, &unit).unwrap().new_picture);

        for kind in [2, 3, 4] {
            assert_eq!(
                observe(&mut tracker, &[kind]),
                Err(Error::Unsupported("AVC slice data partitioning"))
            );
        }
        for kind in [13, 14, 15, 16, 19, 20, 21] {
            assert_eq!(
                observe(&mut tracker, &[kind]),
                Err(Error::Unsupported("AVC extension NAL"))
            );
        }
        assert_eq!(
            observe(&mut tracker, &pps(0, 0, false, 1)),
            Err(Error::Unsupported("AVC flexible macroblock ordering"))
        );
        assert!(!observe(&mut tracker, &unit).unwrap().new_picture);
    }

    #[test]
    fn missing_parameter_sets_and_invalid_escape_refuse() {
        let syntax = Syntax {
            order: OrderMode::Inferred,
            frame_only: true,
            separate_colour_plane: false,
            redundant: false,
        };
        let mut tracker = AvcPictures::default();
        let unit = slice(BASE, syntax);
        assert_eq!(
            observe(&mut tracker, &unit),
            Err(Error::Invalid("AVC slice has no PPS"))
        );
        observe(&mut tracker, &pps(0, 31, false, 0)).unwrap();
        assert_eq!(
            observe(&mut tracker, &unit),
            Err(Error::Invalid("AVC slice has no SPS"))
        );
        let mut bits = Rbsp::new(&[0, 0, 3, 4], AvcReading::Strict);
        assert_eq!(
            bits.fixed(24),
            Err(Error::Invalid("AVC emulation-prevention byte"))
        );
        let mut bits = Rbsp::new(&[0, 0, 3, 1], AvcReading::Strict);
        assert_eq!(bits.fixed(24).unwrap(), 1);
    }
}
