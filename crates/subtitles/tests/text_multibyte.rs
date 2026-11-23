use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, WorkBudget};
use triergon_subtitles::text::{
    Limits,
    charset::{
        Charset, CharsetJob, CharsetStatus,
        multibyte::{DecodeStep, MultibyteCharset as M, MultibyteDecoder},
    },
};

fn decode(raw: &[u8], profile: M, quantum: u64) -> Result<Vec<u8>, Error> {
    let pool = Budget::new(1024 * 1024).unwrap();
    let source = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = CharsetJob::new(
        &source,
        None,
        Charset::Multibyte(profile),
        &pool,
        Limits::default(),
    )?;
    let cancel = AtomicBool::new(false);
    for _ in 0..1000000 {
        if job.poll(&mut WorkBudget::new(quantum), &cancel)? == CharsetStatus::Ready {
            let out = job.take_output(&cancel)?;
            assert!(out.source.shares_storage_with(&source));
            assert_eq!(out.source.as_ref(), raw);
            return Ok(out.utf8.as_ref().to_vec());
        }
    }
    panic!("bounded conversion did not terminate")
}
#[test]
fn multibyte_missing_api_and_four_selected_scripts() {
    for (profile, raw, text) in [
        (M::Gbk, &[0xd6, 0xd0, 0xce, 0xc4][..], "中文"),
        (M::Big5, &[0xa4, 0xa4, 0xa4, 0xe5][..], "中文"),
        (M::ShiftJis, &[0x93, 0xfa, 0x96, 0x7b][..], "日本"),
        (M::EucKr, &[0xc7, 0xd1, 0xb1, 0xdb][..], "한글"),
    ] {
        for q in [1, 7, 4096] {
            assert_eq!(decode(raw, profile, q).unwrap(), text.as_bytes());
        }
    }
}
#[test]
fn windows_variants_are_observably_distinct() {
    assert_eq!(
        decode(&[0x81, 0x60], M::ReceiverShiftJis, 1).unwrap(),
        "〜".as_bytes()
    );
    for profile in [M::ShiftJis, M::Windows932] {
        assert_eq!(decode(&[0x81, 0x60], profile, 1).unwrap(), "～".as_bytes());
    }
    assert_eq!(decode(&[0xa1, 0x45], M::Big5, 1).unwrap(), "‧".as_bytes());
    assert_eq!(
        decode(&[0xa1, 0x45], M::Windows950, 1).unwrap(),
        "‧".as_bytes()
    );
    // UHC pairs: the receiver's EUC-KR refuses them, the Encoding Standard's
    // decodes them as code page 949 does.
    assert!(decode(&[0xa1, 0x41], M::ReceiverEucKrC1, 1).is_err());
    for profile in [M::EucKr, M::Windows949] {
        assert_eq!(decode(&[0x81, 0x41], profile, 1).unwrap(), "갂".as_bytes());
    }
}
#[test]
fn old_resolver_refusals_are_preserved_and_new_aliases_are_explicit() {
    for name in [
        b"shift_jis".as_slice(),
        b"big5",
        b"gbk",
        b"euc-kr",
        b"cp932",
        b"cp950",
        b"cp949",
    ] {
        assert!(matches!(
            Charset::from_label(name),
            Err(Error::Unsupported(_))
        ));
        assert!(M::from_label(name).is_ok());
    }
    for name in [
        b"GB18030".as_slice(),
        b"big5-hkscs",
        b"ms_kanji",
        b"ks_c_5601-1987",
        b" gbk",
        b"gbk\0",
    ] {
        assert!(matches!(M::from_label(name), Err(Error::Unsupported(_))));
    }
    assert_eq!(M::from_label(b"WINDOWS-932").unwrap(), M::Windows932);
    assert!(matches!(
        M::from_label(&[b'a'; 257]),
        Err(Error::Limit { .. })
    ));
}

const PROFILES: [(M, &str, usize); 7] = [
    (
        M::ReceiverGbk,
        include_str!("multibyte-mapping-data/CP936.TXT"),
        21920,
    ),
    (
        M::ReceiverShiftJis,
        include_str!("multibyte-mapping-data/SHIFTJIS.TXT"),
        7070,
    ),
    (
        M::EucKr,
        include_str!("multibyte-mapping-data/CP949.TXT"),
        17176,
    ),
    (
        M::Windows932,
        include_str!("multibyte-mapping-data/CP932.TXT"),
        9795,
    ),
    (
        M::Windows949,
        include_str!("multibyte-mapping-data/CP949.TXT"),
        17176,
    ),
    (
        M::Windows950,
        include_str!("multibyte-mapping-data/CP950.TXT"),
        19850,
    ),
    (
        M::ShiftJis,
        include_str!("multibyte-mapping-data/CP932.TXT"),
        9796,
    ),
];

// Independent test oracle reads original licensed text records, not packed DATA.
fn mapping(profile: M, original: &str) -> std::collections::BTreeMap<u16, char> {
    let mut map = std::collections::BTreeMap::new();
    for line in original.lines() {
        let mut words = line.split('#').next().unwrap().split_whitespace();
        let Some(code) = words.next() else { continue };
        let Some(value) = words.next() else { continue };
        let Some(value) = value.strip_prefix("0x") else {
            continue;
        };
        let code = u16::from_str_radix(code.strip_prefix("0x").unwrap(), 16).unwrap();
        let value = char::from_u32(u32::from_str_radix(value, 16).unwrap()).unwrap();
        // The receiver's EUC-KR holds KS X 1001's A1–FE byte pairs.
        if profile == M::ReceiverEucKrC1 && !(0xa1..=0xfe).contains(&(code >> 8)) {
            continue;
        }
        if profile == M::ReceiverEucKrC1 && !(0xa1..=0xfe).contains(&(code & 255)) {
            continue;
        }
        assert!(map.insert(code, value).is_none());
    }
    if profile == M::ReceiverShiftJis {
        // JIS X 0208 1-32, the reverse solidus, as JIS0208.TXT, Windows and the
        // WHATWG Shift_JIS decoder map it; SHIFTJIS.TXT gives U+005C.
        assert_eq!(map.insert(0x815f, '\u{ff3c}'), Some('\\'));
    }
    if matches!(profile, M::Windows932 | M::ShiftJis) {
        // CP932.TXT 2.01 leaves out the user-defined area, which Windows
        // converts to the private use area, row by row.
        let trails = (0x40..=0x7e).chain(0x80..=0xfc);
        let codes =
            (0xf0_u16..=0xf9).flat_map(|lead| trails.clone().map(move |trail| lead << 8 | trail));
        for (code, scalar) in codes.zip(0xe000..) {
            assert!(map.insert(code, char::from_u32(scalar).unwrap()).is_none());
        }
    }
    if profile == M::Windows950 {
        // CP950.TXT 2.01 leaves out the four user-defined areas, which Windows
        // converts to the private use area, area by area and row by row, and
        // the two single bytes it converts outside the table.
        let rows = |leads: std::ops::RangeInclusive<u16>, trails: Vec<u16>| {
            leads.flat_map(move |lead| {
                trails
                    .clone()
                    .into_iter()
                    .map(move |trail| lead << 8 | trail)
            })
        };
        let whole: Vec<u16> = (0x40..=0x7e).chain(0xa1..=0xfe).collect();
        let codes = rows(0xfa..=0xfe, whole.clone())
            .chain(rows(0x8e..=0xa0, whole.clone()))
            .chain(rows(0x81..=0x8d, whole.clone()))
            .chain(rows(0xc6..=0xc6, (0xa1..=0xfe).collect()))
            .chain(rows(0xc7..=0xc8, whole));
        for (code, scalar) in codes.zip(0xe000..) {
            assert!(map.insert(code, char::from_u32(scalar).unwrap()).is_none());
        }
        assert!(map.insert(0x80, '\u{80}').is_none());
        assert!(map.insert(0xff, '\u{f8f8}').is_none());
    }
    if profile == M::ShiftJis {
        // The Encoding Standard's Shift_JIS decoder gives 0x80 as U+0080.
        assert!(map.insert(0x80, '\u{80}').is_none());
    }
    if profile == M::ReceiverEucKrC1 {
        // CP949.TXT 2.01 predates A2E8, which KS X 1001:2002 assigns.
        assert!(map.insert(0xa2e8, '\u{327e}').is_none());
    }
    if profile == M::ReceiverEucKrC1 {
        for byte in 0..128 {
            assert!(
                map.insert(byte, char::from_u32(u32::from(byte)).unwrap())
                    .is_none()
            );
        }
    }
    if profile == M::ReceiverShiftJis {
        for byte in (0..32).chain(std::iter::once(127)) {
            assert!(
                map.insert(byte, char::from_u32(u32::from(byte)).unwrap())
                    .is_none()
            );
        }
    }
    map
}

fn whole_mapping(index: usize) {
    let (profile, original, count) = PROFILES[index];
    let table = mapping(profile, original);
    assert_eq!(table.len(), count);
    let mut bytes = Vec::new();
    let mut expected = String::new();
    for (&code, &value) in &table {
        if code <= 255 {
            bytes.push(code as u8);
        } else {
            bytes.extend_from_slice(&code.to_be_bytes());
        }
        expected.push(value);
    }
    for q in [1, 7, 4096] {
        assert_eq!(decode(&bytes, profile, q).unwrap(), expected.as_bytes());
    }
}

fn low_level(raw: &[u8], profile: M) -> Result<String, Error> {
    let mut decoder = MultibyteDecoder::new(profile);
    let cancel = AtomicBool::new(false);
    let mut output = String::new();
    for (offset, &byte) in raw.iter().enumerate() {
        match decoder.push(byte, &mut WorkBudget::new(1), &cancel)? {
            DecodeStep::Scalar(scalar) => {
                assert_eq!(scalar.range.end, offset + 1);
                assert!(scalar.range.len() == 1 || scalar.range.len() == 2);
                output.push(scalar.value);
            }
            DecodeStep::NeedInput => {}
            DecodeStep::Yield => panic!("one byte must fit one work unit"),
        }
    }
    decoder.finish(&cancel)?;
    Ok(output)
}

fn all_short_inputs(index: usize) {
    let (profile, original, _) = PROFILES[index];
    check_short_inputs(profile, &mapping(profile, original));
}

fn check_short_inputs(profile: M, table: &std::collections::BTreeMap<u16, char>) {
    // The Big5 codes that are two characters have a test of their own.
    let two_characters =
        |raw: [u8; 2]| profile == M::Big5 && matches!(raw, [0x88, 0x62 | 0x64 | 0xa3 | 0xa5]);
    for first in 0u16..256 {
        match table.get(&first) {
            Some(value) => assert_eq!(
                low_level(&[first as u8], profile).unwrap(),
                value.to_string()
            ),
            None => assert!(low_level(&[first as u8], profile).is_err()),
        }
        for second in 0u16..256 {
            let raw = [first as u8, second as u8];
            if two_characters(raw) {
                continue;
            }
            let expected = if let Some(a) = table.get(&first) {
                table.get(&second).map(|b| format!("{a}{b}"))
            } else {
                table.get(&u16::from_be_bytes(raw)).map(char::to_string)
            };
            let actual = low_level(&raw, profile);
            if let Some(expected) = expected {
                assert_eq!(actual.unwrap(), expected, "{profile:?} {raw:02x?}");
            } else {
                assert!(
                    actual.is_err(),
                    "unexpected acceptance {profile:?} {raw:02x?}"
                );
            }
        }
    }
}

/// The records of a packed table, with ASCII, which every profile decodes as
/// itself.
fn packed(records: &[u8]) -> std::collections::BTreeMap<u16, char> {
    let (records, remainder) = records.as_chunks::<4>();
    assert!(remainder.is_empty());
    let ascii = (0..128).map(|byte| (byte, char::from(byte as u8)));
    ascii
        .chain(records.iter().map(|&[a, b, c, d]| {
            let code = u16::from_be_bytes([a, b]);
            let value = char::from_u32(u32::from(u16::from_be_bytes([c, d]))).unwrap();
            (code, value)
        }))
        .collect()
}

const GBK: &[u8] = include_bytes!("../src/text/charset/mapping-data/gbk.bin");
const BIG5: &[u8] = include_bytes!("../src/text/charset/mapping-data/big5.bin");
const BIG5_PLANE2: &[u8] = include_bytes!("../src/text/charset/mapping-data/big5_plane2.bin");
const WINDOWS932: &[u8] = include_bytes!("../src/text/charset/mapping-data/windows932.bin");
const WINDOWS949: &[u8] = include_bytes!("../src/text/charset/mapping-data/windows949.bin");

// These profiles' tables come from the Encoding Standard's indexes or the
// receiver's C library. Compare the public decoder, including the direct ASCII
// path, with every one- and two-byte table result.
#[test]
fn every_one_and_two_byte_input_of_the_packed_tables() {
    // Big5's plane 2 table holds the low 16 bits of its code points.
    let mut big5 = packed(BIG5);
    for &[a, b, c, d] in BIG5_PLANE2.as_chunks::<4>().0 {
        let scalar = char::from_u32(0x20000 | u32::from(u16::from_be_bytes([c, d]))).unwrap();
        assert_eq!(big5.insert(u16::from_be_bytes([a, b]), scalar), None);
    }
    // The index's 18,590 codes, ASCII, and none of the four of two characters.
    assert_eq!(big5.len(), 18_590 + 128);
    check_short_inputs(M::Big5, &big5);
    for (profile, records) in [
        (
            M::ReceiverBig5,
            include_bytes!("../src/text/charset/mapping-data/receiver_big5.bin").as_slice(),
        ),
        (M::Windows932, WINDOWS932),
        (M::Gbk, GBK),
    ] {
        check_short_inputs(profile, &packed(records));
    }
    // The Encoding Standard's Shift_JIS is code page 932 with 0x80, and its
    // EUC-KR code page 949: the profiles decode those tables.
    let mut shift_jis = packed(WINDOWS932);
    assert_eq!(shift_jis.insert(0x80, '\u{80}'), None);
    check_short_inputs(M::ShiftJis, &shift_jis);
    check_short_inputs(M::EucKr, &packed(WINDOWS949));
}

/// The Encoding Standard's GBK holds every CP936 code and adds GB18030's
/// two-byte codes, the user-defined areas among them.
#[test]
fn gbk_extends_code_page_936() {
    let gbk = packed(GBK);
    let cp936 = mapping(
        M::ReceiverGbk,
        include_str!("multibyte-mapping-data/CP936.TXT"),
    );
    for (code, value) in &cp936 {
        assert_eq!(gbk.get(code), Some(value), "{code:04X}");
    }
    assert_eq!(gbk.len() - cp936.len(), 2149);
    for (code, value) in [
        (0xa2e3, '\u{20ac}'),
        (0xa6d9, '\u{fe10}'),
        (0xaaa1, '\u{e000}'),
        (0xa140, '\u{e4c6}'),
    ] {
        assert_eq!(gbk.get(&code), Some(&value), "{code:04X}");
    }
}

/// Single-byte results from the source repertoire, including an unfinished lead at EOF.
fn euc_kr_single(byte: u8, single_max: u8) -> Result<String, Error> {
    match byte {
        b if b <= single_max => Ok(char::from(b).to_string()),
        0xa1..=0xfe => Err(Error::Truncated),
        _ => Err(Error::Invalid("unmapped multibyte byte")),
    }
}

#[test]
fn every_euc_kr_profile_reports_exact_scalars_and_refusals() {
    for (profile, single_max) in [(M::ReceiverEucKrC1, 0x9f)] {
        let pairs = mapping(profile, include_str!("multibyte-mapping-data/CP949.TXT"));
        for first in 0..=u8::MAX {
            let prefix = euc_kr_single(first, single_max);
            assert_eq!(
                low_level(&[first], profile),
                prefix,
                "{profile:?} {first:02x}"
            );
            for second in 0..=u8::MAX {
                let raw = [first, second];
                let expected = match &prefix {
                    Ok(a) => euc_kr_single(second, single_max).map(|b| format!("{a}{b}")),
                    Err(Error::Truncated) if (0xa1..=0xfe).contains(&second) => pairs
                        .get(&u16::from_be_bytes(raw))
                        .map(char::to_string)
                        .ok_or(Error::Invalid("unmapped multibyte pair")),
                    Err(Error::Truncated) => Err(Error::Invalid("invalid multibyte trail")),
                    Err(error) => Err(*error),
                };
                assert_eq!(low_level(&raw, profile), expected, "{profile:?} {raw:02x?}");
            }
        }
    }
}

macro_rules! table_tests {
    ($whole:ident,$short:ident,$index:expr) => {
        #[test]
        fn $whole() {
            whole_mapping($index);
        }
        #[test]
        fn $short() {
            all_short_inputs($index);
        }
    };
}
table_tests!(
    whole_receiver_gbk_mapping,
    every_receiver_gbk_one_and_two_byte_input,
    0
);
table_tests!(
    whole_receiver_shift_jis_mapping,
    every_receiver_shift_jis_one_and_two_byte_input,
    1
);
table_tests!(whole_euc_kr_mapping, every_euc_kr_one_and_two_byte_input, 2);
table_tests!(
    whole_windows932_mapping,
    every_windows932_one_and_two_byte_input,
    3
);
table_tests!(
    whole_windows949_mapping,
    every_windows949_one_and_two_byte_input,
    4
);
table_tests!(
    whole_windows950_mapping,
    every_windows950_one_and_two_byte_input,
    5
);
table_tests!(
    whole_shift_jis_mapping,
    every_shift_jis_one_and_two_byte_input,
    6
);

#[test]
fn every_physical_partition_retains_lead_and_exact_ranges() {
    let cancel = AtomicBool::new(false);
    for (profile, raw) in [
        (M::Gbk, &[0, 0xd6, 0xd0, 0x41, 0xce, 0xc4][..]),
        (M::Big5, &[0, 0xa4, 0xa4, 0x41, 0xa4, 0xe5][..]),
        (M::ShiftJis, &[0, 0x93, 0xfa, 0x41, 0x96, 0x7b][..]),
        (M::EucKr, &[0, 0xc7, 0xd1, 0x41, 0xb1, 0xdb][..]),
    ] {
        for mask in 0..(1usize << (raw.len() - 1)) {
            let mut decoder = MultibyteDecoder::at_offset(profile, 19);
            let mut actual = String::new();
            let mut ranges = Vec::new();
            let mut start = 0;
            for end in 1..=raw.len() {
                if end == raw.len() || mask & (1 << (end - 1)) != 0 {
                    // Each physical chunk is supplied separately; no joined buffer.
                    for &byte in &raw[start..end] {
                        if let DecodeStep::Scalar(s) = decoder
                            .push(byte, &mut WorkBudget::new(1), &cancel)
                            .unwrap()
                        {
                            actual.push(s.value);
                            ranges.push(s.range);
                        }
                    }
                    start = end;
                }
            }
            decoder.finish(&cancel).unwrap();
            assert_eq!(actual.as_bytes(), decode(raw, profile, 4096).unwrap());
            assert_eq!(ranges, [19..20, 20..22, 22..23, 23..25]);
        }
    }
}

#[test]
fn lead_starvation_is_not_eof_and_finish_is_sticky_truncation() {
    let cancel = AtomicBool::new(false);
    for profile in [
        M::Gbk,
        M::Big5,
        M::ShiftJis,
        M::EucKr,
        M::Windows932,
        M::Windows949,
        M::Windows950,
        M::ReceiverGbk,
        M::ReceiverShiftJis,
        M::ReceiverEucKrC1,
    ] {
        let lead = if matches!(profile, M::ShiftJis | M::ReceiverShiftJis | M::Windows932) {
            0x81
        } else {
            0xa1
        };
        let mut decoder = MultibyteDecoder::new(profile);
        assert_eq!(
            decoder
                .push(lead, &mut WorkBudget::new(1), &cancel)
                .unwrap(),
            DecodeStep::NeedInput
        );
        for _ in 0..3 {
            assert_eq!(
                decoder
                    .push(0xa1, &mut WorkBudget::new(0), &cancel)
                    .unwrap(),
                DecodeStep::Yield
            );
            assert_eq!(decoder.position(), 1);
        }
        assert_eq!(decoder.finish(&cancel), Err(Error::Truncated));
        assert_eq!(
            decoder.push(0xa1, &mut WorkBudget::new(1), &cancel),
            Err(Error::Truncated)
        );
        assert_eq!(decoder.finish(&cancel), Err(Error::Truncated));
    }
}

#[test]
fn cancellation_between_lead_and_trail_preserves_pending_source() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::Gbk);
    assert_eq!(
        decoder
            .push(0xd6, &mut WorkBudget::new(1), &cancel)
            .unwrap(),
        DecodeStep::NeedInput
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        decoder.push(0xd0, &mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    );
    assert_eq!(decoder.finish(&cancel), Err(Error::Cancelled));
    assert_eq!(decoder.position(), 1);
    cancel.store(false, Ordering::Relaxed);
    let DecodeStep::Scalar(s) = decoder
        .push(0xd0, &mut WorkBudget::new(1), &cancel)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(s.value, '中');
    assert_eq!(s.range, 0..2);
    decoder.finish(&cancel).unwrap();
}

#[test]
fn invalid_trail_never_replays_as_ascii_or_publishes_replacement() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::Gbk);
    decoder
        .push(0xd6, &mut WorkBudget::new(1), &cancel)
        .unwrap();
    let error = decoder
        .push(b' ', &mut WorkBudget::new(1), &cancel)
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(decoder.position(), 1);
    assert_eq!(
        decoder.push(b'A', &mut WorkBudget::new(1), &cancel),
        Err(error)
    );
    assert_eq!(decoder.finish(&cancel), Err(error));
}

#[test]
fn big5_is_the_encoding_standards_index() {
    // Pointers of index-big5.txt (identifier 8dfc7710..., 2024-09-18) and
    // their code points as the index lists them: its first and last pointers,
    // plane 2 characters, the neighbours of the two-character codes, and codes
    // where Unicode's historical BIG5.TXT differs or has no mapping.
    for (pointer, code, scalar) in [
        (942, [0x87, 0x40], '\u{43f0}'),
        (947, [0x87, 0x45], '\u{27267}'),
        (1132, [0x88, 0x61], '\u{d2}'),
        (1136, [0x88, 0x65], '\u{1ec0}'),
        (4288, [0x9c, 0x71], '\u{20021}'),
        (5029, [0xa1, 0x45], '\u{2027}'),
        (5050, [0xa1, 0x5a], '\u{2574}'),
        (5121, [0xa1, 0xc3], '\u{ffe3}'),
        (5287, [0xa2, 0xcc], '\u{5341}'),
        (10896, [0xc6, 0xa1], '\u{2460}'),
        (18996, [0xf9, 0xfe], '\u{ffed}'),
        (19781, [0xfe, 0xfe], '\u{79d4}'),
    ] {
        // The decoder's pointer: (lead - 0x81) * 157 + (trail - 0x40, or
        // trail - 0x62 from 0xA1).
        let [lead, trail] = code.map(u32::from);
        let offset = if trail < 0x7f { 0x40 } else { 0x62 };
        assert_eq!((lead - 0x81) * 157 + trail - offset, pointer);
        assert_eq!(
            decode(&code, M::Big5, 1).unwrap(),
            scalar.to_string().as_bytes(),
            "{code:02x?}"
        );
    }
    // No pointer below 942, and no single byte beyond ASCII.
    for refused in [&[0x81, 0x40][..], &[0x86, 0xfe], &[0x80], &[0xff]] {
        assert!(matches!(
            decode(refused, M::Big5, 1),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn big5_codes_of_two_characters_give_a_letter_and_its_mark() {
    let cancel = AtomicBool::new(false);
    for (code, text) in [
        ([0x88, 0x62], "\u{ca}\u{304}"),
        ([0x88, 0x64], "\u{ca}\u{30c}"),
        ([0x88, 0xa3], "\u{ea}\u{304}"),
        ([0x88, 0xa5], "\u{ea}\u{30c}"),
    ] {
        let raw = [b'A', code[0], code[1], b'B'];
        for quantum in [1, 7, 4096] {
            assert_eq!(
                decode(&raw, M::Big5, quantum).unwrap(),
                format!("A{text}B").as_bytes()
            );
        }
        // The decoder gives the letter for the lead byte and the mark for the
        // trail byte, which it takes on its second push.
        let mut decoder = MultibyteDecoder::new(M::Big5);
        let mut scalars = Vec::new();
        while decoder.position() < raw.len() {
            let byte = raw[decoder.position()];
            if let DecodeStep::Scalar(scalar) = decoder
                .push(byte, &mut WorkBudget::new(1), &cancel)
                .unwrap()
            {
                scalars.push((scalar.value, scalar.range));
            }
        }
        decoder.finish(&cancel).unwrap();
        let mut characters = text.chars();
        assert_eq!(
            scalars,
            [
                ('A', 0..1),
                (characters.next().unwrap(), 1..2),
                (characters.next().unwrap(), 2..3),
                ('B', 3..4),
            ]
        );
        // Without its trail byte the code is truncated.
        assert_eq!(decode(&raw[..2], M::Big5, 1), Err(Error::Truncated));
    }
}

#[test]
fn gbk_decodes_gb18030_four_byte_codes_by_their_ranges() {
    for (code, scalar) in [
        ([0x81, 0x30, 0x81, 0x30], '\u{80}'),
        ([0x81, 0x30, 0x81, 0x31], '\u{81}'),
        // Pointer 7457, which the standard decodes before its ranges.
        ([0x81, 0x35, 0xf4, 0x37], '\u{e7c7}'),
        // The last pointer below the gap, 39419.
        ([0x84, 0x31, 0xa4, 0x39], '\u{ffff}'),
        // Pointers from 189000 on are U+10000 onwards.
        ([0x90, 0x30, 0x81, 0x30], '\u{10000}'),
        ([0xe3, 0x32, 0x9a, 0x35], '\u{10ffff}'),
    ] {
        let raw = [b'A', code[0], code[1], code[2], code[3], b'B'];
        for quantum in [1, 7, 4096] {
            assert_eq!(
                decode(&raw, M::Gbk, quantum).unwrap(),
                format!("A{scalar}B").as_bytes(),
                "{code:02x?}"
            );
        }
        let cancel = AtomicBool::new(false);
        let mut decoder = MultibyteDecoder::new(M::Gbk);
        let mut scalars = Vec::new();
        for byte in raw {
            match decoder
                .push(byte, &mut WorkBudget::new(1), &cancel)
                .unwrap()
            {
                DecodeStep::Scalar(scalar) => scalars.push((scalar.value, scalar.range)),
                DecodeStep::NeedInput => {}
                DecodeStep::Yield => panic!("one byte must fit one work unit"),
            }
        }
        decoder.finish(&cancel).unwrap();
        assert_eq!(scalars, [('A', 0..1), (scalar, 1..5), ('B', 5..6)]);
    }
    for refused in [
        // The gap after 39419, and beyond U+10FFFF.
        &[0x84, 0x31, 0xa5, 0x30][..],
        &[0xe3, 0x32, 0x9a, 0x36],
        // A third byte that is not 81 to FE, a fourth that is not 30 to 39.
        &[0x81, 0x30, 0x41, 0x30],
        &[0x81, 0x30, 0x80, 0x30],
        &[0x81, 0x30, 0x81, 0x41],
    ] {
        assert!(decode(refused, M::Gbk, 1).is_err(), "{refused:02x?}");
    }
    for truncated in [&[0x81, 0x30][..], &[0x81, 0x30, 0x81]] {
        assert_eq!(decode(truncated, M::Gbk, 1), Err(Error::Truncated));
    }
    // The receiver's GBK has no four-byte codes.
    assert!(decode(&[0x81, 0x30, 0x81, 0x30], M::ReceiverGbk, 1).is_err());
}

#[test]
fn cp950_user_defined_areas_map_all_6217_codes_to_private_use() {
    // The first and last code of each area, in the order Windows numbers them.
    for (code, scalar) in [
        ([0xfa, 0x40], '\u{e000}'),
        ([0xfe, 0xfe], '\u{e310}'),
        ([0x8e, 0x40], '\u{e311}'),
        ([0xa0, 0xfe], '\u{eeb7}'),
        ([0x81, 0x40], '\u{eeb8}'),
        ([0x8d, 0xfe], '\u{f6b0}'),
        ([0xc6, 0xa1], '\u{f6b1}'),
        ([0xc8, 0xfe], '\u{f848}'),
    ] {
        assert_eq!(
            decode(&code, M::Windows950, 1).unwrap(),
            scalar.to_string().as_bytes()
        );
    }
    assert_eq!(
        decode(&[0x80], M::Windows950, 1).unwrap(),
        "\u{80}".as_bytes()
    );
    assert_eq!(
        decode(&[0xff], M::Windows950, 1).unwrap(),
        "\u{f8f8}".as_bytes()
    );
    // The reference's C library converts cp950 with its Big5 charmap, which
    // has the last area and byte 0x80 only.
    assert_eq!(
        decode(&[0xc6, 0xa1], M::ReceiverBig5, 1).unwrap(),
        "\u{f6b1}".as_bytes()
    );
    assert_eq!(
        decode(&[0x80], M::ReceiverBig5, 1).unwrap(),
        "\u{80}".as_bytes()
    );
    for refused in [&[0xfa, 0x40][..], &[0x8e, 0x40], &[0x81, 0x40], &[0xff]] {
        assert!(decode(refused, M::ReceiverBig5, 1).is_err());
    }
}

#[test]
fn byte_origin_overflow_is_checked_before_source_progress() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::at_offset(M::Gbk, usize::MAX);
    assert_eq!(
        decoder.push(b'A', &mut WorkBudget::new(1), &cancel),
        Err(Error::Overflow)
    );
    assert_eq!(decoder.position(), usize::MAX);
    assert_eq!(decoder.finish(&cancel), Err(Error::Overflow));
}

#[test]
fn finished_decoder_rejects_more_bytes_and_empty_input_is_real_empty_text() {
    let cancel = AtomicBool::new(false);
    let mut decoder = MultibyteDecoder::new(M::Big5);
    decoder.finish(&cancel).unwrap();
    decoder.finish(&cancel).unwrap();
    assert!(matches!(
        decoder.push(0, &mut WorkBudget::new(1), &cancel),
        Err(Error::Invalid(_))
    ));
    for (profile, _, _) in PROFILES {
        assert!(decode(&[], profile, 1).unwrap().is_empty());
    }
}

#[test]
fn complete_validation_refuses_malformed_tail_before_any_output() {
    let pool = Budget::new(8192).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[b'A', 0xd6, 0xd0, 0xff]).unwrap();
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::Gbk),
        &pool,
        Limits::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let baseline = pool.used();
    let error = loop {
        match job.poll(&mut WorkBudget::new(1), &cancel) {
            Ok(CharsetStatus::Pending) => assert!(job.take_output(&cancel).is_err()),
            Ok(CharsetStatus::Ready) => panic!("malformed tail published"),
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::Invalid(_)));
    assert_eq!(pool.used(), baseline);
    assert_eq!(job.poll(&mut WorkBudget::new(4096), &cancel), Err(error));
    assert!(job.take_output(&cancel).is_err());
}

#[test]
fn declared_label_pool_and_backing_source_owner_survive_conversion() {
    let pool = Budget::new(8192).unwrap();
    let backing = Bytes::copy_from_slice(&pool, &[0x55, 0xd6, 0xd0, 0x66]).unwrap();
    let raw = backing.slice(1..3).unwrap();
    let label = Bytes::copy_from_slice(&pool, b"contradictory/windows-1251").unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = CharsetJob::new(
        &raw,
        Some(&label),
        Charset::Multibyte(M::Gbk),
        &pool,
        Limits::default(),
    )
    .unwrap();
    while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
    let out = job.take_output(&cancel).unwrap();
    assert!(out.source.shares_storage_with(&backing));
    assert!(
        out.declared_label
            .as_ref()
            .unwrap()
            .shares_storage_with(&label)
    );
    assert_eq!(out.source.as_ref(), &[0xd6, 0xd0]);
    assert_eq!(out.utf8.as_ref(), "中".as_bytes());
    drop(job);
    drop(backing);
    drop(raw);
    drop(label);
    assert!(pool.used() > 0);
    drop(out);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn foreign_source_and_label_rejected_without_consuming_caller_owners() {
    let pool = Budget::new(4096).unwrap();
    let other = Budget::new(4096).unwrap();
    let source = Bytes::copy_from_slice(&pool, b"A").unwrap();
    let foreign = Bytes::copy_from_slice(&other, b"B").unwrap();
    assert!(matches!(
        CharsetJob::new(
            &foreign,
            None,
            Charset::Multibyte(M::Gbk),
            &pool,
            Limits::default()
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        CharsetJob::new(
            &source,
            Some(&foreign),
            Charset::Multibyte(M::Gbk),
            &pool,
            Limits::default()
        ),
        Err(Error::ForeignPool)
    ));
    assert_eq!(foreign.as_ref(), b"B");
    assert_eq!(source.as_ref(), b"A");
}

#[test]
fn decoded_byte_cap_checks_real_expansion_without_truncation() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let limits = Limits {
        max_decoded_bytes: 2,
        ..Limits::default()
    };
    let mut job = CharsetJob::new(&raw, None, Charset::Multibyte(M::Gbk), &pool, limits).unwrap();
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(Error::Limit {
            requested: 3,
            maximum: 2
        })
    );
    assert!(job.take_output(&AtomicBool::new(false)).is_err());
}

#[test]
fn zero_work_job_has_no_progress_or_allocation() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::Gbk),
        &pool,
        Limits::default(),
    )
    .unwrap();
    let used = pool.used();
    for _ in 0..8 {
        assert_eq!(
            job.poll(&mut WorkBudget::new(0), &AtomicBool::new(false))
                .unwrap(),
            CharsetStatus::Pending
        );
        assert_eq!(pool.used(), used);
    }
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &AtomicBool::new(false))
            .unwrap(),
        CharsetStatus::Ready
    );
}

#[test]
fn job_cancellation_at_each_single_unit_boundary_is_resumable() {
    let raw = [b'A', 0xd6, 0xd0, 0xce, 0xc4, 0];
    for pause in 0..40 {
        let pool = Budget::new(8192).unwrap();
        let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = CharsetJob::new(
            &source,
            None,
            Charset::Multibyte(M::Gbk),
            &pool,
            Limits::default(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..pause {
            if job.poll(&mut WorkBudget::new(1), &cancel).unwrap() == CharsetStatus::Ready {
                break;
            }
        }
        let used = pool.used();
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            job.poll(&mut WorkBudget::new(1), &cancel),
            Err(Error::Cancelled)
        );
        assert_eq!(pool.used(), used);
        cancel.store(false, Ordering::Relaxed);
        while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
        assert_eq!(
            job.take_output(&cancel).unwrap().utf8.as_ref(),
            "A中文\0".as_bytes()
        );
    }
}

#[test]
fn retained_output_pressure_retries_without_replaying_or_losing_source() {
    let pool = Budget::new(4096).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &[0xd6, 0xd0]).unwrap();
    let pressure = pool.reserve(pool.limit() - pool.used()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut job = CharsetJob::new(
        &raw,
        None,
        Charset::Multibyte(M::Gbk),
        &pool,
        Limits::default(),
    )
    .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Backpressure { .. })
    ));
    let used = pool.used();
    for _ in 0..3 {
        let mut work = WorkBudget::new(0);
        assert_eq!(
            job.poll(&mut work, &cancel).unwrap(),
            CharsetStatus::Pending
        );
        assert_eq!(pool.used(), used);
    }
    drop(pressure);
    while job.poll(&mut WorkBudget::new(1), &cancel).unwrap() != CharsetStatus::Ready {}
    assert_eq!(
        job.take_output(&cancel).unwrap().utf8.as_ref(),
        "中".as_bytes()
    );
}

use triergon_core::SourceId;
use triergon_subtitles::text::{Encoding, Extractor, Format, Input, Timing};

#[test]
fn utf8_packets_do_not_adopt_multibyte_selection() {
    for format in [Format::Utf8Packet, Format::MatroskaWebVtt] {
        let pool = Budget::new(4096).unwrap();
        let input = Input {
            format,
            encoding: Encoding::Multibyte(M::Gbk),
            payload: Bytes::copy_from_slice(&pool, b"A").unwrap(),
            codec_id: None,
            codec_private: None,
            charset_label: None,
            addition: None,
            timing: Timing::unknown(),
            source_id: SourceId(91),
            generation: 0,
            sequence: 0,
            configuration_epoch: 0,
            container_position: None,
        };
        let mut extractor = Extractor::new(&pool, Limits::default(), 0);
        extractor.provide(input).unwrap();
        assert!(matches!(
            extractor.step(&mut WorkBudget::new(4096), &AtomicBool::new(false)),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn shift_jis_815f_is_the_fullwidth_reverse_solidus() {
    for profile in [M::ShiftJis, M::ReceiverShiftJis, M::Windows932] {
        assert_eq!(
            decode(&[0x81, 0x5f], profile, 1).unwrap(),
            "\u{ff3c}".as_bytes()
        );
    }
}

#[test]
fn the_encoding_standards_shift_jis_keeps_ascii_and_adds_0x80() {
    assert_eq!(decode(b"\\~", M::ShiftJis, 1).unwrap(), b"\\~");
    assert_eq!(
        decode(b"\\~", M::ReceiverShiftJis, 1).unwrap(),
        "\u{a5}\u{203e}".as_bytes()
    );
    assert_eq!(
        decode(&[0x80], M::ShiftJis, 1).unwrap(),
        "\u{80}".as_bytes()
    );
    assert_eq!(
        decode(&[0xa1, 0xdf], M::ShiftJis, 1).unwrap(),
        "｡ﾟ".as_bytes()
    );
    for refused in [&[0xa0][..], &[0xfd], &[0xfe], &[0xff]] {
        assert!(decode(refused, M::ShiftJis, 1).is_err(), "{refused:02x?}");
    }
}
