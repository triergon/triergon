//! Bounded MP4 diagnostic consumer.
//! Existing core owns input, resource charges, ranges and exact clocks.
#![forbid(unsafe_code)]
#[path = "isobmff_consumer/model.rs"]
mod model;
#[path = "isobmff_consumer/protocol.rs"]
mod protocol;
use model::{
    Driver, Failure, NativeOperation, NativeSeekRequest, Result, SourceRequest, Transport,
};
use protocol::{Retention, Stats};
use std::{
    env,
    fs::File,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    time::Instant,
};
use triergon_core::{Error, Time};
use triergon_isobmff::{RecoveryKind, RecoverySyntax};

#[derive(Clone, Copy)]
struct Options {
    window: usize,
    retain: usize,
    file: bool,
    quantum: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            window: 65536,
            retain: 0,
            file: false,
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
    let file = match args.get(offset + 2).map(String::as_str).unwrap_or("memory") {
        "memory" => false,
        "file" => true,
        _ => return Err(Error::Invalid("I/O mode").into()),
    };
    let quantum = argument(args, offset + 3, 4096)?;
    if window == 0 || window > 65536 || retain > 64 || quantum == 0 {
        return Err(Error::Invalid("worker resource policy").into());
    }
    Ok(Options {
        window,
        retain,
        file,
        quantum,
    })
}
// The immutable corpus image / open file lives outside each measured operation,
// matching H0. Per-operation seek, parser/index creation and teardown stay inside.
enum Input {
    Memory(Cursor<Vec<u8>>),
    File(File, u64),
}
impl Input {
    fn open(path: &Path, file: bool) -> Result<Self> {
        let mut input = File::open(path)?;
        let length = input.metadata()?.len();
        if length > i64::MAX as u64 {
            return Err(Error::Unsupported("native signed transport length").into());
        }
        if file {
            return Ok(Self::File(input, length));
        }
        if length > 64 * 1024 * 1024 {
            return Err(
                Error::Unsupported("preloaded input exceeds 64 MiB; select file I/O").into(),
            );
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(usize::try_from(length).map_err(|_| Error::Overflow)?)
            .map_err(|_| Error::Allocation)?;
        input.read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).map_err(|_| Error::Overflow)? != length {
            return Err(Error::Transport.into());
        }
        Ok(Self::Memory(Cursor::new(bytes)))
    }
    fn transport(&mut self) -> (&mut dyn Transport, u64) {
        match self {
            Self::Memory(c) => {
                let len = c.get_ref().len() as u64;
                (c, len)
            }
            Self::File(f, n) => (f, *n),
        }
    }
    fn rewind(&mut self) -> Result<()> {
        let (t, _) = self.transport();
        Seek::seek(t, SeekFrom::Start(0))?;
        Ok(())
    }
}
fn operation(
    input: &mut Input,
    o: Options,
    op: NativeOperation,
    mut trace: Option<&mut String>,
) -> Result<Stats> {
    let (transport, length) = input.transport();
    let mut stats = Stats::default();
    stats.start(o.retain, o.window, o.file)?;
    let mut driver = Driver::new(transport, length, o.window, o.quantum)?;
    let pool = driver.budget.clone();
    let mut retained = Retention::new(&pool, o.retain)?;
    let model = driver.open()?;
    if matches!(op, NativeOperation::Open) {
        driver.native_open(&model, &mut stats, &mut retained, trace.as_deref_mut())?;
    } else {
        driver.native_operation(&model, &mut stats, &mut retained, op, trace.as_deref_mut())?;
    }
    drop(model);
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
fn main_result() -> Result<()> {
    let args: Vec<_> = env::args().collect();
    let mode = args
        .get(1)
        .map(String::as_str)
        .ok_or(Error::Invalid("missing worker mode"))?;
    if mode == "identity" {
        println!(
            "{{\"schema\":33,\"worker\":\"safe-media-isobmff-consumer34\",\"reference_api\":\"n9.0-public-mov\",\"native_seek\":\"bounded-aligned-native-view\",\"source_variant\":\"native37-profile-batch43-v2\",\"source_variant_sha256\":\"49d244ad58a4ea3c575dc5eebdba36edf86f27df1edb68949cd72566dcc479c9\",\"qualification\":false}}"
        );
        return Ok(());
    }
    let path = args.get(2).ok_or(Error::Invalid("missing input path"))?;
    let bench = matches!(mode, "open-bench" | "native-index-bench" | "seek-bench");
    let seek = matches!(mode, "seek-trace" | "seek-bench" | "seek-memory");
    let option_offset = if seek {
        if bench { 8 } else { 7 }
    } else if bench {
        4
    } else if mode == "source-seek-trace" {
        11
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
        NativeOperation::Seek(NativeSeekRequest {
            track: argument(&args, at, 0i32)?,
            ticks: argument(&args, at + 1, 0i64)?,
            count,
            after_eof,
        })
    } else if matches!(
        mode,
        "native-index-trace" | "native-index-bench" | "native-index-memory"
    ) {
        NativeOperation::Index
    } else {
        NativeOperation::Open
    };
    let o = options(&args, option_offset)?;
    let mut input = Input::open(Path::new(path), o.file)?;
    match mode {
        "baseline" => {
            input.rewind()?;
            println!("{{\"schema\":1,\"baseline\":true}}");
        }
        "open-trace" | "native-index-trace" | "seek-trace" => {
            let mut trace = String::new();
            operation(&mut input, o, op, Some(&mut trace))?;
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
                total.add(operation(&mut input, o, op, None)?)?;
            }
            let elapsed = start.elapsed().as_nanos();
            std::hint::black_box(&total);
            output_stats(&total, n, elapsed)?;
        }
        "open-memory" | "open-memory-baseline" | "native-index-memory" | "seek-memory" => {
            // Always close the observed window, including operation failure.
            io::stdout().write_all(b"SAFE_MEDIA_BEGIN_V1\n")?;
            io::stdout().flush()?;
            let result = if mode == "open-memory-baseline" {
                input.rewind().map(|()| Stats::default())
            } else {
                operation(&mut input, o, op, None)
            };
            io::stdout().write_all(b"SAFE_MEDIA_END_V1\n")?;
            io::stdout().flush()?;
            output_stats(&result?, 1, 0)?;
        }
        "index-trace" | "source-seek-trace" => {
            if o.retain != 0 {
                return Err(Error::Invalid(
                    "source audit retention must be zero; native retention is a separate operation",
                )
                .into());
            }
            let (t, length) = input.transport();
            let mut driver = Driver::new(t, length, o.window, o.quantum)?;
            let model = driver.open()?;
            let mut out = String::new();
            if mode == "index-trace" {
                driver.index_trace(&model, &mut out)?;
            } else {
                let track = argument(&args, 3, 0)?;
                let numerator = argument(&args, 4, 0i128)?;
                let denominator = argument(&args, 5, 0u128)?;
                let following = argument(&args, 6, 0)?;
                let after_eof = match argument(&args, 7, 0u8)? {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::Invalid("after-EOF boolean").into()),
                };
                let kind = match args.get(8).map(String::as_str) {
                    Some("roll") => RecoveryKind::Roll,
                    Some("prol") => RecoveryKind::Prol,
                    _ => return Err(Error::Invalid("recovery kind").into()),
                };
                let syntax = match args.get(9).map(String::as_str) {
                    Some("published01") => RecoverySyntax::Versions0And1,
                    Some("mpeg2026draft") => RecoverySyntax::Mpeg2026Draft,
                    _ => return Err(Error::Invalid("recovery syntax authority").into()),
                };
                // Positional reserved field makes future request versions explicit.
                if args.get(10).map(String::as_str) != Some("v1") {
                    return Err(Error::Invalid("source seek request version").into());
                }
                driver.source_seek(
                    &model,
                    SourceRequest {
                        track,
                        at: Time::new(numerator, denominator)?,
                        following,
                        after_eof,
                        kind,
                        syntax,
                    },
                    &mut out,
                )?;
            }
            drop(model);
            drop(driver);
            println!("{out}");
        }
        _ => return Err(Error::Invalid("unknown worker mode").into()),
    }
    Ok(())
}
fn main() {
    if let Err(error) = main_result() {
        eprintln!("isobmff-consumer: {error}");
        std::process::exit(if matches!(error, Failure::Core(Error::Unsupported(_))) {
            3
        } else {
            1
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(matches!(driver.open(), Err(Failure::Io(_))));
    }
    #[test]
    fn parser_work_and_native_io_settings_remain_explicit() {
        let args: Vec<String> = ["worker", "open-trace", "x", "1", "64", "file", "3"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let o = options(&args, 3).unwrap();
        assert_eq!((o.window, o.retain, o.file, o.quantum), (1, 64, true, 3));
        let mut changed = args;
        changed[4] = "65".into();
        assert!(options(&changed, 3).is_err());
    }
}
