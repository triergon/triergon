//! Strict multibyte charset profiles, each bound to one mapping table, and a
//! decoder that reads them a byte at a time. No charset is detected.
//!
//! The mapping data and licences are described in `MULTIBYTE-CONTRACT.md`.
//! A profile follows the current definition of its charset: the Encoding
//! Standard's decoder, a Windows code page as Windows converts it, or a
//! national standard. The `Receiver` profiles follow the C library of the
//! reference build instead, for
//! [`receiver_charset`](crate::compat::charset::receiver_charset).
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, Result, WorkBudget};

use crate::cancelled;

/// A multibyte charset profile: its lead and trail bytes and its mapping
/// table. Standalone ASCII bytes decode as themselves except where noted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MultibyteCharset {
    /// Microsoft CP936 table 2.01, including its single-byte euro mapping.
    GbkCp936,
    /// Big5 as the Encoding Standard decodes it: its index, with the Hong Kong
    /// supplement and its plane 2 characters. The codes 88 62, 88 64, 88 A3
    /// and 88 A5 are each two characters, a letter and a combining mark.
    Big5,
    /// Unicode historical Shift-JIS table 2.0, which decodes 0x5C as yen and
    /// 0x7E as overline, with 81 5F, JIS X 0208's reverse solidus, as
    /// U+FF3C as JIS0208.TXT maps it.
    ShiftJisUnicode,
    /// KS X 1001:2002 in EUC-KR: CP949 table 2.01 restricted to A1–FE byte
    /// pairs, plus A2 E8 as U+327E, which KS X 1001:2002 adds, and ASCII; the
    /// UHC extension pairs are refused.
    EucKr,
    /// Microsoft CP932 table 2.01 plus the 1,880 user-defined codes
    /// F040–F9FC as U+E000–U+E757, as Windows converts them.
    Windows932,
    /// Microsoft CP949 table 2.01, including the UHC extension pairs.
    Windows949,
    /// Code page 950 as Windows converts it: Microsoft CP950 table 2.01, the
    /// 6,217 user-defined codes FA40–FEFE, 8E40–A0FE, 8140–8DFE and C6A1–C8FE
    /// as U+E000–U+F848 in that order, and the single bytes 0x80 and 0xFF as
    /// U+0080 and U+F8F8.
    Windows950,
    /// The GNU Big5 charmap 0.92 as glibc 2.39 has it, which its `iconv`
    /// converts both `big5` and `cp950` with: the repertoire FFmpeg decodes.
    ReceiverBig5,
    /// [`EucKr`](Self::EucKr) plus bytes 0x80–0x9F as the C1 controls: the
    /// glibc 2.39 EUC-KR charmap.
    ReceiverEucKrC1,
}

impl MultibyteCharset {
    /// The profile of `label`, matched ASCII case-insensitively with no
    /// trimming: gbk, cp936, windows-936, big5, big-5, shift_jis, shift-jis,
    /// sjis, euc-kr, euc_kr, cp932, windows-932, cp949, windows-949, cp950
    /// or windows-950. `Error::Limit` for a label over 256 bytes,
    /// `Error::Unsupported` for any other. The `Receiver` profiles have no
    /// label here.
    pub fn from_label(label: &[u8]) -> Result<Self> {
        if label.len() > 256 {
            return Err(Error::Limit {
                requested: label.len(),
                maximum: 256,
            });
        }
        for (name, profile) in [
            ("gbk", Self::GbkCp936),
            ("cp936", Self::GbkCp936),
            ("windows-936", Self::GbkCp936),
            ("big5", Self::Big5),
            ("big-5", Self::Big5),
            ("shift_jis", Self::ShiftJisUnicode),
            ("shift-jis", Self::ShiftJisUnicode),
            ("sjis", Self::ShiftJisUnicode),
            ("euc-kr", Self::EucKr),
            ("euc_kr", Self::EucKr),
            ("cp932", Self::Windows932),
            ("windows-932", Self::Windows932),
            ("cp949", Self::Windows949),
            ("windows-949", Self::Windows949),
            ("cp950", Self::Windows950),
            ("windows-950", Self::Windows950),
        ] {
            if label.eq_ignore_ascii_case(name.as_bytes()) {
                return Ok(profile);
            }
        }
        Err(Error::Unsupported("explicit multibyte charset label"))
    }

    /// Every profile, in declaration order.
    const ALL: [Self; 9] = [
        Self::GbkCp936,
        Self::Big5,
        Self::ShiftJisUnicode,
        Self::EucKr,
        Self::Windows932,
        Self::Windows949,
        Self::Windows950,
        Self::ReceiverBig5,
        Self::ReceiverEucKrC1,
    ];

    /// The profile's mapping table: sorted fixed-width records of a two-byte
    /// wire code and a two-byte scalar.
    const fn table(self) -> &'static [u8] {
        match self {
            Self::GbkCp936 => include_bytes!("mapping-data/gbk.bin"),
            Self::Big5 => include_bytes!("mapping-data/big5.bin"),
            Self::ShiftJisUnicode => include_bytes!("mapping-data/shift_jis.bin"),
            Self::EucKr | Self::ReceiverEucKrC1 => include_bytes!("mapping-data/euc_kr.bin"),
            Self::Windows932 => include_bytes!("mapping-data/windows932.bin"),
            Self::Windows949 => include_bytes!("mapping-data/windows949.bin"),
            Self::Windows950 => include_bytes!("mapping-data/windows950.bin"),
            Self::ReceiverBig5 => include_bytes!("mapping-data/receiver_big5.bin"),
        }
    }

    /// The last byte the profile decodes as itself although its table does
    /// not hold it: ASCII's, or the receiver EUC-KR's C1 control 0x9F.
    const fn as_itself(self) -> u8 {
        match self {
            Self::ReceiverEucKrC1 => 0x9f,
            _ => 0x7f,
        }
    }

    /// The character of a [`Big5`](Self::Big5) code in plane 2.
    fn big5_plane2(code: u16) -> Option<char> {
        let (records, _) = include_bytes!("mapping-data/big5_plane2.bin").as_chunks::<4>();
        let at = records
            .binary_search_by_key(&code, |&[lead, trail, ..]| {
                u16::from_be_bytes([lead, trail])
            })
            .ok()?;
        let [_, _, high, low] = records[at];
        char::from_u32(0x2_0000 | u32::from(u16::from_be_bytes([high, low])))
    }

    /// The inclusive ranges of the bytes that may lead a pair. Big5's are
    /// those of its standard part; the decoder takes the supplement's, 0x81 to
    /// 0xA0 and 0xFA to 0xFE, with what else lies outside its table.
    const fn lead_ranges(self) -> &'static [(u8, u8)] {
        match self {
            Self::ShiftJisUnicode => &[(0x81, 0x9f), (0xe0, 0xef)],
            Self::Windows932 => &[(0x81, 0x9f), (0xe0, 0xfc)],
            Self::Big5 | Self::ReceiverBig5 => &[(0xa1, 0xf9)],
            Self::EucKr | Self::ReceiverEucKrC1 => &[(0xa1, 0xfe)],
            Self::GbkCp936 | Self::Windows949 | Self::Windows950 => &[(0x81, 0xfe)],
        }
    }

    /// The inclusive ranges of the bytes that may follow a lead byte.
    const fn trail_ranges(self) -> &'static [(u8, u8)] {
        match self {
            Self::ShiftJisUnicode | Self::Windows932 => &[(0x40, 0x7e), (0x80, 0xfc)],
            Self::GbkCp936 => &[(0x40, 0x7e), (0x80, 0xfe)],
            Self::Big5 | Self::Windows950 | Self::ReceiverBig5 => &[(0x40, 0x7e), (0xa1, 0xfe)],
            Self::EucKr | Self::ReceiverEucKrC1 => &[(0xa1, 0xfe)],
            Self::Windows949 => &[(0x41, 0x5a), (0x61, 0x7a), (0x81, 0xfe)],
        }
    }

    // The decoder asks for no ASCII byte. A byte beyond it is searched for
    // among the profile's records of such bytes and a pair among its pairs: at
    // most 16 binary-search comparisons; no allocation.
    //
    // Out of line, as are `lead` and `trail`: inlined, any of them makes the
    // decoder's step dearer for every character, ASCII included.
    #[inline(never)]
    fn lookup(self, code: u16) -> Option<char> {
        let Span {
            as_itself,
            bytes,
            pairs,
            end,
        } = SPANS[self as usize];
        let (mut low, mut high) = if let Ok(byte) = u8::try_from(code) {
            if byte <= as_itself {
                return Some(char::from(byte));
            }
            (bytes, pairs)
        } else {
            (pairs, end)
        };
        while low < high {
            let mid = low + (high - low) / 2;
            let at = mid * 4;
            let key = u16::from_be_bytes([TABLES[at], TABLES[at + 1]]);
            match key.cmp(&code) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    return char::from_u32(u32::from(u16::from_be_bytes([
                        TABLES[at + 2],
                        TABLES[at + 3],
                    ])));
                }
            }
        }
        None
    }

    /// Whether `byte` may lead a pair.
    #[inline(never)]
    fn lead(self, byte: u8) -> bool {
        CLASSES[self as usize][usize::from(byte)] & LEAD != 0
    }

    /// Whether `byte` may follow a lead byte.
    #[inline(never)]
    fn trail(self, byte: u8) -> bool {
        CLASSES[self as usize][usize::from(byte)] & TRAIL != 0
    }
}

const PROFILES: usize = MultibyteCharset::ALL.len();

/// Every profile's table, each table once, one after another. A profile's
/// records are found by an index into it, which needs neither a jump on the
/// profile nor a relocation of the binary, as a slice per profile would.
static TABLES: [u8; LAYOUT.len] = {
    let mut tables = [0; LAYOUT.len];
    let mut index = 0;
    while index < PROFILES {
        let table = MultibyteCharset::ALL[index].table();
        let (_, rest) = tables.split_at_mut(LAYOUT.starts[index]);
        rest.split_at_mut(table.len()).0.copy_from_slice(table);
        index += 1;
    }
    tables
};

/// Each profile's [`Span`], in [`MultibyteCharset::ALL`]'s order.
static SPANS: [Span; PROFILES] = LAYOUT.spans;

/// Where a profile's records lie in [`TABLES`], counted in records: those of
/// ASCII bytes, those of bytes beyond ASCII and those of pairs follow each
/// other.
#[derive(Clone, Copy)]
struct Span {
    /// [`MultibyteCharset::as_itself`].
    as_itself: u8,
    /// The first record of a byte beyond ASCII.
    bytes: usize,
    /// The first record of a pair.
    pairs: usize,
    /// The end of the profile's records.
    end: usize,
}

/// Where each profile's table starts in [`TABLES`], and the length of it.
struct Layout {
    spans: [Span; PROFILES],
    /// The byte at which each profile's table starts.
    starts: [usize; PROFILES],
    len: usize,
}

const LAYOUT: Layout = {
    let mut layout = Layout {
        spans: [Span {
            as_itself: 0,
            bytes: 0,
            pairs: 0,
            end: 0,
        }; PROFILES],
        starts: [0; PROFILES],
        len: 0,
    };
    let mut index = 0;
    while index < PROFILES {
        let profile = MultibyteCharset::ALL[index];
        assert!(
            profile as usize == index,
            "MultibyteCharset::ALL out of order"
        );
        let table = profile.table();
        let mut earlier = 0;
        while earlier < index && !same_bytes(MultibyteCharset::ALL[earlier].table(), table) {
            earlier += 1;
        }
        let start = if earlier < index {
            layout.starts[earlier]
        } else {
            layout.len += table.len();
            layout.len - table.len()
        };
        let (first, records) = (start / 4, table.len() / 4);
        let mut bytes = 0;
        while bytes < records && table[bytes * 4] == 0 && table[bytes * 4 + 1] < 0x80 {
            bytes += 1;
        }
        let mut pairs = bytes;
        while pairs < records && table[pairs * 4] == 0 {
            pairs += 1;
        }
        layout.starts[index] = start;
        layout.spans[index] = Span {
            as_itself: profile.as_itself(),
            bytes: first + bytes,
            pairs: first + pairs,
            end: first + records,
        };
        index += 1;
    }
    layout
};

const fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut at = 0;
    while at < a.len() {
        if a[at] != b[at] {
            return false;
        }
        at += 1;
    }
    true
}

/// Each profile's classes of every byte, [`LEAD`] and [`TRAIL`], in
/// [`MultibyteCharset::ALL`]'s order: a test of a byte is a load, whichever
/// the profile.
static CLASSES: [[u8; 256]; PROFILES] = {
    let mut classes = [[0; 256]; PROFILES];
    let mut index = 0;
    while index < PROFILES {
        let profile = MultibyteCharset::ALL[index];
        mark(&mut classes[index], profile.lead_ranges(), LEAD);
        mark(&mut classes[index], profile.trail_ranges(), TRAIL);
        index += 1;
    }
    classes
};

/// The class of a byte that may lead a pair.
const LEAD: u8 = 1;
/// The class of a byte that may follow a lead byte.
const TRAIL: u8 = 2;

const fn mark(classes: &mut [u8; 256], ranges: &[(u8, u8)], class: u8) {
    let mut range = 0;
    while range < ranges.len() {
        let (first, last) = ranges[range];
        let mut byte = first as usize;
        while byte <= last as usize {
            classes[byte] |= class;
            byte += 1;
        }
        range += 1;
    }
}

/// Lead bytes that stand for a combining mark still to give, the second
/// character of a [`MultibyteCharset::Big5`] code that is two: a macron and a
/// caron. Neither is a Big5 lead byte.
const MACRON_DUE: u8 = 0x80;
const CARON_DUE: u8 = 0xff;

/// A decoded character and the bytes it was decoded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedScalar {
    /// The Unicode scalar the bytes map to.
    pub value: char,
    /// Its byte range in the caller's stream, counted from the decoder's
    /// origin.
    pub range: Range<usize>,
}

/// What [`MultibyteDecoder::push`] did with a byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeStep {
    /// The byte was taken as a lead byte; push its trail byte. This is not
    /// the end of input.
    NeedInput,
    /// The byte completed a character.
    Scalar(DecodedScalar),
    /// The work budget ran out: no byte was consumed and no state changed.
    Yield,
}

/// Decodes a multibyte profile a byte at a time, holding at most one lead
/// byte and allocating nothing. Each byte costs one work unit, its table
/// lookup included, and chunk boundaries do not change the result. Only
/// [`finish`](Self::finish) ends the input: a pending lead byte is not
/// truncation before it. Errors other than cancellation are sticky.
#[derive(Debug)]
pub struct MultibyteDecoder {
    profile: MultibyteCharset,
    position: usize,
    lead: Option<u8>,
    finished: bool,
    failed: Option<Error>,
}

// Fixed inline state on both 64-bit development and 32-bit source targets.
const _: () = assert!(size_of::<MultibyteDecoder>() <= 64);
const _: () = assert!(size_of::<DecodedScalar>() <= 32);
const _: () = assert!(size_of::<DecodeStep>() <= 48);

impl MultibyteDecoder {
    /// A decoder of `profile` whose byte positions start at 0.
    pub fn new(profile: MultibyteCharset) -> Self {
        Self::at_offset(profile, 0)
    }

    /// A decoder of `profile` whose byte positions start at `offset`.
    pub fn at_offset(profile: MultibyteCharset, offset: usize) -> Self {
        Self {
            profile,
            position: offset,
            lead: None,
            finished: false,
            failed: None,
        }
    }

    /// The position of the next byte to push.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Decodes `byte`, the byte at [`position`](Self::position). The trail
    /// byte of a [`MultibyteCharset::Big5`] code that is two characters gives
    /// the letter and stays the next byte to push; pushed again it gives the
    /// combining mark. `Error::Invalid` for input after
    /// [`finish`](Self::finish), a trail byte the profile refuses, or a byte
    /// or pair its table does not map; `Error::Overflow` when the position
    /// does not fit `usize`.
    pub fn push(
        &mut self,
        byte: u8,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DecodeStep> {
        let step = self.push_table(byte, work, cancel);
        match step {
            Err(error) => self.outside_table(byte).ok_or(error),
            step => step,
        }
    }

    /// [`push`](Self::push) for what the profile's table and lead bytes hold;
    /// what else [`MultibyteCharset::Big5`] decodes is refused here and taken
    /// by [`outside_table`](Self::outside_table).
    pub(crate) fn push_table(
        &mut self,
        byte: u8,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<DecodeStep> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !work.try_charge(1) {
            return Ok(DecodeStep::Yield);
        }
        let result = self.consume(byte);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }

    fn consume(&mut self, byte: u8) -> Result<DecodeStep> {
        if self.finished {
            return Err(Error::Invalid("multibyte input after finish"));
        }
        let next = self.position.checked_add(1).ok_or(Error::Overflow)?;
        if let Some(lead) = self.lead {
            if !self.profile.trail(byte) {
                return Err(Error::Invalid("invalid multibyte trail"));
            }
            let code = u16::from_be_bytes([lead, byte]);
            let value = self
                .profile
                .lookup(code)
                .ok_or(Error::Invalid("unmapped multibyte pair"))?;
            let start = self.position.checked_sub(1).ok_or(Error::Overflow)?;
            self.lead = None;
            self.position = next;
            return Ok(DecodeStep::Scalar(DecodedScalar {
                value,
                range: start..next,
            }));
        }
        // Historical Shift-JIS maps 0x5C to yen and 0x7E to overline.
        let single = if byte.is_ascii() {
            Some(match (self.profile, byte) {
                (MultibyteCharset::ShiftJisUnicode, b'\\') => '\u{00a5}',
                (MultibyteCharset::ShiftJisUnicode, b'~') => '\u{203e}',
                _ => char::from(byte),
            })
        } else {
            self.profile.lookup(u16::from(byte))
        };
        if let Some(value) = single {
            let start = self.position;
            self.position = next;
            return Ok(DecodeStep::Scalar(DecodedScalar {
                value,
                range: start..next,
            }));
        }
        if self.profile.lead(byte) {
            self.lead = Some(byte);
            self.position = next;
            return Ok(DecodeStep::NeedInput);
        }
        Err(Error::Invalid("unmapped multibyte byte"))
    }

    /// The step for `byte`, which [`push_table`](Self::push_table) has just
    /// refused, where [`MultibyteCharset::Big5`] decodes it all the same: a
    /// lead byte of the supplement, a plane 2 character, or one of the codes
    /// that are two characters. `None` leaves the refusal standing.
    #[cold]
    pub(crate) fn outside_table(&mut self, byte: u8) -> Option<DecodeStep> {
        if self.profile != MultibyteCharset::Big5 || self.finished {
            return None;
        }
        self.failed?;
        let next = self.position.checked_add(1)?;
        let Some(lead) = self.lead else {
            if !matches!(byte, 0x81..=0xa0 | 0xfa..=0xfe) {
                return None;
            }
            self.failed = None;
            self.lead = Some(byte);
            self.position = next;
            return Some(DecodeStep::NeedInput);
        };
        if !self.profile.trail(byte) {
            return None;
        }
        let start = self.position.checked_sub(1)?;
        let (value, range, lead) = match [lead, byte] {
            // The mark of a letter already given: the trail byte's second turn.
            [MACRON_DUE, _] => ('\u{304}', self.position..next, None),
            [CARON_DUE, _] => ('\u{30c}', self.position..next, None),
            // The letter, with the trail byte left for its mark.
            [0x88, 0x62] => ('\u{ca}', start..self.position, Some(MACRON_DUE)),
            [0x88, 0x64] => ('\u{ca}', start..self.position, Some(CARON_DUE)),
            [0x88, 0xa3] => ('\u{ea}', start..self.position, Some(MACRON_DUE)),
            [0x88, 0xa5] => ('\u{ea}', start..self.position, Some(CARON_DUE)),
            code => (
                MultibyteCharset::big5_plane2(u16::from_be_bytes(code))?,
                start..next,
                None,
            ),
        };
        self.failed = None;
        self.lead = lead;
        self.position = range.end;
        Some(DecodeStep::Scalar(DecodedScalar { value, range }))
    }

    /// Ends the input. A pending lead byte is a sticky `Error::Truncated`;
    /// cancellation is resumable, and finishing again succeeds.
    pub fn finish(&mut self, cancel: &AtomicBool) -> Result<()> {
        cancelled(cancel)?;
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.lead.is_some() {
            self.failed = Some(Error::Truncated);
            return Err(Error::Truncated);
        }
        self.finished = true;
        Ok(())
    }
}
