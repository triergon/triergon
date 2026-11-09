//! Ordinary AVC primary-picture comparison from H.264 (08/2024),
//! §§7.3.2.1.1, 7.3.2.2, 7.3.3 and 7.4.1.2.4. This inspects the syntax
//! needed for picture identity, leaving the rest of each RBSP opaque.
use crate::{Codec, NalHeader, SyntaxError, skip_avc_scaling_matrix};
use triergon_bitstream::{BitError, BitSource};
use triergon_core::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AvcPictureFact {
    pub vcl: bool,
    pub new_picture: bool,
    pub random_access: bool,
}

pub(crate) struct AvcPictures {
    sps: [Option<Sps>; 32],
    pps: [Option<Pps>; 256],
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
                let (id, sps) = parse_sps(payload)?;
                self.sps[id] = Some(sps);
                Ok(AvcPictureFact::default())
            }
            8 => {
                let (id, pps) = parse_pps(payload)?;
                self.pps[id] = Some(pps);
                Ok(AvcPictureFact::default())
            }
            13..=16 | 19..=21 => Err(Error::Unsupported("AVC extension NAL")),
            _ => Ok(AvcPictureFact::default()),
        }
    }

    fn slice(&self, payload: &[u8], idr: bool, reference: bool) -> Result<Picture> {
        let mut bits = Rbsp::new(payload);
        bits.unsigned(u32::MAX)?; // first_mb_in_slice is not a picture condition.
        bits.unsigned(9)?; // slice_type
        let pps_id = bits.unsigned(255)? as u8;
        let pps = self.pps[usize::from(pps_id)].ok_or(Error::Invalid("AVC slice has no PPS"))?;
        let sps =
            self.sps[usize::from(pps.sps_id)].ok_or(Error::Invalid("AVC slice has no SPS"))?;
        if sps.separate_colour_plane && bits.fixed(2)? > 2 {
            return Err(Error::Invalid("AVC colour_plane_id"));
        }
        let frame_num = bits.fixed(sps.frame_num_bits)? as u16;
        let field = if !sps.frame_only && bits.flag()? {
            if bits.flag()? {
                Field::Bottom
            } else {
                Field::Top
            }
        } else {
            Field::Frame
        };
        let idr_pic_id = if idr {
            Some(bits.unsigned(65535)? as u16)
        } else {
            None
        };

        let has_bottom_delta = pps.bottom_delta_present && field == Field::Frame;
        let order = match sps.order {
            OrderMode::Lsb { bits: width } => PictureOrder::Lsb {
                value: bits.fixed(width)? as u16,
                bottom_delta: if has_bottom_delta { bits.signed()? } else { 0 },
            },
            OrderMode::Delta { always_zero } => PictureOrder::Delta {
                first: if always_zero { 0 } else { bits.signed()? },
                second: if !always_zero && has_bottom_delta {
                    bits.signed()?
                } else {
                    0
                },
            },
            OrderMode::Inferred => PictureOrder::Inferred,
        };
        if pps.redundant_count_present && bits.unsigned(127)? != 0 {
            return Err(Error::Unsupported("AVC redundant coded picture"));
        }
        Ok(Picture {
            frame_num,
            pps_id,
            field,
            reference,
            idr_pic_id,
            order,
        })
    }
}

#[derive(Clone, Copy)]
struct Sps {
    frame_num_bits: u8,
    order: OrderMode,
    frame_only: bool,
    separate_colour_plane: bool,
}

#[derive(Clone, Copy)]
enum OrderMode {
    Lsb { bits: u8 },
    Delta { always_zero: bool },
    Inferred,
}

#[derive(Clone, Copy)]
struct Pps {
    sps_id: u8,
    bottom_delta_present: bool,
    redundant_count_present: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Frame,
    Top,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PictureOrder {
    Lsb { value: u16, bottom_delta: i32 },
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

fn parse_sps(payload: &[u8]) -> Result<(usize, Sps)> {
    let mut bits = Rbsp::new(payload);
    let profile = bits.fixed(8)?;
    if bits.fixed(8)? & 3 != 0 {
        return Err(Error::Invalid("AVC SPS reserved flags"));
    }
    bits.fixed(8)?; // level_idc
    let id = bits.unsigned(31)? as usize;
    let separate_colour_plane = match profile {
        66 | 77 | 88 => false,
        44 | 100 | 110 | 122 | 244 => {
            let chroma = bits.unsigned(3)?;
            let separate = chroma == 3 && bits.flag()?;
            bits.unsigned(6)?; // bit_depth_luma_minus8
            bits.unsigned(6)?; // bit_depth_chroma_minus8
            bits.flag()?; // qpprime_y_zero_transform_bypass_flag
            skip_avc_scaling_matrix(&mut bits.0, chroma).map_err(|error| match error {
                SyntaxError::Bits(error) => rbsp_error(error),
                SyntaxError::OutOfRange => Error::Invalid("AVC scaling list delta"),
            })?;
            separate
        }
        _ => return Err(Error::Unsupported("AVC SPS profile for picture comparison")),
    };
    let frame_num_bits = bits.unsigned(12)? as u8 + 4;
    let order = match bits.unsigned(2)? {
        0 => OrderMode::Lsb {
            bits: bits.unsigned(12)? as u8 + 4,
        },
        1 => {
            let always_zero = bits.flag()?;
            bits.signed()?; // offset_for_non_ref_pic
            bits.signed()?; // offset_for_top_to_bottom_field
            for _ in 0..bits.unsigned(255)? {
                bits.signed()?;
            }
            OrderMode::Delta { always_zero }
        }
        _ => OrderMode::Inferred,
    };
    bits.unsigned(u32::MAX)?; // max_num_ref_frames
    bits.flag()?; // gaps_in_frame_num_value_allowed_flag
    bits.unsigned(u32::MAX)?; // pic_width_in_mbs_minus1
    bits.unsigned(u32::MAX)?; // pic_height_in_map_units_minus1
    let frame_only = bits.flag()?;
    Ok((
        id,
        Sps {
            frame_num_bits,
            order,
            frame_only,
            separate_colour_plane,
        },
    ))
}

fn parse_pps(payload: &[u8]) -> Result<(usize, Pps)> {
    let mut bits = Rbsp::new(payload);
    let id = bits.unsigned(255)? as usize;
    let sps_id = bits.unsigned(31)? as u8;
    bits.flag()?; // entropy_coding_mode_flag
    let bottom_delta_present = bits.flag()?;
    if bits.unsigned(7)? != 0 {
        return Err(Error::Unsupported("AVC flexible macroblock ordering"));
    }
    bits.unsigned(31)?; // num_ref_idx_l0_default_active_minus1
    bits.unsigned(31)?; // num_ref_idx_l1_default_active_minus1
    bits.fixed(3)?; // weighted_pred_flag and weighted_bipred_idc
    bits.signed()?; // pic_init_qp_minus26
    bits.signed()?; // pic_init_qs_minus26
    bits.signed()?; // chroma_qp_index_offset
    bits.fixed(2)?; // deblocking filter and constrained intra prediction flags
    let redundant_count_present = bits.flag()?;
    Ok((
        id,
        Pps {
            sps_id,
            bottom_delta_present,
            redundant_count_present,
        },
    ))
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
    fn new(tail: &'a [u8]) -> Self {
        Self(triergon_bitstream::Rbsp::new(tail))
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
        let mut bits = Rbsp::new(&[0, 0, 3, 4]);
        assert_eq!(
            bits.fixed(24),
            Err(Error::Invalid("AVC emulation-prevention byte"))
        );
        let mut bits = Rbsp::new(&[0, 0, 3, 1]);
        assert_eq!(bits.fixed(24).unwrap(), 1);
    }
}
