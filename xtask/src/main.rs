//! Repository tasks: `cargo xtask <command>`.
//!
//! Standard library only, and independent of the library crates, so the checks
//! and generators stay a separate implementation from the code they test.
#![forbid(unsafe_code)]

mod archive;
mod ass;
mod capture;
mod checks;
mod codec_views;
mod codecs;
mod core_time;
mod corpus;
mod ebml;
mod evidence;
mod exceptions;
mod fate;
mod fetch;
mod ffprobe;
mod files;
mod fonts;
mod framing_bench;
mod framing_views;
mod fuzz;
mod gate;
mod generators;
mod harness;
mod isobmff_bench;
mod isobmff_seek;
mod isobmff_views;
mod json;
mod matroska_views;
mod mpg123;
mod numpy;
mod play_views;
mod playback_golds;
mod playback_views;
mod prepare;
mod process;
mod progressive_bench;
mod pyrandom;
mod python;
mod raster;
mod references;
mod session_queues;
mod sha256;
mod shape;
mod stats;
mod status;
mod subtitle_fixtures;
mod subtitle_views;
mod test_skips;
mod toml;
mod transport_fate;
mod transport_fixtures;
mod transport_views;
mod worker_tests;
mod zlib;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// What `cargo xtask help` prints: every command, its arguments and what it does.
/// Each entry is a line that starts with two spaces and the command's name, then lines
/// indented by 17; `cargo xtask COMMAND --help` prints the command's entry, or its own
/// usage text where [`COMMANDS`] names one.
const HELP: &str = "usage: cargo xtask COMMAND [ARGUMENT]...
       cargo xtask COMMAND --help

commands:
  check          [PART]   everything CI runs: format, tests, lints, builds and the checks
                 below; PART runs one of its parts, which CI runs as separate jobs: lint
                 (format, lints, rustdoc and the repository checks), test (the workspace
                 tests), variants (the reference-trace tests, the release build, the
                 nightly SIMD tests and the core time oracle) or parity (the worker tests,
                 the required feature reports, the fuzz smoke rounds and the link orders);
                 docs runs only the steps that read the documents and evidence sets
                 (format, check-design, check-links, check-revisions and the evidence
                 index tests), all a pull request that changes nothing else needs
  check-simd     clippy and tests of the std::simd paths (HEVC, AAC) on the pinned nightly
  prepare-check  [PART]   once before `check`: the pinned FFmpeg, FreeType, HarfBuzz,
                 FriBidi and libass references, the fixtures, the required vectors, the
                 font corpus and the workers (built with the reference first on PATH);
                 with PART, only what that part of `check` reads
  check-links    every relative link in the Markdown files resolves
  check-design   harness data files parse and recorded gates keep their values
  check-safety   owned packages only, no build scripts, no unsafe code
  check-revisions
                 every commit docs/status.md cites is reachable from HEAD (needs the
                 full history)
  status         [--write]   docs/status.md's tables with their Runtime and Memory cells
                 rendered from the evidence index as harness/status-rows.json maps them;
                 --write writes them into the page
  core-time      [--output DIR] [--profiles debug release] [--toolchain NAME]
                 [--target TRIPLE]   compare exact arithmetic against the independent
                 fraction oracle
  fixtures       generate the authored Matroska corpus into .research/ and its manifests
  fixture        FAMILY [OPTION]... --output FILE   write one corpus variant
  session-queues build LIBRARY_TREE NEW_WORKER
                 | check|time|memory BASE_WORKER CANDIDATE_WORKER NEW_OUTPUT_DIR
                 compare baseline and candidate session queues
  progressive    [--output DIR] [--benchmark] [--memory] [OPTION]...   compare all five
                 forward-only AVC/AAC TS188 feeds, and optionally time and measure them
  subtitle-fixtures record|verify FEATURE REFERENCE [--target TRIPLE] [--corpus DIR]
                 [--manifest FILE] [--cases-stdin]   record or verify subtitle folds
  prepare-ffmpeg [--jobs N] [--profile PROFILE]
                 build a pinned FFmpeg reference into .research/reference[-PROFILE]:
                 matroska (default), formats, framing-views, video, subtitles,
                 transport-dts, transport-mpeg-video (git revision on the 9.0 branch);
                 decode, capture, capture-small (the 9.0.2 release)
  prepare-mpg123 [--jobs N]   build the pinned static free-format MPEG audio reference
  references     keys | pack|fetch|upload [NAME]...   the keys of the pinned reference
                 builds, and their packs in .research/reference-cache and the evidence
                 archive, which the prepare commands restore instead of building
  oracle-compare [--contract FILE] [--output DIR]
                 compare candidate traces with an independent oracle
  codec-views    fixtures media|audio | fixtures --write-manifests FEATURE | VIEWS
                 the inputs of the codec views worker's features and their reference
                 pins. For every views command, VIEWS is `fixtures [FEATURE]`, which
                 checks each feature's inputs against its manifest, or `manifest
                 --record [FEATURE [--cases ID...]]`, which records the built
                 reference's folds
  framing-views  fixtures [--write-manifests] config|streams|nal | fixtures dts
                 | manifest --record dts | VIEWS   the framing views worker's inputs
  isobmff-views  author-indexes | author-packets DIR | author-seek DIR | VIEWS
                 the ISO BMFF views worker's inputs
  matroska-views generate | draft DIR | VIEWS   the Matroska views worker's inputs
  play-views     seek --record [--cases ID...] | seek WORKER OUTPUT | VIEWS
                 the play views worker's seek cases
  playback-views author-probes | VIEWS   the playback views worker's probe controls
  subtitle-views fixtures corpus | fixtures --write-to DIR | VIEWS
                 the subtitle views worker's inputs
  transport-views fixtures catalogue-packets | fixtures --write-manifest source-signatures
                 | VIEWS   the transport views worker's inputs
  fetch-specs    [--record]   cache the specification texts and verify specs/sources.json
  fetch-vectors  cache the required external test vectors and verify their hashes
  external-vector --output DIR [--pairs N] [--iterations N] [--benchmark] [--memory]
                 [--baseline-worker W] [--full] [--no-early-stop]
                 qualify the mandatory external metadata vector through the same gates
  fate-run       [--case NAME] [--demux-only]   run one supported upstream FATE case
                 (fate-matroska-xiph-lacing by default)
  trace-identities --output F [--compare F] [--candidate W] [--corpus FEATURE=DIR]...
                 record the digest of every candidate trace over all recorded cases: the
                 Matroska cases for media-worker, every case of each feature that runs W
                 through a worker operation prefix otherwise
  evidence       pack DIR OUT | upload SET OUT | entry SET OUT [--key KEY] | promote SET
                 | fetch SET [DIR] | restorable | measure [SET]...
                 the evidence archive (TRIERGON_EVIDENCE_ARCHIVE) and its index
                 (harness/evidence/sets/SET.json): pack writes the deterministic .tar.gz
                 or .tar.zst of DIR and prints its SHA-256; upload uploads a pack under
                 qualification/ and prints its index entry, which entry prints without
                 uploading; promote moves a set under evidence/ and records it as kept;
                 fetch downloads, verifies and unpacks a set; restorable prints the name
                 and key of each kept set holding results a gate run restores when it
                 reuses them; measure records each gate result's target and what it
                 measured in the index entries of SET, or of every kept set lacking
                 them, read from the archived packs
  playback-golds DIR [--check]   convert the FFprobe captures of evidence set
                 playback-terminal-matrix-ffprobe, unpacked in DIR, into the playback
                 terminal-media records; --check compares them with the committed records
  fuzz           --manifest F | --contract F [--manifest F] [--corpus DIR] [--case TEXT]...
                 [--reference]; --output DIR [--rounds N] [--seed N]
                 [--maximum-input-bytes N] [--maximum-output-bytes N] [--timeout-seconds S]
                 deterministic, replayable mutation campaign through JSON workers; with
                 --contract, over the cases of a feature with a worker operation prefix:
                 its release candidate and a dev build of it, with --reference also its
                 reference, the bound on input size raised to the largest case
  graph          PACKAGE [FILE]...   print the provenance graph of a workspace package
  isobmff-bench  [--benchmark] [--memory] [OPTION]...   compare all 1020 open/index
                 traces; optionally measure 972 labels or --subset52
  isobmff-seek   [fixtures --out DIR] [--benchmark] [--memory] [OPTION]...   compare
                 every ISO BMFF seek and range-plan case; optionally time and measure them
  framing        [fixtures] [--output DIR] [--benchmark] [--memory] [OPTION]...
                 compare the whole-packet framing corpus
  grade          TIMING MEMORY --contract F --manifest F --output DIR
                 [--additional-report F] [--require-slice-acceptance|--require-acceptance]
                 [--full]   join complete reports, recompute runtime and memory bounds,
                 then apply gates
  preflight      [--feature ID]... [--corpus ID=DIR]... [--isobmff] [--codecs F]...
                 [--review F] [--kernel]   before a gate run measures: workers,
                 references, inputs, RSS module review and state
  gate           plan|check|preflight|run|summary|test [OPTION]...   the full gate as one
                 plan of steps, split into shards that run on separate hosts, and their
                 joined result
  build-workers  [--comparison-build] [--only WORKER] [--all-references]
                 build the candidate and reference workers and the memory observers, and
                 record their identity in target/harness/identity.json; anything but the
                 gate build is refused unless it is for paired comparisons only; a
                 profile reference whose profile is not prepared is skipped and printed,
                 and with --all-references it fails the build
  link-order     BINARY [OUT] [--trained FILE] | corpus OUT | separate [--corpus DIR]
                 | train [--corpus DIR] [--only WORKER] | check   the symbol ordering
                 file that lays a program's functions out in a trained order, then
                 module order, as the workers are linked (docs/quality.md); corpus,
                 separate, train and check generate the training corpus, check it is
                 apart from the gate, record the workers' trained orders and check the
                 committed ones
  memory-admission CANDIDATE REFERENCE
                 check the native Linux memory observer (AArch64 or 32-bit ARMv7) for two
                 built workers and print the certificate
  worker-tests   [FILTER]   integration tests of the built workers against the reference
                 workers and the independent oracles (tests whose name contains FILTER)
  report         [--contract F] [--manifest F] [--corpus DIR] [--output DIR]
                 [--fate-cache DIR] [--additional-report F]... [--require-acceptance]
                 [--require-slice-acceptance] [--benchmark] [--memory] [--pairs N]
                 [--memory-pairs N] [--iterations N] [--seed N] [--baseline-worker W]
                 [--full] [--no-early-stop]
                 correctness, paired runtime and memory report of the built workers for
                 one contract; with a baseline worker of another build, timing and memory
                 pair against it; a gate collection a kept evidence set holds is reused
                 unless --full; a gate session stops at an early look once settled unless
                 --no-early-stop
  compare        --candidate W --reference W --input FILE --output DIR
                 [--operation plain|seek|colour|catalogue] [--content] [--no-short-reads]
                 compare one input across a candidate and a reference worker
  same-code      CANDIDATE REFERENCE   compare two worker binaries function by function;
                 fails if any function differs beyond its position
  count          CANDIDATE REFERENCE ARGUMENT...   count instructions of two workers on
                 one workload with perf stat; fails above the quick-check limit
  screen         REPORTS   grade one paired timing round, with each contract's report
                 under REPORTS/<contract>, against the quick-check tolerances
  transport-fate check | inventory OUTPUT | observe SPEC OUTPUT_DIR
                 check TS identity coverage or record pinned original-input observations
  fate-inventory --source DIR --output FILE [--configured-list FILE]
                 index FATE definitions as data; nothing is executed
  codecs         COMMAND [OPTION]...   the codec crates against the pinned FFmpeg
                 decoders: dsp, aac-access-units, aac-ps-tables, aac-sources, derive,
                 manifest, variant-pins, grade, compare-hevc, summarize, readers,
                 isobmff-reader, time, memory, equivalence and fuzz
  capture        compare|qualify|summarize|equivalence [OPTION]...   the capture
                 pipeline against the FFmpeg capture reference
  fonts          prepare|fetch|store|tables|manifest|compare|assemble|probe [OPTION]...
                 the pinned FreeType reference, the font corpus and the font features'
                 manifests; compare reports the required font features, --all every one;
                 probe compares assembled TrueType probe faces
  shape          prepare|manifest|compare|ucd|conformance|assemble|probe [OPTION]...
                 triergon-shape against the pinned HarfBuzz and FriBidi reference
  raster         prepare|manifest|compare [OPTION]...   triergon-raster against the
                 pinned libass reference
  ass            prepare|manifest|compare|equivalence [OPTION]...   triergon-ass's
                 parsing and rendering against the same libass build";

/// What runs a command, and so how it exits.
enum Run {
    /// Takes no arguments: exits 0, or prints its error and exits 1.
    Plain(fn(&Path) -> Task),
    /// Exits 0, or prints its error and exits 1.
    Task(fn(&Path, &[String]) -> Task),
    /// A measuring command: exits with its code, or prints its error and exits 2.
    Measure(fn(&Path, &[String]) -> R<i32>),
    /// A runner with its own usage text: exits with its code, or prints its error and
    /// exits 1; for arguments it cannot use, it prints the usage and exits 2.
    Runner(fn(&Path, &[String]) -> R<i32>),
    /// Gives its own exit.
    Exit(fn(&Path, &[String]) -> ExitCode),
}

/// A command of [`HELP`]: its name, its own usage text if it has one, and what runs it.
struct Subcommand {
    name: &'static str,
    usage: Option<&'static str>,
    run: Run,
}

impl Subcommand {
    const fn new(name: &'static str, usage: Option<&'static str>, run: Run) -> Self {
        Self { name, usage, run }
    }

    const fn plain(name: &'static str, run: fn(&Path) -> Task) -> Self {
        Self::new(name, None, Run::Plain(run))
    }

    const fn task(name: &'static str, run: fn(&Path, &[String]) -> Task) -> Self {
        Self::new(name, None, Run::Task(run))
    }

    const fn measure(name: &'static str, run: fn(&Path, &[String]) -> R<i32>) -> Self {
        Self::new(name, None, Run::Measure(run))
    }

    const fn runner(
        name: &'static str,
        run: fn(&Path, &[String]) -> R<i32>,
        usage: &'static str,
    ) -> Self {
        Self::new(name, Some(usage), Run::Runner(run))
    }

    const fn exit(name: &'static str, run: fn(&Path, &[String]) -> ExitCode) -> Self {
        Self::new(name, None, Run::Exit(run))
    }

    const fn usage(self, usage: &'static str) -> Self {
        Self::new(self.name, Some(usage), self.run)
    }
}

/// Every command of [`HELP`], in its order.
const COMMANDS: &[Subcommand] = &[
    Subcommand::task("check", |root, args| match args {
        [] => check(root, None),
        [part] => Part::named(part).and_then(|part| check(root, Some(part))),
        _ => Err(usage_of("check")),
    }),
    Subcommand::plain("check-simd", |root| {
        simd_lints(root).and_then(|()| simd_tests(root))
    }),
    Subcommand::task("prepare-check", |_, args| match args {
        [] => prepare_check(None),
        [part] => Part::named(part).and_then(|part| prepare_check(Some(part))),
        _ => Err(usage_of("prepare-check")),
    }),
    Subcommand::plain("check-links", check_links),
    Subcommand::plain("check-design", check_design),
    Subcommand::plain("check-safety", check_safety),
    Subcommand::plain("check-revisions", check_revisions),
    Subcommand::task("status", |root, args| {
        let write = match args {
            [] => false,
            [flag] if flag == "--write" => true,
            _ => return Err(usage_of("status")),
        };
        status::tables(root, write).map(|text| println!("{text}"))
    }),
    Subcommand::task("core-time", core_time::main),
    Subcommand::plain("fixtures", fixtures),
    Subcommand::task("fixture", fixture).usage(FIXTURE_USAGE),
    Subcommand::task("session-queues", session_queues::main),
    Subcommand::measure("progressive", progressive_bench::main).usage(progressive_bench::USAGE),
    Subcommand::task("subtitle-fixtures", subtitle_fixtures::main),
    Subcommand::task("prepare-ffmpeg", prepare_ffmpeg),
    Subcommand::task("prepare-mpg123", mpg123::prepare),
    Subcommand::task("references", references::main).usage(references::USAGE),
    Subcommand::measure("oracle-compare", harness::oracle_compare::main),
    Subcommand::task("codec-views", codec_views::main),
    Subcommand::task("framing-views", framing_views::main),
    Subcommand::task("isobmff-views", isobmff_views::main),
    Subcommand::task("matroska-views", matroska_views::main),
    Subcommand::task("play-views", play_views::main),
    Subcommand::task("playback-views", playback_views::main),
    Subcommand::task("subtitle-views", subtitle_views::main),
    Subcommand::task("transport-views", transport_views::main),
    Subcommand::task("fetch-specs", |root, args| {
        let record = match args {
            [] => false,
            [flag] if flag == "--record" => true,
            _ => return Err(usage_of("fetch-specs")),
        };
        fetch::specs(root, record, &fetch::Network).map(|summary| println!("{summary}"))
    }),
    Subcommand::plain("fetch-vectors", |root| {
        let verified = fetch::vectors(root, &fetch::Network, fetch::VECTOR_LIMIT)?;
        println!("Verified required vectors: {}", verified.join(", "));
        Ok(())
    }),
    Subcommand::measure("external-vector", external_vector),
    Subcommand::measure("fate-run", fate_run),
    Subcommand::measure("trace-identities", trace_identities),
    Subcommand::task("evidence", evidence),
    Subcommand::task("playback-golds", |root, args| {
        playback_golds::main(root, args).map(|summary| println!("{summary}"))
    }),
    Subcommand::measure("fuzz", fuzz_campaign),
    Subcommand::task("graph", graph),
    Subcommand::runner("isobmff-bench", isobmff_bench::main, isobmff_bench::USAGE),
    Subcommand::runner("isobmff-seek", isobmff_seek::main, isobmff_seek::USAGE),
    Subcommand::runner("framing", framing_bench::main, framing_bench::USAGE),
    Subcommand::runner("grade", harness::grade::main, harness::grade::USAGE),
    Subcommand::runner(
        "preflight",
        harness::preflight::main,
        harness::preflight::USAGE,
    ),
    Subcommand::runner("gate", gate::main, gate::USAGE),
    Subcommand::task("build-workers", |root, args| {
        harness::build::main(root, args).map(|path| println!("{}", path.display()))
    }),
    Subcommand::task("link-order", link_order).usage(LINK_ORDER_USAGE),
    Subcommand::task("memory-admission", |root, args| match args {
        [candidate, reference] => {
            harness::memory::admission(root, Path::new(candidate), Path::new(reference))
        }
        _ => Err(usage_of("memory-admission")),
    }),
    Subcommand::task("worker-tests", |root, args| match args {
        [] => worker_tests::run(root, None),
        [filter] => worker_tests::run(root, Some(filter)),
        _ => Err(usage_of("worker-tests")),
    }),
    Subcommand::measure("report", report),
    Subcommand::task("compare", compare),
    Subcommand::exit("same-code", |_, args| match args {
        [candidate, reference] => passing(harness::screen::same_code(
            Path::new(candidate),
            Path::new(reference),
        )),
        _ => task(Err(usage_of("same-code"))),
    }),
    Subcommand::exit("count", |_, args| match args {
        [candidate, reference, arguments @ ..] if !arguments.is_empty() => passing(
            harness::screen::count(Path::new(candidate), Path::new(reference), arguments),
        ),
        _ => task(Err(usage_of("count"))),
    }),
    Subcommand::exit("screen", |root, args| match args {
        [reports] => passing(harness::screen::screen(root, Path::new(reports))),
        _ => task(Err(usage_of("screen"))),
    }),
    Subcommand::task("transport-fate", transport_fate::main),
    Subcommand::task("fate-inventory", |_, args| fate_inventory(args)),
    Subcommand::runner("codecs", codecs::main, codecs::USAGE),
    Subcommand::runner("capture", capture::main, capture::USAGE),
    Subcommand::exit("fonts", |root, args| {
        generator_main(root, &fonts::GENERATOR, args)
    })
    .usage(fonts::USAGE),
    Subcommand::exit("shape", |root, args| {
        generator_main(root, &shape::GENERATOR, args)
    })
    .usage(shape::USAGE),
    Subcommand::exit("raster", |root, args| {
        generator_main(root, &raster::GENERATOR, args)
    })
    .usage(raster::USAGE),
    Subcommand::exit("ass", |root, args| {
        generator_main(root, &ass::GENERATOR, args)
    })
    .usage(ass::USAGE),
];

fn main() -> ExitCode {
    if let Some(code) = without_cargo_library_path() {
        return code;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let help = |flag: &str| matches!(flag, "help" | "--help" | "-h");
    match args.as_slice() {
        [] => {
            eprintln!("{HELP}");
            ExitCode::FAILURE
        }
        [flag] if help(flag) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        [name, flag] | [flag, name] if help(flag) && command(name).is_some() => {
            println!("{}", usage_of(name));
            ExitCode::SUCCESS
        }
        [name, args @ ..] => match command(name) {
            Some(command) => {
                let root = root();
                match command.run {
                    Run::Plain(_) if !args.is_empty() => task(Err(usage_of(name))),
                    Run::Plain(run) => task(run(&root)),
                    Run::Task(run) => task(run(&root, args)),
                    Run::Measure(run) => exit_with(run(&root, args), 2),
                    Run::Runner(run) => {
                        failing(run(&root, args), command.usage.unwrap_or_default())
                    }
                    Run::Exit(run) => run(&root, args),
                }
            }
            None => task(Err(format!(
                "{name}: no such command; `cargo xtask help` lists them"
            ))),
        },
    }
}

fn command(name: &str) -> Option<&'static Subcommand> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// The lines of command `name`'s entry in [`HELP`], without their indentation.
fn entry(name: &str) -> Vec<String> {
    let mut lines = HELP.lines().skip_while(|line| *line != "commands:").skip(1);
    let Some(first) = lines.find(|line| {
        line.strip_prefix("  ")
            .and_then(|line| line.strip_prefix(name))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    }) else {
        return Vec::new();
    };
    let synopsis = first[2 + name.len()..].trim_start();
    std::iter::once(format!("{name} {synopsis}").trim_end().to_owned())
        .chain(lines.map_while(|line| Some(line.strip_prefix(&" ".repeat(17))?.to_owned())))
        .collect()
}

/// What `cargo xtask NAME --help` prints: the command's own usage, or its entry in
/// [`HELP`]. A command refuses arguments it cannot use with this text.
fn usage_of(name: &str) -> String {
    match command(name).and_then(|command| command.usage) {
        Some(usage) => usage.to_owned(),
        None => format!("usage: cargo xtask {}", entry(name).join("\n       ")),
    }
}

/// A command's exit: 0 on success; an error is printed and exits 1.
fn task(result: Task) -> ExitCode {
    exit_with(result.map(|()| 0), 1)
}

/// `cargo run` (and the rustup proxy in front of it) prepends build and
/// toolchain library directories to `LD_LIBRARY_PATH`. Workers and the allocator observer must load exactly as
/// they would from a shell, so run once more without those entries.
fn without_cargo_library_path() -> Option<ExitCode> {
    std::env::var_os("CARGO")?;
    let current = std::env::var_os("LD_LIBRARY_PATH")?;
    let exe = std::env::current_exe().ok()?;
    let toolchains =
        std::env::var_os("RUSTUP_HOME").map(|home| Path::new(&home).join("toolchains"));
    let kept = shell_library_path(&current, exe.parent()?, toolchains.as_deref());
    if kept.as_ref() == Some(&current) {
        return None;
    }
    let mut command = Command::new(&exe);
    command.args(std::env::args_os().skip(1));
    match kept {
        Some(value) => command.env("LD_LIBRARY_PATH", value),
        None => command.env_remove("LD_LIBRARY_PATH"),
    };
    Some(match command.status() {
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("{}: {error}", exe.display());
            ExitCode::FAILURE
        }
    })
}

/// `value` without the entries `cargo run` and rustup add: the running
/// binary's output directory and everything below it, toolchain
/// `lib/rustlib/*/lib` directories and anything under rustup's `toolchains`.
/// `None` when nothing remains.
fn shell_library_path(value: &OsStr, built: &Path, toolchains: Option<&Path>) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(value)
        .filter(|path| {
            !path.starts_with(built)
                && !toolchains.is_some_and(|t| path.starts_with(t))
                && !path.components().any(|c| c.as_os_str() == "rustlib")
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        std::env::join_paths(kept).ok()
    }
}

fn evidence(root: &Path, args: &[String]) -> Task {
    match args {
        [command, dir, out] if command == "pack" => {
            println!(
                "{}",
                evidence::pack_command(Path::new(dir), Path::new(out))?
            );
            Ok(())
        }
        [command, set, rest @ ..] if command == "fetch" && rest.len() <= 1 => {
            let into = rest.first().map(Path::new);
            println!("{}", evidence::fetch(root, set, into)?);
            Ok(())
        }
        [command, set, archive] if command == "upload" => {
            println!("{}", evidence::upload(root, set, Path::new(archive))?);
            Ok(())
        }
        [command, set, archive, rest @ ..] if command == "entry" => {
            let key = match rest {
                [] => None,
                [flag, key] if flag == "--key" => Some(key.as_str()),
                _ => return Err(usage_of("evidence")),
            };
            println!("{}", evidence::entry(root, set, Path::new(archive), key)?.1);
            Ok(())
        }
        [command, set] if command == "promote" => {
            println!("{}", evidence::promote(root, set)?);
            Ok(())
        }
        [command, sets @ ..] if command == "measure" => {
            println!("{}", evidence::measure(root, sets)?);
            Ok(())
        }
        [command] if command == "restorable" => {
            for (set, key) in evidence::restorable(root)? {
                println!("{set} {key}");
            }
            Ok(())
        }
        _ => Err(usage_of("evidence")),
    }
}

/// The workspace root: the parent of this package's manifest directory.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

type Task = Result<(), String>;
type R<T> = Result<T, String>;

fn check_links(root: &Path) -> Task {
    let errors = checks::links(root);
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    println!("Local Markdown links OK");
    Ok(())
}

fn check_design(root: &Path) -> Task {
    let summary = checks::design(root).map_err(|errors| errors.join("\n"))?;
    println!("{summary}");
    Ok(())
}

fn check_revisions(root: &Path) -> Task {
    let summary = checks::revisions(root).map_err(|errors| errors.join("\n"))?;
    println!("{summary}");
    Ok(())
}

fn check_safety(root: &Path) -> Task {
    println!("{}", checks::safety(root)?);
    Ok(())
}

fn fixtures(root: &Path) -> Task {
    let paths = corpus::Paths {
        root: root.to_path_buf(),
        out: root.to_path_buf(),
    };
    for line in corpus::generate_all(&paths)? {
        println!("{line}");
    }
    Ok(())
}

/// A command's exit code, clamped to a byte; an error is printed and exits `error`.
fn exit_with(result: Result<i32, String>, error: u8) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(error)
        }
    }
}

/// Success when a check holds; failure when it does not, or an error is printed.
fn passing(result: Result<bool, String>) -> ExitCode {
    exit_with(result.map(|holds| i32::from(!holds)), 1)
}

/// A runner's exit code; an error is printed and exits 1, `--help` prints `usage`.
fn failing(result: Result<i32, String>, usage: &str) -> ExitCode {
    match result {
        Err(message) if message == usage => {
            println!("{usage}");
            ExitCode::from(2)
        }
        result => exit_with(result, 1),
    }
}

fn external_vector(root: &Path, args: &[String]) -> Result<i32, String> {
    let mut options = harness::vectors::ExternalOptions {
        output: PathBuf::new(),
        pairs: 30,
        iterations: 50,
        benchmark: false,
        memory: false,
        baseline_worker: None,
        full: false,
        early_stop: true,
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--output" => options.output = PathBuf::from(value()?),
            "--pairs" => options.pairs = value()?.parse().map_err(|_| "--pairs: not a number")?,
            "--iterations" => {
                options.iterations = value()?.parse().map_err(|_| "--iterations: not a number")?;
            }
            "--benchmark" => options.benchmark = true,
            "--memory" => options.memory = true,
            "--full" => options.full = true,
            "--no-early-stop" => options.early_stop = false,
            "--baseline-worker" => options.baseline_worker = Some(PathBuf::from(value()?)),
            _ => return Err(usage_of("external-vector")),
        }
    }
    if options.output.as_os_str().is_empty() {
        return Err("--output is required".into());
    }
    harness::vectors::external_vector(root, &options)
}

fn fate_run(root: &Path, args: &[String]) -> Result<i32, String> {
    let (mut case, mut demux_only) = ("fate-matroska-xiph-lacing".to_owned(), false);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--case" => case = rest.next().cloned().ok_or("--case needs a value")?,
            "--demux-only" => demux_only = true,
            _ => return Err(usage_of("fate-run")),
        }
    }
    harness::vectors::fate_run(root, &case, demux_only, &harness::vectors::NativeFate)
}

fn trace_identities(root: &Path, args: &[String]) -> Result<i32, String> {
    let (mut output, mut compare, mut candidate) = (None, None, None);
    let mut corpora = std::collections::BTreeMap::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--output" => output = Some(PathBuf::from(value)),
            "--compare" => compare = Some(PathBuf::from(value)),
            "--candidate" => candidate = Some(PathBuf::from(value)),
            "--corpus" => {
                let (feature, directory) = value
                    .split_once('=')
                    .filter(|(feature, directory)| !feature.is_empty() && !directory.is_empty())
                    .ok_or("--corpus needs FEATURE=DIR")?;
                if corpora
                    .insert(feature.to_owned(), root.join(directory))
                    .is_some()
                {
                    return Err(format!("corpus specified twice for {feature}"));
                }
            }
            _ => return Err(usage_of("trace-identities")),
        }
    }
    let output = output.ok_or("--output is required")?;
    let candidate = match candidate {
        Some(candidate) => candidate,
        None => root
            .join(harness::gate_build::release_dir()?)
            .join("media-worker"),
    };
    harness::vectors::trace_identities(root, &candidate, &output, compare.as_deref(), &corpora)
}

fn fuzz_campaign(root: &Path, args: &[String]) -> Result<i32, String> {
    let mut campaign = fuzz::Campaign {
        manifest: PathBuf::new(),
        output: PathBuf::new(),
        rounds: 4,
        seed: 280,
        maximum_input_bytes: 4 << 20,
        maximum_output_bytes: 8 << 20,
        timeout_seconds: None,
    };
    // With --contract, the campaign runs that feature's cases and --manifest names
    // the feature's corpus manifest (by default the one beside the contract).
    let (mut contract, mut manifest, mut corpus, mut cases, mut reference) =
        (None, None, None, Vec::new(), false);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        if flag == "--reference" {
            reference = true;
            continue;
        }
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{flag}: not a number: {value}"))
        };
        match flag.as_str() {
            "--contract" => contract = Some(PathBuf::from(value)),
            "--manifest" => manifest = Some(PathBuf::from(value)),
            "--corpus" => corpus = Some(PathBuf::from(value)),
            "--case" => cases.push(value.clone()),
            "--output" => campaign.output = PathBuf::from(value),
            "--rounds" => campaign.rounds = number()? as usize,
            "--seed" => campaign.seed = number()?,
            "--maximum-input-bytes" => campaign.maximum_input_bytes = number()? as usize,
            "--maximum-output-bytes" => campaign.maximum_output_bytes = number()? as usize,
            "--timeout-seconds" => {
                campaign.timeout_seconds =
                    Some(value.parse().map_err(|_| format!("{flag}: not a number"))?);
            }
            _ => return Err(usage_of("fuzz")),
        }
    }
    match contract {
        Some(contract) => {
            let feature = fuzz::Feature {
                manifest: manifest.unwrap_or_else(|| contract.with_file_name("corpus.json")),
                contract,
                corpus,
                cases,
                reference,
            };
            fuzz::feature_campaign(root, &feature, &campaign)
        }
        None if corpus.is_none() && cases.is_empty() && !reference => {
            campaign.manifest = manifest.unwrap_or_default();
            fuzz::campaign(root, &campaign)
        }
        None => Err("--corpus, --case and --reference need --contract".into()),
    }
}

fn graph(root: &Path, args: &[String]) -> Task {
    let (package, files) = args.split_first().ok_or("graph needs a package name")?;
    let metadata = harness::provenance::cargo_metadata(root)?;
    let graph = harness::provenance::selected_graph(root, &metadata, package, files)?;
    println!("{}", json::to_python(&graph, None));
    Ok(())
}

fn report(root: &Path, args: &[String]) -> Result<i32, String> {
    report_with_inputs(root, args, None)
}

fn report_with_inputs(
    root: &Path,
    args: &[String],
    optional: Option<&test_skips::ParityInputs>,
) -> Result<i32, String> {
    let mut options = harness::report::Options::defaults(root);
    let mut corpus = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let mut value = || {
            rest.next()
                .map(PathBuf::from)
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--contract" => options.contract = value()?,
            "--manifest" => options.manifest = value()?,
            "--corpus" => corpus = Some(value()?),
            "--output" => options.output = value()?,
            "--fate-cache" => options.fate_cache = Some(value()?),
            "--baseline-worker" => options.baseline_worker = Some(value()?),
            "--additional-report" => options.additional_reports.push(value()?),
            "--require-acceptance" => options.require_acceptance = true,
            "--require-slice-acceptance" => options.require_slice_acceptance = true,
            "--benchmark" => options.benchmark = true,
            "--memory" => options.memory = true,
            "--full" => options.full = true,
            "--no-early-stop" => options.early_stop = false,
            "--pairs" | "--memory-pairs" | "--iterations" | "--seed" => {
                let text = value()?.display().to_string();
                let number: u64 = text
                    .parse()
                    .map_err(|_| format!("{flag}: not a number: {text}"))?;
                match flag.as_str() {
                    "--pairs" => options.pairs = number as usize,
                    "--memory-pairs" => {
                        options.memory_pairs =
                            Some(usize::try_from(number).map_err(|e| e.to_string())?);
                    }
                    "--iterations" => {
                        options.iterations =
                            Some(u32::try_from(number).map_err(|e| e.to_string())?);
                    }
                    _ => options.seed = number,
                }
            }
            _ => return Err(usage_of("report")),
        }
    }
    // Without --corpus, a manifest that records its directory names it.
    options.corpus = match corpus {
        Some(corpus) => corpus,
        None => json::read(&options.contract)
            .ok()
            .and_then(|contract| {
                contract
                    .get("corpus_directory")
                    .and_then(json::Value::as_str)
                    .map(|directory| root.join(directory))
            })
            .or_else(|| {
                harness::manifest::read(root, &options.manifest)
                    .ok()
                    .and_then(|(manifest, _)| harness::report::recorded_corpus(root, &manifest))
            })
            .unwrap_or_else(|| std::mem::take(&mut options.corpus)),
    };
    harness::report::run_with_inputs(root, &options, optional)
}

fn compare(root: &Path, args: &[String]) -> Task {
    let mut values = std::collections::BTreeMap::new();
    let (mut content, mut sequential) = (false, true);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--content" => content = true,
            "--no-short-reads" => sequential = false,
            "--candidate" | "--reference" | "--input" | "--output" | "--operation" => {
                values.insert(
                    flag.clone(),
                    rest.next()
                        .ok_or_else(|| format!("{flag} needs a value"))?
                        .clone(),
                );
            }
            _ => return Err(usage_of("compare")),
        }
    }
    let path = |key: &str| {
        values
            .get(key)
            .map(PathBuf::from)
            .ok_or_else(|| format!("{key} is required"))
    };
    let (candidate, reference, input) =
        (path("--candidate")?, path("--reference")?, path("--input")?);
    let run = harness::parity::Run {
        root,
        candidate: &candidate,
        reference: &reference,
        input: &input,
        raw: path("--output")?,
        timeout: harness::worker::DEFAULT_TIMEOUT,
    };
    use harness::parity::{catalogue_parity, colour_parity, parity, seek_parity};
    let result = match values.get("--operation").map_or("plain", String::as_str) {
        "plain" => parity(&run, sequential, None, content, "")?,
        "seek" => seek_parity(&run, sequential, None, content)?,
        "colour" => colour_parity(&run, sequential, None, content)?,
        "catalogue" => catalogue_parity(&run, sequential, None, content)?,
        other => return Err(format!("unknown operation {other}")),
    };
    json::write(&run.raw.join("result.json"), &result)?;
    println!(
        "{}",
        result
            .get("status")
            .and_then(json::Value::as_str)
            .unwrap_or("?")
    );
    Ok(())
}

fn fate_inventory(args: &[String]) -> Task {
    let (mut source, mut output, mut configured) = (None, None, None);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = PathBuf::from(rest.next().ok_or_else(|| format!("{flag} needs a value"))?);
        match flag.as_str() {
            "--source" => source = Some(value),
            "--output" => output = Some(value),
            "--configured-list" => configured = Some(value),
            _ => return Err(usage_of("fate-inventory")),
        }
    }
    let (source, output) = (
        source.ok_or("--source is required")?,
        output.ok_or("--output is required")?,
    );
    let mut report = fate::inventory(&source)?;
    if let Some(list) = configured {
        fate::annotate(
            &mut report,
            &std::fs::read(&list).map_err(|e| format!("{}: {e}", list.display()))?,
        )?;
    }
    json::write(&output, &report)?;
    let count = report
        .get("cases")
        .and_then(json::Value::as_array)
        .map_or(0, <[_]>::len);
    println!("Indexed {count} identities; none executed or accepted.");
    Ok(())
}

fn prepare_ffmpeg(root: &Path, args: &[String]) -> Task {
    let (mut jobs, mut profile) = (2, prepare::Profile::Matroska);
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--jobs" => jobs = value.parse().map_err(|_| "jobs must be 1..8")?,
            "--profile" => {
                profile =
                    prepare::Profile::parse(value).ok_or_else(|| usage_of("prepare-ffmpeg"))?;
            }
            _ => return Err(usage_of("prepare-ffmpeg")),
        }
    }
    let built = references::ffmpeg_build(profile).prepare(root, || {
        prepare::prepare(root, jobs, profile, &fetch::Network)
    })?;
    println!("{built}");
    Ok(())
}

const FIXTURE_USAGE: &str = "usage: cargo xtask fixture <family> [options] --output FILE

families and options:
  m2ts-program --record REFERENCE   record the complete authored M2TS program
  m2ts INPUT.ts OUTPUT.m2ts   wrap TS188 packets for the program fixtures
  transport-access-units --output ROOT [--record --reference-worker FILE --observations DIR]
           regenerate MPEG-TS access-unit inputs and their reference-pinned corpus
  transport-dts --record REFERENCE   regenerate DVB DTS core transport inputs and pins
  transport-dts-discovery FATE_DTS_TS [--record REFERENCE]   generate automatic DTS controls
  transport-dts-hd --record REFERENCE   regenerate DTS-HD transport inputs and pins
  transport-eac3-six-block [--record REFERENCE]   generate and pin E-AC-3 controls
  transport-eac3-probe OUTPUT   generate ATSC short-group packet-boundary probes
  transport-dts-hd-grouped [--record REFERENCE]   generate DTS-HD source AU controls
  transport-dts-seek --record REFERENCE   regenerate DTS core seek inputs and pins
  transport-audio-seek --record REFERENCE   regenerate additional MPEG-TS audio seek inputs and pins
  transport-mpeg-video --record REFERENCE [--reference-library PATH]   generate MPEG video picture inputs and observations
  transport-matched --record SEEK_REFERENCE AUDIO_REFERENCE
           regenerate MPEG-TS seek and free-format audio controls and pins
  zlib     [--kind stored|fixed|dynamic] [--scope N] [--lace 0-3] [--aac] [--mixed] [--empty]
  header   [--scope N] [--lace] [--aac]
  colour   --mastering-width 4|8 --values V0,...,V9   (empty Colour with one MasteringMetadata)";

/// One corpus variant, for harness tests that need cases beyond the manifests.
fn fixture(root: &Path, args: &[String]) -> Task {
    let family = args.first().map(String::as_str).ok_or(FIXTURE_USAGE)?;
    if family == "m2ts-program" {
        let [flag, reference] = &args[1..] else {
            return Err(FIXTURE_USAGE.into());
        };
        if flag != "--record" {
            return Err(FIXTURE_USAGE.into());
        }
        return transport_fixtures::program::record(root, Path::new(reference));
    }
    if family == "m2ts" {
        let [source, target] = &args[1..] else {
            return Err(FIXTURE_USAGE.into());
        };
        return transport_fixtures::program_m2ts(Path::new(source), Path::new(target));
    }
    if family == "transport-eac3-six-block" {
        return transport_fixtures::eac3_probe::report_main(root, &args[1..]);
    }
    if family == "transport-eac3-probe" {
        return transport_fixtures::eac3_probe::main(root, &args[1..]);
    }
    if family == "transport-access-units" {
        return transport_fixtures::main(root, &args[1..]);
    }
    if family == "transport-dts-discovery" {
        return transport_fixtures::dts_discovery::main(root, &args[1..]);
    }
    if family == "transport-dts-hd-grouped" {
        return transport_fixtures::dts_hd::main(root, &args[1..]);
    }
    if family == "transport-dts-hd" {
        return transport_fixtures::matched::author_dts_hd(root, &args[1..]);
    }
    if family == "transport-dts-seek" {
        return transport_fixtures::matched::author_dts_seek(root, &args[1..]);
    }
    if family == "transport-dts" {
        return transport_fixtures::matched::author_dts(root, &args[1..]);
    }
    if family == "transport-audio-seek" {
        return transport_fixtures::matched::author_audio_seek(root, &args[1..]);
    }
    if family == "transport-mpeg-video" {
        return transport_fixtures::matched::author_mpeg_video(root, &args[1..]);
    }
    if family == "transport-matched" {
        return transport_fixtures::matched::author(root, &args[1..]);
    }
    let mut options = std::collections::BTreeMap::new();
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("unexpected argument {flag}\n{FIXTURE_USAGE}"))?;
        if name == "help" {
            return Err(FIXTURE_USAGE.to_owned());
        }
        let switch = matches!(
            (family, name),
            ("zlib", "aac" | "mixed" | "empty") | ("header", "lace" | "aac")
        );
        let value = if switch {
            String::new()
        } else {
            rest.next()
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone()
        };
        options.insert(name.to_owned(), value);
    }
    let number = |key: &str, default: u64| -> Result<u64, String> {
        options.get(key).map_or(Ok(default), |v| {
            v.parse().map_err(|_| format!("--{key}: not a number: {v}"))
        })
    };
    let flag = |key: &str| options.contains_key(key);
    let output = PathBuf::from(options.get("output").ok_or("--output is required")?);
    let source = corpus::Source::load(root)?;
    let data = match family {
        "zlib" => {
            let kind = match options.get("kind").map_or("dynamic", String::as_str) {
                "stored" => corpus::Kind::Stored,
                "fixed" => corpus::Kind::Fixed,
                "dynamic" => corpus::Kind::Dynamic,
                other => return Err(format!("unknown zlib kind {other}")),
            };
            let case = corpus::ZlibCase {
                kind,
                scope: number("scope", 1)?,
                lace: u8::try_from(number("lace", 0)?).map_err(|e| e.to_string())?,
                aac: flag("aac"),
                mixed: flag("mixed"),
                empty: flag("empty"),
            };
            corpus::zlib_case(&source, case)?
        }
        "header" => {
            corpus::header_removal_case(&source, number("scope", 1)?, flag("lace"), flag("aac"))?
        }
        "colour" => {
            let width = number("mastering-width", 8)? as usize;
            let values = options
                .get("values")
                .ok_or("--values is required")?
                .split(',')
                .map(|v| {
                    v.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("--values: not a number: {v}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            corpus::colour_case(
                &source,
                Some(&Vec::new()),
                &corpus::mastering(width, false, &values),
                1,
            )?
        }
        _ => return Err(FIXTURE_USAGE.to_owned()),
    };
    std::fs::write(&output, data).map_err(|e| format!("{}: {e}", output.display()))
}

fn cargo(root: &Path, toolchain: Option<&str>, args: &[&str]) -> Task {
    let mut command = Command::new("cargo");
    if let Some(toolchain) = toolchain {
        command.arg(toolchain);
    }
    run(command
        .args(args)
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root))
}

/// `cargo SUBCOMMAND ARGS` compiled for the worker target, for the steps that build or
/// test what the workers run. The target follows the subcommand, ahead of any `--`.
fn cargo_for_workers(root: &Path, toolchain: Option<&str>, args: &[&str]) -> Task {
    let target = harness::gate_build::target_args()?;
    cargo(root, toolchain, &with_target(args, &target))
}

fn with_target<'a>(args: &[&'a str], target: &'a [String]) -> Vec<&'a str> {
    let (subcommand, rest) = args.split_at(args.len().min(1));
    subcommand
        .iter()
        .copied()
        .chain(target.iter().map(String::as_str))
        .chain(rest.iter().copied())
        .collect()
}

/// Run with the pinned FFmpeg reference first on `PATH`, as the harness expects.
fn run(command: &mut Command) -> Task {
    let shown = shown_on_reference_path(command)?;
    let status = command.status().map_err(|e| format!("{shown}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{shown}: {status}"))
    }
}

/// Puts the pinned FFmpeg reference first on `command`'s `PATH`, and prints and
/// returns the command line.
fn shown_on_reference_path(command: &mut Command) -> Result<String, String> {
    let reference = root().join(".research/reference/install/bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(reference).chain(std::env::split_paths(&path)))
        .map_err(|e| e.to_string())?;
    command.env("PATH", path);
    let shown = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    println!("$ {shown}");
    Ok(shown)
}

/// The workspace tests for the worker target, as [`run`] runs them, then how many
/// checks they left out for inputs this checkout lacks ([`test_skips`]).
fn workspace_tests(root: &Path) -> Task {
    tests_with_skips(root, None, &["test", "--workspace", "--locked"])
}

fn tests_with_skips(root: &Path, toolchain: Option<&str>, args: &[&str]) -> Task {
    let target = harness::gate_build::target_args()?;
    let args: Vec<String> = with_target(args, &target)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let cargo = || {
        let mut command = Command::new("cargo");
        if let Some(toolchain) = toolchain {
            command.arg(toolchain);
        }
        command.env("CARGO_INCREMENTAL", "0").current_dir(root);
        command
    };
    let mut command = cargo();
    command.args(&args);
    let shown = shown_on_reference_path(&mut command)?;
    let (code, skips) = test_skips::run(&mut command)?;
    let sources = if skips.is_empty() {
        std::collections::BTreeMap::default()
    } else {
        test_skips::sources(root, &mut cargo(), &args)?
    };
    println!("{}", test_skips::summary(&skips, &sources));
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{shown}: exit status {code}"))
    }
}

/// The nightly toolchain of the `std::simd` paths, pinned so that lints and code
/// generation do not change underneath the checks and measurements: the opt-in HEVC
/// path, and the AAC decoder's, which the gate build's codec worker uses.
pub(crate) const NIGHTLY: &str = "nightly-2026-10-17";

/// Everything `check` needs once, or what one part of it reads: each step runs as
/// its own `cargo xtask` command with the pinned reference first on `PATH`, so the
/// worker build records that reference.
fn prepare_check(part: Option<Part>) -> Task {
    let steps: &[&[&str]] = match part {
        None | Some(Part::Parity) => &[
            &["prepare-ffmpeg"],
            &["prepare-ffmpeg", "--profile", "formats"],
            &["prepare-ffmpeg", "--profile", "framing-views"],
            &["prepare-ffmpeg", "--profile", "decode"],
            &[
                "codecs",
                "derive",
                "--ffmpeg",
                ".research/reference-decode/install/bin/ffmpeg",
            ],
            &["framing-views", "fixtures", "dts"],
            &["framing-views", "fixtures", "streams"],
            &["framing-views", "fixtures", "config"],
            &["prepare-ffmpeg", "--profile", "transport-dts"],
            &["prepare-ffmpeg", "--profile", "transport-mpeg-video"],
            &["prepare-mpg123"],
            &["prepare-ffmpeg", "--profile", "video"],
            &["prepare-ffmpeg", "--profile", "subtitles"],
            &["fixtures"],
            &["fetch-vectors"],
            &["codec-views", "fixtures", "media"],
            &["codec-views", "fixtures", "audio"],
            &["subtitle-views", "fixtures", "corpus"],
            &["fonts", "prepare"],
            &["fonts", "fetch"],
            &["shape", "prepare"],
            &["raster", "prepare"],
            &["build-workers"],
        ],
        // The table checks read the unpacked FreeType and HarfBuzz releases.
        Some(Part::Lint) => &[&["fonts", "prepare"], &["shape", "prepare"]],
        Some(Part::Test | Part::Variants) => &[&["fixtures"], &["fetch-vectors"]],
        Some(Part::Docs) => &[],
    };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    for step in steps {
        run(Command::new(&exe).args(*step))?;
    }
    Ok(())
}

/// The pinned nightly with clippy, for the `std::simd` paths and the workers the gate
/// build compiles with it.
pub(crate) fn simd_toolchain() -> Task {
    run(Command::new("rustup")
        .args([
            "toolchain",
            "install",
            NIGHTLY,
            "--profile",
            "minimal",
            "--component",
            "clippy",
        ])
        .args(harness::gate_build::target_args()?))
}

/// Clippy over the nightly `std::simd` paths of the HEVC and AAC decoders.
fn simd_lints(root: &Path) -> Task {
    simd_toolchain()?;
    cargo(
        root,
        Some(&format!("+{NIGHTLY}")),
        &[
            "clippy",
            "-p",
            "triergon-codec-hevc",
            "-p",
            "triergon-codec-aac",
            "-p",
            "codec-worker",
            "--features",
            "triergon-codec-hevc/simd,triergon-codec-aac/simd,codec-worker/hevc-simd,codec-worker/aac-simd",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// The nightly `std::simd` paths must decode exactly as the scalar defaults do: the
/// HEVC conformance tests compare both with FFmpeg digests, and the AAC tests hold the
/// SBR kernels to the scalar arithmetic bit for bit.
fn simd_tests(root: &Path) -> Task {
    simd_toolchain()?;
    tests_with_skips(
        root,
        Some(&format!("+{NIGHTLY}")),
        &[
            "test",
            "-p",
            "triergon-codec-hevc",
            "-p",
            "triergon-codec-aac",
            "--features",
            "triergon-codec-hevc/simd,triergon-codec-aac/simd",
            "--release",
            "--locked",
        ],
    )
}

/// Crates whose comparison-only projections sit behind the `reference-trace` feature.
const REFERENCE_TRACE_CRATES: [&str; 11] = [
    "triergon",
    "triergon-bridge",
    "triergon-codec-ac3",
    "triergon-core",
    "triergon-font",
    "triergon-framing",
    "triergon-isobmff",
    "triergon-matroska",
    "triergon-mpegts",
    "triergon-playback",
    "triergon-subtitles",
];

/// `cargo xtask COMMAND ARGS` for a generator: `compare` runs the reports of the
/// features it selects, anything else is the generator's own subcommand.
fn generator_main(root: &Path, generator: &generators::Generator, args: &[String]) -> ExitCode {
    let result = match args {
        [command, rest @ ..] if command == "compare" => {
            match generator
                .reports(root, rest)
                .and_then(|reports| (generator.write_inputs)(root).map(|()| reports))
            {
                Ok(reports) => return run_reports(root, &reports, None),
                Err(error) => Err(error),
            }
        }
        _ => (generator.main)(root, args),
    };
    exit_with(result.map(|()| 0), 1)
}

/// The reports `COMMAND compare` stands for; exits with the first failing one's
/// code.
fn run_reports(
    root: &Path,
    reports: &[Vec<String>],
    optional: Option<&test_skips::ParityInputs>,
) -> ExitCode {
    let mut result = ExitCode::SUCCESS;
    for args in reports {
        println!("$ cargo xtask report {}", args.join(" "));
        let code = exit_with(report_with_inputs(root, args, optional), 2);
        if result == ExitCode::SUCCESS {
            result = code;
        }
    }
    result
}

/// Prepare generated inputs and run the contracts required by the workspace check.
fn check_features(root: &Path) -> Task {
    let contracts = harness::features::required(root)?;
    if contracts.is_empty() {
        return Err("no required feature contracts".into());
    }
    for generator in generators::GENERATORS {
        if contracts.iter().any(|contract| {
            contract.generated() && contract.candidate() == Some(generator.candidate)
        }) {
            (generator.write_inputs)(root)?;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let output = root
        .join(".research/feature-reports")
        .join(format!("check-{}-{stamp}", std::process::id()));
    harness::report::create_output(&output)?;
    let optional = test_skips::ParityInputs::load(root)?;
    for contract in contracts
        .iter()
        .filter(|contract| contract.document.get("oracle").is_some())
    {
        if harness::oracle_compare::compare_with_inputs(
            root,
            &contract.path,
            &output.join(&contract.id),
            Some(&optional),
        )? != 0
        {
            return Err(format!(
                "{}: required oracle comparison failed",
                contract.id
            ));
        }
    }
    let configured_aac = |contract: &harness::features::Contract| {
        harness::manifest::configured_aac_runner(&contract.document)
    };
    let dsp = |contract: &harness::features::Contract| {
        contract
            .document
            .get("runner")
            .and_then(json::Value::as_str)
            == Some("cargo xtask codecs dsp")
    };
    for contract in contracts.iter().filter(|contract| configured_aac(contract)) {
        let (manifest, _) = harness::manifest::read(root, &contract.manifest())?;
        harness::manifest::validate_required(&contract.document, &manifest)?;
        let args = [
            "aac-access-units".to_owned(),
            "--out".to_owned(),
            output.join(&contract.id).display().to_string(),
        ];
        if codecs::main(root, &args)? != 0 {
            return Err(format!("{}: required AAC comparison failed", contract.id));
        }
    }
    for contract in contracts.iter().filter(|contract| dsp(contract)) {
        let args = [
            "dsp".to_owned(),
            "--output".to_owned(),
            output.join(&contract.id).display().to_string(),
        ];
        if codecs::main(root, &args)? != 0 {
            return Err(format!("{}: required DSP comparison failed", contract.id));
        }
    }
    let reports = contracts
        .iter()
        .filter(|contract| {
            contract.document.get("oracle").is_none() && !configured_aac(contract) && !dsp(contract)
        })
        .map(|contract| {
            let manifest = contract.manifest();
            let (document, _) = harness::manifest::read(root, &manifest)?;
            harness::manifest::validate_required(&contract.document, &document)
                .map_err(|error| format!("{}: {error}", contract.id))?;
            Ok(vec![
                "--contract".to_owned(),
                contract.path.display().to_string(),
                "--manifest".to_owned(),
                manifest.display().to_string(),
                "--output".to_owned(),
                output.join(&contract.id).display().to_string(),
            ])
        })
        .collect::<Result<Vec<_>, String>>()?;
    if run_reports(root, &reports, Some(&optional)) != ExitCode::SUCCESS {
        return Err(format!(
            "required feature comparison failed; reports in {}",
            output.display()
        ));
    }
    let passed = contracts
        .iter()
        .filter(|contract| !configured_aac(contract) && !dsp(contract))
        .all(|contract| {
            json::read(&output.join(&contract.id).join("report.json"))
                .ok()
                .and_then(|report| {
                    report
                        .get("cases")
                        .and_then(json::Value::as_array)
                        .map(|cases| {
                            !cases.is_empty()
                                && cases.iter().all(harness::acceptance::parity_passes)
                        })
                })
                == Some(true)
        });
    if passed {
        std::fs::remove_dir_all(&output)
            .map_err(|error| format!("{}: {error}", output.display()))?;
    } else {
        println!(
            "Feature reports with unresolved parity in {}",
            output.display()
        );
    }
    for generator in generators::GENERATORS {
        smoke_campaigns(root, generator.smoke.iter().copied())?;
    }
    Ok(())
}

/// One fuzz round over the given cases of each feature through the release and
/// dev builds of its worker; a campaign that finds nothing is removed.
fn smoke_campaigns<'a>(
    root: &Path,
    features: impl Iterator<Item = (&'a str, &'a [&'a str])>,
) -> Task {
    for (feature, cases) in features {
        let contract = format!("harness/features/{feature}/contract.json");
        let output = format!(".research/check-fuzz-{feature}-{}", std::process::id());
        let mut args = vec![
            "--contract",
            &contract,
            "--output",
            &output,
            "--rounds",
            "1",
            "--timeout-seconds",
            "60",
        ];
        for case in cases {
            args.extend(["--case", case]);
        }
        println!("$ cargo xtask fuzz {}", args.join(" "));
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        if fuzz_campaign(root, &args)? != 0 {
            return Err(format!("the {feature} campaign found problems: {output}"));
        }
        std::fs::remove_dir_all(root.join(&output)).map_err(|e| format!("{output}: {e}"))?;
    }
    Ok(())
}

const LINK_ORDER_USAGE: &str = "usage: cargo xtask link-order BINARY [OUT] [--trained FILE]
       cargo xtask link-order corpus OUT
       cargo xtask link-order separate [--corpus DIR]
       cargo xtask link-order train [--corpus DIR] [--only WORKER]
       cargo xtask link-order check";

fn link_order(root: &Path, args: &[String]) -> Task {
    let option = |name: &str| -> Result<Option<&Path>, String> {
        match args.iter().position(|argument| argument == name) {
            Some(at) => args
                .get(at + 1)
                .map(|value| Some(Path::new(value.as_str())))
                .ok_or_else(|| format!("{name} needs a value\n{LINK_ORDER_USAGE}")),
            None => Ok(None),
        }
    };
    match args.first().map(String::as_str) {
        Some("corpus") => match args {
            [_, out] => harness::link_corpus::corpus(Path::new(out)),
            _ => Err(LINK_ORDER_USAGE.into()),
        },
        Some("separate") => harness::link_training::separate(root, option("--corpus")?),
        Some("train") => harness::link_training::train(
            root,
            option("--corpus")?,
            match args.iter().position(|argument| argument == "--only") {
                Some(at) => Some(
                    args.get(at + 1)
                        .map(String::as_str)
                        .ok_or_else(|| format!("--only needs a value\n{LINK_ORDER_USAGE}"))?,
                ),
                None => None,
            },
        ),
        Some("check") => harness::link_training::check(root),
        Some(_) => {
            let trained = match option("--trained")? {
                Some(path) => {
                    let text = std::fs::read_to_string(path)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    harness::link_order::parse_trained(&text)
                        .map_err(|error| format!("{}: {error}", path.display()))?
                }
                None => Vec::new(),
            };
            let positional: Vec<&String> = args
                .iter()
                .enumerate()
                .filter(|(at, _)| {
                    args[..*at].last().map(String::as_str) != Some("--trained")
                        && args[*at] != "--trained"
                })
                .map(|(_, argument)| argument)
                .collect();
            match positional[..] {
                [binary] => harness::link_order::symbol_order(Path::new(binary), &trained)
                    .map(|order| print!("{}", order.text)),
                [binary, out] => harness::link_order::symbol_order(Path::new(binary), &trained)
                    .and_then(|order| {
                        std::fs::write(out, order.text).map_err(|error| format!("{out}: {error}"))
                    }),
                _ => Err(LINK_ORDER_USAGE.into()),
            }
        }
        None => Err(LINK_ORDER_USAGE.into()),
    }
}

/// A part of `cargo xtask check`. CI runs each part as its own job; `check` alone
/// runs every step of every part, in the order of [`check`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Formatting, lints, rustdoc, the generated tables and the repository checks.
    Lint,
    /// The workspace tests.
    Test,
    /// The workspace in its other configurations: the reference-trace tests, the
    /// release build, the nightly SIMD tests and the core time oracle.
    Variants,
    /// The built workers against the references: the worker tests, the required
    /// feature reports, the fuzz smoke rounds and the trained link orders.
    Parity,
    /// Not a part of the full run but a selection from it: the steps that read the
    /// documents and the evidence sets ([`DOCS`]).
    Docs,
}

impl Part {
    fn named(name: &str) -> Result<Self, String> {
        match name {
            "lint" => Ok(Self::Lint),
            "test" => Ok(Self::Test),
            "variants" => Ok(Self::Variants),
            "parity" => Ok(Self::Parity),
            "docs" => Ok(Self::Docs),
            _ => Err(format!(
                "{name}: no such part of check (lint, test, variants, parity or docs)"
            )),
        }
    }
}

type Step = fn(&Path) -> Task;

/// Every step of `check`, in order, with the part it belongs to.
const CHECK: &[(Part, Step)] = &[
    (Part::Lint, fmt),
    (Part::Test, workspace_tests),
    (Part::Variants, |root| {
        for package in REFERENCE_TRACE_CRATES {
            cargo_for_workers(
                root,
                None,
                &[
                    "test",
                    "-p",
                    package,
                    "--features",
                    "reference-trace",
                    "--locked",
                ],
            )?;
        }
        Ok(())
    }),
    (Part::Lint, |root| {
        cargo(
            root,
            None,
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
        )
    }),
    (Part::Lint, |root| {
        cargo(
            root,
            None,
            &["check", "--workspace", "--no-default-features", "--locked"],
        )
    }),
    (Part::Lint, |root| rustdoc(root, &["--workspace"])),
    // Workspace feature unification can change the prepared worker binaries.
    // Keep this build separate from the workers checked against their identities.
    (Part::Variants, |root| {
        cargo_for_workers(
            root,
            None,
            &[
                "build",
                "--workspace",
                "--release",
                "--locked",
                "--target-dir",
                "target/check-release",
            ],
        )
    }),
    (Part::Lint, |root| {
        for package in REFERENCE_TRACE_CRATES {
            cargo(
                root,
                None,
                &[
                    "clippy",
                    "-p",
                    package,
                    "--features",
                    "reference-trace",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            )?;
            rustdoc(root, &["-p", package, "--features", "reference-trace"])?;
        }
        Ok(())
    }),
    (Part::Lint, simd_lints),
    (Part::Variants, simd_tests),
    (Part::Parity, |root| {
        println!("$ cargo xtask worker-tests");
        worker_tests::run(root, None)
    }),
    // The Unicode and glyph-name tables are what their generators write.
    (Part::Lint, |root| {
        println!("$ cargo xtask shape ucd --check");
        shape::main(root, &["ucd".to_owned(), "--check".to_owned()])
    }),
    (Part::Lint, |root| {
        println!("$ cargo xtask fonts tables --check");
        fonts::main(root, &["tables".to_owned(), "--check".to_owned()])
    }),
    (Part::Parity, check_features),
    (Part::Lint, check_safety),
    (Part::Lint, check_design),
    (Part::Lint, check_links),
    (Part::Lint, check_revisions),
    (Part::Lint, |root| {
        run(Command::new("python3")
            .arg("tools/test-ci-scope.py")
            .current_dir(root))
    }),
    (Part::Parity, |root| {
        println!("$ cargo xtask link-order check");
        harness::link_training::check(root)
    }),
    (Part::Variants, |root| {
        let mut args = vec!["--toolchain".to_owned(), NIGHTLY.to_owned()];
        let target = harness::gate_build::worker_target()?;
        if target != harness::gate_build::host()? {
            args.extend(["--target".to_owned(), target.to_owned()]);
        }
        core_time::main(root, &args)
    }),
];

/// What a change to the documents or the evidence sets alone can affect: the
/// formatting, the checks that read the documents, and the tests that read the
/// evidence index ([`gate::INDEX_TESTS`]). Each is also run by the full check.
const DOCS: &[Step] = &[
    fmt,
    check_design,
    check_links,
    check_revisions,
    evidence_index_tests,
];

fn fmt(root: &Path) -> Task {
    cargo(root, None, &["fmt", "--all", "--check"])
}

fn evidence_index_tests(root: &Path) -> Task {
    let mut args = vec!["test", "-p", "xtask", "--bin", "xtask", "--locked"];
    args.extend(["--", "--exact"]);
    args.extend(gate::INDEX_TESTS);
    cargo_for_workers(root, None, &args)
}

/// Every step of `check`, or those of one part.
fn check(root: &Path, part: Option<Part>) -> Task {
    if part == Some(Part::Docs) {
        return DOCS.iter().try_for_each(|step| step(root));
    }
    for &(owner, step) in CHECK {
        if part.is_none_or(|part| part == owner) {
            step(root)?;
        }
    }
    Ok(())
}

/// `cargo doc ARGS --no-deps --locked` with warnings denied.
fn rustdoc(root: &Path, args: &[&str]) -> Task {
    run(Command::new("cargo")
        .arg("doc")
        .args(args)
        .args(["--no-deps", "--locked"])
        .env("CARGO_INCREMENTAL", "0")
        .env("RUSTDOCFLAGS", "-D warnings")
        .current_dir(root))
}

#[cfg(test)]
mod tests {
    use super::{
        COMMANDS, HELP, REFERENCE_TRACE_CRATES, shell_library_path, usage_of, with_target,
    };
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn every_crate_with_a_reference_trace_feature_is_checked_with_it() {
        let mut declaring = Vec::new();
        for entry in std::fs::read_dir(super::root().join("crates")).unwrap() {
            let manifest = entry.unwrap().path().join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            let toml = crate::toml::parse(&text).unwrap();
            if toml
                .get("features")
                .and_then(|f| f.get("reference-trace"))
                .is_some()
            {
                let name = toml.get("package").and_then(|p| p.get("name"));
                declaring.push(name.and_then(|n| n.as_str()).unwrap().to_owned());
            }
        }
        declaring.sort();
        assert_eq!(declaring, REFERENCE_TRACE_CRATES);
    }

    #[test]
    fn every_command_has_one_entry_in_the_help_in_its_order() {
        let listed: Vec<&str> = HELP
            .lines()
            .skip_while(|line| *line != "commands:")
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|line| !line.starts_with(' '))
            .filter_map(|line| line.split(' ').next())
            .collect();
        let named: Vec<&str> = COMMANDS.iter().map(|command| command.name).collect();
        assert_eq!(listed, named);
        for name in named {
            assert!(usage_of(name).starts_with(&format!("usage: cargo xtask {name}")));
        }
    }

    #[test]
    fn the_worker_target_follows_the_subcommand() {
        let target = ["--target".to_owned(), "t".to_owned()];
        assert_eq!(
            with_target(&["clippy", "--workspace", "--", "-D", "warnings"], &target),
            [
                "clippy",
                "--target",
                "t",
                "--workspace",
                "--",
                "-D",
                "warnings"
            ]
        );
        assert_eq!(with_target(&["test"], &[]), ["test"]);
    }

    #[test]
    fn cargo_library_entries_are_dropped_and_others_kept() {
        let built = Path::new("/w/target/debug");
        let toolchains = Some(Path::new("/h/.rustup/toolchains"));
        let cargo = "/w/target/debug:/w/target/debug/deps:\
                     /h/.rustup/toolchains/t/lib/rustlib/aarch64-unknown-linux-gnu/lib:\
                     /h/.rustup/toolchains/t/lib";
        assert_eq!(
            shell_library_path(OsStr::new(cargo), built, toolchains),
            None
        );
        let mixed = format!("{cargo}:/opt/lib:/usr/local/lib");
        assert_eq!(
            shell_library_path(OsStr::new(&mixed), built, toolchains).unwrap(),
            OsStr::new("/opt/lib:/usr/local/lib")
        );
        let user = OsStr::new("/opt/lib");
        assert_eq!(shell_library_path(user, built, toolchains).unwrap(), user);
        assert_eq!(shell_library_path(user, built, None).unwrap(), user);
    }
}
