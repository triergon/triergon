#![forbid(unsafe_code)]
//! The FFmpeg-compatible text receivers: `compat::document` and `compat::ass`.
//! Their parity with the reference is the `subtitle-text` feature's; these
//! tests cover the rules they document and their resource behaviour.

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, TimeBase, WorkBudget};
use triergon_subtitles::compat::{ass, document};

type Folded = (i64, i64, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

fn read(source: &[u8], syntax: document::Syntax, quantum: u64) -> Vec<Folded> {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let bytes = Bytes::copy_from_slice(&pool, source).unwrap();
    let mut reader =
        document::Reader::new(&pool, bytes, syntax, document::Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut cues = Vec::new();
    for _ in 0..1_000_000 {
        match reader.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() {
            document::Status::Yield => {}
            document::Status::Cue(cue) => {
                let owned = |field: &Option<Bytes>| field.as_ref().map(|b| b.as_ref().to_vec());
                cues.push((
                    cue.start_ms,
                    cue.duration_ms,
                    cue.text.as_ref().to_vec(),
                    owned(&cue.identifier),
                    owned(&cue.settings),
                ));
            }
            document::Status::Finished => {
                drop(reader);
                assert_eq!(pool.used(), baseline);
                return cues;
            }
        }
    }
    panic!("the reader did not finish")
}

fn cue(start: i64, duration: i64, text: &str) -> Folded {
    (start, duration, text.as_bytes().to_vec(), None, None)
}

#[test]
fn subrip_cues_follow_the_demuxer() {
    let source = concat!(
        "garbage\n",
        "2\n00:00:05,000 --> 00:00:04,000\nlate\n\n\nstill late\n",
        "1\n0:0:1,5 --> 0:0:2,25 X1:1\n-3\n3a\n",
        "00:00:09,000 --> 00:00:10,000\n42\n",
        "00:00:09,000 --> 00:00:09,500\n7\n",
    );
    let expected = vec![
        // `3a` before the next timing line is that cue's number.
        cue(1005, 1020, "-3"),
        // Ends before it starts, so it lasts until the next cue.
        cue(5000, 4000, "late\nstill late"),
        // The cue at 9 s whose only line is the number `42` has no text; the
        // last cue's `7` is followed by no timing line.
        cue(9000, 500, "7"),
    ];
    for quantum in [1, 2, 7, 4096] {
        assert_eq!(
            read(source.as_bytes(), document::Syntax::SubRip, quantum),
            expected
        );
    }
}

#[test]
fn subrip_durations_are_32_bit_and_nul_ends_the_document() {
    let source =
        b"1\n00:00:00,000 --> 999:00:00,000\nA\n\n2\n00:00:03,000 --> 00:00:04,000\nB\0C\n";
    assert_eq!(
        read(source, document::Syntax::SubRip, 7),
        [cue(0, -698_567_296, "A")]
    );
}

#[test]
fn utf16_subrip_keeps_one_byte_of_a_looked_at_character() {
    let text = "1\r\n00:00:01,000 --> 00:00:02,000\r\n\u{c5}x\r\n\u{c6}y\n\u{c7}z\n\n";
    let mut source = vec![0xfe, 0xff];
    for unit in text.encode_utf16() {
        source.extend_from_slice(&unit.to_be_bytes());
    }
    let cues = read(&source, document::Syntax::SubRip, 3);
    assert_eq!(cues.len(), 1);
    assert_eq!(cues[0].2, b"\xc3\x85x\n\xc3\x86y\n\xc3z");
}

#[test]
fn webvtt_blocks_follow_the_demuxer() {
    let source = concat!(
        "WEBVTT\n\n",
        "NOTE skipped\n\n",
        "id\n00:05.000 --> 00:04.000 align:start\nA\rB\r\n\n",
        "00:01.000-->00:02.000x  s t\n\n",
        "\r\rlast\n00:09.000 --> 00:10.000\nC\0ignored\n\n",
        "00:11.000 --> 00:12.000\nlost\n\n",
    );
    let expected = vec![
        (1000, 1000, Vec::new(), None, Some(b"s t".to_vec())),
        (
            5000,
            4000,
            b"A\rB".to_vec(),
            Some(b"id".to_vec()),
            Some(b"align:start".to_vec()),
        ),
        (9000, 1000, b"C".to_vec(), Some(b"last".to_vec()), None),
    ];
    for quantum in [1, 2, 7, 4096] {
        assert_eq!(
            read(source.as_bytes(), document::Syntax::WebVtt, quantum),
            expected
        );
    }
}

#[test]
fn a_cue_that_repeats_the_one_before_is_dropped() {
    let long = "x".repeat(300);
    let source = format!(
        "1\n00:00:01,000 --> 00:00:02,000\n{long}\n\n\
         2\n00:00:05,000 --> 00:00:06,000\nB\n\n\
         3\n00:00:01,000 --> 00:00:02,000\n{long}\n\n\
         4\n00:00:07,000 --> 00:00:06,000\nC\n\n\
         5\n00:00:07,000 --> 00:00:06,000\nC\n\n\
         6\n00:00:10,000 --> 00:00:11,000\nA\n\n\
         7\n00:00:10,000 --> 00:00:11,000\nB\n\n\
         8\n00:00:10,000 --> 00:00:11,000\nA\n\n"
    );
    let expected = vec![
        // The third cue, sorted next to the first, repeats it.
        cue(1000, 1000, &long),
        cue(5000, 1000, "B"),
        // Durations are completed first: these two no longer repeat.
        cue(7000, 0, "C"),
        cue(7000, 3000, "C"),
        // Only the cue just before counts.
        cue(10_000, 1000, "A"),
        cue(10_000, 1000, "B"),
        cue(10_000, 1000, "A"),
    ];
    for quantum in [1, 7, 4096] {
        assert_eq!(
            read(source.as_bytes(), document::Syntax::SubRip, quantum),
            expected
        );
    }
    let source = concat!(
        "WEBVTT\n\n",
        "a\n00:01.000 --> 00:02.000 align:start\nA\n\n",
        "b\n00:03.000 --> 00:04.000\nB\n\n",
        "c\n00:01.000 --> 00:02.000\nA\n\n",
    );
    // Identifier and settings do not count.
    let expected = vec![
        (
            1000,
            1000,
            b"A".to_vec(),
            Some(b"a".to_vec()),
            Some(b"align:start".to_vec()),
        ),
        (3000, 1000, b"B".to_vec(), Some(b"b".to_vec()), None),
    ];
    assert_eq!(
        read(source.as_bytes(), document::Syntax::WebVtt, 1),
        expected
    );
}

#[test]
fn reader_limits_cancellation_and_zero_work() {
    let pool = Budget::new(1 << 20).unwrap();
    let source =
        Bytes::copy_from_slice(&pool, b"WEBVTT\n\n00:01.000 --> 00:02.000\nA\n\n").unwrap();
    let small = document::Limits {
        document_bytes: 4,
        ..document::Limits::default()
    };
    assert!(matches!(
        document::Reader::new(&pool, source.clone(), document::Syntax::WebVtt, small),
        Err(Error::Limit { .. })
    ));
    let none = document::Limits {
        cues: 0,
        ..document::Limits::default()
    };
    let mut reader =
        document::Reader::new(&pool, source.clone(), document::Syntax::WebVtt, none).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1 << 20), &cancel),
        Err(Error::Limit { .. })
    ));
    let mut reader = document::Reader::new(
        &pool,
        source,
        document::Syntax::WebVtt,
        document::Limits::default(),
    )
    .unwrap();
    let mut work = WorkBudget::new(0);
    assert!(matches!(
        reader.poll(&mut work, &cancel).unwrap(),
        document::Status::Yield
    ));
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1 << 20), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        reader.poll(&mut WorkBudget::new(1 << 20), &cancel).unwrap(),
        document::Status::Cue(_)
    ));
}

fn decode(decoder: &mut ass::Decoder, packet: &[u8], pool: &Budget) -> Option<ass::Subtitle> {
    let packet = Bytes::copy_from_slice(pool, packet).unwrap();
    let clock = ass::Clock {
        pts: Some(1000),
        dts: None,
        duration: Some(2001),
    };
    decoder.provide(&packet, clock).unwrap();
    let cancel = AtomicBool::new(false);
    loop {
        match decoder.poll(&mut WorkBudget::new(3), &cancel).unwrap() {
            ass::Status::Pending => {}
            ass::Status::Decoded(subtitle) => return subtitle,
            ass::Status::NeedInput => panic!("no packet"),
        }
    }
}

#[test]
fn ass_packets_decode_as_the_decode_call_returns_them() {
    let pool = Budget::new(1 << 20).unwrap();
    let options = ass::Options {
        packet_timebase: Some(TimeBase::new(1, 10_000).unwrap()),
    };
    let header = Bytes::copy_from_slice(&pool, b"[Script Info]").unwrap();
    let mut decoder =
        ass::Decoder::new(&pool, Some(&header), None, options, ass::Limits::default()).unwrap();
    assert_eq!(decoder.header(), b"[Script Info]");
    assert!(decode(&mut decoder, b"", &pool).is_none());
    let subtitle = decode(&mut decoder, b"0,0,Default,,0,0,0,,A\0\xff", &pool).unwrap();
    assert_eq!(subtitle.ass.as_ref(), b"0,0,Default,,0,0,0,,A");
    assert_eq!(subtitle.pts_microseconds, Some(100_000));
    assert_eq!(
        (subtitle.start_display_ms, subtitle.end_display_ms),
        (0, 200)
    );
    for refused in [
        &b"\xc0\x80"[..],
        b"\xed\xa0\x80",
        b"\xef\xbf\xbe",
        b"\xe6\xb0",
    ] {
        assert!(
            decode(&mut decoder, refused, &pool).is_none(),
            "{refused:?}"
        );
    }
    assert!(decode(&mut decoder, b"\xef\xbf\xbf", &pool).is_some());
}

#[test]
fn ass_charset_labels_open_and_convert_as_the_receiver_does() {
    let pool = Budget::new(1 << 20).unwrap();
    let unknown = Bytes::copy_from_slice(&pool, b"x-unknown").unwrap();
    assert!(matches!(
        ass::Decoder::new(
            &pool,
            None,
            Some(&unknown),
            ass::Options::default(),
            ass::Limits::default()
        ),
        Err(Error::Unsupported(_))
    ));
    let big5 = Bytes::copy_from_slice(&pool, b"big5").unwrap();
    let mut decoder = ass::Decoder::new(
        &pool,
        None,
        Some(&big5),
        ass::Options::default(),
        ass::Limits::default(),
    )
    .unwrap();
    let subtitle = decode(&mut decoder, b"\xa4\xa4", &pool).unwrap();
    assert_eq!(subtitle.ass.as_ref(), "\u{4e2d}".as_bytes());
    assert_eq!(subtitle.pts_microseconds, None);
    // An invalid trail byte refuses the conversion.
    assert!(decode(&mut decoder, b"\xa4\x00", &pool).is_none());
}

/// Each cue of a SubRip document converted from `label` as FFmpeg's
/// `sub_charenc` decoder option converts it, `None` where the conversion is
/// refused: `compat::document` reads the cues, `CharsetJob` converts each
/// with the repertoire the reference build accepts.
fn recoded(source: &[u8], label: &[u8]) -> Vec<Option<Vec<u8>>> {
    use triergon_subtitles::compat::charset::receiver_charset_with_glibc_euc_kr;
    use triergon_subtitles::text::Limits;
    use triergon_subtitles::text::charset::{CharsetJob, CharsetStatus};
    let pool = Budget::new(1 << 20).unwrap();
    let charset = receiver_charset_with_glibc_euc_kr(label).unwrap();
    let label = Bytes::copy_from_slice(&pool, label).unwrap();
    let cancel = AtomicBool::new(false);
    let document = Bytes::copy_from_slice(&pool, source).unwrap();
    let mut reader = document::Reader::new(
        &pool,
        document,
        document::Syntax::SubRip,
        document::Limits::default(),
    )
    .unwrap();
    let mut texts = Vec::new();
    loop {
        match reader.poll(&mut WorkBudget::new(7), &cancel).unwrap() {
            document::Status::Yield => {}
            document::Status::Cue(cue) => {
                let mut job =
                    CharsetJob::new(&cue.text, Some(&label), charset, &pool, Limits::default())
                        .unwrap();
                texts.push(loop {
                    match job.poll(&mut WorkBudget::new(3), &cancel) {
                        Ok(CharsetStatus::Pending) => {}
                        Ok(CharsetStatus::Ready) => {
                            break Some(job.take_output(&cancel).unwrap().utf8.as_ref().to_vec());
                        }
                        Err(_) => break None,
                    }
                });
            }
            document::Status::Finished => return texts,
        }
    }
}

#[test]
fn subrip_cues_convert_as_sub_charenc_converts_them() {
    // FFmpeg's subrip decoder with sub_charenc set to the label publishes these
    // texts for the cue `1\r\n00:00:01,000 --> 00:00:02,500\r\nTEXT\r\n`.
    for (label, text, expected) in [
        (&b"gbk"[..], &b"\xd6\xd0\xce\xc4"[..], "中文"),
        (b"big5", b"\xa4\xa4\xa4\xe5", "中文"),
        (b"shift_jis", b"\x93\xfa\x96\x7b", "日本"),
        (b"euc-kr", b"\xc7\xd1\xb1\xdb", "한글"),
        (b"cp932", b"\x81\x60", "～"),
        (b"cp949", b"\x81\x41", "갂"),
        (b"cp950", b"\xa1\x45", "‧"),
    ] {
        let mut source = b"1\r\n00:00:01,000 --> 00:00:02,500\r\n".to_vec();
        source.extend_from_slice(text);
        source.extend_from_slice(b"\r\n");
        assert_eq!(
            recoded(&source, label),
            [Some(expected.as_bytes().to_vec())]
        );
    }
}

#[test]
fn subrip_cues_convert_from_single_byte_charsets_as_sub_charenc_converts_them() {
    // FFmpeg's subrip decoder with sub_charenc: Latin-1 keeps 0x80 to 0x9F as
    // the C1 controls under both labels; Windows-1252 refuses the bytes its
    // table leaves undefined, Windows-1251 its one undefined byte, 0x98.
    let cue = |text: &[u8]| {
        let mut source = b"1\r\n00:00:01,000 --> 00:00:02,500\r\n".to_vec();
        source.extend_from_slice(text);
        source.extend_from_slice(b"\r\n");
        source
    };
    let one = |text: &str| vec![Some(text.as_bytes().to_vec())];
    assert_eq!(
        recoded(&cue(b"\xcf\xf0\xe8\xe2\xe5\xf2"), b"windows-1251"),
        one("Привет")
    );
    assert_eq!(recoded(&cue(b"\x80 \xe9"), b"windows-1252"), one("€ é"));
    for label in [&b"latin1"[..], b"iso-8859-1"] {
        assert_eq!(recoded(&cue(b"\xa0 \xe9"), label), one("\u{a0} é"));
        assert_eq!(recoded(&cue(b"\x80\x9f"), label), one("\u{80}\u{9f}"));
    }
    for undefined in [0x81, 0x8d, 0x8f, 0x90, 0x9d] {
        assert_eq!(recoded(&cue(&[b'A', undefined]), b"windows-1252"), [None]);
    }
    assert_eq!(recoded(&cue(b"\x98"), b"windows-1251"), [None]);
}

#[test]
fn a_cue_the_charset_refuses_leaves_the_others() {
    // FFmpeg publishes the first cue and refuses only the second packet.
    let source =
        b"1\n00:00:00,000 --> 00:00:01,000\nvalid\n\n2\n00:00:02,000 --> 00:00:03,000\n\xd6\n";
    assert_eq!(recoded(source, b"gbk"), [Some(b"valid".to_vec()), None]);
    let source =
        b"1\n00:00:00,000 --> 00:00:01,000\nvalid\n\n2\n00:00:02,000 --> 00:00:03,000\n\x98\n";
    assert_eq!(
        recoded(source, b"windows-1251"),
        [Some(b"valid".to_vec()), None]
    );
}
