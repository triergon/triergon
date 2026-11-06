#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AacAscParser, AacAudioSpecificConfig, AacConfigLimits, AacExtensionSignal, AacPceElement,
    AacPceElementKind, AacSource, BuildStep,
};

fn limits() -> AacConfigLimits {
    AacConfigLimits {
        maximum_record_bytes: 100_000,
        maximum_pce_elements: 70,
        maximum_comment_bytes: 255,
    }
}
fn source() -> AacSource {
    AacSource {
        source: SourceId(613),
        generation: 42,
        offset: 97,
    }
}
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}
impl Bits {
    fn new() -> Self {
        Self {
            bytes: vec![],
            len: 0,
        }
    }
    fn put(&mut self, value: u32, count: usize) {
        for i in (0..count).rev() {
            if self.len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let byte = self.len / 8;
            self.bytes[byte] |= (((value >> i) & 1) as u8) << (7 - self.len % 8);
            self.len += 1;
        }
    }
    fn align(&mut self, pattern: u8) -> (u8, u8) {
        let count = (8 - self.len % 8) % 8;
        let value = pattern & ((1_u16 << count) - 1) as u8;
        self.put(u32::from(value), count);
        (value, count as u8)
    }
}
fn base(channel: u8) -> Bits {
    let mut b = Bits::new();
    b.put(2, 5);
    b.put(3, 4);
    b.put(u32::from(channel), 4);
    b
}
fn lc() -> Bits {
    let mut b = base(2);
    b.put(0, 3);
    b
}
fn pce(counts: [u8; 6], comment: &[u8]) -> (Bits, (u8, u8)) {
    let mut b = base(0);
    b.put(0, 3);
    b.put(9, 4);
    b.put(1, 2);
    b.put(3, 4);
    for (count, width) in counts.into_iter().zip([4, 4, 4, 2, 3, 4]) {
        b.put(u32::from(count), width);
    }
    b.put(1, 1);
    b.put(7, 4);
    b.put(1, 1);
    b.put(2, 4);
    b.put(1, 1);
    b.put(3, 2);
    b.put(1, 1);
    for (group, count) in counts.into_iter().enumerate() {
        for tag in 0..count {
            if group < 3 || group == 5 {
                b.put(1, 1);
            }
            b.put(u32::from(tag % 16), 4);
        }
    }
    let align = b.align(0x55);
    b.put(comment.len() as u32, 8);
    for byte in comment {
        b.put(u32::from(*byte), 8);
    }
    (b, align)
}
fn run(
    pool: &Budget,
    raw: Bytes,
    bits: usize,
    lim: AacConfigLimits,
) -> Result<AacAudioSpecificConfig, Error> {
    let mut p = AacAscParser::with_bit_length(pool, source(), raw, bits, lim)?;
    for _ in 0..5000 {
        match p.step(&mut WorkBudget::new(1), &AtomicBool::new(false))? {
            BuildStep::Ready(o) => {
                assert!(matches!(
                    p.step(&mut WorkBudget::new(0), &AtomicBool::new(false))?,
                    BuildStep::Complete
                ));
                return Ok(o);
            }
            BuildStep::Yield => {}
            BuildStep::Complete => panic!("missing publication"),
        }
    }
    panic!("bounded record failed to terminate")
}
fn parse(b: &Bits) -> Result<AacAudioSpecificConfig, Error> {
    let pool = Budget::new(100_000).unwrap();
    run(
        &pool,
        Bytes::copy_from_slice(&pool, &b.bytes).unwrap(),
        b.len,
        limits(),
    )
}

#[test]
fn all15_original_primary_examples_retain_core_render_and_source_views() {
    let pool = Budget::new(100_000).unwrap();
    let mut count = 0;
    for line in include_str!("aac-fixtures/original15.tsv").lines().skip(1) {
        let fields: Vec<_> = line.split('\t').collect();
        let raw: Vec<_> = fields[1]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let o = run(
            &pool,
            Bytes::copy_from_slice(&pool, &raw).unwrap(),
            raw.len() * 8,
            limits(),
        )
        .unwrap();
        let f = o.facts();
        assert_eq!(
            f.signalled_render_rate_hz(),
            fields[2].parse::<u32>().unwrap(),
            "{}",
            fields[0]
        );
        assert_eq!(
            f.core_frequency.hz,
            fields[3].parse::<u32>().unwrap(),
            "{}",
            fields[0]
        );
        assert_eq!(
            f.core_channels,
            fields[4].parse::<u16>().unwrap(),
            "{}",
            fields[0]
        );
        assert_eq!(
            f.signalled_render_channels(),
            fields[5].parse::<u16>().unwrap(),
            "{}",
            fields[0]
        );
        assert_eq!(o.source_bytes().as_ref(), raw);
        assert_eq!(o.source(), source());
        count += 1;
    }
    assert_eq!(count, 15);
}

#[test]
fn explicit24bit_frequency_is_literal_and_never_silently_indexed() {
    for hz in [1, 48000, 0x800001, 0xffffff] {
        let mut b = Bits::new();
        b.put(2, 5);
        b.put(15, 4);
        b.put(hz, 24);
        b.put(7, 4);
        b.put(0, 3);
        let f = parse(&b).unwrap().facts();
        assert_eq!(
            (
                f.core_frequency.index,
                f.core_frequency.explicit_hz,
                f.core_frequency.hz
            ),
            (15, Some(hz), hz)
        );
        assert_eq!(f.core_channels, 8);
    }
}

#[test]
fn every_ga_flag_delay_and_selected_extension_is_retained() {
    for short in [false, true] {
        for delay in [None, Some(0), Some(16383)] {
            for ext in [false, true] {
                let mut b = base(1);
                b.put(u32::from(short), 1);
                b.put(u32::from(delay.is_some()), 1);
                if let Some(n) = delay {
                    b.put(n, 14)
                }
                b.put(u32::from(ext), 1);
                if ext {
                    b.put(0, 1)
                }
                let f = parse(&b).unwrap().facts();
                assert_eq!(f.ga.frame_length_flag, short);
                assert_eq!(f.ga.core_coder_delay, delay.map(|n| n as u16));
                assert_eq!(f.ga.extension_flag, ext);
                assert_eq!(f.ga.extension_flag3, ext.then_some(false));
                assert_eq!(f.core_samples_per_frame(), if short { 960 } else { 1024 });
            }
        }
    }
    let mut b = base(2);
    b.put(1, 3);
    b.put(1, 1);
    assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
}

#[test]
fn all70_pce_elements_mixdowns_comment_alignment_and_bit_ranges_survive() {
    let comment: Vec<_> = (0..255).map(|n| n as u8).collect();
    let (b, alignment) = pce([15, 15, 15, 3, 7, 15], &comment);
    let o = parse(&b).unwrap();
    let p = o.program_config().unwrap();
    let f = p.facts();
    assert_eq!(
        (f.element_instance_tag, f.profile_code, f.frequency_index),
        (9, 1, 3)
    );
    assert_eq!(f.counts, [15, 15, 15, 3, 7, 15]);
    assert_eq!(
        (f.mono_mixdown, f.stereo_mixdown, f.matrix_mixdown),
        (Some(7), Some(2), Some((3, true)))
    );
    assert_eq!((f.alignment_value, f.alignment_bits), alignment);
    assert_eq!(f.channel_count, 93);
    assert_eq!(p.elements().len(), 70);
    assert_eq!(f.source_bits.start_bit, 16);
    assert_eq!(f.source_bits.start_bit + f.source_bits.bit_count, b.len);
    assert_eq!(o.facts().core_channels, 93);
    assert_eq!(o.syntax_issues(), 0);
    assert_eq!(p.comment_bytes().as_ref(), comment);
    assert!(p.comment_bytes().shares_storage_with(o.source_bytes()));
    assert_eq!(o.source_bytes().as_ref(), b.bytes);
    let mut at = 0;
    for (group, count) in f.counts.into_iter().enumerate() {
        for tag in 0..count {
            let e = p.elements()[at];
            assert_eq!(e.tag_select, tag % 16);
            assert_eq!(e.channel_pair, (group < 3).then_some(true));
            assert_eq!(e.independent_switch, (group == 5).then_some(true));
            assert_eq!(
                e.source_bits.bit_count,
                if group < 3 || group == 5 { 5 } else { 4 }
            );
            at += 1;
        }
    }
    assert_eq!(p.elements()[45].kind, AacPceElementKind::Lfe);
    assert_eq!(p.elements()[48].kind, AacPceElementKind::AssociatedData);
    assert_eq!(p.elements()[55].kind, AacPceElementKind::Coupling);
}

#[test]
fn pce_profile_frequency_and_zero_channel_issues_are_literal_not_repairs() {
    let (mut b, _) = pce([0; 6], &[]);
    b.bytes[2] &= 0xf0;
    b.bytes[3] &= 0x3f; // actual PCE profile/frequency become zero.
    let o = parse(&b).unwrap();
    assert_eq!(o.source_bytes().as_ref(), b.bytes);
    assert_eq!(o.facts().core_channels, 0);
    assert_eq!(o.syntax_issues() & 7, 7);
}

#[test]
fn every_required_pce_bit_and_ordinary_prefix_bit_has_truncation_control() {
    let (b, _) = pce([2, 1, 1, 1, 1, 1], &[0xff, 0, 0x80]);
    let pool = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &b.bytes).unwrap();
    for end in 0..b.len {
        assert!(
            matches!(
                run(&pool, raw.clone(), end, limits()),
                Err(Error::Truncated)
            ),
            "end={end}"
        );
    }
    let plain = lc();
    for end in 0..plain.len {
        assert!(
            matches!(
                run(
                    &pool,
                    Bytes::copy_from_slice(&pool, &plain.bytes).unwrap(),
                    end,
                    limits()
                ),
                Err(Error::Truncated)
            ),
            "LCend={end}"
        );
    }
}

#[test]
fn sbr_ps_sync_signals_only_at_the_actual_immediate_bit_boundary() {
    let mut b = lc();
    b.put(0x2b7, 11);
    b.put(5, 5);
    b.put(1, 1);
    b.put(0, 4);
    b.put(0x548, 11);
    b.put(1, 1);
    let o = parse(&b).unwrap();
    let f = o.facts();
    assert_eq!(f.sbr, AacExtensionSignal::Sync(true));
    assert_eq!(f.ps, AacExtensionSignal::Sync(true));
    assert_eq!(f.extension_frequency.unwrap().hz, 96000);
    assert_eq!(f.understood_end_bit, b.len);
    let mut opaque = lc();
    opaque.put(0, 1);
    opaque.put(0x2b7, 11);
    opaque.put(5, 5);
    opaque.put(1, 1);
    opaque.put(0, 4);
    let f = parse(&opaque).unwrap().facts();
    assert_eq!(f.sbr, AacExtensionSignal::Unsignalled);
    assert_eq!(f.understood_end_bit, 16);
    let mut absent = lc();
    absent.put(0x2b7, 11);
    absent.put(5, 5);
    absent.put(0, 1);
    let f = parse(&absent).unwrap().facts();
    assert_eq!(f.sbr, AacExtensionSignal::Sync(false));
    assert_eq!(f.ps, AacExtensionSignal::Unsignalled);
    assert!(f.extension_frequency.is_none());
}

#[test]
fn complete_extension_markers_cannot_hide_truncated_selected_fields() {
    let mut b = lc();
    b.put(0x2b7, 11);
    b.put(5, 5);
    b.put(1, 1);
    b.put(0, 4);
    b.put(0x548, 11);
    b.put(1, 1);
    let pool = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &b.bytes).unwrap();
    for end in [27, 31, 32, 36, 48] {
        assert!(
            matches!(
                run(&pool, raw.clone(), end, limits()),
                Err(Error::Truncated)
            ),
            "end={end}"
        );
    }
    for end in 38..48 {
        let o = run(&pool, raw.clone(), end, limits()).unwrap();
        assert_eq!(o.facts().ps, AacExtensionSignal::Unsignalled);
        assert_eq!(o.facts().understood_end_bit, 37);
    }
}

#[test]
fn unselected_objects_channels_frequencies_and_extensions_are_explicit() {
    for aot in [0, 1, 3, 4, 6, 17, 23, 30] {
        let mut b = Bits::new();
        b.put(aot, 5);
        b.put(3, 4);
        b.put(2, 4);
        b.put(0, 3);
        assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
    }
    for tail in [0, 10, 63] {
        let mut b = Bits::new();
        b.put(31, 5);
        b.put(tail, 6);
        b.put(3, 4);
        b.put(2, 4);
        b.put(0, 3);
        assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
    }
    for channel in 8..16 {
        let mut b = base(channel);
        b.put(0, 3);
        assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
    }
    for index in [13, 14] {
        let mut b = Bits::new();
        b.put(2, 5);
        b.put(index, 4);
        b.put(2, 4);
        b.put(0, 3);
        assert!(matches!(parse(&b), Err(Error::Invalid(_))));
    }
    let mut b = Bits::new();
    b.put(2, 5);
    b.put(15, 4);
    b.put(0, 24);
    b.put(2, 4);
    b.put(0, 3);
    assert!(matches!(parse(&b), Err(Error::Invalid(_))));
    let mut b = lc();
    b.put(0x2b7, 11);
    b.put(29, 5);
    assert!(matches!(parse(&b), Err(Error::Unsupported(_))));
}

#[test]
fn descriptor_and_comment_bounds_preserve_complete_failure_membership() {
    let (b, _) = pce([15, 15, 15, 3, 7, 15], &[0; 255]);
    let pool = Budget::new(100_000).unwrap();
    for lim in [
        AacConfigLimits {
            maximum_record_bytes: b.bytes.len() - 1,
            ..limits()
        },
        AacConfigLimits {
            maximum_pce_elements: 69,
            ..limits()
        },
        AacConfigLimits {
            maximum_comment_bytes: 254,
            ..limits()
        },
    ] {
        assert!(matches!(
            run(
                &pool,
                Bytes::copy_from_slice(&pool, &b.bytes).unwrap(),
                b.len,
                lim
            ),
            Err(Error::Limit { .. })
        ));
    }
}

#[test]
fn pce_reservation_retries_without_repeating_consumed_setup_or_losing_input() {
    let (b, _) = pce([1, 1, 1, 1, 1, 1], &[9, 8, 7]);
    let pool = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &b.bytes).unwrap();
    let mut blocker = BudgetVec::<u8>::new(&pool);
    blocker.try_reserve_exact(pool.available() - 1).unwrap();
    let mut p =
        AacAscParser::with_bit_length(&pool, source(), raw.clone(), b.len, limits()).unwrap();
    let e = match p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)) {
        Err(e @ Error::Backpressure { .. }) => e,
        _ => panic!("no actual descriptor pressure"),
    };
    let charged = pool.used();
    for _ in 0..5 {
        assert!(
            matches!(p.step(&mut WorkBudget::new(1),&AtomicBool::new(false)),Err(other) if other==e)
        );
        assert_eq!(pool.used(), charged);
    }
    drop(blocker);
    let o = loop {
        match p
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            BuildStep::Ready(o) => break o,
            BuildStep::Yield => {}
            _ => panic!(),
        }
    };
    assert_eq!(o.source_bytes().as_ref(), b.bytes);
    assert_eq!(o.program_config().unwrap().elements().len(), 6);
}

#[test]
fn impossible_live_set_is_limit_rather_than_permanent_backpressure() {
    let (b, _) = pce([15, 15, 15, 3, 7, 15], &[]);
    let pool = Budget::new(
        Budget::control_storage_bytes() + Bytes::owner_overhead_bytes() + b.bytes.len() + 1,
    )
    .unwrap();
    let baseline = pool.used();
    assert!(70 * std::mem::size_of::<AacPceElement>() > pool.limit());
    assert!(matches!(
        run(
            &pool,
            Bytes::copy_from_slice(&pool, &b.bytes).unwrap(),
            b.len,
            limits()
        ),
        Err(Error::Limit { .. })
    ));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn cancellation_at_independent_work_boundaries_is_sticky_and_drops_scratch() {
    let (b, _) = pce([15, 15, 15, 3, 7, 15], &[1, 2, 3]);
    for boundary in [0, 1, 2, 5, 10, 20, 40, 60] {
        let pool = Budget::new(100_000).unwrap();
        let raw = Bytes::copy_from_slice(&pool, &b.bytes).unwrap();
        let baseline = pool.used();
        let mut p =
            AacAscParser::with_bit_length(&pool, source(), raw.clone(), b.len, limits()).unwrap();
        for _ in 0..boundary {
            assert!(matches!(
                p.step(&mut WorkBudget::new(1), &AtomicBool::new(false)),
                Ok(BuildStep::Yield)
            ));
        }
        assert!(matches!(
            p.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            p.step(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
            Err(Error::Cancelled)
        ));
    }
}

#[test]
fn physical_padding_and_opaque_suffix_are_retained_under_explicit_bit_bounds() {
    let mut b = lc();
    let valid = b.len;
    b.bytes.extend_from_slice(&[0xff, 0x81, 0]);
    let pool = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&pool, &b.bytes).unwrap();
    let o = run(&pool, raw.clone(), valid, limits()).unwrap();
    assert_eq!(o.source_bytes().as_ref(), b.bytes);
    assert_eq!(o.opaque_suffix_bits().bit_count, 0);
    let mut p = AacAscParser::new(&pool, source(), raw.clone(), limits()).unwrap();
    let o = match p
        .step(&mut WorkBudget::new(1000), &AtomicBool::new(false))
        .unwrap()
    {
        BuildStep::Ready(o) => o,
        _ => panic!(),
    };
    assert_eq!(o.opaque_suffix_bits().bit_count, 24);
    assert_eq!(o.source_bytes().as_ref(), b.bytes);
    assert!(matches!(
        AacAscParser::with_bit_length(&pool, source(), raw, b.bytes.len() * 8 + 1, limits()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn foreign_pool_absolute_overflow_and_zero_work_do_not_allocate() {
    let a = Budget::new(100_000).unwrap();
    let b = Budget::new(100_000).unwrap();
    let raw = Bytes::copy_from_slice(&a, &lc().bytes).unwrap();
    assert!(matches!(
        AacAscParser::new(&b, source(), raw.clone(), limits()),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        AacAscParser::new(
            &a,
            AacSource {
                offset: u64::MAX,
                ..source()
            },
            raw.clone(),
            limits()
        ),
        Err(Error::Overflow)
    ));
    let before = a.used();
    let mut p = AacAscParser::new(&a, source(), raw, limits()).unwrap();
    assert!(matches!(
        p.step(&mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(BuildStep::Yield)
    ));
    assert_eq!(a.used(), before);
}

#[test]
fn comment_and_original_bytes_survive_parser_close_and_release_on_other_thread() {
    let pool = Budget::new(100_000).unwrap();
    let baseline = pool.used();
    let (b, _) = pce([1, 0, 0, 0, 0, 0], &[0xff, 0x80, 0]);
    let o = run(
        &pool,
        Bytes::copy_from_slice(&pool, &b.bytes).unwrap(),
        b.len,
        limits(),
    )
    .unwrap();
    let comment = o.program_config().unwrap().comment_bytes().clone();
    drop(o);
    assert_eq!(comment.as_ref(), [0xff, 0x80, 0]);
    assert!(pool.used() > baseline);
    std::thread::spawn(move || drop(comment)).join().unwrap();
    assert_eq!(pool.used(), baseline);
}
