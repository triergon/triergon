use triergon_core::{Error, Result};

/// The coding standard of a NAL unit stream, which fixes its header layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (AVC): a one-byte NAL unit header.
    Avc,
    /// H.265 (HEVC): a two-byte NAL unit header.
    Hevc,
}

impl Codec {
    /// Bytes of the fixed NAL unit header: 1 for H.264, 2 for H.265. A NAL unit
    /// is at least this long.
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
    /// The one-byte header of H.264 7.3.1.
    Avc {
        /// `nal_ref_idc`, 0 to 3.
        nal_ref_idc: u8,
        /// `nal_unit_type`, 0 to 31 (Table 7-1).
        nal_unit_type: u8,
    },
    /// The two-byte header of H.265 7.3.1.2.
    Hevc {
        /// `nal_unit_type`, 0 to 63 (Table 7-1).
        nal_unit_type: u8,
        /// `nuh_layer_id`, 0 to 63; zero is the base layer.
        layer_id: u8,
        /// `nuh_temporal_id_plus1`, 1 to 7.
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
    ///
    /// # Errors
    ///
    /// [`Error::Truncated`] when `nal` is shorter than the header;
    /// [`Error::Invalid`] when `forbidden_zero_bit` is set or an H.265
    /// `nuh_temporal_id_plus1` is zero.
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

    /// The standard whose header layout this is.
    pub const fn codec(self) -> Codec {
        match self {
            Self::Avc { .. } => Codec::Avc,
            Self::Hevc { .. } => Codec::Hevc,
        }
    }

    /// `nal_unit_type` of either standard: 0 to 31 for H.264, 0 to 63 for H.265.
    /// The values are named in [`avc`](crate::avc) and [`hevc`](crate::hevc).
    pub const fn nal_unit_type(self) -> u8 {
        match self {
            Self::Avc { nal_unit_type, .. } | Self::Hevc { nal_unit_type, .. } => nal_unit_type,
        }
    }

    /// The parameter set `nal_unit_type` names: H.264 types 7 and 8, H.265 types
    /// 32 to 34; `None` for every other type.
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

/// The parameter sets a configuration carries, by `nal_unit_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterSetKind {
    /// H.264 sequence parameter set, type 7.
    AvcSps,
    /// H.264 picture parameter set, type 8.
    AvcPps,
    /// H.265 video parameter set, type 32.
    HevcVps,
    /// H.265 sequence parameter set, type 33.
    HevcSps,
    /// H.265 picture parameter set, type 34.
    HevcPps,
}
