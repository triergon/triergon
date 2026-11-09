//! Caption user data: ATSC A/53 Part 4 `ATSC_user_data()` (identifier `GA94`,
//! carried in MPEG-2 picture user data and in ITU-T T.35 SEI messages of
//! H.264 and HEVC), SCTE 20 user data and the line 21 user data of DVD-Video.

use triergon_bitstream::{BitCursor, BitSource};

/// `user_data_identifier` of ATSC user data.
pub const ATSC_IDENTIFIER: [u8; 4] = *b"GA94";
/// `user_data_type_code` of `cc_data()`.
const CC_DATA: u8 = 0x03;

/// The `cc_data()` of one `ATSC_user_data()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtscCaptions<'a> {
    pub process_em_data: bool,
    pub process_cc_data: bool,
    pub additional_data: bool,
    /// `cc_count` as sent.
    pub cc_count: u8,
    pub em_data: u8,
    /// The constructs present, at most `cc_count` (see [`cc_data::triplets`](super::cc_data::triplets)).
    pub cc_data: &'a [u8],
}

impl AtscCaptions<'_> {
    /// True when all `cc_count` constructs are present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.cc_data.len() == usize::from(self.cc_count) * 3
    }
}

/// The captions of `ATSC_user_data()` (from `user_data_identifier` on), `None`
/// when it is not `GA94` `cc_data()`.
#[must_use]
pub fn atsc_captions(user_data: &[u8]) -> Option<AtscCaptions<'_>> {
    let rest = user_data.strip_prefix(&ATSC_IDENTIFIER)?;
    let (&[kind, flags, em_data], rest) = rest.split_first_chunk::<3>()?;
    if kind != CC_DATA {
        return None;
    }
    let cc_count = flags & 0x1f;
    let present = rest.len() / 3 * 3;
    Some(AtscCaptions {
        process_em_data: flags & 0x80 != 0,
        process_cc_data: flags & 0x40 != 0,
        additional_data: flags & 0x20 != 0,
        cc_count,
        em_data,
        cc_data: &rest[..present.min(usize::from(cc_count) * 3)],
    })
}

/// `itu_t_t35_country_code` of the United States.
const T35_UNITED_STATES: u8 = 0xb5;
/// `itu_t_t35_provider_code` of ATSC.
const T35_ATSC: [u8; 2] = [0x00, 0x31];

/// The `ATSC_user_data()` of an ITU-T T.35 payload (`user_data_registered_itu_t_t35`
/// SEI, H.264 D.1.6 and HEVC D.2.6), `None` for another country or provider.
#[must_use]
pub fn t35_atsc_user_data(payload: &[u8]) -> Option<&[u8]> {
    let rest = payload.strip_prefix(&[T35_UNITED_STATES])?;
    rest.strip_prefix(&T35_ATSC)
}

/// One caption pair of SCTE 20 user data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scte20Caption {
    pub priority: u8,
    /// `field_number`: 1 odd field, 2 even field, 3 the repeated field (0 is forbidden).
    pub field: u8,
    /// `line_offset` from line 10.
    pub line_offset: u8,
    /// `cc_data_1` and `cc_data_2` in transmission bit order (bit 1 first),
    /// that is as line 21 bytes.
    pub data: [u8; 2],
}

/// The caption pairs of SCTE 20 user data (from `user_data_type_code` on),
/// `None` when it does not carry them.
///
/// The syntax: `user_data_type_code` (`0x03`), a byte whose low seven bits are
/// 1, `cc_count` (5 bits) and per pair `cc_priority` (2), `field_number` (2),
/// `line_offset` (5), `cc_data_1` (8), `cc_data_2` (8) and a marker bit. The
/// data bytes are sent least significant bit first.
#[must_use]
pub fn scte20_captions(user_data: &[u8]) -> Option<Scte20Captions<'_>> {
    let (&[kind, flag], rest) = user_data.split_first_chunk::<2>()?;
    if kind != CC_DATA || flag & 0x7f != 0x01 {
        return None;
    }
    let mut bits = BitCursor::new(rest);
    let count = bits.read(5).ok()?;
    Some(Scte20Captions {
        bits,
        remaining: count as u8,
    })
}

/// Iterator over SCTE 20 caption pairs; a pair cut short ends it.
#[derive(Clone, Debug)]
pub struct Scte20Captions<'a> {
    bits: BitCursor<'a>,
    remaining: u8,
}

impl Scte20Captions<'_> {
    /// The pairs announced and not yet read, present or not.
    #[must_use]
    pub fn remaining(&self) -> usize {
        usize::from(self.remaining)
    }
}

impl Iterator for Scte20Captions<'_> {
    type Item = Scte20Caption;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let bits = &mut self.bits;
        let caption = (|| {
            let priority = bits.read(2).ok()? as u8;
            let field = bits.read(2).ok()? as u8;
            let line_offset = bits.read(5).ok()? as u8;
            let first = (bits.read(8).ok()? as u8).reverse_bits();
            let second = (bits.read(8).ok()? as u8).reverse_bits();
            bits.read(1).ok()?;
            Some(Scte20Caption {
                priority,
                field,
                line_offset,
                data: [first, second],
            })
        })();
        if caption.is_none() {
            self.remaining = 0;
        }
        caption
    }
}

/// The identifier of DVD-Video line 21 user data: `CC`, `user_data_type_code`
/// 0x01 and `caption_block_size` 0xF8.
pub const DVD_IDENTIFIER: [u8; 4] = *b"CC\x01\xf8";

/// The line 21 user data of a DVD-Video GOP.
///
/// After the identifier, one byte: `caption_odd_field_first` (bit 7), a filler
/// bit, `caption_block_count` (bits 5 to 1) and `caption_extra_field_added`
/// (bit 0). Then `2 * caption_block_count + caption_extra_field_added`
/// caption words of three bytes: seven filler bits, `caption_field_odd`
/// (bit 0), and the two line 21 bytes of that field. Words alternate between
/// the fields from the one `caption_odd_field_first` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdCaptions<'a> {
    pub odd_field_first: bool,
    pub block_count: u8,
    pub extra_field: bool,
    /// The bytes after the header byte.
    pub words: &'a [u8],
}

/// One DVD caption word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvdWord {
    /// `caption_field_odd`: field 1 (CC1, CC2) data.
    pub odd_field: bool,
    pub data: [u8; 2],
}

impl DvdCaptions<'_> {
    /// The words the header announces, as far as they are present.
    pub fn words(&self) -> impl Iterator<Item = DvdWord> + '_ {
        let count = 2 * usize::from(self.block_count) + usize::from(self.extra_field);
        self.words.chunks_exact(3).take(count).map(|w| DvdWord {
            odd_field: w[0] & 1 == 1,
            data: [w[1], w[2]],
        })
    }
}

/// The DVD line 21 user data (from the identifier on), `None` for other data.
#[must_use]
pub fn dvd_captions(user_data: &[u8]) -> Option<DvdCaptions<'_>> {
    let rest = user_data.strip_prefix(&DVD_IDENTIFIER)?;
    let (&header, words) = rest.split_first()?;
    Some(DvdCaptions {
        odd_field_first: header & 0x80 != 0,
        block_count: (header >> 1) & 0x1f,
        extra_field: header & 1 == 1,
        words,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_atsc_captions() {
        let data = b"GA94\x03\x42\xff\xfc\x94\x20\xfd\x80\x80\xff";
        let captions = atsc_captions(data).expect("captions");
        assert!(captions.process_cc_data && captions.is_complete());
        assert_eq!(captions.cc_data, &data[7..13]);
        assert!(atsc_captions(b"DTG1\x03\x42\xff").is_none());
        assert_eq!(t35_atsc_user_data(b"\xb5\x00\x31GA94"), Some(&b"GA94"[..]));
    }

    #[test]
    fn reads_scte20_pairs() {
        // Two pairs of the FATE sample's first picture: null pairs on fields 1 and 2.
        let data = [0x03, 0x81, 0x10, 0xac, 0x04, 0x06, 0x4b, 0x01, 0x01, 0x80];
        let pairs: Vec<_> = scte20_captions(&data).expect("pairs").collect();
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            (pairs[0].field, pairs[0].line_offset, pairs[0].data),
            (1, 11, [0x80, 0x80])
        );
        assert_eq!((pairs[1].field, pairs[1].data), (2, [0x80, 0x80]));
    }

    #[test]
    fn reads_dvd_words() {
        let data = b"CC\x01\xf8\x83\xff\x94\x20\xfe\x80\x80\xff\xc1\xc2";
        let captions = dvd_captions(data).expect("captions");
        assert!(captions.odd_field_first && captions.extra_field);
        let words: Vec<_> = captions.words().map(|w| (w.odd_field, w.data)).collect();
        assert_eq!(
            words,
            [
                (true, [0x94, 0x20]),
                (false, [0x80, 0x80]),
                (true, [0xc1, 0xc2])
            ]
        );
    }
}
