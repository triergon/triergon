//! The ISO BMFF comparison worker: opens, indexes, seeks and plans source ranges in one
//! input with the FFmpeg-compatible views of `triergon_isobmff::compat::mov`, and prints
//! the traces and work counts the harness compares with the reference worker.
#![deny(unsafe_code)]
#![cfg_attr(not(test), no_main)]
#[path = "isobmff_consumer/model.rs"]
mod model;
#[path = "isobmff_consumer/protocol.rs"]
mod protocol;
use model::{DemuxOperation, DemuxSeekRequest, Driver, Failure, OpenPolicy, Result};
use protocol::{Retention, Stats};
use std::{
    env,
    fs::File,
    io::{self, Cursor, Read, Seek, Write},
    path::Path,
    process::ExitCode,
    time::Instant,
};
use triergon_core::Error;
use triergon_isobmff::{
    boxes::MediaDataPolicy,
    table::{SampleTablePolicy, SyncSamplePolicy, TimingRunPolicy},
};

#[derive(Clone, Copy)]
struct Options {
    window: usize,
    retain: usize,
    quantum: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            window: 65536,
            retain: 0,
            quantum: 4096,
        }
    }
}
fn argument<T: std::str::FromStr>(args: &[String], at: usize, default: T) -> Result<T> {
    match args.get(at) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| Error::Invalid("worker argument").into()),
    }
}
fn options(args: &[String], offset: usize) -> Result<Options> {
    if args.len() > offset + 4 {
        return Err(Error::Invalid("extra worker arguments").into());
    }
    let window = argument(args, offset, 65536)?;
    let retain = argument(args, offset + 1, 0)?;
    // The input is read into memory before any measured operation; the protocol's
    // streaming `file` mode is not provided.
    match args.get(offset + 2).map_or("memory", String::as_str) {
        "memory" => {}
        "file" => return Err(Error::Unsupported("file I/O mode").into()),
        _ => return Err(Error::Invalid("I/O mode").into()),
    }
    let quantum = argument(args, offset + 3, 4096)?;
    if window == 0 || window > 65536 || retain > 64 || quantum == 0 {
        return Err(Error::Invalid("worker resource policy").into());
    }
    Ok(Options {
        window,
        retain,
        quantum,
    })
}
// The immutable corpus image lives outside each measured operation, as in the
// reference worker. Per-operation seek, parser/index creation and teardown stay inside.
type Input = Cursor<Vec<u8>>;
fn load(path: &Path) -> Result<Input> {
    let mut input = File::open(path)?;
    let length = input.metadata()?.len();
    if length > 64 * 1024 * 1024 {
        return Err(Error::Unsupported("input exceeds 64 MiB").into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(length).map_err(|_| Error::Overflow)?)
        .map_err(|_| Error::Allocation)?;
    input.read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| Error::Overflow)? != length {
        return Err(Error::Transport.into());
    }
    Ok(Cursor::new(bytes))
}
fn length(input: &Input) -> Result<u64> {
    Ok(u64::try_from(input.get_ref().len()).map_err(|_| Error::Overflow)?)
}
fn operation(
    input: &mut Input,
    o: Options,
    op: DemuxOperation,
    policy: OpenPolicy,
    mut trace: Option<&mut String>,
) -> Result<Stats> {
    let length = length(input)?;
    let mut stats = Stats::default();
    stats.start(o.retain, o.window)?;
    let mut driver = Driver::new(input, length, o.window, o.quantum)?;
    driver.shared_payloads = o.retain == 0;
    let pool = driver.budget.clone();
    let mut retained = Retention::new(&pool, o.retain)?;
    if policy.regular_recovery {
        driver.recovered_regular_operation(op, &mut stats, &mut retained, trace.as_deref_mut())?;
    } else if let DemuxOperation::RangePlan(request) = op {
        if o.retain != 0 {
            return Err(Error::Unsupported("range plan packet retention").into());
        }
        let model = driver.open_with_policy(policy)?;
        driver.range_plan(&model, request, &mut stats, trace.as_deref_mut())?;
    } else {
        let model = driver.open_with_policy(policy)?;
        if matches!(op, DemuxOperation::Open) {
            driver.demux_open(&model, &mut stats, &mut retained, trace.as_deref_mut())?;
        } else {
            driver.demux_operation(&model, &mut stats, &mut retained, op, trace.as_deref_mut())?;
        }
    }
    drop(driver);
    retained.consume_after_close(&mut stats.digest)?;
    drop(retained);
    drop(pool);
    if let Some(out) = trace {
        use std::fmt::Write as _;
        write!(
            out,
            ",\"work\":{{\"packets\":{},\"bytes\":{},\"digest\":{}}}}}",
            stats.packets, stats.bytes, stats.digest
        )
        .expect("String write");
    }
    Ok(stats)
}
fn output_stats(stats: &Stats, iterations: u64, elapsed: u128) -> Result<()> {
    println!(
        "{{\"schema\":1,\"iterations\":{iterations},\"elapsed_ns\":{elapsed},\"packets\":{},\"bytes\":{},\"digest\":{}}}",
        stats.packets, stats.bytes, stats.digest
    );
    Ok(())
}
fn main_result(mut args: Vec<String>) -> Result<()> {
    let mut policy = OpenPolicy::default();
    let mut range_plan = false;
    loop {
        match args.get(1).map(String::as_str) {
            Some("--range-plan") => range_plan = true,
            Some("--bounded-timing") => policy.tables.timing_runs = TimingRunPolicy::BoundedPrefix,
            Some("--bounded-sync") => {
                policy.tables.sync_samples = SyncSamplePolicy::BoundedPrefix;
            }
            Some("--terminal-media-prefix") => {
                policy.media_data = MediaDataPolicy::PreserveTerminalDeclaration;
            }
            Some("--recover-regular-tables") => policy.regular_recovery = true,
            _ => break,
        }
        args.remove(1);
    }
    let mode = args
        .get(1)
        .map(String::as_str)
        .ok_or(Error::Invalid("missing worker mode"))?;
    if policy.regular_recovery {
        if policy.tables != SampleTablePolicy::default()
            || policy.media_data != MediaDataPolicy::Strict
        {
            return Err(Error::Invalid("regular table recovery is a standalone profile").into());
        }
        if !matches!(
            mode,
            "open-trace"
                | "open-bench"
                | "open-memory"
                | "open-memory-baseline"
                | "native-index-trace"
                | "native-index-bench"
                | "native-index-memory"
        ) {
            return Err(Error::Unsupported("regular table recovery requires open or index").into());
        }
    }
    if mode == "identity" {
        println!(
            "{{\"schema\":33,\"worker\":\"triergon-isobmff-consumer\",\"reference_api\":\"n9.0-public-mov\",\"native_seek\":\"bounded-aligned-native-view\",\"qualification\":false}}"
        );
        return Ok(());
    }
    let path = args.get(2).ok_or(Error::Invalid("missing input path"))?;
    let bench = matches!(mode, "open-bench" | "native-index-bench" | "seek-bench");
    let seek = matches!(mode, "seek-trace" | "seek-bench" | "seek-memory");
    if range_plan
        && (!seek
            || policy.regular_recovery
            || policy.tables != SampleTablePolicy::default()
            || policy.media_data != MediaDataPolicy::Strict)
    {
        return Err(
            Error::Invalid("a range plan is a seek mode with strict tables and media").into(),
        );
    }
    let option_offset = if seek {
        if bench { 8 } else { 7 }
    } else if bench {
        4
    } else {
        3
    };
    let op = if seek {
        let at = if bench { 4 } else { 3 };
        let count = argument(&args, at + 2, 0u32)?;
        let after_eof = match argument(&args, at + 3, 0u8)? {
            0 => false,
            1 => true,
            _ => return Err(Error::Invalid("native afterEOF boolean").into()),
        };
        let request = DemuxSeekRequest {
            track: argument(&args, at, 0i32)?,
            ticks: argument(&args, at + 1, 0i64)?,
            count,
            after_eof,
        };
        if range_plan {
            DemuxOperation::RangePlan(request)
        } else {
            DemuxOperation::Seek(request)
        }
    } else if matches!(
        mode,
        "native-index-trace" | "native-index-bench" | "native-index-memory"
    ) {
        DemuxOperation::Index
    } else {
        DemuxOperation::Open
    };
    let o = options(&args, option_offset)?;
    let mut input = load(Path::new(path))?;
    match mode {
        "baseline" => {
            input.rewind()?;
            println!("{{\"schema\":1,\"baseline\":true}}");
        }
        "open-trace" | "native-index-trace" | "seek-trace" => {
            let mut trace = String::new();
            operation(&mut input, o, op, policy, Some(&mut trace))?;
            println!("{trace}");
        }
        "open-bench" | "native-index-bench" | "seek-bench" => {
            let n = argument(&args, 3, 0u64)?;
            if n == 0 || n > 1_000_000 {
                return Err(Error::Invalid("iteration bound").into());
            }
            let mut total = Stats::default();
            let start = Instant::now();
            for _ in 0..n {
                total.add(operation(&mut input, o, op, policy, None)?)?;
            }
            let elapsed = start.elapsed().as_nanos();
            std::hint::black_box(&total);
            output_stats(&total, n, elapsed)?;
        }
        "open-memory" | "open-memory-baseline" | "native-index-memory" | "seek-memory" => {
            // Always close the observed window, including operation failure.
            io::stdout().write_all(b"TRIERGON_BEGIN_V1\n")?;
            io::stdout().flush()?;
            let result = if mode == "open-memory-baseline" {
                input
                    .rewind()
                    .map(|()| Stats::default())
                    .map_err(Failure::from)
            } else {
                operation(&mut input, o, op, policy, None)
            };
            io::stdout().write_all(b"TRIERGON_END_V1\n")?;
            io::stdout().flush()?;
            output_stats(&result?, 1, 0)?;
        }
        _ => return Err(Error::Invalid("unknown worker mode").into()),
    }
    Ok(())
}
#[cfg_attr(test, allow(dead_code))]
fn worker_main() -> ExitCode {
    match main_result(env::args().collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("isobmff-consumer: {error}");
            if matches!(error, Failure::Core(Error::Unsupported(_))) {
                ExitCode::from(3)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// C's entry point, the one exception to the worker's `deny(unsafe_code)`
/// (`triergon-worker-entry`).
#[cfg(not(test))]
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(
    _argc: std::ffi::c_int,
    _argv: *const *const std::ffi::c_char,
) -> std::ffi::c_int {
    triergon_worker_entry::enter(worker_main)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;
    #[test]
    fn regular_recovery_rejects_conflicting_profiles() {
        for flag in [
            "--bounded-timing",
            "--bounded-sync",
            "--terminal-media-prefix",
        ] {
            let args = ["worker", flag, "--recover-regular-tables", "open-trace"];
            assert!(matches!(
                main_result(args.into_iter().map(str::to_owned).collect()),
                Err(Failure::Core(Error::Invalid(
                    "regular table recovery is a standalone profile"
                )))
            ));
        }
        let args = ["worker", "--recover-regular-tables", "index-trace"];
        assert!(matches!(
            main_result(args.into_iter().map(str::to_owned).collect()),
            Err(Failure::Core(Error::Unsupported(
                "regular table recovery requires open or index"
            )))
        ));
    }

    #[test]
    fn transport_failure_is_not_confirmed_eof() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("authored transport failure"))
            }
        }
        impl Seek for Broken {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }
        let mut broken = Broken;
        let mut driver = Driver::new(&mut broken, 64, 1, 3).unwrap();
        assert!(matches!(
            driver.open_with_policy(OpenPolicy::default()),
            Err(Failure::Core(Error::Io(io::ErrorKind::Other)))
        ));
    }
    #[test]
    fn parser_work_and_demux_io_settings_remain_explicit() {
        let args: Vec<String> = ["worker", "open-trace", "x", "1", "64", "memory", "3"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let o = options(&args, 3).unwrap();
        assert_eq!((o.window, o.retain, o.quantum), (1, 64, 3));
        let mut changed = args.clone();
        changed[4] = "65".into();
        assert!(options(&changed, 3).is_err());
        let mut streamed = args;
        streamed[5] = "file".into();
        assert!(matches!(
            options(&streamed, 3),
            Err(Failure::Core(Error::Unsupported("file I/O mode")))
        ));
    }
}
