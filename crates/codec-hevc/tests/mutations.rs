//! Seeded corruptions of the conformance vectors in the (gitignored) FATE suite download
//! never panic, whether access units or single NAL units are fed. Skips cleanly when the
//! vectors are absent. HEVC_MUTATION_SEEDS sets the corruptions per vector (default 8) and
//! HEVC_MUTATION_VECTORS limits the run to a comma-separated list of vector names.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;

use triergon_codec_hevc::nal::AnnexBIter;
use triergon_codec_hevc::{AccessUnitSplitter, Decoder};

/// Access units decoded from the start of each corrupted vector.
const ACCESS_UNITS: usize = 12;

/// xorshift64*: a fixed, replayable sequence per seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One to eight edits: flipped bits, overwritten bytes, an inserted start code, or a
/// truncation.
fn corrupt(data: &[u8], seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut out = data.to_vec();
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(out.len());
        match rng.below(4) {
            0 => out[at] ^= 1 << rng.below(8),
            1 => out[at] = [0x00, 0xff, 0x03, 0x80][rng.below(4)],
            2 => {
                out.splice(at..at, [0, 0, 1]);
            }
            _ => out.truncate(at.max(1)),
        }
    }
    out
}

fn decode_access_units(data: &[u8]) {
    let mut decoder = Decoder::new();
    for au in AccessUnitSplitter::new(data).take(ACCESS_UNITS) {
        let _ = decoder.send(au, None);
        while decoder.receive().is_some() {}
    }
    decoder.flush();
    while decoder.receive().is_some() {}
}

fn decode_nal_units(data: &[u8]) {
    let mut decoder = Decoder::new();
    for nal in AnnexBIter::new(data).take(ACCESS_UNITS * 4) {
        let _ = decoder.send_nal(nal, None);
        while decoder.receive().is_some() {}
    }
    decoder.flush();
    while decoder.receive().is_some() {}
}

#[test]
fn corrupted_vectors_never_panic() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/fate-samples/hevc-conformance");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipped: no vectors in {}", dir.display());
        return;
    };
    let seeds: u64 = std::env::var("HEVC_MUTATION_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let only = std::env::var("HEVC_MUTATION_VECTORS").ok();
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| {
            only.as_deref().is_none_or(|only| {
                let name = p.file_name().unwrap_or_default().to_string_lossy();
                only.split(',').any(|o| o == name)
            })
        })
        .collect();
    paths.sort();
    static LAST: Mutex<String> = Mutex::new(String::new());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(ToString::to_string).unwrap_or_default();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(ToString::to_string)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if let Ok(mut last) = LAST.lock() {
            *last = format!("{place}: {message}");
        }
    }));
    let mut panics = Vec::new();
    let mut runs = 0;
    for path in paths.iter().filter(|p| p.is_file()) {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        for seed in 0..seeds {
            let input = corrupt(&data, seed);
            for (mode, decode) in [
                ("access units", decode_access_units as fn(&[u8])),
                ("NAL units", decode_nal_units),
            ] {
                runs += 1;
                if catch_unwind(AssertUnwindSafe(|| decode(&input))).is_err() {
                    let at = LAST.lock().map(|l| l.clone()).unwrap_or_default();
                    panics.push(format!("{name} seed {seed} ({mode}) at {at}"));
                }
            }
        }
    }
    std::panic::set_hook(previous);
    eprintln!("{runs} corrupted decodes, {} panics", panics.len());
    assert!(panics.is_empty(), "{panics:#?}");
}
