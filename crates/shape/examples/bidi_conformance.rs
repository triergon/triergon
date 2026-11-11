//! Runs Unicode's bidi conformance tests through `triergon_shape::bidi`:
//!
//! ```text
//! cargo run --release -p triergon-shape --example bidi_conformance -- \
//!     BidiTest.txt BidiCharacterTest.txt
//! ```
//!
//! `cargo xtask shape conformance` fetches the hash-pinned Unicode 14.0.0 files
//! and runs it on them.
//!
//! Each case is resolved as one paragraph and reordered as one line, as the
//! files assume; levels and visual order are compared for the characters the
//! files give levels (rule X9 removes the others). It prints the cases that pass
//! and fail in each file, and the first failures.

use std::process::ExitCode;

use triergon_core::Budget;
use triergon_shape::Direction;
use triergon_shape::bidi::{BaseDirection, BidiClass, Bracket, Level, Resolver, reorder_line};

/// The failures printed for each file.
const SHOWN: usize = 20;

/// What a case expects: a level per character (`None` where X9 removes it) and
/// the visual order of the others.
struct Expected {
    levels: Vec<Option<u8>>,
    order: Vec<usize>,
}

/// Resolves and reorders one case; `None` where the result matches.
fn run(
    resolver: &mut Resolver,
    classes: &[BidiClass],
    brackets: Option<&[Option<Bracket>]>,
    base: BaseDirection,
    expected: &Expected,
) -> Option<String> {
    let mut levels = vec![Level::default(); classes.len()];
    let direction = match resolver.resolve(classes, brackets, base, &mut levels) {
        Ok(direction) => direction,
        Err(error) => return Some(format!("error {error:?}")),
    };
    let mut map: Vec<usize> = (0..classes.len()).collect();
    if let Err(error) = reorder_line(classes, direction, &mut levels, &mut map) {
        return Some(format!("error {error:?}"));
    }
    let got: Vec<Option<u8>> = levels
        .iter()
        .zip(&expected.levels)
        .map(|(level, e)| e.map(|_| level.get()))
        .collect();
    let order: Vec<usize> = map
        .into_iter()
        .filter(|&at| expected.levels[at].is_some())
        .collect();
    (got != expected.levels || order != expected.order).then(|| {
        format!(
            "levels {} order {:?}, expected {} order {:?}",
            show(&got),
            order,
            show(&expected.levels),
            expected.order
        )
    })
}

fn show(levels: &[Option<u8>]) -> String {
    levels
        .iter()
        .map(|l| l.map_or("x".into(), |l| l.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn levels(field: &str) -> Option<Vec<Option<u8>>> {
    field
        .split_whitespace()
        .map(|l| {
            if l == "x" {
                Some(None)
            } else {
                l.parse().ok().map(Some)
            }
        })
        .collect()
}

fn numbers(field: &str) -> Option<Vec<usize>> {
    field.split_whitespace().map(|n| n.parse().ok()).collect()
}

/// The classes by the short names the files use.
const CLASSES: [(&str, BidiClass); 23] = [
    ("L", BidiClass::L),
    ("R", BidiClass::R),
    ("AL", BidiClass::Al),
    ("EN", BidiClass::En),
    ("ES", BidiClass::Es),
    ("ET", BidiClass::Et),
    ("AN", BidiClass::An),
    ("CS", BidiClass::Cs),
    ("NSM", BidiClass::Nsm),
    ("BN", BidiClass::Bn),
    ("B", BidiClass::B),
    ("S", BidiClass::S),
    ("WS", BidiClass::Ws),
    ("ON", BidiClass::On),
    ("LRE", BidiClass::Lre),
    ("LRO", BidiClass::Lro),
    ("RLE", BidiClass::Rle),
    ("RLO", BidiClass::Rlo),
    ("PDF", BidiClass::Pdf),
    ("LRI", BidiClass::Lri),
    ("RLI", BidiClass::Rli),
    ("FSI", BidiClass::Fsi),
    ("PDI", BidiClass::Pdi),
];

fn class(name: &str) -> Option<BidiClass> {
    CLASSES.iter().find(|&&(n, _)| n == name).map(|&(_, c)| c)
}

#[derive(Default)]
struct Tally {
    passed: usize,
    failed: usize,
}

impl Tally {
    fn record(&mut self, file: &str, line: usize, case: &str, failure: Option<String>) {
        match failure {
            None => self.passed += 1,
            Some(failure) => {
                if self.failed < SHOWN {
                    println!("{file}:{line}: {case}: {failure}");
                }
                self.failed += 1;
            }
        }
    }
}

/// BidiTest.txt: classes, with levels and orders given by `@Levels` and
/// `@Reorder` lines, and a bit set of paragraph directions per case.
fn bidi_test(text: &str, resolver: &mut Resolver) -> Result<Tally, String> {
    let mut tally = Tally::default();
    let mut expected = Expected {
        levels: Vec::new(),
        order: Vec::new(),
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("@Levels:") {
            expected.levels = levels(rest).ok_or(format!("line {}: levels", n + 1))?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("@Reorder:") {
            expected.order = numbers(rest).ok_or(format!("line {}: order", n + 1))?;
            continue;
        }
        if line.is_empty() || line.starts_with('@') {
            continue;
        }
        let (input, bits) = line.split_once(';').ok_or(format!("line {}", n + 1))?;
        let classes: Vec<BidiClass> = input
            .split_whitespace()
            .map(class)
            .collect::<Option<_>>()
            .ok_or(format!("line {}: class", n + 1))?;
        let bits = u8::from_str_radix(bits.trim(), 16).map_err(|e| e.to_string())?;
        for (bit, base) in [
            (1, BaseDirection::Auto),
            (2, BaseDirection::LeftToRight),
            (4, BaseDirection::RightToLeft),
        ] {
            if bits & bit != 0 {
                let failure = run(resolver, &classes, None, base, &expected);
                tally.record(
                    "BidiTest.txt",
                    n + 1,
                    &format!("{input} ({base:?})"),
                    failure,
                );
            }
        }
    }
    Ok(tally)
}

/// BidiCharacterTest.txt: code points, paragraph direction, resolved paragraph
/// level, levels and visual order.
fn character_test(text: &str, resolver: &mut Resolver) -> Result<Tally, String> {
    let mut tally = Tally::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        let [points, direction, paragraph, levels_field, order] = fields[..] else {
            return Err(format!("line {}: fields", n + 1));
        };
        let chars: Vec<char> = points
            .split_whitespace()
            .map(|p| u32::from_str_radix(p, 16).ok().and_then(char::from_u32))
            .collect::<Option<_>>()
            .ok_or(format!("line {}: code points", n + 1))?;
        let base = match direction.trim() {
            "0" => BaseDirection::LeftToRight,
            "1" => BaseDirection::RightToLeft,
            _ => BaseDirection::Auto,
        };
        let expected = Expected {
            levels: levels(levels_field).ok_or(format!("line {}: levels", n + 1))?,
            order: numbers(order).ok_or(format!("line {}: order", n + 1))?,
        };
        let classes: Vec<BidiClass> = chars.iter().map(|&c| BidiClass::of(c)).collect();
        let brackets: Vec<Option<Bracket>> = chars.iter().map(|&c| Bracket::of(c)).collect();
        let mut failure = run(resolver, &classes, Some(&brackets), base, &expected);
        if failure.is_none() {
            let mut levels = vec![Level::default(); classes.len()];
            let resolved = resolver.resolve(&classes, Some(&brackets), base, &mut levels);
            let want = if paragraph.trim() == "1" {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            };
            if resolved != Ok(want) {
                failure = Some(format!("paragraph {resolved:?}, expected {want:?}"));
            }
        }
        tally.record("BidiCharacterTest.txt", n + 1, points.trim(), failure);
    }
    Ok(tally)
}

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let [bidi, character] = &paths[..] else {
        eprintln!("usage: bidi_conformance BidiTest.txt BidiCharacterTest.txt");
        return ExitCode::FAILURE;
    };
    let budget = Budget::new(1 << 26).expect("budget");
    let mut resolver = Resolver::new(&budget);
    let mut failed = 0;
    for (path, test) in [
        (
            bidi,
            bidi_test as fn(&str, &mut Resolver) -> Result<Tally, String>,
        ),
        (character, character_test),
    ] {
        let tally = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| test(&text, &mut resolver));
        match tally {
            Ok(tally) => {
                println!("{path}: {} passed, {} failed", tally.passed, tally.failed);
                failed += tally.failed;
            }
            Err(error) => {
                eprintln!("{path}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
