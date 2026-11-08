//! Whole-algorithm cases in the form of UAX #9's BidiCharacterTest.txt (text,
//! paragraph direction, levels, visual order), with the results of FriBidi
//! 1.0.17 as libass calls it: levels per paragraph, then each line reordered.

use super::*;

/// Resolves and reorders `text` (code points), with lines starting at `breaks`
/// as well as after paragraph separators. Returns the paragraph directions, the
/// levels and the visual-to-logical map.
fn run(
    text: &[u32],
    base: BaseDirection,
    brackets: bool,
    breaks: &[usize],
) -> (Vec<Direction>, Vec<u8>, Vec<usize>) {
    let text: Vec<char> = text.iter().filter_map(|&c| char::from_u32(c)).collect();
    let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
    let brackets: Option<Vec<Option<Bracket>>> =
        brackets.then(|| text.iter().map(|&c| Bracket::of(c)).collect());
    let budget = Budget::new(1 << 24).expect("budget");
    let mut resolver = Resolver::new(&budget);
    let mut levels = vec![Level::default(); text.len()];
    let mut directions = Vec::new();
    let mut lines = Vec::new();
    for paragraph in paragraphs(&classes) {
        let direction = resolver
            .resolve(
                &classes[paragraph.clone()],
                brackets.as_deref().map(|b| &b[paragraph.clone()]),
                base,
                &mut levels[paragraph.clone()],
            )
            .expect("resolve");
        directions.push(direction);
        let mut start = paragraph.start;
        for &at in breaks {
            if at > start && paragraph.contains(&at) {
                lines.push((start..at, direction));
                start = at;
            }
        }
        lines.push((start..paragraph.end, direction));
    }
    let mut map: Vec<usize> = (0..text.len()).collect();
    for (line, direction) in lines {
        reorder_line(
            &classes[line.clone()],
            direction,
            &mut levels[line.clone()],
            &mut map[line],
        )
        .expect("reorder");
    }
    (directions, levels.iter().map(|l| l.get()).collect(), map)
}

use BaseDirection::{Auto, LeftToRight as Ltr, RightToLeft as Rtl};
const LTR: Direction = Direction::LeftToRight;
const RTL: Direction = Direction::RightToLeft;

#[track_caller]
fn check(
    text: &[u32],
    base: BaseDirection,
    brackets: bool,
    breaks: &[usize],
    expected: (Direction, &[u8], &[usize]),
) {
    let (directions, levels, map) = run(text, base, brackets, breaks);
    assert_eq!(directions[0], expected.0, "direction of {text:04X?}");
    assert_eq!(levels, expected.1, "levels of {text:04X?}");
    assert_eq!(map, expected.2, "order of {text:04X?}");
}

#[test]
fn implicit_levels_and_numbers() {
    check(
        &[0x61, 0x20, 0x5D1, 0x20, 0x63],
        Ltr,
        false,
        &[],
        (LTR, &[0, 0, 1, 0, 0], &[0, 1, 2, 3, 4]),
    );
    check(
        &[0x5D0, 0x20, 0x31, 0x32, 0x20, 0x5D1],
        Rtl,
        false,
        &[],
        (RTL, &[1, 1, 2, 2, 1, 1], &[5, 4, 2, 3, 1, 0]),
    );
    // W2 and W4: European digits after AL are Arabic numbers, and so is a
    // common separator between them.
    check(
        &[0x627, 0x31, 0x2E, 0x32, 0x20, 0x61],
        Auto,
        false,
        &[],
        (RTL, &[1, 2, 2, 2, 1, 2], &[5, 4, 1, 2, 3, 0]),
    );
    // P2 and P3 skip numbers.
    check(
        &[0x31, 0x32, 0x20, 0x5D0],
        Auto,
        false,
        &[],
        (RTL, &[2, 2, 1, 1], &[3, 2, 0, 1]),
    );
}

#[test]
fn paired_brackets() {
    // N0b: the brackets take the embedding direction of L inside.
    check(
        &[0x61, 0x28, 0x62, 0x29, 0x5D2],
        Ltr,
        true,
        &[],
        (LTR, &[0, 0, 0, 0, 1], &[0, 1, 2, 3, 4]),
    );
    // N0c1: R inside and before in a right-to-left paragraph, with and without
    // bracket pairs taking part.
    let text = [0x61, 0x28, 0x5D1, 0x29, 0x20, 0x63];
    for brackets in [false, true] {
        check(
            &text,
            Rtl,
            brackets,
            &[],
            (RTL, &[2, 1, 1, 1, 1, 2], &[5, 4, 3, 2, 1, 0]),
        );
    }
}

#[test]
fn isolates() {
    check(
        &[0x5D0, 0x2067, 0x61, 0x20, 0x62, 0x2069, 0x20, 0x5D1],
        Ltr,
        false,
        &[],
        (LTR, &[1, 1, 2, 2, 2, 1, 1, 1], &[7, 6, 5, 2, 3, 4, 1, 0]),
    );
    // FSI takes the direction of its first strong letter.
    check(
        &[0x2068, 0x5D0, 0x61, 0x2069, 0x20, 0x62],
        Ltr,
        false,
        &[],
        (LTR, &[0, 1, 2, 0, 0, 0], &[0, 2, 1, 3, 4, 5]),
    );
}

#[test]
fn fribidi_choices() {
    // The PDI inside an override is not overridden; it resolves as a neutral
    // between the overridden isolate initiator and B.
    check(
        &[0x202E, 0x2066, 0x61, 0x2069, 0x62, 0x202C],
        Ltr,
        false,
        &[],
        (LTR, &[0, 1, 2, 1, 1, 0], &[0, 4, 3, 2, 1, 5]),
    );
    // Marks after an unmatched PDI are ON.
    check(
        &[0x61, 0x300, 0x5D0, 0x300, 0x2069, 0x300],
        Ltr,
        false,
        &[],
        (LTR, &[0, 0, 1, 1, 0, 0], &[0, 1, 3, 2, 4, 5]),
    );
    // L1: a segment separator and the whitespace before it, and trailing
    // whitespace and isolate initiators, take the paragraph level.
    check(
        &[0x61, 0x20, 0x2067, 0x20, 0x09, 0x62, 0x20, 0x2066],
        Rtl,
        false,
        &[],
        (RTL, &[2, 1, 1, 1, 1, 4, 1, 1], &[7, 6, 5, 4, 3, 2, 1, 0]),
    );
    check(
        &[0x5D0, 0x20, 0x20, 0x2029],
        Ltr,
        false,
        &[],
        (LTR, &[1, 0, 0, 0], &[0, 1, 2, 3]),
    );
}

#[test]
fn shared_bracket_stacks() {
    // Seventeen opening brackets outside the isolates: the last overwrites the
    // stack of depth 1, so the closing bracket inside the RLI pairs with it.
    let text = [
        0x7B, 0xFF08, 0x5B, 0x7B, 0x3008, 0x28, 0x7B, 0x28, 0x2066, 0x28, 0x2069, 0x3008, 0xFF08,
        0xFF08, 0x7B, 0x2329, 0xFF08, 0x5B, 0x7B, 0x28, 0x31, 0x2067, 0xFF09,
    ];
    let mut levels = [0; 23];
    levels[9] = 2;
    levels[22] = 2;
    let map: Vec<usize> = (0..23).collect();
    check(&text, Ltr, true, &[], (LTR, &levels, &map));
}

#[test]
fn explicit_levels_stop_at_125() {
    let mut text = vec![0x202B; 130];
    text.push(0x61);
    text.extend([0x202C; 130]);
    let (_, levels, _) = run(&text, Ltr, false, &[]);
    assert_eq!(levels[130], 126, "L at the deepest odd level");
}

#[test]
fn lines() {
    check(
        &[0x61, 0x5D0, 0x62, 0x5D1, 0x63],
        Ltr,
        false,
        &[2, 4],
        (LTR, &[0, 1, 0, 1, 0], &[0, 1, 2, 3, 4]),
    );
    // A paragraph separator ends a paragraph and a line.
    let (directions, levels, map) = run(&[0x5D0, 0x2029, 0x61, 0x5D1], Auto, false, &[]);
    assert_eq!(directions, [RTL, LTR]);
    assert_eq!(levels, [1, 1, 0, 1]);
    assert_eq!(map, [1, 0, 2, 3]);
}

#[test]
fn simple_shaping() {
    let shape = |text: &[u32], base| {
        let mut text: Vec<char> = text.iter().filter_map(|&c| char::from_u32(c)).collect();
        let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
        let budget = Budget::new(1 << 20).expect("budget");
        let mut levels = vec![Level::default(); text.len()];
        Resolver::new(&budget)
            .resolve(&classes, None, base, &mut levels)
            .expect("resolve");
        shape_simple(&mut text, &classes, &levels).expect("shape");
        text.into_iter().map(u32::from).collect::<Vec<_>>()
    };
    assert_eq!(
        shape(&[0x644, 0x627, 0x20, 0x628, 0x628, 0x628], Rtl),
        [0xFEFF, 0xFEFB, 0x20, 0xFE91, 0xFE92, 0xFE90]
    );
    assert_eq!(
        shape(&[0x628, 0x64E, 0x628, 0x20, 0x28, 0x29], Ltr),
        [0xFE91, 0x64E, 0xFE90, 0x20, 0x28, 0x29],
        "an even level does not shape the mark between joined letters"
    );
    // One paragraph with separators inside: explicit processing stops at the
    // first; the bracket resolves to R and is mirrored.
    assert_eq!(
        shape(&[0x627, 0x28, 0x2066, 0x0A, 0x0A, 0x5D0], Auto),
        [0xFE8D, 0x29, 0x2066, 0x0A, 0x0A, 0x5D0]
    );
}

#[test]
fn paragraphs_end_after_separators() {
    let classes = [BidiClass::L, BidiClass::B, BidiClass::B, BidiClass::R];
    assert_eq!(paragraphs(&classes).collect::<Vec<_>>(), [0..2, 2..3, 3..4]);
    assert!(paragraphs(&classes[..2]).eq(std::iter::once(0..2)));
    assert_eq!(paragraphs(&[]).count(), 0);
}

#[test]
fn inputs_must_agree() {
    let budget = Budget::new(1 << 20).expect("budget");
    let mut resolver = Resolver::new(&budget);
    let mut levels = [Level::default(); 2];
    let classes = [BidiClass::L];
    assert!(resolver.resolve(&classes, None, Auto, &mut levels).is_err());
    assert!(
        resolver
            .resolve(&classes, Some(&[]), Auto, &mut levels[..1])
            .is_err()
    );
    assert_eq!(resolver.resolve(&[], None, Rtl, &mut []), Ok(RTL));
    assert_eq!(resolver.resolve(&[], None, Auto, &mut []), Ok(LTR));
}

#[test]
fn mirrors() {
    assert_eq!(mirror('('), Some(')'));
    assert_eq!(mirror('\u{2329}'), Some('\u{232A}'));
    assert_eq!(mirror('a'), None);
}

/// Every class at random, nested deeply: no panics, levels in range and the
/// visual order a permutation, with the working storage reused.
#[test]
fn arbitrary_input() {
    const CHARS: [u32; 30] = [
        0x61, 0x5D0, 0x627, 0x30, 0x2B, 0x23, 0x660, 0x2C, 0x300, 0xAD, 0x2029, 0x09, 0x20, 0x21,
        0x28, 0x29, 0x5B, 0x5D, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066, 0x2067, 0x2068,
        0x2069, 0x2066, 0x2067, 0x202B,
    ];
    let budget = Budget::new(1 << 24).expect("budget");
    let mut resolver = Resolver::new(&budget);
    let mut state = 0x2545_F491_u32;
    for round in 0..400 {
        let len = 1 + (round * 7) % 600;
        let text: Vec<char> = (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                char::from_u32(CHARS[state as usize % CHARS.len()]).unwrap_or('a')
            })
            .collect();
        let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
        let brackets: Vec<Option<Bracket>> = text.iter().map(|&c| Bracket::of(c)).collect();
        let mut levels = vec![Level::default(); len];
        let base = [Ltr, Rtl, Auto][round % 3];
        let direction = resolver
            .resolve(&classes, Some(&brackets), base, &mut levels)
            .expect("resolve");
        assert!(levels.iter().all(|l| l.get() <= 126));
        let mut map: Vec<usize> = (0..len).collect();
        reorder_line(&classes, direction, &mut levels, &mut map).expect("reorder");
        let mut sorted = map.clone();
        sorted.sort_unstable();
        assert!(sorted.iter().copied().eq(0..len));
        let mut shaped = text.clone();
        shape_simple(&mut shaped, &classes, &levels).expect("shape");
    }
}

/// Brackets nested 63 deep in each of 120 nested isolates: every run is inside
/// thousands of pairs, so N0 looks at each thousands of times.
fn nested_brackets_and_isolates() -> Vec<char> {
    let mut text = Vec::new();
    for _ in 0..120 {
        text.push('\u{2067}');
        text.extend(std::iter::repeat_n('(', 63));
    }
    text.push('a');
    for _ in 0..120 {
        text.extend(std::iter::repeat_n(')', 63));
        text.push('\u{2069}');
    }
    text
}

#[test]
fn work_beyond_the_limits_is_a_policy_error() {
    let budget = Budget::new(1 << 24).expect("budget");
    let text = nested_brackets_and_isolates();
    let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
    let brackets: Vec<Option<Bracket>> = text.iter().map(|&c| Bracket::of(c)).collect();
    let mut levels = vec![Level::default(); text.len()];
    let mut resolve =
        |resolver: &mut Resolver, brackets| resolver.resolve(&classes, brackets, Auto, &mut levels);
    // Without brackets the isolates alone cost little.
    let mut resolver = Resolver::new(&budget);
    assert!(resolve(&mut resolver, None).is_ok());
    assert!(resolver.work_spent() < 64 * text.len() as u64);
    // With them, the default limits stop the paragraph.
    assert_eq!(
        resolve(&mut resolver, Some(&brackets)),
        Err(Error::Policy("bidi work"))
    );
    let tight = Limits {
        work_per_char: 1,
        minimum_work: 0,
    };
    assert_eq!(
        resolve(&mut Resolver::with_limits(&budget, tight), None),
        Err(Error::Policy("bidi work"))
    );
}
