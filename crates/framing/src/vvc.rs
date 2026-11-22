//! Fixed VVC header facts and conservative access-unit framing.

mod stream;
pub use stream::{VvcAnnexBReader, VvcStreamAccessUnit};

use crate::AnnexBUnits;
use triergon_core::{Error, Result};

/// The two-byte H.266 §7.3.1.2 header, including nonconforming values.
/// Inspecting it does not establish type-specific or payload conformance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VvcNalHeader {
    pub forbidden_zero_bit: bool,
    pub reserved_zero_bit: bool,
    pub layer_id: u8,
    pub nal_unit_type: u8,
    pub temporal_id_plus_one: u8,
}

impl VvcNalHeader {
    /// Reads only two bytes. All bit patterns are retained; a short header is
    /// `Truncated`. H.266 §7.4.2.2 requires the forbidden bit to be zero and
    /// the temporal identifier plus one to be nonzero.
    pub fn inspect(nal: &[u8]) -> Result<Self> {
        let [first, second, ..] = *nal else {
            return Err(Error::Truncated);
        };
        Ok(Self {
            forbidden_zero_bit: first & 0x80 != 0,
            reserved_zero_bit: first & 0x40 != 0,
            layer_id: first & 0x3f,
            nal_unit_type: second >> 3,
            temporal_id_plus_one: second & 7,
        })
    }

    /// H.266 (09/2023) §7.4.2.2 requires decoders to discard a NAL with
    /// the reserved bit set or a layer identifier above 55. No decoder runs here.
    #[must_use]
    pub const fn requires_discard(self) -> bool {
        self.reserved_zero_bit || self.layer_id > 55
    }
}

/// A borrowed NAL, with its original bytes and independently inspected header.
#[derive(Debug)]
pub struct VvcNal<'a> {
    pub bytes: &'a [u8],
    pub header: Result<VvcNalHeader>,
}

/// Allocation-free inspection of a complete, caller-bounded Annex B buffer.
///
/// Reuses `AnnexBUnits`: ignores bytes before the first prefix, empty units and
/// trailing zeros. Preserves emulation-prevention bytes. This is extraction,
/// not Annex B conformance validation (including mandatory `zero_byte` rules).
/// Never pass an arbitrary PES fragment as if it were a complete byte stream:
/// prefixes, headers and payloads may cross PES boundaries. No AU is inferred.
#[derive(Debug)]
pub struct VvcAnnexBUnits<'a>(AnnexBUnits<'a>);

impl<'a> VvcAnnexBUnits<'a> {
    /// Refuses an oversized buffer before scanning. Total work is linear in
    /// `data.len()`, bounded by `maximum_bytes`; no input-dependent allocation.
    pub fn new(data: &'a [u8], maximum_bytes: usize) -> Result<Self> {
        if data.len() > maximum_bytes {
            return Err(Error::Limit {
                requested: data.len(),
                maximum: maximum_bytes,
            });
        }
        Ok(Self(AnnexBUnits::new(data)))
    }
}

impl<'a> Iterator for VvcAnnexBUnits<'a> {
    type Item = VvcNal<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|bytes| VvcNal {
            bytes,
            header: VvcNalHeader::inspect(bytes),
        })
    }
}

/// An access unit borrowed from a complete Annex B byte stream.
/// `range` is relative to the input and includes the initial zero byte and
/// start code, ending before the next unit's zero byte or at supplied EOF.
#[derive(Debug, PartialEq, Eq)]
pub struct VvcAccessUnit<'a> {
    pub range: core::ops::Range<usize>,
    pub bytes: &'a [u8],
}

/// Conservative, allocation-free framing of a complete, bounded byte stream.
/// `with_delimiters` requires an AUD per unit. `with_picture_headers` also accepts explicit
/// standalone or embedded picture headers. A standalone picture header can
/// precede several consecutive slices. The delimiter-only path groups a single
/// slice per unit. Source bytes, including
/// padding and emulation-prevention bytes, are preserved. Work is linear in the
/// supplied byte limit. The caller must supply the actual end of the stream.
#[derive(Debug)]
pub struct VvcAccessUnits<'a> {
    data: &'a [u8],
    at: usize,
    next_prefix: Option<usize>,
    failed: bool,
    require_aud: bool,
    allow_short_embedded_prefix: bool,
}

impl<'a> VvcAccessUnits<'a> {
    /// Bounds the complete input before any scanning.
    pub fn with_delimiters(data: &'a [u8], maximum_bytes: usize) -> Result<Self> {
        if data.len() > maximum_bytes {
            return Err(Error::Limit {
                requested: data.len(),
                maximum: maximum_bytes,
            });
        }
        Ok(Self {
            data,
            at: 0,
            next_prefix: None,
            failed: false,
            require_aud: true,
            allow_short_embedded_prefix: false,
        })
    }

    /// Frames single-layer pictures with an explicit standalone or embedded
    /// picture header. A standalone header permits consecutive slices that
    /// use that header. Picture syntax beyond framing remains opaque.
    pub fn with_picture_headers(data: &'a [u8], maximum_bytes: usize) -> Result<Self> {
        let mut reader = Self::with_delimiters(data, maximum_bytes)?;
        reader.require_aud = false;
        Ok(reader)
    }

    pub(crate) fn allow_short_embedded_prefix(
        data: &'a [u8],
        maximum_bytes: usize,
    ) -> Result<Self> {
        let mut units = Self::with_picture_headers(data, maximum_bytes)?;
        units.allow_short_embedded_prefix = true;
        Ok(units)
    }

    fn read_unit(&mut self) -> Result<VvcAccessUnit<'a>> {
        let start = self.at;
        let mut prefix = if let Some(prefix) = self.next_prefix.take() {
            prefix
        } else {
            let first =
                crate::start_code(self.data, 0).ok_or(Error::Invalid("missing VVC start code"))?;
            // The first NAL needs a zero_byte. Earlier bytes are leading zeros.
            if first == 0 || self.data[..first].iter().any(|&byte| byte != 0) {
                return Err(Error::Invalid("missing VVC start-code zero byte"));
            }
            first
        };
        let mut header = VvcNalHeader::inspect(&self.data[prefix + 3..])?;
        let mut picture = PictureState::default();
        loop {
            let nal_start = prefix + 3;
            let next = crate::start_code(self.data, nal_start);
            let raw_end = next.unwrap_or(self.data.len());
            let mut nal_end = raw_end;
            while nal_end > nal_start && self.data[nal_end - 1] == 0 {
                nal_end -= 1;
            }
            let nal = &self.data[nal_start..nal_end];
            if nal.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2) {
                return Err(Error::Invalid("invalid VVC Annex B payload"));
            }
            picture.check(
                prefix != 0 && self.data[prefix - 1] == 0,
                nal.len(),
                nal.get(2).copied(),
                header,
                self.require_aud,
                next.is_some(),
            )?;
            let Some(next_prefix) = next else {
                if picture.first_vcl_type.is_none() {
                    return Err(Error::Invalid("VVC access unit without a slice"));
                }
                self.at = self.data.len();
                return Ok(VvcAccessUnit {
                    range: start..self.at,
                    bytes: &self.data[start..self.at],
                });
            };
            // Check the next NAL for a boundary without consuming it.
            let next_header = VvcNalHeader::inspect(&self.data[next_prefix + 3..])?;
            let next_kind = next_header.nal_unit_type;
            let embedded = embedded_header(next_kind, self.data.get(next_prefix + 5).copied());
            let boundary = picture.boundary(next_kind, embedded, self.require_aud);
            if boundary {
                let zero_byte = self.data[next_prefix - 1] == 0;
                if picture.first_vcl_type.is_none()
                    || !(zero_byte || (embedded && self.allow_short_embedded_prefix))
                {
                    return Err(Error::Invalid("ambiguous VVC access-unit boundary"));
                }
                self.at = next_prefix - usize::from(zero_byte);
                self.next_prefix = Some(next_prefix);
                return Ok(VvcAccessUnit {
                    range: start..self.at,
                    bytes: &self.data[start..self.at],
                });
            }
            prefix = next_prefix;
            header = next_header;
        }
    }
}

impl<'a> Iterator for VvcAccessUnits<'a> {
    type Item = Result<VvcAccessUnit<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.at == self.data.len() {
            return None;
        }
        let result = self.read_unit();
        if result.is_err() {
            self.failed = true;
        }
        Some(result)
    }
}

fn slice_kind(kind: u8) -> bool {
    matches!(kind, 0..=3 | 7..=10)
}

fn embedded_header(kind: u8, first_payload: Option<u8>) -> bool {
    slice_kind(kind) && first_payload.is_some_and(|byte| byte & 0x80 != 0)
}

#[derive(Debug, Clone, Copy)]
struct PictureState {
    first_vcl_type: Option<u8>,
    all_vcl_idr: bool,
    suffix: bool,
    picture_header: bool,
    first: bool,
}

impl Default for PictureState {
    fn default() -> Self {
        Self {
            first_vcl_type: None,
            all_vcl_idr: true,
            suffix: false,
            picture_header: false,
            first: true,
        }
    }
}

impl PictureState {
    fn boundary(&self, kind: u8, embedded: bool, require_aud: bool) -> bool {
        kind == 20
            || (!require_aud
                && self.first_vcl_type.is_some()
                && (matches!(kind, 12..=17 | 19 | 23 | 26 | 28..=29) || embedded))
    }

    fn check(
        &mut self,
        zero_byte: bool,
        nal_len: usize,
        first_payload: Option<u8>,
        header: VvcNalHeader,
        require_aud: bool,
        has_next: bool,
    ) -> Result<()> {
        if nal_len < 2 {
            return Err(Error::Truncated);
        }
        if header.forbidden_zero_bit
            || header.requires_discard()
            || header.temporal_id_plus_one == 0
        {
            return Err(Error::Invalid("invalid VVC NAL header"));
        }
        if header.layer_id != 0 {
            return Err(Error::Invalid("unsupported VVC layer"));
        }
        let kind = header.nal_unit_type;
        // H.266 Annex B also requires zero_byte for DCI through suffix APS.
        if matches!(kind, 12..=18) && !zero_byte {
            return Err(Error::Invalid("VVC NAL without the required zero byte"));
        }
        if self.first && (require_aud || kind == 20) {
            if kind != 20
                || nal_len != 3
                || first_payload.is_none_or(|byte| byte & 0x0f != 8 || (byte >> 4) & 7 > 2)
            {
                return Err(Error::Invalid("missing or malformed VVC AUD"));
            }
            self.first = false;
        } else {
            self.first = false;
            if kind == 20 {
                return Err(Error::Invalid("unexpected VVC AUD"));
            }
            match kind {
                0..=3 | 7..=10 => {
                    if nal_len < 3
                        || (self.first_vcl_type.is_some()
                            && (require_aud || !self.picture_header || self.suffix))
                    {
                        return Err(Error::Invalid("ambiguous VVC picture boundary"));
                    }
                    if !require_aud && (embedded_header(kind, first_payload) == self.picture_header)
                    {
                        return Err(Error::Invalid("missing or conflicting VVC picture header"));
                    }
                    self.first_vcl_type.get_or_insert(kind);
                    self.all_vcl_idr &= matches!(kind, 7 | 8);
                }
                19 if !require_aud && self.first_vcl_type.is_none() => {
                    if self.picture_header || nal_len < 3 {
                        return Err(Error::Invalid("duplicate or empty VVC picture header"));
                    }
                    self.picture_header = true;
                }
                12..=17 | 19 | 23 | 26 | 28..=29 if self.first_vcl_type.is_some() => {
                    return Err(Error::Invalid("missing VVC AUD before prefix NAL"));
                }
                18 | 21..=22 | 24..=25 | 27 | 30..=31 if self.first_vcl_type.is_none() => {
                    return Err(Error::Invalid("VVC suffix before picture"));
                }
                4..=6 | 11 => return Err(Error::Unsupported("reserved VVC VCL type")),
                22 if has_next => return Err(Error::Invalid("NAL after VVC end marker")),
                _ => self.suffix = self.first_vcl_type.is_some(),
            }
        }
        Ok(())
    }
}
