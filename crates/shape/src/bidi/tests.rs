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

/// `text` with each `(code point, count)` repeated.
fn repeated(parts: &[(u32, usize)]) -> Vec<u32> {
    parts
        .iter()
        .flat_map(|&(c, n)| std::iter::repeat_n(c, n))
        .collect()
}

/// The levels after resolving `text` as one paragraph and reordering it as one
/// line, with brackets taking part.
fn levels_of(text: &[u32], base: BaseDirection) -> Vec<u8> {
    run(text, base, true, &[]).1
}

/// FriBidi's levels where explicit embeddings and isolates reach level 125, as
/// its comparison shows them.
#[test]
fn deepest_levels() {
    const LRE: u32 = 0x202A;
    const LRO: u32 = 0x202D;
    const PDF: u32 = 0x202C;
    const RLI: u32 = 0x2067;
    const LRI: u32 = 0x2066;
    const FSI: u32 = 0x2068;
    const PDI: u32 = 0x2069;
    // An isolate that fits is raised past a full stack; its PDI empties it.
    let text = repeated(&[(LRE, 63), (RLI, 1), (0x21, 1), (PDI, 1), (0x21, 1)]);
    assert_eq!(levels_of(&text, Ltr)[63..], [124, 125, 0, 0]);
    // A PDF pops an embedding inside an overflowed isolate.
    let text = repeated(&[
        (LRE, 62),
        (LRI, 1),
        (0x21, 1),
        (PDF, 1),
        (0x21, 1),
        (PDI, 1),
        (0x21, 1),
    ]);
    assert_eq!(levels_of(&text, Ltr)[62..], [124, 124, 124, 122, 122, 122]);
    // The raised isolate counts as an overflowed embedding for the PDFs.
    let text = repeated(&[(LRO, 63), (RLI, 1), (PDF, 3), (0x661, 1)]);
    assert_eq!(levels_of(&text, Ltr)[63..], [124, 124, 124, 124, 122]);
    // Looking forward, W5 passes over the PDI of an overflowed isolate.
    let text = repeated(&[(FSI, 64), (0x5D0, 1), (0x24, 1), (PDI, 1), (0x31, 1)]);
    assert_eq!(levels_of(&text, Ltr)[62..], [124, 124, 125, 126, 125, 126]);
    // The PDI of a valid isolate returns to its initiator's depth past an
    // overflowed one, so that the two link.
    let text = repeated(&[(LRE, 61), (0x627, 1), (LRI, 2), (PDI, 2), (0x31, 1)]);
    assert_eq!(levels_of(&text, Ltr)[61..], [123, 123, 124, 124, 123, 124]);
    // A mark after the PDI of an overflowed isolate, after a European number
    // after an Arabic letter, is an Arabic number.
    let text = repeated(&[(LRI, 63), (0x627, 1), (0x31, 1), (PDI, 1), (0x300, 1)]);
    assert_eq!(levels_of(&text, Ltr)[63..], [125, 126, 125, 126]);
    // A PDF under an isolate leaves an overflowed embedding counted.
    let text = repeated(&[
        (LRI, 62),
        (LRE, 1),
        (PDF, 1),
        (0x202B, 1),
        (0x21, 1),
        (PDI, 1),
        (0x202B, 1),
        (0x21, 1),
    ]);
    assert_eq!(
        levels_of(&text, Ltr)[62..],
        [122, 122, 122, 124, 122, 122, 123]
    );
    // An empty isolate raised past overflowed ones, at the deepest depth: the
    // text before it takes the higher level's direction only where a strong
    // class follows at that level.
    let mut text = repeated(&[(LRI, 124), (0x5D0, 1), (0x24, 1), (RLI, 1), (PDI, 1)]);
    text.push(0x21);
    assert_eq!(levels_of(&text, Ltr)[124..], [125, 124, 124, 125, 125]);
    *text.last_mut().unwrap() = 0x61;
    assert_eq!(levels_of(&text, Ltr)[124..], [125, 125, 125, 125, 126]);
}

/// FriBidi's bracket pairs, as its comparison shows them.
#[test]
fn bracket_pairs() {
    // Brackets in two isolates of one depth pair.
    let text = [0x2067, 0x61, 0x28, 0x61, 0x2069, 0x2067, 0x29, 0x2069];
    assert_eq!(levels_of(&text, Ltr), [0, 2, 2, 2, 0, 0, 2, 0]);
    // A full stack keeps the pairs found before it...
    let mut text = vec![0x5D0, 0x28, 0x5D0, 0x29];
    text.extend(repeated(&[(0x28, 64), (0x61, 1)]));
    assert_eq!(levels_of(&text, Ltr)[..4], [1, 1, 1, 1]);
    // ...and ends the pairing of the paragraph, at every depth.
    let mut text = repeated(&[(0x2066, 1), (0x28, 64), (0x2069, 1)]);
    text.extend([0x5D0, 0x28, 0x5D0, 0x29, 0x61]);
    assert_eq!(levels_of(&text, Ltr)[66..], [1, 1, 1, 0, 0]);
    // A bracket after a mark after a neutral joins the neutral's run and
    // loses its bracket property.
    let text = [0x5B, 0x7C0, 0xFF08, 0x300, 0x5D, 0x31];
    assert_eq!(levels_of(&text, Ltr), [0, 1, 1, 1, 1, 2]);
    let text = [0x7C0, 0x2069, 0x20D0, 0xFE59, 0x2070, 0x29];
    assert_eq!(levels_of(&text, Ltr), [1, 1, 1, 1, 2, 0]);
    // A level run that starts with a PDI keeps the brackets open at its
    // depth: past 63 embeddings the PDI drops to level 0, and the bracket
    // before the isolate pairs with the one after it.
    let mut text = repeated(&[(0x202A, 63), (0x6DD, 1), (0x28, 1), (0x61C, 1)]);
    text.extend([0x2068, 0x710, 0x2069, 0xFF09]);
    assert_eq!(levels_of(&text, Ltr).last(), Some(&1));
    let last = text.len() - 1;
    text[last] = 0x21;
    assert_eq!(levels_of(&text, Ltr).last(), Some(&0));
}

/// FriBidi's simple shaping, as its comparison shows it.
#[test]
fn simple_shaping_choices() {
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
    // At an even level a right-joining letter joins the letter after it.
    assert_eq!(shape(&[0x202D, 0x76B, 0x62B], Rtl)[2], 0xFE9B);
    // A mark between LAM and ALEF stops the ligature, not the joining.
    assert_eq!(shape(&[0x644, 0x670, 0x625], Rtl), [0xFEDF, 0x670, 0xFE88]);
    // A mark at another level ends the joining.
    assert_eq!(
        shape(&[0x0A, 0x684, 0x202A, 0x670, 0x635], Ltr),
        [0x0A, 0xFB72, 0x202A, 0x670, 0xFEB9]
    );
    // After a paragraph separator the implicit rules go on at level 0.
    assert_eq!(
        shape(&[0x5D0, 0x2029, 0x627, 0x28, 0x628], Rtl),
        [0x5D0, 0x2029, 0xFE8D, 0x29, 0xFE8F]
    );
    assert_eq!(shape(&[0x2029, 0x7B, 0x5D0], Rtl)[1], 0x7B);
    assert_eq!(shape(&[0x2066, 0x2029, 0x7B, 0x5D0], Rtl)[2], 0x7D);
    // An isolate open across the separator keeps N1 from joining neutrals
    // across it, unless a PDI closing such an isolate comes right after it.
    let text = [0x600, 0x2068, 0x2029, 0x21, 0x2069, 0xFF08, 0x6DD];
    assert_eq!(shape(&text, Ltr)[5], 0xFF08);
    let text = [0x600, 0x2068, 0x2029, 0x2069, 0xFF08, 0x6DD];
    assert_eq!(shape(&text, Ltr)[4], 0xFF09);
    let text = [0x660, 0x2068, 0x2068, 0x2029, 0x2069, 0x29, 0x5D1];
    assert_eq!(shape(&text, Ltr)[5], 0x28);
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
fn deep_nesting_costs_linear_work() {
    // Isolates far deeper than level 125, with brackets and empty isolates at
    // the deepest depth: the levels are FriBidi's, and the work per character
    // does not grow with the depth.
    let budget = Budget::new(1 << 26).expect("budget");
    let mut spent = Vec::new();
    for depth in [5_000, 20_000] {
        let mut text = repeated(&[(0x2066, depth), (0x5D0, 1), (0x24, 1)]);
        for _ in 0..depth / 4 {
            text.extend([0x2067, 0x2069, 0x28, 0x21]);
        }
        let text: Vec<char> = text.iter().filter_map(|&c| char::from_u32(c)).collect();
        let classes: Vec<BidiClass> = text.iter().map(|&c| BidiClass::of(c)).collect();
        let brackets: Vec<Option<Bracket>> = text.iter().map(|&c| Bracket::of(c)).collect();
        let mut levels = vec![Level::default(); text.len()];
        let mut resolver = Resolver::new(&budget);
        resolver
            .resolve(&classes, Some(&brackets), Ltr, &mut levels)
            .expect("resolve");
        let levels: Vec<u8> = levels[depth..depth + 4].iter().map(|l| l.get()).collect();
        assert_eq!(levels, [125, 124, 124, 125]);
        spent.push(resolver.work_spent() as f64 / text.len() as f64);
    }
    assert!(spent[1] < 32.0 && spent[1] < spent[0] * 1.1, "{spent:?}");
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
    let chars = text.len() as u64;
    // Without brackets the isolates alone cost little.
    let mut resolver = Resolver::new(&budget);
    assert!(resolve(&mut resolver, None).is_ok());
    assert!(resolver.work_spent() < 64 * chars);
    // With them, N0 looks inside every pair at every depth: thousands of units
    // per character, within the default limits but not within half of them.
    assert!(resolve(&mut resolver, Some(&brackets)).is_ok());
    assert!(resolver.work_spent() > 2048 * chars);
    let half = Limits {
        work_per_char: 2048,
        ..Limits::default()
    };
    assert_eq!(
        resolve(&mut Resolver::with_limits(&budget, half), Some(&brackets)),
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
