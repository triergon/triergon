//! Robustness smoke test: parse and read mutated copies of the given font files
//! (the mutator of `tests/support`) and report any panic or hang; the reader must
//! return errors within its work limits. Build it in the dev profile so
//! arithmetic overflow is checked too. A failing input is saved next to the
//! working directory as `fuzz_crash_*` or `fuzz_hang_*`.
//!
//! Usage: fuzz [--iters N] [--timeout SECONDS] FILE...

#[path = "../tests/support/mod.rs"]
mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Rng, exercise, mutate};
use triergon_font::Limits;

/// The input under test and when it started, for the watchdog.
struct Current {
    name: String,
    data: Vec<u8>,
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let (mut iters, mut timeout) = (20usize, 10u64);
    while args.len() > 1 && args[0].starts_with("--") {
        let value = args[1].parse().expect("a number");
        match args[0].as_str() {
            "--iters" => iters = value as usize,
            "--timeout" => timeout = value,
            flag => panic!("unknown flag {flag}"),
        }
        args.drain(..2);
    }
    std::panic::set_hook(Box::new(|_| {}));
    let limits = Limits {
        work: 1 << 20,
        ..Limits::default()
    };
    // A watchdog: an input that runs past the timeout is a hang.
    let started = Arc::new(AtomicU64::new(0));
    let current = Arc::new(Mutex::new(Current {
        name: String::new(),
        data: Vec::new(),
    }));
    let epoch = Instant::now();
    {
        let (started, current) = (Arc::clone(&started), Arc::clone(&current));
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let since = started.load(Ordering::Relaxed);
                if since != 0 && epoch.elapsed().as_secs() > since / 1000 + timeout {
                    let current = current
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let out = format!("fuzz_hang_{}", current.name);
                    std::fs::write(&out, &current.data).expect("hang file");
                    eprintln!("HANG {} -> saved {out}", current.name);
                    std::process::exit(1);
                }
            }
        });
    }
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut panics = 0;
    for path in &args {
        let original = std::fs::read(path).expect("font file");
        let base = std::path::Path::new(path)
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        for it in 0..=iters {
            let mut data = original.clone();
            if it > 0 {
                mutate(&mut data, &mut rng, it);
            }
            *current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Current {
                name: format!("{base}_{it}"),
                data: data.clone(),
            };
            started.store(epoch.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
            let result = catch_unwind(AssertUnwindSafe(|| exercise(&data, limits)));
            started.store(0, Ordering::Relaxed);
            if result.is_err() {
                panics += 1;
                let out = format!("fuzz_crash_{base}_{it}");
                std::fs::write(&out, &data).expect("crash file");
                eprintln!("PANIC {path} iter {it} -> saved {out}");
            }
        }
    }
    eprintln!("done, {panics} panics");
    if panics > 0 {
        std::process::exit(1);
    }
}
