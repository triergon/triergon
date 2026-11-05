#![forbid(unsafe_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_subtitles::text::charset::{
    Charset, CharsetJob, CharsetStatus, multibyte::MultibyteCharset as M,
};
use triergon_subtitles::text::native_charset::{
    AssPacketJob, Outcome, PacketStatus, receiver_charset,
};
use triergon_subtitles::text::{Encoding, Format, Input, Limits, Timing};
const PREFIX: &[u8] = b"2,0,Default,,0,0,0,,";
fn input(p: &Budget, label: &[u8], suffix: &[u8]) -> Input {
    let raw = [PREFIX, suffix].concat();
    Input {
        format: Format::OpaqueAss,
        encoding: Encoding::Opaque,
        payload: Bytes::copy_from_slice(p, &raw).unwrap(),
        codec_id: Some(Bytes::copy_from_slice(p, b"S_TEXT/ASS").unwrap()),
        codec_private: None,
        charset_label: Some(Bytes::copy_from_slice(p, label).unwrap()),
        addition: None,
        timing: Timing::unknown(),
        source_id: SourceId(33),
        generation: 7,
        sequence: 91,
        configuration_epoch: 12,
        container_position: None,
    }
}
fn run(p: &Budget, i: Input, q: u64) -> triergon_subtitles::text::native_charset::PacketView {
    let mut j = AssPacketJob::new(i, p, Limits::default(), 7).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..100000 {
        if j.poll(&mut WorkBudget::new(q), &c).unwrap() == PacketStatus::Ready {
            return j.take_output(&c).unwrap();
        }
    }
    panic!("nonterminal")
}
fn decode(p: &Budget, raw: &[u8], profile: M) -> Result<Vec<u8>, Error> {
    let b = Bytes::copy_from_slice(p, raw).unwrap();
    let mut j = CharsetJob::new(&b, None, Charset::Multibyte(profile), p, Limits::default())?;
    let c = AtomicBool::new(false);
    for _ in 0..100 {
        if j.poll(&mut WorkBudget::new(4096), &c)? == CharsetStatus::Ready {
            return Ok(j.take_output(&c)?.utf8.as_ref().to_vec());
        }
    }
    panic!("nonterminal")
}
#[test]
fn big5_seven_undefined_and_punctuation_are_receiver_only() {
    let p = Budget::new(1 << 20).unwrap();
    for (raw, text) in [
        (&[0xa1, 0x5a][..], "╴"),
        (&[0xa1, 0xc3], "￣"),
        (&[0xa1, 0xc5], "ˍ"),
        (&[0xa1, 0xfe], "／"),
        (&[0xa2, 0x40], "＼"),
        (&[0xa2, 0xcc], "十"),
        (&[0xa2, 0xce], "卅"),
        (&[0xa1, 0x45, 0xa1, 0x4e, 0xa1, 0xc2], "‧﹑¯"),
    ] {
        assert_eq!(
            decode(&p, raw, M::ReceiverBig5Glibc239).unwrap(),
            text.as_bytes()
        );
    }
    assert!(decode(&p, &[0xa1, 0x5a], M::Big5Unicode).is_err());
}
#[test]
fn euc_c1_controls_are_explicit_not_cp949_alias() {
    let p = Budget::new(1 << 20).unwrap();
    for b in 0x80..=0x9f {
        assert_eq!(
            decode(&p, &[b, b'A'], M::ReceiverEucKrC1).unwrap(),
            format!("{}A", char::from(b)).as_bytes()
        )
    }
    assert!(decode(&p, &[0x81, 0x41], M::EucKr).is_err());
    assert_eq!(
        decode(&p, &[0x81, 0x41], M::Windows949).unwrap(),
        "갂".as_bytes()
    );
    assert!(decode(&p, &[0xa2, 0xe8], M::ReceiverEucKrC1).is_err());
}
#[test]
fn cp932_pua_all1880_are_independently_indexed() {
    let p = Budget::new(1 << 20).unwrap();
    let mut n = 0;
    for lead in 0xf0..=0xf9 {
        for trail in (0x40..=0x7e).chain(0x80..=0xfc) {
            let ch = char::from_u32(0xe000 + n).unwrap();
            assert_eq!(
                decode(&p, &[lead, trail], M::ReceiverWindows932Pua).unwrap(),
                ch.to_string().as_bytes()
            );
            n += 1;
        }
    }
    assert_eq!(n, 1880);
    assert!(decode(&p, &[0xf0, 0x40], M::Windows932).is_err());
}
#[test]
fn labels_do_not_gain_unavailable_native_aliases() {
    for l in [
        b"euc_kr".as_slice(),
        b"windows-932",
        b"windows-949",
        b"windows-950",
        b" gbk",
        b"gbk\0",
    ] {
        assert!(receiver_charset(l).is_err());
    }
    assert_eq!(M::from_label(b"euc_kr").unwrap(), M::EucKr);
    assert_eq!(
        receiver_charset(b"BIG-5").unwrap(),
        Charset::Multibyte(M::ReceiverBig5Glibc239)
    );
}
#[test]
fn nul_prefix_retains_full_utf8_and_source_owners() {
    for q in [1, 7, 4096] {
        let p = Budget::new(1 << 20).unwrap();
        let i = input(&p, b"gbk", b"A\0B");
        let raw = i.payload.clone();
        let v = run(&p, i, q);
        assert_eq!(v.outcome(), Outcome::Converted);
        assert_eq!(v.visible().unwrap(), [PREFIX, b"A"].concat());
        assert_eq!(
            v.converted().unwrap().utf8.as_ref(),
            [PREFIX, b"A\0B"].concat()
        );
        assert_eq!(v.text().unwrap(), b"A");
        assert!(v.source().input.payload.shares_storage_with(&raw));
        assert_eq!(v.source().input.payload.as_ref(), raw.as_ref());
        assert_eq!(v.source().input.configuration_epoch, 12);
        assert_eq!(v.source().input.sequence, 91);
        assert!(v.source().decoded_utf8.is_none());
    }
}
#[test]
fn empty_text_is_fact_not_synthetic_native_return() {
    let p = Budget::new(1 << 20).unwrap();
    let v = run(&p, input(&p, b"big5", b""), 1);
    assert_eq!(v.outcome(), Outcome::Converted);
    assert_eq!(v.text(), Some(b"".as_slice()));
    assert_eq!(v.visible(), Some(PREFIX));
}
#[test]
fn malformed_after_nul_is_not_hidden() {
    let p = Budget::new(1 << 20).unwrap();
    let v = run(&p, input(&p, b"big5", &[0, 0xa1]), 1);
    assert_eq!(v.outcome(), Outcome::ConversionRefused(Error::Truncated));
    assert!(v.converted().is_none());
    assert_eq!(v.source().input.payload.as_ref().last(), Some(&0xa1));
}
#[test]
fn malformed_and_truncated_stay_typed_for_every_label() {
    let p = Budget::new(1 << 20).unwrap();
    for (label, lead) in [
        (b"gbk".as_slice(), 0x81),
        (b"big5", 0xa1),
        (b"shift_jis", 0x81),
        (b"euc-kr", 0xa1),
        (b"cp932", 0x81),
        (b"cp949", 0x81),
        (b"cp950", 0xa1),
    ] {
        let v = run(&p, input(&p, label, &[lead]), 1);
        assert_eq!(v.outcome(), Outcome::ConversionRefused(Error::Truncated));
        let v = run(&p, input(&p, label, &[lead, 0x20]), 1);
        assert!(matches!(
            v.outcome(),
            Outcome::ConversionRefused(Error::Invalid(_))
        ));
    }
}
#[test]
fn unsupported_label_retains_source_without_guess() {
    let p = Budget::new(1 << 20).unwrap();
    let v = run(&p, input(&p, b"windows-932", &[0xf0, 0x40]), 1);
    assert_eq!(v.outcome(), Outcome::UnsupportedLabel);
    assert!(v.converted().is_none());
    assert_eq!(
        v.source().input.charset_label.as_ref().unwrap().as_ref(),
        b"windows-932"
    );
}
#[test]
fn work_zero_does_not_advance_and_cancel_resumes() {
    let p = Budget::new(1 << 20).unwrap();
    let mut j =
        AssPacketJob::new(input(&p, b"cp932", &[0xf0, 0x40]), &p, Limits::default(), 7).unwrap();
    let c = AtomicBool::new(false);
    let used = p.used();
    for _ in 0..10 {
        assert_eq!(
            j.poll(&mut WorkBudget::new(0), &c).unwrap(),
            PacketStatus::Pending
        );
        assert_eq!(p.used(), used);
    }
    c.store(true, Ordering::Relaxed);
    assert!(matches!(
        j.poll(&mut WorkBudget::new(1), &c),
        Err(Error::Cancelled)
    ));
    c.store(false, Ordering::Relaxed);
    for _ in 0..1000 {
        if j.poll(&mut WorkBudget::new(1), &c).unwrap() == PacketStatus::Ready {
            assert_eq!(
                j.take_output(&c).unwrap().text().unwrap(),
                "\u{e000}".as_bytes()
            );
            return;
        }
    }
    panic!("nonterminal")
}
#[test]
fn foreign_label_configuration_and_generation_are_validated() {
    let p = Budget::new(1 << 20).unwrap();
    let other = Budget::new(1 << 20).unwrap();
    let mut i = input(&p, b"gbk", b"A");
    i.charset_label = Some(Bytes::copy_from_slice(&other, b"gbk").unwrap());
    assert!(matches!(
        AssPacketJob::new(i, &p, Limits::default(), 7),
        Err(Error::ForeignPool)
    ));
    let mut i = input(&p, b"gbk", b"A");
    i.codec_private = Some(Bytes::copy_from_slice(&other, b"cfg").unwrap());
    assert!(matches!(
        AssPacketJob::new(i, &p, Limits::default(), 7),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        AssPacketJob::new(input(&p, b"gbk", b"A"), &p, Limits::default(), 8),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn unsupported_format_is_not_assumed_validated() {
    let p = Budget::new(1 << 20).unwrap();
    let mut i = input(&p, b"gbk", b"A");
    i.format = Format::Utf8Packet;
    assert!(matches!(
        AssPacketJob::new(i, &p, Limits::default(), 7),
        Err(Error::Unsupported(_))
    ));
}
#[test]
fn decoded_limit_retains_source_as_refusal() {
    let p = Budget::new(1 << 20).unwrap();
    let lim = Limits {
        max_decoded_bytes: 20,
        ..Limits::default()
    };
    let mut j = AssPacketJob::new(input(&p, b"cp932", &[0xf0, 0x40]), &p, lim, 7).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..2000 {
        if j.poll(&mut WorkBudget::new(1), &c).unwrap() == PacketStatus::Ready {
            assert!(matches!(
                j.take_output(&c).unwrap().outcome(),
                Outcome::ConversionRefused(Error::Limit { .. })
            ));
            return;
        }
    }
    panic!("nonterminal")
}
#[test]
fn held_view_accounts_storage_until_drop() {
    let p = Budget::new(1 << 20).unwrap();
    let v = run(&p, input(&p, b"gbk", b"A\0B"), 1);
    assert!(p.used() > 0);
    let view = v.converted().unwrap().utf8.clone();
    drop(v);
    assert!(p.used() > 0);
    drop(view);
    assert_eq!(p.used(), Budget::control_storage_bytes());
}
#[test]
fn absent_separators_and_embedded_commas_are_literal() {
    let p = Budget::new(1 << 20).unwrap();
    let mut i = input(&p, b"gbk", b"");
    i.payload = Bytes::copy_from_slice(&p, b"a,b").unwrap();
    let v = run(&p, i, 1);
    assert_eq!(v.visible(), Some(b"a,b".as_slice()));
    assert_eq!(v.text(), None);
    let v = run(&p, input(&p, b"gbk", b"a,b,c"), 1);
    assert_eq!(v.text(), Some(b"a,b,c".as_slice()));
}

#[test]
fn whole_free_big5_charmap_is_checked_independently() {
    let p = Budget::new(1 << 20).unwrap();
    let mut count = 0;
    for line in include_str!("receiver-mapping-data/BIG5").lines() {
        let line = line.strip_prefix("%IRREVERSIBLE%").unwrap_or(line);
        let Some(rest) = line.strip_prefix("<U") else {
            continue;
        };
        let (scalar, rest) = rest.split_once('>').unwrap();
        let expected = char::from_u32(u32::from_str_radix(scalar, 16).unwrap()).unwrap();
        let wire = rest.split_whitespace().next().unwrap();
        if !wire.starts_with("/x") {
            continue;
        }
        let raw: Vec<u8> = wire
            .split("/x")
            .skip(1)
            .map(|s| u8::from_str_radix(s, 16).unwrap())
            .collect();
        for q in [1, 7, 4096] {
            let bytes = Bytes::copy_from_slice(&p, &raw).unwrap();
            let mut j = CharsetJob::new(
                &bytes,
                None,
                Charset::Multibyte(M::ReceiverBig5Glibc239),
                &p,
                Limits::default(),
            )
            .unwrap();
            let c = AtomicBool::new(false);
            let mut terminal = false;
            for _ in 0..64 {
                if j.poll(&mut WorkBudget::new(q), &c).unwrap() == CharsetStatus::Ready {
                    assert_eq!(
                        j.take_output(&c).unwrap().utf8.as_ref(),
                        expected.to_string().as_bytes(),
                        "{wire}"
                    );
                    terminal = true;
                    break;
                }
            }
            assert!(terminal);
        }
        count += 1;
    }
    assert_eq!(count, 14040);
}
#[test]
fn euc_extension_preserves_every_original_cp949_restricted_record() {
    let p = Budget::new(1 << 20).unwrap();
    let mut count = 0;
    for line in include_str!("multibyte-mapping-data/CP949.TXT").lines() {
        let fields: Vec<_> = line.split('#').next().unwrap().split_whitespace().collect();
        if fields.len() < 2 || !fields[1].starts_with("0x") {
            continue;
        }
        let wire = u16::from_str_radix(fields[0].trim_start_matches("0x"), 16).unwrap();
        if !(0xa1..=0xfe).contains(&(wire >> 8)) || !(0xa1..=0xfe).contains(&(wire & 255)) {
            continue;
        }
        let scalar = u32::from_str_radix(fields[1].trim_start_matches("0x"), 16).unwrap();
        assert_eq!(
            decode(&p, &wire.to_be_bytes(), M::ReceiverEucKrC1).unwrap(),
            char::from_u32(scalar).unwrap().to_string().as_bytes()
        );
        count += 1;
    }
    assert_eq!(count, 8226);
}
#[test]
fn pressure_retry_never_consumes_unbudgeted_work_or_discards_source() {
    let p = Budget::new(4096).unwrap();
    let i = input(&p, b"cp932", &[0xf0, 0x40]);
    let raw = i.payload.clone();
    let mut j = AssPacketJob::new(i, &p, Limits::default(), 7).unwrap();
    let held = p.reserve(p.available()).unwrap();
    let c = AtomicBool::new(false);
    let mut pressure = false;
    for _ in 0..1000 {
        match j.poll(&mut WorkBudget::new(1), &c) {
            Err(Error::Backpressure { .. }) => {
                pressure = true;
                break;
            }
            Ok(PacketStatus::Pending) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(pressure);
    let used = p.used();
    for _ in 0..10 {
        let mut work = WorkBudget::new(0);
        assert_eq!(j.poll(&mut work, &c).unwrap(), PacketStatus::Pending);
        assert_eq!(work.remaining(), 0);
        assert_eq!(p.used(), used)
    }
    drop(held);
    for _ in 0..1000 {
        if j.poll(&mut WorkBudget::new(1), &c).unwrap() == PacketStatus::Ready {
            let out = j.take_output(&c).unwrap();
            assert!(out.source().input.payload.shares_storage_with(&raw));
            assert_eq!(out.text(), Some("\u{e000}".as_bytes()));
            return;
        }
    }
    panic!("nonterminal")
}
#[test]
fn source_limit_is_checked_before_opaque_publication() {
    let p = Budget::new(4096).unwrap();
    let l = Limits {
        max_input_bytes: 19,
        ..Limits::default()
    };
    assert!(matches!(
        AssPacketJob::new(input(&p, b"gbk", b""), &p, l, 7),
        Err(Error::Limit { .. })
    ));
}
#[test]
fn take_before_ready_and_repeated_take_are_explicit() {
    let p = Budget::new(4096).unwrap();
    let c = AtomicBool::new(false);
    let mut j = AssPacketJob::new(input(&p, b"gbk", b"A"), &p, Limits::default(), 7).unwrap();
    assert!(j.take_output(&c).is_err());
    for _ in 0..1000 {
        if j.poll(&mut WorkBudget::new(1), &c).unwrap() == PacketStatus::Ready {
            let _v = j.take_output(&c).unwrap();
            assert!(j.take_output(&c).is_err());
            assert!(j.poll(&mut WorkBudget::new(1), &c).is_err());
            return;
        }
    }
    panic!("nonterminal")
}
#[path = "receiver-mapping-data/cases74.rs"]
mod native74;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
#[test]
fn original74_literal_native_payloads_and_refusals_keep_physical_geometry() {
    let mut counts = [0; 4];
    for &(id, label, raw, payloads, available) in native74::CASES {
        for q in [1, 7, 4096] {
            let p = Budget::new(1 << 20).unwrap();
            let mut i = input(&p, label, b"");
            i.payload = Bytes::copy_from_slice(&p, raw).unwrap();
            let v = run(&p, i, q);
            assert_eq!(v.source().input.payload.as_ref(), raw);
            assert_eq!(
                v.source().input.charset_label.as_ref().unwrap().as_ref(),
                label
            );
            if !payloads.is_empty() {
                assert_eq!(v.outcome(), Outcome::Converted, "{id}");
                for expected in payloads {
                    assert_eq!(v.visible().unwrap(), *expected, "{id}");
                }
                if q == 1 {
                    counts[0] += 1
                }
            } else if !available {
                assert_eq!(v.outcome(), Outcome::UnsupportedLabel, "{id}");
                if q == 1 {
                    counts[1] += 1
                }
            } else if raw.is_empty() {
                assert_eq!(v.outcome(), Outcome::Converted, "{id}");
                assert_eq!(v.visible(), Some(b"".as_slice()));
                assert_eq!(v.text(), None);
                if q == 1 {
                    counts[2] += 1
                }
            } else {
                assert!(matches!(v.outcome(), Outcome::ConversionRefused(_)), "{id}");
                if q == 1 {
                    counts[3] += 1
                }
            }
            println!(
                "receiver-row {{\"id\":\"{id}\",\"quantum\":{q},\"source_hex\":\"{}\",\"full_utf8_hex\":{},\"visible_hex\":{},\"text_hex\":{},\"outcome\":\"{}\"}}",
                hex(raw),
                v.converted()
                    .map(|c| format!("\"{}\"", hex(c.utf8.as_ref())))
                    .unwrap_or("null".into()),
                v.visible()
                    .map(|b| format!("\"{}\"", hex(b)))
                    .unwrap_or("null".into()),
                v.text()
                    .map(|b| format!("\"{}\"", hex(b)))
                    .unwrap_or("null".into()),
                format!("{:?}", v.outcome())
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
            );
        }
    }
    assert_eq!(counts, [47, 4, 7, 16]);
}
#[test]
fn signed_and_unknown_clock_facts_are_retained_without_inference() {
    use triergon_subtitles::text::RawClock;
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, b"gbk", b"A");
    let clock = RawClock {
        numerator: 1,
        denominator: 1000,
        pts_ticks: Some(-7),
        dts_ticks: Some(-11),
        duration_ticks: None,
    };
    i.timing = Timing::from_ticks(clock).unwrap();
    let v = run(&p, i, 1);
    assert_eq!(v.source().input.timing.raw_clock(), Some(clock));
    assert_eq!(v.source().input.timing.duration(), None);
    assert_eq!(v.source().duration, None);
    assert_eq!(v.source().end, None);
    assert_eq!(v.source().input.source_id, SourceId(33));
    assert_eq!(v.source().input.generation, 7);
}
#[test]
fn uncaptured_charset_profile_is_not_silently_admitted_to_receiver_policy() {
    let p = Budget::new(4096).unwrap();
    assert!(receiver_charset(b"windows-1252").is_err());
    let v = run(&p, input(&p, b"windows-1252", b"A"), 1);
    assert_eq!(v.outcome(), Outcome::UnsupportedLabel);
    assert!(v.converted().is_none());
    assert_eq!(
        Charset::from_label(b"windows-1252").unwrap(),
        Charset::Windows1252
    );
}
#[test]
fn absent_receiver_declaration_remains_unknown_while_source_is_retained() {
    let p = Budget::new(4096).unwrap();
    let mut i = input(&p, b"gbk", b"A");
    i.charset_label = None;
    let original = i.payload.clone();
    let v = run(&p, i, 1);
    assert_eq!(v.outcome(), Outcome::UnsupportedLabel);
    assert!(v.source().input.charset_label.is_none());
    assert!(v.source().input.payload.shares_storage_with(&original));
    assert!(v.converted().is_none());
}

fn run_v2(p: &Budget, i: Input, q: u64) -> triergon_subtitles::text::native_charset::PacketView {
    let mut j = AssPacketJob::new_v2(i, p, Limits::default(), 7).unwrap();
    let c = AtomicBool::new(false);
    for _ in 0..100000 {
        if j.poll(&mut WorkBudget::new(q), &c).unwrap() == PacketStatus::Ready {
            return j.take_output(&c).unwrap();
        }
    }
    panic!("nonterminal v2")
}
#[test]
fn receiver_v2_adds_primary_a2e8_without_upgrading_strict_or_v1() {
    use triergon_subtitles::text::native_charset::receiver_charset_v2;
    let p = Budget::new(1 << 20).unwrap();
    assert_eq!(
        decode(&p, &[0xa2, 0xe8], M::ReceiverEucKrGlibc239).unwrap(),
        "\u{327e}".as_bytes()
    );
    assert!(decode(&p, &[0xa2, 0xe8], M::EucKr).is_err());
    assert!(decode(&p, &[0xa2, 0xe8], M::ReceiverEucKrC1).is_err());
    assert_eq!(
        receiver_charset(b"euc-kr").unwrap(),
        Charset::Multibyte(M::ReceiverEucKrC1)
    );
    assert_eq!(
        receiver_charset_v2(b"EUC-KR").unwrap(),
        Charset::Multibyte(M::ReceiverEucKrGlibc239)
    );
    for label in [
        b"euc_kr".as_slice(),
        b"windows-949",
        b" euc-kr",
        b"euc-kr\0",
    ] {
        assert!(receiver_charset_v2(label).is_err())
    }
}
#[test]
fn receiver_v2_literal_ass_payload_agrees_with_fresh_native_control() {
    for q in [1, 7, 4096] {
        let p = Budget::new(1 << 20).unwrap();
        let v = run_v2(&p, input(&p, b"euc-kr", &[0xa2, 0xe8]), q);
        assert_eq!(v.outcome(), Outcome::Converted);
        assert_eq!(
            v.visible().unwrap(),
            [PREFIX, "\u{327e}".as_bytes()].concat()
        );
        assert_eq!(v.text(), Some("\u{327e}".as_bytes()));
        assert_eq!(
            v.source().input.payload.as_ref(),
            [PREFIX, &[0xa2, 0xe8]].concat()
        );
        assert!(v.source().decoded_utf8.is_none());
        let old = run(&p, input(&p, b"euc-kr", &[0xa2, 0xe8]), q);
        assert!(matches!(
            old.outcome(),
            Outcome::ConversionRefused(Error::Invalid(_))
        ));
    }
}
#[test]
fn receiver_v2_nul_tail_retains_all_physical_and_full_unicode_owners() {
    let p = Budget::new(1 << 20).unwrap();
    let v = run_v2(&p, input(&p, b"euc-kr", &[0xa2, 0xe8, 0, 0xa2, 0xe8]), 1);
    assert_eq!(
        v.visible().unwrap(),
        [PREFIX, "\u{327e}".as_bytes()].concat()
    );
    assert_eq!(
        v.converted().unwrap().utf8.as_ref(),
        [PREFIX, "\u{327e}\0\u{327e}".as_bytes()].concat()
    );
    assert_eq!(v.converted().unwrap().utf8.len(), 27);
    assert_eq!(v.source().input.payload.len(), 25);
    assert_eq!(v.source().input.configuration_epoch, 12);
    let alias = v.converted().unwrap().utf8.clone();
    drop(v);
    assert!(p.used() > Budget::control_storage_bytes());
    drop(alias);
    assert_eq!(p.used(), Budget::control_storage_bytes());
}
#[test]
fn receiver_v2_truncated_tail_work_zero_and_cancel_do_not_hide_source() {
    let p = Budget::new(1 << 20).unwrap();
    let i = input(&p, b"euc-kr", &[0xa2, 0xe8, 0xa2]);
    let original = i.payload.clone();
    let mut j = AssPacketJob::new_v2(i, &p, Limits::default(), 7).unwrap();
    let c = AtomicBool::new(false);
    let used = p.used();
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &c).unwrap(),
        PacketStatus::Pending
    );
    assert_eq!(p.used(), used);
    c.store(true, Ordering::Relaxed);
    assert!(matches!(
        j.poll(&mut WorkBudget::new(1), &c),
        Err(Error::Cancelled)
    ));
    c.store(false, Ordering::Relaxed);
    for _ in 0..10000 {
        if j.poll(&mut WorkBudget::new(1), &c).unwrap() == PacketStatus::Ready {
            let v = j.take_output(&c).unwrap();
            assert_eq!(v.outcome(), Outcome::ConversionRefused(Error::Truncated));
            assert!(v.source().input.payload.shares_storage_with(&original));
            assert!(v.converted().is_none());
            return;
        }
    }
    panic!("nonterminal")
}
#[test]
fn receiver_v2_all8387_primary_records_and_negative_neighbours() {
    use std::collections::BTreeMap;
    let p = Budget::new(1 << 20).unwrap();
    let mut expected = BTreeMap::new();
    for line in include_str!("multibyte-mapping-data/CP949.TXT").lines() {
        let f: Vec<_> = line.split('#').next().unwrap().split_whitespace().collect();
        if f.len() < 2 || !f[1].starts_with("0x") {
            continue;
        }
        let code = u16::from_str_radix(f[0].trim_start_matches("0x"), 16).unwrap();
        if !(0xa1..=0xfe).contains(&(code >> 8)) || !(0xa1..=0xfe).contains(&(code & 255)) {
            continue;
        }
        let scalar = u32::from_str_radix(f[1].trim_start_matches("0x"), 16).unwrap();
        expected.insert(code, char::from_u32(scalar).unwrap());
    }
    for byte in 0..160u16 {
        expected.insert(byte, char::from_u32(u32::from(byte)).unwrap());
    }
    assert!(expected.insert(0xa2e8, '\u{327e}').is_none());
    assert_eq!(expected.len(), 8387);
    for (wire, ch) in expected {
        let raw = if wire < 256 {
            vec![wire as u8]
        } else {
            wire.to_be_bytes().to_vec()
        };
        assert_eq!(
            decode(&p, &raw, M::ReceiverEucKrGlibc239).unwrap(),
            ch.to_string().as_bytes(),
            "{wire:04x}"
        );
    }
    assert!(matches!(
        decode(&p, &[0xa2], M::ReceiverEucKrGlibc239),
        Err(Error::Truncated)
    ));
    assert!(decode(&p, &[0xa2, 0x20], M::ReceiverEucKrGlibc239).is_err());
    assert!(decode(&p, &[0xa2, 0xe9], M::ReceiverEucKrGlibc239).is_err());
}
