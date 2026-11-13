use triergon_core::{Error, Result};

/// H.264 `nal_unit_type` values (H.264 Table 7-1) that callers act on.
pub mod avc {
    pub const SLICE: u8 = 1;
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// H.265 `nal_unit_type` values (H.265 Table 7-1) and their classes.
pub mod hevc {
    pub const RADL_N: u8 = 6;
    pub const RADL_R: u8 = 7;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const RSV_VCL_N14: u8 = 14;
    pub const BLA_W_LP: u8 = 16;
    pub const BLA_N_LP: u8 = 18;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA: u8 = 21;
    /// The last type reserved for IRAP pictures.
    pub const RSV_IRAP_23: u8 = 23;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const EOS: u8 = 36;
    pub const EOB: u8 = 37;
    pub const PREFIX_SEI: u8 = 39;
    pub const SUFFIX_SEI: u8 = 40;

    /// A slice of a coded picture. The reserved VCL types (10 to 15, 22 to
    /// 31) are not: FFmpeg's HEVC parser splits access units on these types
    /// only, and its decoder skips the reserved ones.
    #[inline]
    pub const fn is_slice(t: u8) -> bool {
        matches!(t, 0..=9 | 16..=21)
    }

    /// Whether a base-layer NAL unit of type `t` after the last VCL NAL unit
    /// of a picture starts the next access unit.
    ///
    /// H.265 §7.4.2.4.4 starts it with a VPS, SPS, PPS, access unit
    /// delimiter, prefix SEI, a reserved type 41 to 44 or an unspecified type
    /// 48 to 55. With each type 0 to 63 inserted after a picture, raw and in
    /// TS, the reference also starts it with an end of sequence (36) or end
    /// of bitstream (37), which the specification places last in the access
    /// unit they end. Every other non-VCL type stays in the access unit before
    /// it: filler data (38), the suffix SEI (40), the reserved types 45 to 47
    /// and the unspecified types 56 to 63, as do the reserved VCL types. A slice ([`is_slice`]) starts a picture by its
    /// `first_slice_segment_in_pic_flag` instead, and is `false` here.
    #[inline]
    pub const fn begins_access_unit(t: u8) -> bool {
        matches!(t, VPS..=EOB | PREFIX_SEI | 41..=44 | 48..=55)
    }

    /// Intra random access point, reserved IRAP types included (7.4.2.2).
    #[inline]
    pub const fn is_irap(t: u8) -> bool {
        matches!(t, BLA_W_LP..=RSV_IRAP_23)
    }

    #[inline]
    pub const fn is_idr(t: u8) -> bool {
        matches!(t, IDR_W_RADL | IDR_N_LP)
    }

    #[inline]
    pub const fn is_bla(t: u8) -> bool {
        matches!(t, BLA_W_LP..=BLA_N_LP)
    }

    #[inline]
    pub const fn is_rasl(t: u8) -> bool {
        matches!(t, RASL_N | RASL_R)
    }

    #[inline]
    pub const fn is_radl(t: u8) -> bool {
        matches!(t, RADL_N | RADL_R)
    }

    /// A sub-layer non-reference picture (7.4.2.2): the even types up to 14.
    #[inline]
    pub const fn is_sub_layer_non_ref(t: u8) -> bool {
        t <= RSV_VCL_N14 && t.is_multiple_of(2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Avc,
    Hevc,
}

impl Codec {
    pub const fn minimum_header_bytes(self) -> usize {
        match self {
            Self::Avc => 1,
            Self::Hevc => 2,
        }
    }
}

/// Only the fixed NAL header is inspected. Reserved types and HEVC layers are
/// retained, without claiming that the corresponding coding tool is supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalHeader {
    Avc {
        nal_ref_idc: u8,
        nal_unit_type: u8,
    },
    Hevc {
        nal_unit_type: u8,
        layer_id: u8,
        temporal_id_plus_one: u8,
    },
}

/// The fixed-header parser has exactly these three failures. Keep its hot
/// internal result and retained refusal facts compact; the public API still
/// returns the original core Error without changing priority or diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NalHeaderError {
    Truncated,
    ForbiddenZeroBit,
    ZeroTemporalId,
}
impl NalHeaderError {
    pub(crate) fn error(self) -> Error {
        match self {
            Self::Truncated => Error::Truncated,
            Self::ForbiddenZeroBit => Error::Invalid("NAL forbidden_zero_bit"),
            Self::ZeroTemporalId => Error::Invalid("HEVC temporal_id_plus1 is zero"),
        }
    }
}

impl NalHeader {
    /// H.264 (08/2024) 7.3.1 and H.265 (07/2024) 7.3.1.2. Extended AVC
    /// headers, RBSP syntax and type-specific constraints are outside this call.
    pub fn parse(codec: Codec, nal: &[u8]) -> Result<Self> {
        Self::parse_compact(codec, nal).map_err(NalHeaderError::error)
    }
    pub(crate) fn parse_compact(
        codec: Codec,
        nal: &[u8],
    ) -> std::result::Result<Self, NalHeaderError> {
        if nal.len() < codec.minimum_header_bytes() {
            return Err(NalHeaderError::Truncated);
        }
        if nal[0] & 0x80 != 0 {
            return Err(NalHeaderError::ForbiddenZeroBit);
        }
        match codec {
            Codec::Avc => Ok(Self::Avc {
                nal_ref_idc: (nal[0] >> 5) & 3,
                nal_unit_type: nal[0] & 31,
            }),
            Codec::Hevc => {
                let temporal_id_plus_one = nal[1] & 7;
                if temporal_id_plus_one == 0 {
                    return Err(NalHeaderError::ZeroTemporalId);
                }
                Ok(Self::Hevc {
                    nal_unit_type: (nal[0] >> 1) & 63,
                    layer_id: ((nal[0] & 1) << 5) | (nal[1] >> 3),
                    temporal_id_plus_one,
                })
            }
        }
    }

    pub const fn codec(self) -> Codec {
        match self {
            Self::Avc { .. } => Codec::Avc,
            Self::Hevc { .. } => Codec::Hevc,
        }
    }

    pub const fn nal_unit_type(self) -> u8 {
        match self {
            Self::Avc { nal_unit_type, .. } | Self::Hevc { nal_unit_type, .. } => nal_unit_type,
        }
    }

    /// This is a type classification, not a parse of parameter-set contents.
    pub const fn parameter_set_kind(self) -> Option<ParameterSetKind> {
        match self {
            Self::Avc {
                nal_unit_type: 7, ..
            } => Some(ParameterSetKind::AvcSps),
            Self::Avc {
                nal_unit_type: 8, ..
            } => Some(ParameterSetKind::AvcPps),
            Self::Hevc {
                nal_unit_type: 32, ..
            } => Some(ParameterSetKind::HevcVps),
            Self::Hevc {
                nal_unit_type: 33, ..
            } => Some(ParameterSetKind::HevcSps),
            Self::Hevc {
                nal_unit_type: 34, ..
            } => Some(ParameterSetKind::HevcPps),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterSetKind {
    AvcSps,
    AvcPps,
    HevcVps,
    HevcSps,
    HevcPps,
}
