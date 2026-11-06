//! RFC code, truncation and resumable construction guards.
use super::*;

fn bits(data: &[u8]) -> Bits<'_> {
    Bits {
        data,
        at: 0,
        reservoir: 0,
        available: 0,
    }
}
fn encoded(code: u16, width: usize) -> [u8; 2] {
    // Independent std bit reversal, rather than the implementation's const
    // reverse_width helper. RFC codes are transmitted most-significant first.
    (code.reverse_bits() >> (16 - width)).to_le_bytes()
}

#[test]
fn immutable_fixed_tables_match_every_rfc_code_including_reserved_symbols() {
    for symbol in 0_u16..288 {
        let (code, width) = match symbol {
            0..=143 => (0x30 + symbol, 8),
            144..=255 => (0x190 + symbol - 144, 9),
            256..=279 => (symbol - 256, 7),
            _ => (0xc0 + symbol - 280, 8),
        };
        let input = encoded(code, width);
        let mut input = bits(&input[..width.div_ceil(8)]);
        assert_eq!(
            FIXED_LITERAL.symbol(&mut input).unwrap(),
            usize::from(symbol)
        );
        // Take/peek bookkeeping must consume exactly the code, not padded bits.
        assert_eq!(input.at * 8 - input.available, width);
    }
    for symbol in 0_u16..32 {
        let input = encoded(symbol, 5);
        let mut input = bits(&input[..1]);
        assert_eq!(
            FIXED_DISTANCE.symbol(&mut input).unwrap(),
            usize::from(symbol)
        );
        assert_eq!(input.at * 8 - input.available, 5);
    }
}

#[test]
fn dynamic_quick_fill_resumes_one_actual_slot_at_a_time_and_keeps_long_codes() {
    // Complete prefix tree: 0, 10, 110, ... and two 15-bit leaves.
    // This includes short quick entries and a real >9-bit fallback prefix.
    let lengths = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 15];
    let mut tree = Tree::empty();
    let mut builder = Build::new(false);
    let cancel = AtomicBool::new(false);
    let mut ready = false;
    for _ in 0..4096 {
        let mut work = WorkBudget::new(1);
        ready = builder
            .advance(&mut tree, &lengths, &mut work, &cancel)
            .unwrap();
        assert!(work.spent() <= 1);
        if ready {
            break;
        }
    }
    assert!(
        ready,
        "one-unit work must not defer an indivisible whole table build"
    );
    for (symbol, width) in lengths.into_iter().enumerate() {
        let width = usize::from(width);
        let code = if symbol == 15 {
            0x7fff
        } else {
            (1_u16 << width) - 2
        };
        let input = encoded(code, width);
        assert_eq!(
            tree.symbol(&mut bits(&input[..width.div_ceil(8)])).unwrap(),
            symbol
        );
    }
    // Nine all-one prefix bits have no short code in this tree.
    assert_eq!(tree.quick[511], 0);
    let long = encoded(0x7fff, 15);
    assert!(matches!(
        tree.symbol(&mut bits(&long[..1])),
        Err(Error::Truncated)
    ));
    let buffered = [0xff, 0xff];
    let mut partial = Bits {
        data: &buffered,
        at: 2,
        reservoir: 0xfff,
        available: 12,
    };
    assert!(matches!(tree.symbol(&mut partial), Err(Error::Truncated)));
}

#[test]
fn quick_lookup_never_promotes_zero_padding_into_missing_input() {
    let mut empty = bits(&[]);
    assert!(matches!(
        FIXED_LITERAL.symbol(&mut empty),
        Err(Error::Truncated)
    ));
    // EOB is a seven-bit zero code. A padded 9-bit peek is legal only when
    // those seven actual encoded bits exist; a full ninth bit is unnecessary.
    let eob = [0_u8];
    assert_eq!(FIXED_LITERAL.symbol(&mut bits(&eob)).unwrap(), 256);
    let mut too_short = Bits {
        data: &eob,
        at: 1,
        reservoir: 0,
        available: 6,
    };
    assert!(matches!(
        FIXED_LITERAL.symbol(&mut too_short),
        Err(Error::Truncated)
    ));
}

#[test]
fn cancellation_interrupts_dynamic_lookup_fill_without_a_completed_table() {
    let lengths = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 15];
    let mut tree = Tree::empty();
    let mut builder = Build::new(false);
    let cancel = AtomicBool::new(false);
    for _ in 0..80 {
        builder
            .advance(&mut tree, &lengths, &mut WorkBudget::new(1), &cancel)
            .unwrap();
    }
    cancel.store(true, Ordering::Relaxed);
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        builder.advance(&mut tree, &lengths, &mut work, &cancel),
        Err(Error::Cancelled)
    ));
    assert_eq!(work.spent(), 0);
}

#[test]
fn short_dynamic_alphabets_build_with_work_proportional_to_their_prefix_space() {
    for (lengths, code_lengths) in [
        (vec![1, 1], true),
        (vec![2; 4], true),
        (vec![5; 32], false),
        (vec![1, 2, 3, 4, 5, 6, 7, 7], true),
    ] {
        let width = usize::from(*lengths.iter().max().unwrap());
        // Histogram/ordered-symbol work plus the actual short prefix space,
        // and bounded canonical validation. A two-symbol tree must not build
        // the redundant 512-row representation of a nine-bit alphabet.
        let bound = (4 * lengths.len() + (1 << width) + 64) as u64;
        for units in [1, 17, bound] {
            let mut tree = Tree::empty();
            let mut builder = Build::new(code_lengths);
            let cancel = AtomicBool::new(false);
            let mut spent = 0;
            let mut ready = false;
            while spent <= bound {
                let mut work = WorkBudget::new(units.min(bound + 1 - spent));
                ready = builder
                    .advance(&mut tree, &lengths, &mut work, &cancel)
                    .unwrap();
                assert!(work.spent() > 0 && work.spent() <= units);
                spent += work.spent();
                if ready {
                    break;
                }
            }
            assert!(
                ready && spent <= bound,
                "{lengths:?}: spent {spent}, bound {bound}"
            );
        }
    }
}

#[test]
fn short_tree_lookup_preserves_following_bits_and_rejects_missing_input() {
    let cancel = AtomicBool::new(false);
    for width in 1_usize..=7 {
        // Full constant-width alphabets give an independent code == symbol
        // oracle, including the widths used by distance and code-length trees.
        let lengths = vec![width as u8; 1 << width];
        let mut tree = Tree::empty();
        let mut builder = Build::new(false);
        assert!(
            builder
                .advance(&mut tree, &lengths, &mut WorkBudget::new(4096), &cancel)
                .unwrap()
        );
        for symbol in 0..1_u16 << width {
            let input = encoded(symbol, width);
            let code = u16::from_le_bytes(input);
            for suffix in [0, 0xffff_u16 << width] {
                let data = (code | suffix).to_le_bytes();
                let mut input = bits(&data);
                assert_eq!(tree.symbol(&mut input).unwrap(), usize::from(symbol));
                assert_eq!(input.at * 8 - input.available, width);
                assert_eq!(
                    input.take(16 - width).unwrap(),
                    u32::from((code | suffix) >> width)
                );
            }
            let mut short = Bits {
                data: &input,
                at: input.len(),
                reservoir: u64::from(code & ((1 << (width - 1)) - 1)),
                available: width - 1,
            };
            assert!(matches!(tree.symbol(&mut short), Err(Error::Truncated)));
        }
    }
}
