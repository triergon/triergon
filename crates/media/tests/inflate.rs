//! RFC-authored DEFLATE fixtures. The bit writer is independent of the decoder.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, WorkBudget};
use triergon_matroska::{
    Error,
    incremental::ParseStep,
    storage::SharedBytes,
    zlib::{InflateLimits, Inflater},
};

struct Bits {
    data: Vec<u8>,
    used: usize,
}
impl Bits {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            used: 0,
        }
    }
    fn put(&mut self, value: u32, width: usize) {
        for bit in 0..width {
            if self.used.is_multiple_of(8) {
                self.data.push(0);
            }
            if value & (1 << bit) != 0 {
                let last = self.data.len() - 1;
                self.data[last] |= 1 << (self.used % 8);
            }
            self.used += 1;
        }
    }
    fn fixed(&mut self, symbol: u32) {
        let (code, width) = match symbol {
            0..=143 => (0x30 + symbol, 8),
            144..=255 => (0x190 + symbol - 144, 9),
            256..=279 => (symbol - 256, 7),
            280..=287 => (0xc0 + symbol - 280, 8),
            _ => panic!("fixture symbol"),
        };
        self.put(code.reverse_bits() >> (32 - width), width);
    }
}
fn wrap(raw: Vec<u8>, expected: &[u8]) -> Vec<u8> {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in expected {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    [
        vec![0x78, 0x01],
        raw,
        ((b << 16) | a).to_be_bytes().to_vec(),
    ]
    .concat()
}
fn fixed_match() -> (Vec<u8>, Vec<u8>) {
    let mut bits = Bits::new();
    bits.put(1, 1);
    bits.put(1, 2);
    bits.fixed(u32::from(b'a'));
    bits.fixed(285);
    bits.put(0, 5);
    bits.fixed(256);
    let expected = vec![b'a'; 259];
    (wrap(bits.data, &expected), expected)
}
fn dynamic() -> (Vec<u8>, Vec<u8>) {
    let mut bits = Bits::new();
    dynamic_block(&mut bits, true);
    (wrap(bits.data, b"a"), b"a".to_vec())
}
fn dynamic_block(bits: &mut Bits, last: bool) {
    bits.put(u32::from(last), 1);
    bits.put(2, 2);
    bits.put(0, 5);
    bits.put(0, 5);
    bits.put(14, 4);
    let order = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1];
    for symbol in order {
        bits.put(u32::from(symbol == 1 || symbol == 18), 3);
    }
    // Code-length tree: value 1 has code 0, repeat-zero 18 has code 1.
    bits.put(1, 1);
    bits.put(86, 7); // 97 zero widths
    bits.put(0, 1); // literal 'a' has width 1
    bits.put(1, 1);
    bits.put(127, 7); // 138 zero widths
    bits.put(1, 1);
    bits.put(9, 7); // 20 zero widths
    bits.put(0, 1);
    bits.put(0, 1); // EOB and sole distance have width 1
    bits.put(0, 1);
    bits.put(1, 1); // literal 'a', EOB
}
fn dynamic_pair_block(bits: &mut Bits, last: bool) {
    bits.put(u32::from(last), 1);
    bits.put(2, 2);
    bits.put(0, 5); // literal alphabet through EOB
    bits.put(0, 5); // one unused zero-width distance entry
    bits.put(14, 4);
    let order = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1];
    for symbol in order {
        bits.put(
            if [0, 1, 2, 18].contains(&symbol) {
                2
            } else {
                0
            },
            3,
        );
    }
    // Four two-bit CL codes in symbol order: 0=00, 1=01, 2=10, 18=11.
    bits.put(3, 2);
    bits.put(86, 7); // widths 0..96 are zero
    bits.put(1, 2);
    bits.put(1, 2); // 'a' and 'b' have two-bit codes
    bits.put(3, 2);
    bits.put(127, 7); // 138 zero widths
    bits.put(3, 2);
    bits.put(8, 7); // 19 zero widths
    bits.put(2, 2); // EOB has width one
    bits.put(0, 2); // no distance codes used
    bits.put(1, 2); // literal 'a' has canonical code 10
    bits.put(3, 2); // literal 'b' has canonical code 11
    bits.put(0, 1); // EOB has canonical code 0
}
fn limits() -> InflateLimits {
    InflateLimits {
        output_bytes: 4096,
        expansion_ratio: 1024,
        work: 100_000,
    }
}

fn multiple_stored() -> (Vec<u8>, Vec<u8>) {
    let expected: Vec<u8> = (0..12_000).map(|n| (n % 251) as u8).collect();
    let mut raw = Vec::new();
    for (index, chunk) in expected.chunks(6000).enumerate() {
        raw.push(u8::from(index == 1));
        let length = chunk.len() as u16;
        raw.extend(length.to_le_bytes());
        raw.extend((!length).to_le_bytes());
        raw.extend(chunk);
    }
    (wrap(raw, &expected), expected)
}

#[test]
fn dynamic_distance_alphabet_keeps_fifteen_bit_codes_under_all_work_schedules() {
    let mut bits = Bits::new();
    bits.put(1, 1);
    bits.put(2, 2);
    bits.put(1, 5); // literals 0..257
    bits.put(15, 5); // sixteen distance symbols
    bits.put(15, 4); // all nineteen code-length entries
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    for symbol in order {
        bits.put(
            match symbol {
                16 | 17 => 0,
                15 | 18 => 5,
                _ => 4,
            },
            3,
        );
    }
    // CL symbols 0..14 have four-bit canonical codes 0..14;
    // symbols 15 and 18 have five-bit canonical codes 30 and 31.
    bits.put(31, 5);
    bits.put(86, 7); // 97 zero widths
    bits.put(8, 4); // literal 'a' has width 1
    bits.put(31, 5);
    bits.put(127, 7); // 138 zero widths
    bits.put(31, 5);
    bits.put(9, 7); // 20 zero widths
    bits.put(4, 4);
    bits.put(4, 4); // EOB and match-length symbol 257 have width 2
    // Complete distance prefix tree: 0, 10, 110, ... two fifteen-bit leaves.
    for width in (1_u32..=15).chain([15]) {
        if width == 15 {
            bits.put(15, 5); // CL symbol 15, canonical 11110
        } else {
            bits.put(width.reverse_bits() >> 28, 4);
        }
    }
    // Supply enough identical history for every distance, including code 15
    // (distance 193). Each subsequent three-byte copy must remain all 'a'.
    bits.put(0, 16);
    for _ in 16..193 {
        bits.put(0, 1);
    }
    for code in 0_u32..16 {
        bits.put(3, 2); // literal/length symbol 257, match length 3
        let width = (code + 1).min(15);
        let canonical = if code == 15 {
            0x7fff
        } else {
            (1_u32 << width) - 2
        };
        bits.put(canonical.reverse_bits() >> (32 - width), width as usize);
        let extra = if code < 4 { 0 } else { code / 2 - 1 };
        bits.put(0, extra as usize);
    }
    bits.put(1, 2); // EOB canonical 10
    let expected = vec![b'a'; 193 + 16 * 3];
    let input = wrap(bits.data, &expected);
    for units in [1, 2, 7, 8192] {
        for policy in [258, 259] {
            let pool = Budget::new(64 * 1024).unwrap();
            let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
            let mut decoder = Inflater::new(
                source,
                InflateLimits {
                    work: policy,
                    ..limits()
                },
                &pool,
            )
            .unwrap();
            let mut outcome = None;
            for _ in 0..50_000 {
                let mut work = WorkBudget::new(units);
                let event = decoder.step(&mut work, &AtomicBool::new(false));
                assert!(work.spent() <= units);
                match event {
                    Ok(ParseStep::Yield) => (),
                    Ok(ParseStep::Event(bytes)) => {
                        outcome = Some(Ok(bytes));
                        break;
                    }
                    Err(error) => {
                        outcome = Some(Err(error));
                        break;
                    }
                    _ => panic!("bounded complete dynamic stream with ample pool"),
                }
            }
            if policy == 259 {
                assert_eq!(outcome.unwrap().unwrap().as_ref(), expected.as_slice());
            } else {
                assert!(matches!(outcome, Some(Err(Error::Policy("inflate work")))));
            }
        }
    }
}

#[test]
fn long_match_stream_stays_within_its_decode_and_checksum_work_bound() {
    const REPEATS: usize = 128;
    let mut bits = Bits::new();
    bits.put(1, 1);
    bits.put(1, 2);
    bits.fixed(u32::from(b'a'));
    for _ in 0..REPEATS {
        bits.fixed(285);
        bits.put(0, 5); // 258 bytes from distance one
    }
    bits.fixed(256);
    let expected = vec![b'a'; 1 + 258 * REPEATS];
    let input = wrap(bits.data, &expected);
    // One unit per materialized byte and checksum byte, two per encoded
    // match (length/distance), plus a fixed allowance for stream boundaries.
    let bound = (2 * expected.len() + 2 * REPEATS + 64) as u64;
    for units in [1, 7, 8192] {
        let pool = Budget::new(256 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut decoder = Inflater::new(
            source,
            InflateLimits {
                output_bytes: expected.len(),
                work: (3 + 259 * REPEATS) as u64,
                ..limits()
            },
            &pool,
        )
        .unwrap();
        let mut spent = 0;
        let mut result = None;
        for _ in 0..200_000 {
            let mut work = WorkBudget::new(units);
            let event = decoder.step(&mut work, &AtomicBool::new(false)).unwrap();
            assert!(work.spent() <= units);
            spent += work.spent();
            match event {
                ParseStep::Yield => (),
                ParseStep::Event(bytes) => {
                    result = Some(bytes);
                    break;
                }
                _ => panic!("complete fixed stream with ample pooled storage"),
            }
        }
        assert_eq!(result.unwrap().as_ref(), expected.as_slice());
        assert!(
            spent <= bound,
            "spent {spent}, bound {bound}, step units {units}"
        );
    }
}

#[test]
fn successive_dynamic_blocks_replace_code_lengths_literals_and_empty_distance_tables() {
    let mut bits = Bits::new();
    dynamic_block(&mut bits, false);
    dynamic_pair_block(&mut bits, false);
    dynamic_block(&mut bits, false);
    dynamic_pair_block(&mut bits, true);
    let input = wrap(bits.data, b"aabaab");
    for units in [1, 3, 17, 8192] {
        for policy in [13, 14] {
            let pool = Budget::new(64 * 1024).unwrap();
            let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
            let mut decoder = Inflater::new(
                source,
                InflateLimits {
                    work: policy,
                    ..limits()
                },
                &pool,
            )
            .unwrap();
            let mut outcome = None;
            for _ in 0..50_000 {
                let mut work = WorkBudget::new(units);
                let event = decoder.step(&mut work, &AtomicBool::new(false));
                assert!(work.spent() <= units);
                match event {
                    Ok(ParseStep::Yield) => (),
                    Ok(ParseStep::Event(bytes)) => {
                        outcome = Some(Ok(bytes));
                        break;
                    }
                    Err(error) => {
                        outcome = Some(Err(error));
                        break;
                    }
                    _ => {
                        panic!("complete dynamic blocks must finish or report the permanent policy")
                    }
                }
            }
            if policy == 14 {
                assert_eq!(outcome.unwrap().unwrap().as_ref(), b"aabaab");
            } else {
                assert!(matches!(outcome, Some(Err(Error::Policy("inflate work")))));
            }
        }
    }
}

#[test]
fn literal_backpressure_under_larger_steps_keeps_exact_permanent_work_and_cancel_state() {
    let (input, expected) = fixed_match();
    for units in [2, 3, 7, 8192] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut decoder = Inflater::new(
            source,
            InflateLimits {
                work: 262,
                ..limits()
            },
            &pool,
        )
        .unwrap();
        let occupied = pool.reserve(pool.available()).unwrap();
        let mut blocked = false;
        for _ in 0..100 {
            let mut work = WorkBudget::new(units);
            let event = decoder.step(&mut work, &AtomicBool::new(false)).unwrap();
            assert!(work.spent() <= units);
            match event {
                ParseStep::Yield => (),
                ParseStep::Backpressure { .. } => {
                    blocked = true;
                    break;
                }
                _ => panic!("the initial literal must wait for output storage"),
            }
        }
        assert!(blocked);
        assert_eq!(decoder.output_len(), 0);
        drop(occupied);
        let mut result = None;
        for _ in 0..20_000 {
            let mut work = WorkBudget::new(units);
            let event = decoder.step(&mut work, &AtomicBool::new(false)).unwrap();
            assert!(work.spent() <= units);
            match event {
                ParseStep::Yield => (),
                ParseStep::Event(bytes) => {
                    result = Some(bytes);
                    break;
                }
                _ => panic!("released storage must finish within the exact permanent policy"),
            }
        }
        assert_eq!(result.unwrap().as_ref(), expected.as_slice());
        assert!(matches!(
            decoder.step(&mut WorkBudget::new(units), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        assert!(
            decoder
                .step(&mut WorkBudget::new(units), &AtomicBool::new(false))
                .is_err()
        );
    }
}

#[test]
fn mixed_dynamic_fixed_and_stored_blocks_keep_bit_state_across_uneven_yields() {
    let mut bits = Bits::new();
    dynamic_block(&mut bits, false);
    bits.put(0, 1);
    bits.put(1, 2);
    bits.fixed(u32::from(b'b'));
    bits.fixed(256);
    bits.put(1, 1);
    bits.put(0, 2);
    while !bits.used.is_multiple_of(8) {
        bits.put(0, 1);
    }
    bits.put(4, 16);
    bits.put(u32::from(!4_u16), 16);
    for byte in b"cdef" {
        bits.put(u32::from(*byte), 8);
    }
    let input = wrap(bits.data, b"abcdef");
    for schedule in [[1, 1, 1, 1], [0, 2, 7, 3], [17, 257, 4, 8192]] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut decoder = Inflater::new(source, limits(), &pool).unwrap();
        let mut result = None;
        for step in 0..20_000 {
            let units = schedule[step % schedule.len()];
            let mut work = WorkBudget::new(units);
            let event = decoder.step(&mut work, &AtomicBool::new(false)).unwrap();
            assert!(work.spent() <= units);
            match event {
                ParseStep::Yield => (),
                ParseStep::Event(bytes) => {
                    result = Some(bytes);
                    break;
                }
                _ => panic!("complete mixed stream must retain the same bit position"),
            }
        }
        assert_eq!(result.unwrap().as_ref(), b"abcdef");
    }
}

#[test]
fn multi_stored_batches_and_checksum_boundaries_preserve_exact_bytes_and_work_policy() {
    let (input, expected) = multiple_stored();
    for units in [1, 7, 4096, 32768] {
        let pool = Budget::new(256 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut decoder = Inflater::new(
            source,
            InflateLimits {
                output_bytes: expected.len(),
                expansion_ratio: 1,
                work: expected.len() as u64 + 2,
            },
            &pool,
        )
        .unwrap();
        let mut result = None;
        for _ in 0..100_000 {
            let mut work = WorkBudget::new(units);
            match decoder.step(&mut work, &AtomicBool::new(false)).unwrap() {
                ParseStep::Yield => (),
                ParseStep::Event(bytes) => {
                    result = Some(bytes);
                    break;
                }
                _ => panic!("bounded complete input with ample storage"),
            }
            assert!(work.spent() <= units);
        }
        assert_eq!(result.unwrap().as_ref(), expected.as_slice());
    }
}

#[test]
fn history_copy_retry_keeps_the_middle_of_an_overlapping_match() {
    let (input, expected) = fixed_match();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
    let mut decoder = Inflater::new(source, limits(), &pool).unwrap();
    for _ in 0..20_000 {
        if decoder.output_len() == 256 {
            break;
        }
        assert!(matches!(
            decoder
                .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap(),
            ParseStep::Yield
        ));
    }
    assert_eq!(decoder.output_len(), 256);
    let occupied = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        decoder
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        ParseStep::Backpressure { .. }
    ));
    assert_eq!(decoder.output_len(), 256);
    drop(occupied);
    let mut result = None;
    for _ in 0..20_000 {
        match decoder
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(bytes) => {
                result = Some(bytes);
                break;
            }
            _ => panic!("released storage must resume the same match"),
        }
    }
    assert_eq!(result.unwrap().as_ref(), expected.as_slice());
}

#[test]
fn overlapping_three_byte_history_keeps_its_phase_across_work_schedules() {
    let mut bits = Bits::new();
    bits.put(1, 1);
    bits.put(1, 2);
    for byte in b"abc" {
        bits.fixed(u32::from(*byte));
    }
    bits.fixed(285); // 258-byte repeat, fixed distance code 2 means distance 3.
    bits.put(2_u32.reverse_bits() >> 27, 5);
    bits.fixed(256);
    let expected = b"abc".repeat(87);
    let input = wrap(bits.data, &expected);
    for units in [1, 17, 8192] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut decoder = Inflater::new(source, limits(), &pool).unwrap();
        let mut result = None;
        for _ in 0..20_000 {
            let mut work = WorkBudget::new(units);
            match decoder.step(&mut work, &AtomicBool::new(false)).unwrap() {
                ParseStep::Yield => (),
                ParseStep::Event(bytes) => {
                    result = Some(bytes);
                    break;
                }
                _ => panic!("valid overlapping history must progress"),
            }
            assert!(work.spent() <= units);
        }
        assert_eq!(result.unwrap().as_ref(), expected.as_slice());
    }
}

#[test]
fn empty_decoded_event_after_checksum_needs_no_empty_storage_owner() {
    let input = wrap(vec![3, 0], &[]);
    let pool = Budget::new(4096).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
    let before = pool.used();
    let mut decoder = Inflater::new(source, limits(), &pool).unwrap();
    let mut result = None;
    for _ in 0..4096 {
        match decoder
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Yield => (),
            ParseStep::Event(bytes) => {
                result = Some(bytes);
                break;
            }
            _ => panic!("empty valid stream must still publish an event"),
        }
    }
    assert!(result.unwrap().is_empty());
    assert_eq!(pool.used(), before);
    let mut corrupt = input;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    let source = SharedBytes::copy_from_slice(&pool, &corrupt).unwrap();
    let mut decoder = Inflater::new(source, limits(), &pool).unwrap();
    assert!(matches!(
        decoder.step(&mut WorkBudget::new(10_000), &AtomicBool::new(false)),
        Err(Error::Invalid("Adler-32 mismatch"))
    ));
}

#[test]
fn all_deflate_block_types_progress_with_one_unit_and_keep_output_private_until_checksum() {
    let stored = vec![120, 1, 1, 3, 0, 252, 255, 97, 98, 99, 2, 77, 1, 39];
    for (input, expected) in [(stored, b"abc".to_vec()), fixed_match(), dynamic()] {
        let pool = Budget::new(1024 * 1024).unwrap();
        let baseline = pool.used();
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut inflater = Inflater::new(source, limits(), &pool).unwrap();
        let mut result = None;
        for _ in 0..20_000 {
            let mut work = WorkBudget::new(1);
            match inflater.step(&mut work, &AtomicBool::new(false)).unwrap() {
                ParseStep::Yield => (),
                ParseStep::Event(output) => {
                    result = Some(output);
                    break;
                }
                _ => panic!("unexpected inflater transition"),
            }
            assert!(work.spent() <= 1);
        }
        let result = result.expect("one-unit work must not deadlock");
        assert_eq!(result.as_ref(), expected);
        let before = pool.used();
        let alias = result.clone();
        assert_eq!(pool.used(), before);
        drop(inflater);
        assert_eq!(alias.as_ref(), expected);
        assert!(pool.used() > baseline);
        drop((alias, result));
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn inflate_backpressure_preserves_literal_or_match_state_for_retry() {
    let (input, expected) = fixed_match();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
    let mut inflater = Inflater::new(source, limits(), &pool).unwrap();
    let occupied = pool.reserve(pool.available()).unwrap();
    let mut blocked = false;
    for _ in 0..20_000 {
        match inflater
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Backpressure { .. } => {
                blocked = true;
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("output must wait for storage"),
        }
    }
    assert!(blocked);
    drop(occupied);
    let mut result = None;
    for _ in 0..20_000 {
        match inflater
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            ParseStep::Event(output) => {
                result = Some(output);
                break;
            }
            ParseStep::Yield => (),
            _ => panic!("released output must unblock"),
        }
    }
    assert_eq!(result.unwrap().as_ref(), expected);
}

#[test]
fn corrupt_checksum_cancellation_and_permanent_limits_never_publish_partial_output() {
    let (input, _) = fixed_match();
    for corrupt in [true, false] {
        let pool = Budget::new(64 * 1024).unwrap();
        let mut input = input.clone();
        if corrupt {
            let last = input.len() - 1;
            input[last] ^= 1;
        }
        let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
        let mut inflater = Inflater::new(
            source,
            if corrupt {
                limits()
            } else {
                InflateLimits {
                    output_bytes: 100,
                    ..limits()
                }
            },
            &pool,
        )
        .unwrap();
        let mut error = None;
        for _ in 0..20_000 {
            match inflater.step(&mut WorkBudget::new(1), &AtomicBool::new(false)) {
                Ok(ParseStep::Yield) => (),
                Err(value) => {
                    error = Some(value);
                    break;
                }
                _ => panic!("invalid/over-policy packet must not be emitted"),
            }
        }
        assert!(
            matches!(error, Some(Error::Invalid("Adler-32 mismatch")))
                || !corrupt && matches!(error, Some(Error::Policy(_)))
        );
    }
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &input).unwrap();
    let mut inflater = Inflater::new(source, limits(), &pool).unwrap();
    assert!(matches!(
        inflater.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(
        inflater
            .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .is_err()
    );
}
