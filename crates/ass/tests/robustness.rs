//! Mutated scripts must produce errors or libass's lenient results, never
//! panics or unbounded work. `cargo xtask fuzz --contract` mutates the whole
//! fixture corpus through the measurement worker.

mod support;

use support::exercise;
use triergon_ass::Limits;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Bytes that matter to the grammar, for overwrites.
const SYNTAX: &[u8] = b"{}()\\,:;.-+&H0123456789mnlbspc tkfr\n\r\t";

/// One of five edits: bit flips, a truncation, overwritten syntax bytes, a
/// removed range, a duplicated range.
fn mutate(data: &mut Vec<u8>, rng: &mut Rng, round: usize) {
    match round % 5 {
        0 => {
            for _ in 0..1 + rng.below(8) {
                let at = rng.below(data.len());
                data[at] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let len = rng.below(data.len());
            data.truncate(len);
        }
        2 => {
            let at = rng.below(data.len());
            for k in 0..rng.below(16) {
                if let Some(byte) = data.get_mut(at + k) {
                    *byte = SYNTAX[rng.below(SYNTAX.len())];
                }
            }
        }
        3 => {
            let a = rng.below(data.len());
            let b = (a + rng.below(64)).min(data.len());
            data.drain(a..b);
        }
        _ => {
            let a = rng.below(data.len());
            let b = (a + rng.below(256)).min(data.len());
            let copy = data[a..b].to_vec();
            let at = rng.below(data.len());
            data.splice(at..at, copy);
        }
    }
}

#[test]
fn mutated_scripts_never_panic() {
    let seeds: [&[u8]; 5] = [
        include_bytes!("../../../harness/fixtures/ass/tags-parens.ass"),
        include_bytes!("../../../harness/fixtures/ass/drawings.ass"),
        include_bytes!("../../../harness/fixtures/ass/karaoke.ass"),
        include_bytes!("../../../harness/fixtures/ass/styles-quirks.ass"),
        include_bytes!("../../../harness/fixtures/ass/fonts-extracted.ass"),
    ];
    let limits = Limits {
        work: 1 << 18,
        ..Limits::default()
    };
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for seed in seeds {
        exercise(seed, limits);
        for round in 0..300 {
            let mut data = seed.to_vec();
            mutate(&mut data, &mut rng, round);
            if round % 3 == 0 {
                mutate(&mut data, &mut rng, round + 1);
            }
            exercise(&data, limits);
        }
    }
}

#[test]
fn work_and_storage_limits_are_errors() {
    use triergon_ass::{Context, Options, Script};
    use triergon_core::{Budget, Error};

    let mut script = b"[Script Info]\nScriptType: v4.00+\n[Events]\n\
        Format: Layer, Start, End, Style, Text\n"
        .to_vec();
    for _ in 0..200 {
        script.extend_from_slice(b"Dialogue: 0,0:00:00.00,0:00:01.00,Default,{\\t(\\fs40)}x\n");
    }
    let budget = Budget::new(1 << 20).unwrap();
    let tight = Options {
        limits: Limits {
            work: 1000,
            ..Limits::default()
        },
        ..Options::default()
    };
    assert_eq!(
        Script::parse(&budget, &script, &tight).err(),
        Some(Error::Policy("ass work"))
    );
    let few_events = Options {
        limits: Limits {
            events: 10,
            ..Limits::default()
        },
        ..Options::default()
    };
    assert_eq!(
        Script::parse(&budget, &script, &few_events).err(),
        Some(Error::Policy("ass events"))
    );
    let small = Budget::new(4 << 10).unwrap();
    assert!(matches!(
        Script::parse(&small, &script, &Options::default()),
        Err(Error::Limit { .. } | Error::Backpressure { .. })
    ));
    // A walk charges each tag besides the block's bytes, so an event can fit
    // the work bound of reading it and exceed it when evaluated.
    let mut stream = Script::new(
        &budget,
        &Options {
            limits: Limits {
                work: 230,
                ..Limits::default()
            },
            ..Options::default()
        },
    )
    .unwrap();
    stream
        .process_codec_private(b"[Script Info]\nScriptType: v4.00+\n")
        .unwrap();
    let mut packet = b"0,0,Default,,0,0,0,,".to_vec();
    for _ in 0..40 {
        packet.extend_from_slice(b"{\\b1}");
    }
    packet.push(b'x');
    stream.process_chunk(&packet, 0, 100).unwrap();
    let event = &stream.events()[0];
    let mut context = Context::new(&stream, event, 0, (384, 288)).unwrap();
    assert_eq!(context.next_glyph().err(), Some(Error::Policy("ass work")));
}

/// The fewest work units a walk over `text`, as one Matroska packet's event,
/// needs to finish.
fn walk_units(text: &[u8]) -> u64 {
    use triergon_ass::{Context, Options, Script};
    use triergon_core::Budget;

    let budget = Budget::new(16 << 20).unwrap();
    let walks = |work: u64| {
        let options = Options {
            limits: Limits {
                work,
                ..Limits::default()
            },
            ..Options::default()
        };
        let mut script = Script::new(&budget, &options).unwrap();
        script
            .process_codec_private(b"[Script Info]\nScriptType: v4.00+\n")
            .unwrap();
        let mut packet = b"0,0,Default,,0,0,0,,".to_vec();
        packet.extend_from_slice(text);
        if script.process_chunk(&packet, 0, 100).is_err() {
            return false;
        }
        let Ok(mut context) = Context::new(&script, &script.events()[0], 0, (384, 288)) else {
            return false;
        };
        loop {
            match context.next_glyph() {
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    };
    let (mut low, mut high) = (0, 1 << 30);
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if walks(mid) { high = mid } else { low = mid }
    }
    high
}

#[test]
fn walks_charge_work_linear_in_the_text() {
    // Every unclosed `{` is text, and in drawing mode starts a drawing: a
    // scan for `}` per brace would be quadratic, as would a scan for `)` per
    // nested `\t`. The walk charges each byte it scans (the hard-override
    // scan, the `}` and `)` searches, the block and each glyph), so its work
    // covers what it scans, and that work grows linearly with the text,
    // whatever the machine's speed.
    let shapes: [(&[u8], &[u8], &[u8]); 4] = [
        (b"", b"{", b""),
        (b"{\\p1}", b"{", b""),
        (b"", b"{a\\b", b""),
        // Nested `\t`s in one block, each continuing in place: a search for
        // `)` per `\t` would rescan the rest of the block.
        (b"{", b"\\t(1,2,", b"}x"),
    ];
    for (prefix, unit, suffix) in shapes {
        let text = |n: usize| [prefix, &unit.repeat(n), suffix].concat();
        let (small, large) = (walk_units(&text(4000)), walk_units(&text(16000)));
        assert!(small >= 3 * 4000 * unit.len() as u64, "{small}");
        assert!(large <= 4 * small + 1000, "{small} then {large}");
    }
}
