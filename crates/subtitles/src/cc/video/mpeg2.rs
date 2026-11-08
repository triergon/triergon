//! MPEG-2 video (ISO/IEC 13818-2) start codes, sequence and picture headers,
//! as far as caption user data needs them.
//!
//! An elementary stream is a run of start codes (`00 00 01` and a code byte),
//! each followed by its data up to the next start code. A picture is its
//! picture header, its extensions and user data, and its slices; its user data
//! (`0xB2`) comes before the first slice.

/// The start codes of `es` in order, each with its data up to the next one.
#[must_use]
pub fn start_codes(es: &[u8]) -> StartCodes<'_> {
    StartCodes {
        es,
        next: find_prefix(es, 0),
    }
}

/// Iterator over start codes.
#[derive(Clone, Debug)]
pub struct StartCodes<'a> {
    es: &'a [u8],
    /// Where the next start code prefix begins.
    next: Option<usize>,
}

/// One start code and the data after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartCode<'a> {
    /// Where its `00 00 01` prefix begins in the stream.
    pub offset: usize,
    pub code: u8,
    pub data: &'a [u8],
}

impl<'a> Iterator for StartCodes<'a> {
    type Item = StartCode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next?;
        let Some(&code) = self.es.get(offset + 3) else {
            self.next = None;
            return None;
        };
        self.next = find_prefix(self.es, offset + 4);
        let end = self.next.unwrap_or(self.es.len());
        Some(StartCode {
            offset,
            code,
            data: &self.es[offset + 4..end],
        })
    }
}

/// The first whole start code at or after `from`: where its prefix begins and
/// its code byte. Unlike [`start_codes`], this does not look for the next one.
#[must_use]
pub fn next_start_code(es: &[u8], from: usize) -> Option<(usize, u8)> {
    let offset = find_prefix(es, from)?;
    Some((offset, *es.get(offset + 3)?))
}

/// The first `00 00 01` at or after `from`.
fn find_prefix(es: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < es.len() {
        let b = es[i + 2];
        if b > 1 {
            i += 3;
        } else if b == 1 && es[i] == 0 && es[i + 1] == 0 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

pub const PICTURE: u8 = 0x00;
pub const USER_DATA: u8 = 0xb2;
pub const SEQUENCE_HEADER: u8 = 0xb3;
pub const EXTENSION: u8 = 0xb5;
pub const SEQUENCE_END: u8 = 0xb7;
pub const GROUP: u8 = 0xb8;

/// True for a slice start code.
#[must_use]
pub const fn is_slice(code: u8) -> bool {
    matches!(code, 0x01..=0xaf)
}

/// `picture_coding_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PictureType {
    Intra,
    Predictive,
    Bidirectional,
    /// DC intra-coded (MPEG-1).
    DcIntra,
    /// A forbidden or reserved value.
    Reserved(u8),
}

impl PictureType {
    #[must_use]
    pub const fn is_reference(self) -> bool {
        matches!(self, Self::Intra | Self::Predictive)
    }
}

/// `picture_structure`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PictureStructure {
    TopField,
    BottomField,
    #[default]
    Frame,
}

/// The fields of a picture header (§6.2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureHeader {
    pub temporal_reference: u16,
    pub picture_type: PictureType,
}

impl PictureHeader {
    /// Reads a picture header's data, `None` when cut short.
    #[must_use]
    pub fn parse(data: &[u8]) -> Option<Self> {
        let &[a, b, ..] = data else { return None };
        Some(Self {
            temporal_reference: (u16::from(a) << 2) | u16::from(b >> 6),
            picture_type: match (b >> 3) & 7 {
                1 => PictureType::Intra,
                2 => PictureType::Predictive,
                3 => PictureType::Bidirectional,
                4 => PictureType::DcIntra,
                other => PictureType::Reserved(other),
            },
        })
    }
}

/// The fields of a picture coding extension (§6.2.3.1) that place a picture in
/// time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PictureCoding {
    pub structure: PictureStructure,
    pub top_field_first: bool,
    pub repeat_first_field: bool,
    pub progressive_frame: bool,
}

impl PictureCoding {
    /// Reads an extension's data, `None` unless it is a complete picture coding
    /// extension.
    #[must_use]
    pub fn parse(data: &[u8]) -> Option<Self> {
        let &[id, _, structure, flags, progressive, ..] = data else {
            return None;
        };
        if id >> 4 != 8 {
            return None;
        }
        Some(Self {
            structure: match structure & 3 {
                1 => PictureStructure::TopField,
                2 => PictureStructure::BottomField,
                _ => PictureStructure::Frame,
            },
            top_field_first: flags & 0x80 != 0,
            repeat_first_field: flags & 0x02 != 0,
            progressive_frame: progressive & 0x80 != 0,
        })
    }
}

/// The frame rate of a sequence header (`frame_rate_code`, Table 6-4) with the
/// sequence extension's `frame_rate_extension_n` and `_d`, as a ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

/// The frame rate of a sequence header's data, `None` for a reserved code.
#[must_use]
pub fn sequence_frame_rate(data: &[u8]) -> Option<FrameRate> {
    let code = data.get(3)? & 0x0f;
    let (numerator, denominator) = match code {
        1 => (24_000, 1001),
        2 => (24, 1),
        3 => (25, 1),
        4 => (30_000, 1001),
        5 => (30, 1),
        6 => (50, 1),
        7 => (60_000, 1001),
        8 => (60, 1),
        _ => return None,
    };
    Some(FrameRate {
        numerator,
        denominator,
    })
}

/// The fields of a sequence extension (§6.2.2.3) that time pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceExtension {
    pub progressive_sequence: bool,
    pub frame_rate_extension_n: u8,
    pub frame_rate_extension_d: u8,
}

impl SequenceExtension {
    /// Reads an extension's data, `None` unless it is a complete sequence
    /// extension.
    #[must_use]
    pub fn parse(data: &[u8]) -> Option<Self> {
        let &[id, flags, _, _, _, rate, ..] = data else {
            return None;
        };
        (id >> 4 == 1).then_some(Self {
            progressive_sequence: flags & 0x08 != 0,
            frame_rate_extension_n: (rate >> 5) & 3,
            frame_rate_extension_d: rate & 0x1f,
        })
    }

    /// The frame rate of a sequence header with this extension's factor.
    #[must_use]
    pub fn frame_rate(&self, rate: FrameRate) -> FrameRate {
        FrameRate {
            numerator: rate.numerator * (u32::from(self.frame_rate_extension_n) + 1),
            denominator: rate.denominator * (u32::from(self.frame_rate_extension_d) + 1),
        }
    }
}

impl PictureCoding {
    /// How long the picture is shown, in field periods (§6.3.10): two, three
    /// for a progressive frame repeating its first field in an interlaced
    /// sequence, and in a progressive sequence four or, with the top field
    /// first, six for a repeated frame.
    #[must_use]
    pub const fn display_fields(&self, progressive_sequence: bool) -> u32 {
        match (self.repeat_first_field, progressive_sequence) {
            (false, _) => 2,
            (true, true) if self.top_field_first => 6,
            (true, true) => 4,
            (true, false) if self.progressive_frame => 3,
            (true, false) => 2,
        }
    }
}

/// The user data payloads of one picture: the data of each user data start code
/// after `picture` and before its first slice.
pub fn picture_user_data<'a>(
    mut after_picture: impl Iterator<Item = StartCode<'a>>,
) -> impl Iterator<Item = &'a [u8]> {
    std::iter::from_fn(move || {
        loop {
            let unit = after_picture.next()?;
            match unit.code {
                USER_DATA => return Some(unit.data),
                code if is_slice(code) || matches!(code, PICTURE | SEQUENCE_HEADER | GROUP) => {
                    return None;
                }
                _ => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_start_codes_and_headers() {
        let es = [
            0, 0, 1, 0x00, 0x00, 0x8f, 0xff, // picture: temporal reference 2, I
            0, 0, 1, 0xb5, 0x8f, 0xff, 0xf3, 0x41, 0x80, // coding extension
            0, 0, 1, 0xb2, b'G', b'A', b'9', b'4', // user data
            0, 0, 1, 0x01, 0x12, // slice
        ];
        let codes: Vec<_> = start_codes(&es).map(|u| u.code).collect();
        assert_eq!(codes, [0x00, 0xb5, 0xb2, 0x01]);
        let mut units = start_codes(&es);
        let picture = units.next().expect("picture");
        let header = PictureHeader::parse(picture.data).expect("header");
        assert_eq!(header.temporal_reference, 2);
        assert_eq!(header.picture_type, PictureType::Intra);
        let user: Vec<_> = picture_user_data(units).collect();
        assert_eq!(user, [&b"GA94"[..]]);
        let coding = PictureCoding::parse(&[0x8f, 0xff, 0xf3, 0x41, 0x80]).expect("coding");
        assert_eq!(coding.structure, PictureStructure::Frame);
        assert!(coding.progressive_frame && !coding.repeat_first_field);
    }

    #[test]
    fn repeated_fields_lengthen_a_picture() {
        let extension = SequenceExtension::parse(&[0x14, 0x8a, 0, 1, 0, 0x21]).expect("extension");
        assert!(extension.progressive_sequence);
        let rate = extension.frame_rate(FrameRate {
            numerator: 30_000,
            denominator: 1001,
        });
        assert_eq!((rate.numerator, rate.denominator), (60_000, 2002));
        let picture = |top_field_first, repeat_first_field, progressive_frame| PictureCoding {
            structure: PictureStructure::Frame,
            top_field_first,
            repeat_first_field,
            progressive_frame,
        };
        assert_eq!(picture(false, false, true).display_fields(false), 2);
        assert_eq!(picture(false, true, true).display_fields(false), 3);
        assert_eq!(picture(true, true, false).display_fields(false), 2);
        assert_eq!(picture(false, true, true).display_fields(true), 4);
        assert_eq!(picture(true, true, true).display_fields(true), 6);
    }
}
