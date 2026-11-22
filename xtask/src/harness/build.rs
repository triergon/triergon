//! `cargo xtask build-workers`: build the candidate worker, the FFmpeg reference
//! workers and the memory instrumentation, and record the build identity every
//! collection is later checked against (`target/harness/identity.json`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::gate_build;
use super::linux;
use super::provenance::{cargo_metadata, selected_graph, shared_inputs};
use crate::json::{self, Value};

type R<T> = Result<T, String>;

const USAGE: &str =
    "usage: cargo xtask build-workers [--comparison-build] [--only WORKER] [--all-references]";

fn options(args: &[String]) -> R<(bool, Option<&str>, bool)> {
    let mut comparison_build = false;
    let mut only = None;
    let mut all_references = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--comparison-build" if !comparison_build => comparison_build = true,
            "--only" if only.is_none() => {
                only = Some(args.next().ok_or(USAGE)?.as_str());
            }
            "--all-references" if !all_references => all_references = true,
            _ => return Err(USAGE.into()),
        }
    }
    Ok((comparison_build, only, all_references))
}

pub fn main(root: &Path, args: &[String]) -> R<PathBuf> {
    let (comparison_build, only, all_references) = options(args)?;
    build_selected(root, comparison_build, only, all_references)
}

fn sha(path: &Path) -> R<String> {
    linux::file_sha(path)
}

fn output(program: &str, args: &[&str], env: &[(&str, String)]) -> R<String> {
    let mut command = Command::new(program);
    command.args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {}: {}\n{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

fn run(argv: &[String]) -> R<()> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{}: {status}", argv.join(" ")))
    }
}

pub(crate) fn test_transport_entities(root: &Path) -> R<()> {
    let profile = PROFILE_REFERENCES
        .iter()
        .find(|profile| profile.record == "ffmpeg-formats-native")
        .expect("formats reference profile");
    let pkgconfig = root.join(profile.profile).join("install/lib/pkgconfig");
    let mut query = vec!["--static", "--cflags", "--libs"];
    query.extend(profile.packages);
    let flags = output(
        "pkg-config",
        &query,
        &[("PKG_CONFIG_LIBDIR", pkgconfig.display().to_string())],
    )?;
    let binary = root.join("target/harness/reference-ts-sequence-test");
    let mut command = vec![std::env::var("CC").unwrap_or_else(|_| "cc".into())];
    command.extend(profile.options.iter().map(ToString::to_string));
    command.extend(profile.define.map(str::to_owned));
    command.push(
        root.join("harness/reference_ts_sequence_test.c")
            .display()
            .to_string(),
    );
    command.extend(shell_words(&flags)?);
    command.extend(["-o".into(), binary.display().to_string()]);
    run(&command)?;
    run(&[binary.display().to_string(), root.display().to_string()])
}

/// POSIX shell word splitting of `pkg-config` output (quotes and backslashes).
pub fn shell_words(text: &str) -> R<Vec<String>> {
    let (mut words, mut word, mut active) = (Vec::new(), String::new(), false);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if active {
                    words.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            '\'' => {
                active = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                active = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('\\' | '"' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                active = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => word.push(c),
                    None => return Err("trailing backslash".into()),
                }
            }
            c => {
                active = true;
                word.push(c);
            }
        }
    }
    if active {
        words.push(word);
    }
    Ok(words)
}

fn strings(argv: &[String]) -> Value {
    Value::strings(argv.iter().cloned())
}

pub(super) fn instrumentation_commands(
    compiler: &str,
    root: &Path,
    out: &Path,
    linux: bool,
) -> Vec<Vec<String>> {
    let p = |path: PathBuf| path.display().to_string();
    let owned = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
    if linux {
        let mut observer = owned(&[
            compiler,
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fno-builtin",
            "-Wl,-z,now",
            "-shared",
            "-fPIC",
        ]);
        observer.extend([
            p(root.join("harness/allocations-linux.c")),
            "-o".into(),
            p(out.join("allocations.so")),
            "-ldl".into(),
            "-pthread".into(),
        ]);
        let mut probe = owned(&[
            compiler,
            "-std=c11",
            "-O0",
            "-fno-builtin",
            "-Wall",
            "-Wextra",
            "-Werror",
        ]);
        probe.extend([
            p(root.join("harness/portable-memory-check.c")),
            "-o".into(),
            p(out.join("memory-check")),
            "-ldl".into(),
            "-pthread".into(),
            // The probe calibrates libstdc++'s operator new and delete.
            "-lstdc++".into(),
        ]);
        vec![observer, probe]
    } else {
        let mut observer = owned(&[
            compiler,
            "-std=c11",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-dynamiclib",
        ]);
        observer.extend([
            p(root.join("harness/allocations.c")),
            "-o".into(),
            p(out.join("allocations.dylib")),
        ]);
        let mut probe = owned(&[
            compiler,
            "-O0",
            "-fno-builtin",
            "-Wall",
            "-Wextra",
            "-Werror",
        ]);
        probe.extend([
            p(root.join("harness/memory-check.c")),
            "-o".into(),
            p(out.join("memory-check")),
        ]);
        vec![observer, probe]
    }
}

/// Darwin: the dynamic links of a worker and the OS-provided zlib it uses.
fn darwin_libraries(worker: &Path) -> R<Value> {
    let listing = output("otool", &["-L", &worker.display().to_string()], &[])?;
    let zlib = listing.lines().skip(1).find(|l| l.contains("/libz."));
    let mut info = vec![
        ("status", Value::str("recorded")),
        ("dynamic_links", Value::str(&listing)),
        (
            "os_build",
            Value::str(output("sw_vers", &["-buildVersion"], &[])?.trim()),
        ),
    ];
    if let Some(line) = zlib {
        let path = line.trim().split(" (").next().unwrap_or_default();
        let version = line
            .split("current version ")
            .nth(1)
            .and_then(|v| v.split(')').next())
            .unwrap_or_default();
        info.push((
            "system_zlib",
            Value::object([
                ("path", Value::str(path)),
                ("version", Value::str(version)),
                ("provenance", Value::str("OS shared-cache component identified by path, linked version and OS build; not a standalone content hash")),
            ]),
        ));
    }
    Ok(Value::object(info))
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// A measurement worker and how `cargo` builds it: its name, package, example (none
/// for a binary of the same name), features and compiler.
#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub name: &'static str,
    pub package: &'static str,
    pub example: Option<&'static str>,
    pub features: &'static [&'static str],
    /// Built by the pinned nightly ([`crate::NIGHTLY`]) instead of the release
    /// `rust-toolchain.toml` pins: a worker whose measured code uses `std::simd`.
    pub nightly: bool,
}

impl Candidate {
    /// The toolchain that builds the worker, as rustup names it.
    pub fn toolchain(&self, pinned: &str) -> String {
        if self.nightly {
            crate::NIGHTLY.to_owned()
        } else {
            pinned.to_owned()
        }
    }
}

/// The worker every contract names unless it names another.
const MEDIA_WORKER: Candidate = Candidate {
    name: "media-worker",
    package: "media-worker",
    example: None,
    features: &[],
    nightly: false,
};

/// Measurement workers a contract may name as its candidate besides
/// `media-worker`.
const ADDITIONAL_CANDIDATES: [Candidate; 23] = [
    Candidate {
        name: "playback-worker",
        package: "playback-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    // A second binary of the playback-worker package with the Matroska runners
    // alone, the runner families its reference build can demux.
    Candidate {
        name: "playback-matroska-worker",
        package: "playback-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "transport-worker",
        package: "transport-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "transport-views-worker",
        package: "transport-worker",
        example: None,
        features: &["views"],
        nightly: false,
    },
    Candidate {
        name: "playback-views-worker",
        package: "playback-worker",
        example: None,
        features: &["views"],
        nightly: false,
    },
    Candidate {
        name: "play-views-worker",
        package: "codec-worker",
        example: None,
        features: &["play"],
        nightly: false,
    },
    Candidate {
        name: "codec-worker",
        package: "codec-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    // The AAC decode operations alone, with the decoder's SBR filter banks in
    // `std::simd` form: without explicit vector types LLVM emits no NEON float code for
    // the 32-bit ARM target. Every other operation is measured from `codec-worker`.
    Candidate {
        name: "codec-aac-worker",
        package: "codec-worker",
        example: None,
        features: &["aac-simd"],
        nightly: true,
    },
    Candidate {
        name: "codec-hevc-worker",
        package: "codec-worker",
        example: None,
        features: &["hevc-simd"],
        nightly: true,
    },
    Candidate {
        name: "codec-views-worker",
        package: "codec-worker",
        example: None,
        features: &["views"],
        nightly: false,
    },
    Candidate {
        name: "capture-worker",
        package: "capture-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "font-worker",
        package: "font-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "shape-worker",
        package: "shape-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "raster-worker",
        package: "raster-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "ass-worker",
        package: "ass-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "subtitle-worker",
        package: "subtitle-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    // A second binary of the subtitle-worker package.
    Candidate {
        name: "subtitle-capsule-worker",
        package: "subtitle-worker",
        example: None,
        features: &[],
        nightly: false,
    },
    Candidate {
        name: "subtitle-views-worker",
        package: "subtitle-worker",
        example: None,
        features: &["triergon-subtitles/reference-trace"],
        nightly: false,
    },
    Candidate {
        name: "matroska-views-worker",
        package: "triergon-matroska",
        example: Some("matroska_views_worker"),
        features: &["reference-trace"],
        nightly: false,
    },
    Candidate {
        name: "isobmff-worker",
        package: "triergon-isobmff",
        example: Some("isobmff_consumer_worker"),
        features: &["reference-trace"],
        nightly: false,
    },
    Candidate {
        name: "framing-worker",
        package: "triergon-framing",
        example: Some("framing_worker"),
        features: &["reference-trace"],
        nightly: false,
    },
    Candidate {
        name: "framing-views-worker",
        package: "triergon-framing",
        example: Some("framing_views_worker"),
        features: &["reference-trace"],
        nightly: false,
    },
    Candidate {
        name: "isobmff-views-worker",
        package: "triergon-isobmff",
        example: Some("isobmff_views_worker"),
        features: &["reference-trace"],
        nightly: false,
    },
];

/// Every measurement worker, `media-worker` first.
pub fn workers() -> impl Iterator<Item = &'static Candidate> {
    std::iter::once(&MEDIA_WORKER).chain(ADDITIONAL_CANDIDATES.iter())
}

/// The `cargo rustc` command line that builds `spec` into `target_dir`, in the
/// release profile for the worker target or the dev profile for this host, up to
/// and including the `--` before the worker's own compiler arguments.
pub fn cargo_rustc(spec: &Candidate, target_dir: &Path, release: bool) -> R<Vec<String>> {
    let mut command = vec!["cargo".to_owned()];
    if spec.nightly {
        command.push(format!("+{}", crate::NIGHTLY));
    }
    command.extend(["rustc", "-p", spec.package].map(String::from));
    if release {
        command.push("--release".into());
    }
    command.extend([
        "--locked".into(),
        "--target-dir".into(),
        target_dir.display().to_string(),
    ]);
    if release {
        command.extend(gate_build::target_args()?);
    }
    match spec.example {
        Some(example) => command.extend(["--example".into(), example.into()]),
        None => command.extend(["--bin".into(), spec.name.into()]),
    }
    if !spec.features.is_empty() {
        command.extend(["--features".into(), spec.features.join(",")]);
    }
    command.push("--".into());
    Ok(command)
}

pub(super) fn candidate_graph(root: &Path, name: &str) -> R<Value> {
    let spec = candidate(name)?;
    let features: Vec<String> = spec
        .features
        .iter()
        .map(|feature| {
            if feature.contains('/') {
                (*feature).to_owned()
            } else {
                format!("{}/{feature}", spec.package)
            }
        })
        .collect();
    let metadata = super::provenance::cargo_metadata_with_features(root, &features)?;
    let mut inputs = shared_inputs(root)?;
    if name == "isobmff-worker" {
        inputs.extend(crate::isobmff_bench::source_inputs(root)?);
        inputs.extend(crate::isobmff_seek::source_inputs(root)?);
    }
    if name == "transport-worker" {
        inputs.extend(crate::transport_fixtures::entity_inputs(root)?);
    }
    if name == "framing-worker" {
        inputs.extend(
            [
                "xtask/src/framing_bench.rs",
                "harness/reference_framing.c",
                "harness/reference_framing.h",
            ]
            .map(str::to_owned),
        );
        for file in [
            "avc-profile-extension-inputs.json",
            "contract.json",
            "corpus.json",
            "fixtures.rs",
            "inputs.json",
            "observations.json",
            "oracle.rs",
            "provenance.json",
            "worker.rs",
            "worker_model.rs",
            "worker_operation.rs",
        ] {
            inputs.push(format!("harness/features/packet-framing/{file}"));
        }
    }
    let view_headers: &[&str] = match name {
        "transport-views-worker" => &[
            "harness/reference_transport_views.c",
            "harness/reference_transport_catalogue.h",
            "harness/reference_transport_packets.h",
            "harness/reference_transport_source.h",
            "harness/reference_transport_facts.h",
            "harness/reference_transport_reader.h",
            "harness/reference_transport_mux.h",
            "harness/reference_ts.h",
            "harness/reference_ts_input.h",
            "harness/reference_ts_sequence.h",
            "harness/features/ts-mux-readback/capture-pictures.hex",
            "harness/ts_descriptor_oracle.py",
            "harness/ts_packet_oracle.py",
            "harness/ts_time_oracle.py",
            "xtask/src/transport_views/catalogue.rs",
            "xtask/src/transport_views/mux.rs",
            "xtask/src/transport_views/packets.rs",
            "xtask/src/transport_views/time.rs",
            "xtask/src/transport_views/mod.rs",
        ],
        "playback-views-worker" => &[
            "harness/reference_playback_views.c",
            "harness/reference_playback_probe.h",
            "harness/reference_playback_source_view.h",
            "harness/reference_playback_player.h",
            "harness/reference_playback_hls.h",
            "harness/reference_playback_bridge.h",
            "harness/av1_bridge_oracle.py",
            "harness/codec_stream_oracle.py",
            "harness/spec_oracle.py",
            "harness/reference_playback_subtitles.h",
            "harness/reference_subtitles.h",
            "harness/reference_packet_fold.h",
            "harness/reference_subtitles_host.h",
            "harness/reference_views_protocol.h",
            "xtask/src/playback_views/mod.rs",
            "xtask/src/playback_views/probe.rs",
            "harness/mp4_source_clock_oracle.py",
        ],
        "play-views-worker" => &[
            "harness/reference_play_views.c",
            "harness/reference_play_raw.h",
            "harness/reference_playback_probe.h",
            "harness/reference_decode.h",
            "xtask/src/play_views/mod.rs",
            "xtask/src/play_views/seek.rs",
            "harness/features/facade-seek/container-pins.json",
            "crates/triergon/examples/play/pipeline.rs",
            "harness/codecs/play-manifest.json",
            "harness/codecs/play-workloads.json",
            "xtask/src/codecs/correctness/play.rs",
        ],
        "codec-views-worker" => &[
            "harness/reference_codec_views.c",
            "harness/reference_codec_readers.h",
            "harness/reference_codec_video.h",
            "harness/reference_codec_audio.h",
            "harness/reference_codec_blocks.h",
        ],
        "matroska-views-worker" => &[
            "crates/worker-entry/src/lib.rs",
            "crates/worker-entry/README.md",
            "harness/reference_matroska_views.c",
            "harness/reference_matroska_side_data.h",
            "harness/reference_matroska_metadata.h",
        ],
        "subtitle-views-worker" => &[
            "harness/reference_subtitle_views.c",
            "harness/reference_subtitle_views_text.h",
            "harness/reference_subtitle_views_source.h",
        ],
        "framing-views-worker" => &[
            "harness/reference_framing_views.c",
            "harness/reference_views_protocol.h",
            "harness/reference_framing.h",
            "harness/reference_nal_units.h",
            "harness/reference_obu.h",
            "harness/reference_adts.h",
            "harness/reference_config_views.h",
            "harness/reference_dts_frames.h",
            "harness/dts_oracle.py",
            "harness/codec_stream_oracle.py",
            "harness/codec_config_oracle.py",
            "xtask/src/framing_views/obu.rs",
            "xtask/src/framing_views/adts.rs",
            "xtask/src/framing_views/config.rs",
            "xtask/src/framing_views/dts.rs",
            "xtask/src/framing_views/nal.rs",
        ],
        "isobmff-views-worker" => &[
            "crates/framing/examples/framing_views/fold.rs",
            "crates/framing/examples/framing_views/protocol.rs",
            "harness/reference_isobmff_views.c",
            "harness/reference_views_protocol.h",
            "harness/reference_isobmff_packets.h",
            "harness/reference_isobmff_index.h",
            "harness/reference_isobmff_trace.h",
            "harness/isobmff_index_oracle.py",
            "xtask/src/isobmff_views/index.rs",
        ],
        _ => &[],
    };
    inputs.extend(view_headers.iter().map(ToString::to_string));
    if matches!(
        name,
        "codec-views-worker"
            | "matroska-views-worker"
            | "subtitle-views-worker"
            | "transport-views-worker"
            | "play-views-worker"
    ) {
        inputs.extend(
            [
                "crates/framing/examples/framing_views/fold.rs",
                "crates/framing/examples/framing_views/protocol.rs",
                "harness/reference_views_protocol.h",
            ]
            .map(str::to_owned),
        );
    }
    inputs.sort();
    inputs.dedup();
    selected_graph(root, &metadata, spec.package, &inputs)
}

/// A known measurement worker whose binary and selected source graph still match.
pub(crate) fn candidate_worker(root: &Path, identity: &Value, name: &str) -> R<PathBuf> {
    let record = identity
        .get("additional_candidates")
        .and_then(|workers| workers.get(name))
        .ok_or_else(|| format!("{name} is not in the build identity; rebuild"))?;
    let path = root.join(gate_build::release_dir()?).join(name);
    if record.get("sha256").and_then(Value::as_str) != Some(sha(&path)?.as_str()) {
        return Err(format!("{name} changed since build identity; rebuild"));
    }
    let current = candidate_graph(root, name)?;
    if !record
        .get("selected_graph")
        .is_some_and(|graph| super::diff::equal(graph, &current))
    {
        return Err(format!(
            "{name} sources changed since build identity; rebuild"
        ));
    }
    Ok(path)
}

fn selected_candidates(only: Option<&str>) -> R<Vec<&'static Candidate>> {
    match only {
        None => Ok(ADDITIONAL_CANDIDATES.iter().collect()),
        Some("media-worker") => Ok(Vec::new()),
        Some(name) => candidate(name).map(|spec| vec![spec]),
    }
}

/// How a worker's trained order laid it out: the SHA-256 of the committed trained
/// order, how many names it lists and how many of them the binary holds, and the
/// SHA-256 of the ordering file the link used.
pub struct LinkOrder {
    trained_sha256: String,
    trained: usize,
    held: usize,
    ordering_sha256: String,
}

impl LinkOrder {
    fn record(order: Option<&Self>) -> Value {
        order.map_or(Value::Null, |order| {
            Value::object([
                ("trained_sha256", Value::str(&order.trained_sha256)),
                ("trained", Value::int(order.trained as i128)),
                ("held", Value::int(order.held as i128)),
                ("ordering_sha256", Value::str(&order.ordering_sha256)),
            ])
        })
    }
}

/// Builds the worker `name` with `command`, a `cargo rustc` command line that ends
/// with its link arguments. A worker the training manifest orders for the worker
/// target ([`super::link_training::trained_order`]) is linked by the pinned
/// toolchain's `lld`, then linked again with its functions in its trained order, the
/// rest in module order; `command` then holds the second build. `binary` is where
/// the build puts the worker. The ordering file is written to
/// `target/harness/link-order/NAME-HASH.txt`, HASH the first 16 hex digits of its
/// SHA-256, so the second build's link arguments change whenever the order does and
/// cargo never reuses a link made with another order. The build fails when the
/// binary holds fewer than [`super::link_order::MIN_HELD_PERCENT`] of the trained
/// names, or when the second build's own ordering file differs. Returns how the
/// order laid the worker out, none for a worker linked as its toolchain links it.
fn build_in_link_order(
    root: &Path,
    command: &mut Vec<String>,
    binary: &Path,
    name: &str,
) -> R<Option<LinkOrder>> {
    let cargo = |command: &[String]| -> R<()> {
        let status = Command::new("cargo")
            .args(&command[1..])
            .env("CARGO_INCREMENTAL", "0")
            .current_dir(root)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{name} build failed: {status}"))
        }
    };
    let linker = gate_build::ordered_link_arguments(root, None)?;
    let trained = if linker.is_empty() {
        None
    } else {
        super::link_training::trained_order(root, gate_build::worker_target()?, name)?
    };
    let Some(trained) = trained else {
        cargo(command)?;
        return Ok(None);
    };
    let first: Vec<String> = command.iter().cloned().chain(linker).collect();
    cargo(&first)?;
    let order = super::link_order::symbol_order(binary, &trained.names)?;
    if !super::link_order::holds_enough(order.held, trained.names.len()) {
        return Err(format!(
            "{name} holds {} of the {} functions of its trained order; retrain it \
             (cargo xtask link-order train)",
            order.held,
            trained.names.len()
        ));
    }
    let hash = crate::sha256::hex(order.text.as_bytes());
    let directory = root.join("target/harness/link-order");
    fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let path = directory.join(format!("{name}-{}.txt", &hash[..16]));
    fs::write(&path, &order.text).map_err(|e| format!("{}: {e}", path.display()))?;
    command.extend(gate_build::ordered_link_arguments(root, Some(&path))?);
    cargo(command)?;
    if super::link_order::symbol_order(binary, &trained.names)?.text != order.text {
        return Err(format!(
            "{name}: the ordered link holds other functions than the first link"
        ));
    }
    Ok(Some(LinkOrder {
        trained_sha256: trained.sha256,
        trained: trained.names.len(),
        held: order.held,
        ordering_sha256: hash,
    }))
}

/// A built worker: its name, the command that built it and how its trained order
/// laid it out, none when it links as its toolchain does.
type Built = (&'static str, Vec<String>, Option<LinkOrder>);

/// Where cargo builds a worker: a nightly worker has a target directory of its own, so
/// that changing compilers never rebuilds the other workers' dependencies.
fn candidate_target_dir(root: &Path, spec: &Candidate) -> PathBuf {
    if spec.nightly {
        root.join("target/nightly")
    } else {
        root.join("target")
    }
}

fn additional_candidates(root: &Path, selected: &[&Candidate]) -> R<Vec<Built>> {
    let mut built = Vec::new();
    for spec in selected {
        let target_dir = candidate_target_dir(root, spec);
        let mut command = cargo_rustc(spec, &target_dir, true)?;
        command.extend(gate_build::worker_link_arguments()?);
        let output = target_dir.join(gate_build::release_subdir()?);
        let binary = match spec.example {
            Some(example) => output.join("examples").join(example),
            None => output.join(spec.name),
        };
        let order = build_in_link_order(root, &mut command, &binary, spec.name)?;
        let release = root.join(gate_build::release_dir()?);
        let worker = release.join(spec.name);
        if binary != worker {
            fs::create_dir_all(&release).map_err(|error| format!("{}: {error}", spec.name))?;
            fs::copy(&binary, &worker).map_err(|error| format!("{}: {error}", spec.name))?;
        }
        built.push((spec.name, command, order));
    }
    Ok(built)
}

/// The additional candidate `name`.
pub(super) fn candidate(name: &str) -> R<&'static Candidate> {
    ADDITIONAL_CANDIDATES
        .iter()
        .find(|spec| spec.name == name)
        .ok_or_else(|| format!("unknown measurement worker {name}"))
}

fn candidate_records(root: &Path, built: &[Built]) -> R<Value> {
    let mut candidates = Vec::new();
    for (name, command, order) in built {
        let path = format!("{}/{name}", gate_build::release_dir()?);
        let toolchain = candidate(name)?.toolchain(&gate_build::pinned_release(root)?);
        candidates.push((
            *name,
            Value::object([
                ("path", Value::str(&path)),
                ("sha256", Value::str(sha(&root.join(&path))?)),
                (
                    "rustc",
                    Value::str(gate_build::toolchain_compiler(root, &toolchain)?),
                ),
                ("toolchain", Value::str(toolchain)),
                ("command", strings(command)),
                ("link_order", LinkOrder::record(order.as_ref())),
                ("selected_graph", candidate_graph(root, name)?),
            ]),
        ));
    }
    Ok(Value::object(candidates))
}

/// A comparison reference linked against one pinned profile, and possibly
/// further profiles.
struct ProfileReference {
    profile: &'static str,
    /// Further pinned profiles it links: their pkg-config directories follow the
    /// profile's and their build records join its identity.
    linked: &'static [&'static str],
    binary: &'static str,
    source: &'static str,
    define: Option<&'static str>,
    record: &'static str,
    kind: &'static str,
    libraries: &'static str,
    /// pkg-config packages, in link order.
    packages: &'static [&'static str],
    /// Compiler options before the source file.
    options: &'static [&'static str],
    /// C library functions linked with `--wrap`: the source defines each
    /// `__wrap_` entry point, and every call of the function in the source and
    /// the pinned archives goes through it.
    wrapped: &'static [&'static str],
    /// Whether the build answers `identity -` (the container builds do; the
    /// decode, capture and font builds record their configuration in the profile's
    /// `build.json` instead).
    identity: bool,
    /// Fails when the profile's `build.json` is not the build its prepare command
    /// makes now.
    prepared: Option<fn(&Value) -> R<()>>,
    /// Writes generated sources the reference includes and returns their
    /// directory, which joins its system include path.
    generated: Option<fn(&Path) -> R<PathBuf>>,
    /// Whether the build compiles the library's own sources with checks of its
    /// own, as the ASS parsing reference notes each conversion C leaves to the
    /// target: its results are compared, and it is never timed or measured.
    instrumented: bool,
}

const CONTAINER_PACKAGES: &[&str] = &["libavformat", "libavcodec", "libavutil"];
const CONTAINER_OPTIONS: &[&str] = &["-O3", "-Wall", "-Wextra", "-Werror"];
const DECODE_PACKAGES: &[&str] = &[
    "libavformat",
    "libavfilter",
    "libswscale",
    "libswresample",
    "libavcodec",
    "libavutil",
];
// These conversions can differ between targets (reference_subtitles_host.h).
const SUBTITLE_WRAPS: &[&str] = &[
    "strtol",
    "strtoul",
    "__isoc23_strtol",
    "__isoc23_strtoul",
    "__isoc99_sscanf",
    "lrint",
    "lrintf",
    "lround",
    "llrint",
    "llrintf",
];
/// The options the decoder and capture comparisons were measured with.
const RELEASE_OPTIONS: &[&str] = &["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"];
/// The shaping reference is compiled like its libraries, without contraction.
const SHAPE_OPTIONS: &[&str] = &[
    "-std=c11",
    "-O2",
    "-ffp-contract=off",
    "-Wall",
    "-Wextra",
    "-Werror",
];
/// The rasterising and ASS references take libass's own compiler flags from
/// `libass-internal.pc`, so its included stages compute as the library does.
/// Those zero uninitialised locals in the harness's own code too, so a read of
/// one stays an error there (`-Werror=uninitialized`; GCC's
/// `-Wmaybe-uninitialized`, which clang does not know, through `-Wall
/// -Werror`). libass's sources are included from an `-isystem` directory and
/// compiled without warnings, as system headers are.
const RASTER_OPTIONS: &[&str] = &["-Wall", "-Wextra", "-Werror", "-Werror=uninitialized"];
const CAPTURE_PACKAGES: &[&str] = &["libavformat", "libavcodec", "libswscale", "libavutil"];

/// What most reference builds share: the one reference source, with nothing
/// generated and nothing instrumented.
const REFERENCE: ProfileReference = ProfileReference {
    profile: "",
    linked: &[],
    binary: "",
    source: "harness/reference.c",
    define: None,
    record: "",
    kind: "",
    libraries: "",
    packages: &[],
    options: RELEASE_OPTIONS,
    wrapped: &[],
    identity: false,
    prepared: None,
    generated: None,
    instrumented: false,
};

/// The libass builds: against the FreeType, FriBidi and HarfBuzz references,
/// with libass's private headers and its own compiler flags.
const LIBASS: ProfileReference = ProfileReference {
    profile: ".research/libass",
    linked: &[".research/freetype", ".research/harfbuzz"],
    packages: &["libass-internal"],
    options: RASTER_OPTIONS,
    prepared: Some(crate::raster::check_prepared),
    ..REFERENCE
};

const PROFILE_REFERENCES: [ProfileReference; 32] = [
    ProfileReference {
        profile: ".research/reference-video",
        binary: "ffmpeg-video-worker",
        define: Some("-DTRIERGON_FEED_REFERENCE=1"),
        record: "ffmpeg-video-native",
        kind: "pinned FFmpeg with the H.264 and HEVC decoders; feed operations",
        libraries: "video_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-formats-worker",
        define: Some("-DTRIERGON_HLS_REFERENCE=1"),
        record: "ffmpeg-formats-native",
        kind: "pinned FFmpeg MOV, TS and subtitle build for HLS and MP4 range operations",
        libraries: "formats_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-ts-seek-worker",
        source: "harness/reference_ts_matched.c",
        record: "ffmpeg-ts-seek",
        kind: "FFmpeg MPEG-TS access-unit scan and replay by preceding picture",
        libraries: "ts_seek_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-ts-audio-seek-worker",
        source: "harness/reference_ts_matched.c",
        define: Some("-DTRIERGON_REFERENCE_AUDIO_SEEK=1"),
        record: "ffmpeg-ts-audio-seek",
        kind: "FFmpeg MPEG-TS access-unit scan and replay by preceding timestamped audio frame",
        libraries: "ts_audio_seek_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-transport-dts",
        binary: "ffmpeg-ts-dts-seek-worker",
        source: "harness/reference_ts_matched.c",
        define: Some("-DTRIERGON_REFERENCE_AUDIO_SEEK=1"),
        record: "ffmpeg-ts-dts-seek",
        kind: "FFmpeg MPEG-TS access-unit scan and replay for DTS core",
        libraries: "ts_dts_seek_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-transport-dts",
        binary: "ffmpeg-transport-dts-worker",
        source: "harness/reference_ts_worker.c",
        record: "ffmpeg-transport-dts",
        kind: "FFmpeg MPEG-TS and DTS core access units",
        libraries: "transport_dts_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: crate::mpg123::CACHE,
        linked: &[".research/reference-formats"],
        binary: "mpg123-ts-worker",
        source: "harness/reference_ts_matched.c",
        define: Some("-DTRIERGON_REFERENCE_MPG123=1"),
        record: "mpg123-ts",
        kind: "FFmpeg MPEG-TS demuxer and mpg123 free-format frame parser",
        libraries: "ts_free_audio_reference",
        packages: &["libavformat", "libavcodec", "libavutil", "libmpg123"],
        options: CONTAINER_OPTIONS,
        prepared: Some(crate::mpg123::verify),
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-subtitles",
        binary: "ffmpeg-subtitles-worker",
        define: Some("-DTRIERGON_SUBTITLE_REFERENCE=1"),
        record: "ffmpeg-subtitles-native",
        kind: "pinned FFmpeg subtitle decoders with libzvbi and the video decoders closed captions need; subtitle decode operations",
        libraries: "subtitles_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        wrapped: SUBTITLE_WRAPS,
        identity: true,
        // Its wrappers run inside the timed operation, so it is never timed or
        // measured; `ffmpeg-subtitles-plain-native` is.
        instrumented: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-subtitles",
        binary: "ffmpeg-subtitles-plain-worker",
        define: Some("-DTRIERGON_SUBTITLE_PLAIN_REFERENCE=1"),
        record: "ffmpeg-subtitles-plain-native",
        kind: "the subtitles reference without its flagging wrappers; the timing and memory reference of the subtitle decode features",
        libraries: "subtitles_plain_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: "ffmpeg-decode-worker",
        define: Some("-DTRIERGON_DECODE_REFERENCE=1"),
        record: "ffmpeg-decode-native",
        kind: "FFmpeg 9.0.2 decoders with the letterbox and downmix filter chains; codec operations",
        libraries: "decode_reference",
        packages: DECODE_PACKAGES,
        options: RELEASE_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: crate::codecs::INITIALIZED_AAC_REFERENCE,
        source: "harness/reference_aac_pcm.c",
        record: "ffmpeg-aac-initialized-pcm",
        kind: "AAC floating-point correctness reference with zero-initialized planes",
        libraries: "aac_initialized_pcm_reference",
        packages: CONTAINER_PACKAGES,
        options: RELEASE_OPTIONS,
        instrumented: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-capture-static",
        binary: "ffmpeg-capture-worker",
        define: Some("-DTRIERGON_CAPTURE_REFERENCE=1"),
        record: "ffmpeg-capture-native",
        kind: "FFmpeg 9.0.2 capture component set (gating); RGBA conversion, MPEG-1 encode, MPEG-TS mux",
        libraries: "capture_reference",
        packages: CAPTURE_PACKAGES,
        options: RELEASE_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-capture-small-static",
        binary: "ffmpeg-capture-small-worker",
        define: Some("-DTRIERGON_CAPTURE_REFERENCE=1"),
        record: "ffmpeg-capture-small-native",
        kind: "FFmpeg 9.0.2 capture component set built with --enable-small; reported, not gating",
        libraries: "capture_small_reference",
        packages: CAPTURE_PACKAGES,
        options: RELEASE_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-isobmff-worker",
        source: "harness/reference_isobmff.c",
        record: "ffmpeg-isobmff-native",
        kind: "pinned FFmpeg MOV public-API open and index operations",
        libraries: "isobmff_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-framing-worker",
        source: "harness/reference_framing.c",
        record: "ffmpeg-framing-native",
        kind: "pinned FFmpeg AVC and HEVC public-API packet framing operations",
        libraries: "framing_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-framing-views",
        binary: "ffmpeg-framing-views-worker",
        source: "harness/reference_framing_views.c",
        record: "ffmpeg-framing-views-native",
        kind: "pinned FFmpeg framing public-API views",
        libraries: "framing_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-isobmff-views-worker",
        source: "harness/reference_isobmff_views.c",
        record: "ffmpeg-isobmff-views-native",
        kind: "pinned FFmpeg isobmff public-API views",
        libraries: "isobmff_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: "ffmpeg-codec-views-worker",
        source: "harness/reference_codec_views.c",
        record: "ffmpeg-codec-views-native",
        kind: "FFmpeg 9.0.2 codec public-API views",
        libraries: "codec_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference",
        binary: "ffmpeg-matroska-views-worker",
        source: "harness/reference_matroska_views.c",
        record: "ffmpeg-matroska-views-native",
        kind: "pinned FFmpeg Matroska public-API views",
        libraries: "matroska_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-subtitle-views-worker",
        source: "harness/reference_subtitle_views.c",
        record: "ffmpeg-subtitle-views-native",
        kind: "pinned FFmpeg subtitle public-API views",
        libraries: "subtitle_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-transport-views-worker",
        source: "harness/reference_transport_views.c",
        record: "ffmpeg-transport-views-native",
        kind: "pinned FFmpeg MPEG-TS public-API views",
        libraries: "transport_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: "ffmpeg-transport-views-raw-worker",
        source: "harness/reference_transport_views.c",
        record: "ffmpeg-transport-views-raw-native",
        kind: "FFmpeg 9.0.2 raw MPEG-TS packet views",
        libraries: "transport_views_raw_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-formats",
        binary: "ffmpeg-playback-views-worker",
        source: "harness/reference_playback_views.c",
        record: "ffmpeg-playback-views-native",
        kind: "pinned FFmpeg playback public-API views",
        libraries: "playback_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: "ffmpeg-bridge-views-worker",
        source: "harness/reference_playback_views.c",
        record: "ffmpeg-bridge-views-native",
        kind: "FFmpeg 9.0.2 bitstream filter and muxer views",
        libraries: "bridge_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-subtitles",
        binary: "ffmpeg-playback-subtitle-views-worker",
        source: "harness/reference_playback_views.c",
        define: Some("-DTRIERGON_PLAYBACK_SUBTITLE_WRAP=1"),
        record: "ffmpeg-playback-subtitle-views-native",
        kind: "pinned FFmpeg subtitle playback views with host-dependent conversion detection",
        libraries: "playback_subtitle_views_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        wrapped: SUBTITLE_WRAPS,
        identity: true,
        instrumented: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-subtitles",
        binary: "ffmpeg-playback-subtitle-views-plain-worker",
        source: "harness/reference_playback_views.c",
        record: "ffmpeg-playback-subtitle-views-plain-native",
        kind: "pinned FFmpeg subtitle playback views for timing and memory",
        libraries: "playback_subtitle_views_plain_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/reference-decode",
        binary: "ffmpeg-play-views-worker",
        source: "harness/reference_play_views.c",
        record: "ffmpeg-play-views-native",
        kind: "FFmpeg 9.0.2 demux, decode and presentation output",
        libraries: "play_views_reference",
        packages: DECODE_PACKAGES,
        options: CONTAINER_OPTIONS,
        identity: true,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/freetype",
        binary: "freetype-font-worker",
        define: Some("-DTRIERGON_FONT_REFERENCE=1"),
        record: "freetype-native",
        kind: "FreeType 2.15.0, static, without optional libraries; face tables, cmaps, kerning and unhinted outlines",
        libraries: "freetype_reference",
        packages: &["freetype2"],
        options: RELEASE_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        profile: ".research/harfbuzz",
        linked: &[".research/freetype"],
        binary: "harfbuzz-shape-worker",
        define: Some("-DTRIERGON_SHAPE_REFERENCE=1"),
        record: "harfbuzz-native",
        kind: "HarfBuzz 14.6.0 from its amalgamation and FriBidi 1.0.17, static, without floating-point contraction, with FreeType 2.15.0 for libass's font functions; OpenType shaping, bidi and Unicode properties",
        libraries: "harfbuzz_reference",
        packages: &["harfbuzz", "fribidi", "freetype2"],
        options: SHAPE_OPTIONS,
        ..REFERENCE
    },
    ProfileReference {
        binary: "libass-raster-worker",
        define: Some("-DTRIERGON_RASTER_REFERENCE=1"),
        record: "libass-native",
        kind: "libass 0.17.5, static, against the FreeType, FriBidi and HarfBuzz references, without floating-point contraction, with uninitialised automatic variables zeroed; its outline, stroker, rasteriser, blur, composition and clipping stages on the plain C bitmap engine (c: tile order 4, align order 4) on every host, and on the host's own engine for raster-simd- operations",
        libraries: "libass_reference",
        ..LIBASS
    },
    ProfileReference {
        binary: "libass-ass-worker",
        define: Some("-DTRIERGON_ASS_REFERENCE=1"),
        record: "libass-ass-native",
        kind: "libass 0.17.5, the build of the rasterising reference; its script, Matroska packet, override tag, drawing and karaoke parsing",
        libraries: "libass_ass_reference",
        generated: Some(crate::ass::observed_sources),
        instrumented: true,
        ..LIBASS
    },
    ProfileReference {
        binary: "libass-player-worker",
        define: Some("-DTRIERGON_ASS_PLAYER_REFERENCE=1"),
        record: "libass-player-native",
        kind: "libass 0.17.5, the build of the rasterising reference linked as a player links it, none of its sources compiled into the worker; frame rendering through its public API on the plain C bitmap engine, the measurement reference of the ASS rendering features",
        libraries: "libass_player_reference",
        ..LIBASS
    },
];

/// Why a contract's `measurement_reference` cannot stand in for its
/// `reference`, if it cannot: it must be a registered build that is not
/// instrumented, linked against the same pinned profiles as an instrumented
/// `reference`.
pub fn measurement_reference_error(contract: &Value) -> Option<String> {
    let measurement = contract.get("measurement_reference")?;
    let registered = |value: Option<&Value>| {
        let name = value.and_then(Value::as_str)?;
        PROFILE_REFERENCES.iter().find(|r| r.record == name)
    };
    let Some(measured) = registered(Some(measurement)) else {
        return Some("measurement_reference must name a registered reference build".into());
    };
    let Some(reference) = registered(contract.get("reference")) else {
        return Some("measurement_reference needs a registered reference build".into());
    };
    if measured.instrumented {
        Some(format!(
            "measurement_reference {} is instrumented",
            measured.record
        ))
    } else if !reference.instrumented {
        Some(format!(
            "measurement_reference stands in only for an instrumented reference, not {}",
            reference.record
        ))
    } else if (measured.profile, measured.linked) != (reference.profile, reference.linked) {
        Some(format!(
            "measurement_reference {} must link the pinned builds of {}",
            measured.record, reference.record
        ))
    } else {
        None
    }
}

/// Builds the workers and references. A profile reference is built only when every
/// profile it links is prepared, and `comparison_references` in `identity.json` lists
/// those built; the others are printed, and with `all_references` fail the build.
fn build_selected(
    root: &Path,
    comparison_build: bool,
    only: Option<&str>,
    all_references: bool,
) -> R<PathBuf> {
    let selected = selected_candidates(only)?;
    gate_build::admit(root, comparison_build)?;
    let linux = cfg!(target_os = "linux");
    let mut candidate_command = cargo_rustc(&MEDIA_WORKER, &root.join("target"), true)?;
    candidate_command.extend(gate_build::worker_link_arguments()?);
    // The views use the media worker only to share the report's build identity.
    // Its trained layout does not qualify a comparison-only build of these workers.
    let candidate_order = if comparison_build
        && matches!(
            only,
            Some(
                "framing-views-worker"
                    | "isobmff-views-worker"
                    | "codec-views-worker"
                    | "matroska-views-worker"
                    | "subtitle-views-worker"
                    | "transport-views-worker"
                    | "playback-views-worker"
                    | "play-views-worker"
                    | "codec-hevc-worker"
            )
        ) {
        run(&candidate_command)?;
        None
    } else {
        build_in_link_order(
            root,
            &mut candidate_command,
            &root.join(gate_build::release_dir()?).join("media-worker"),
            "media-worker",
        )?
    };
    if selected.iter().any(|spec| spec.nightly) {
        crate::simd_toolchain()?;
    }
    let additional = additional_candidates(root, &selected)?;
    let out = root.join("target/harness");
    fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let pinned = root.join(".research/reference/build.json");
    let mut env = Vec::new();
    if pinned.exists() {
        env.push((
            "PKG_CONFIG_LIBDIR",
            root.join(".research/reference/install/lib/pkgconfig")
                .display()
                .to_string(),
        ));
    }
    let flags = output(
        "pkg-config",
        &[
            "--static",
            "--cflags",
            "--libs",
            "libavformat",
            "libavcodec",
            "libavutil",
        ],
        &env,
    )?;
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let worker = out.join("ffmpeg-worker");
    let combined = out.join("ffmpeg-combined-worker");
    let mut command: Vec<String> = vec![
        compiler.clone(),
        "-O3".into(),
        "-Wall".into(),
        "-Wextra".into(),
        "-Werror".into(),
        root.join("harness/reference.c").display().to_string(),
    ];
    command.extend(gate_build::reference_link_flags(shell_words(&flags)?)?);
    command.extend(["-o".into(), worker.display().to_string()]);
    run(&command)?;
    let mut combined_command = vec![command[0].clone(), "-DTRIERGON_COMBINED_REFERENCE=1".into()];
    combined_command.extend(command[1..].iter().cloned());
    *combined_command.last_mut().expect("output path") = combined.display().to_string();
    run(&combined_command)?;
    // Operations that need another pinned profile get their own build of the same
    // source, made only when that profile exists, so the other references and
    // their identity records are unchanged.
    let mut profile_workers = Vec::new();
    let mut skipped = Vec::new();
    for reference in PROFILE_REFERENCES {
        let path = out.join(reference.binary);
        let build = root.join(reference.profile).join("build.json");
        let profiles =
            || std::iter::once(reference.profile).chain(reference.linked.iter().copied());
        let missing: Vec<&str> = profiles()
            .filter(|profile| !root.join(profile).join("build.json").exists())
            .collect();
        if !missing.is_empty() {
            let _ = fs::remove_file(&path);
            println!(
                "skipped {}: {} not prepared",
                reference.binary,
                missing.join(", ")
            );
            skipped.push(reference.binary);
            continue;
        }
        if let Some(prepared) = reference.prepared {
            prepared(&json::read(&build)?)?;
        }
        let pkgconfig = std::env::join_paths(
            profiles().map(|profile| root.join(profile).join("install/lib/pkgconfig")),
        )
        .map_err(|e| e.to_string())?;
        // Every reference links its pinned libraries statically.
        let mut query = vec!["--static", "--cflags", "--libs"];
        query.extend(reference.packages);
        let flags = output(
            "pkg-config",
            &query,
            &[(
                "PKG_CONFIG_LIBDIR",
                pkgconfig.to_string_lossy().into_owned(),
            )],
        )?;
        let mut command: Vec<String> = vec![compiler.clone()];
        command.extend(reference.options.iter().map(ToString::to_string));
        if let Some(define) = reference.define {
            command.push(define.into());
        }
        command.extend(
            reference
                .wrapped
                .iter()
                .map(|name| format!("-Wl,--wrap={name}")),
        );
        if let Some(generate) = reference.generated {
            // A system directory, as the pinned tree's own is: libass's code is
            // compiled with its own warnings.
            command.push("-isystem".into());
            command.push(generate(root)?.display().to_string());
        }
        command.push(root.join(reference.source).display().to_string());
        command.extend(gate_build::reference_link_flags(shell_words(&flags)?)?);
        command.extend(["-o".into(), path.display().to_string()]);
        run(&command)?;
        profile_workers.push((reference, path, build, command));
    }
    if all_references && !skipped.is_empty() {
        return Err(format!(
            "--all-references: {} reference(s) skipped: {}",
            skipped.len(),
            skipped.join(", ")
        ));
    }
    let commands = instrumentation_commands(&compiler, root, &out, linux);
    for compile in &commands {
        run(compile)?;
    }
    let observer = out.join(if linux {
        "allocations.so"
    } else {
        "allocations.dylib"
    });
    let probe = out.join("memory-check");
    let mut instrumentation = vec![
        ("observer_sha256".to_owned(), Value::str(sha(&observer)?)),
        ("probe_sha256".to_owned(), Value::str(sha(&probe)?)),
    ];
    let candidate = root.join(gate_build::release_dir()?).join("media-worker");
    let mut libraries: Vec<(String, Value)> = Vec::new();
    if linux {
        for (name, path) in [
            ("candidate", &candidate),
            ("reference", &worker),
            ("combined_reference", &combined),
            ("probe", &probe),
        ] {
            libraries.push((name.into(), linux::linked_libraries(path, None)?));
        }
        let interpreter = libraries[3]
            .1
            .get("interpreter")
            .and_then(Value::as_str)
            .map(str::to_owned);
        libraries.push((
            "observer".into(),
            linux::linked_libraries(&observer, interpreter.as_deref())?,
        ));
        for (reference, path, _, _) in &profile_workers {
            libraries.push((
                reference.libraries.into(),
                linux::linked_libraries(path, None)?,
            ));
        }
        let of = |name: &str| {
            libraries
                .iter()
                .find(|(k, _)| k == name)
                .map_or(Value::Null, |(_, v)| v.clone())
        };
        let selected = Value::object([("observer", of("observer")), ("probe", of("probe"))]);
        let sources = Value::object(
            [
                "harness/allocations-linux.c",
                "harness/portable-memory-check.c",
            ]
            .map(|n| (n, Value::str(sha(&root.join(n)).unwrap_or_default()))),
        );
        instrumentation.extend([
            ("platform".into(), Value::str("Linux")),
            ("architecture".into(), Value::str(linux::machine())),
            ("schema".into(), Value::int(2)),
            (
                "observer_path".into(),
                Value::str("target/harness/allocations.so"),
            ),
            ("preload_variable".into(), Value::str("LD_PRELOAD")),
            (
                "linked_library_hashes".into(),
                linux::linked_hashes(&selected),
            ),
            (
                "binding_environment".into(),
                of("probe")
                    .get("binding_environment")
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
            (
                "compiler_commands".into(),
                Value::Array(commands.iter().map(|c| strings(c)).collect()),
            ),
            ("source_hashes".into(), sources),
        ]);
        if let Ok(proof_path) = std::env::var("TRIERGON_RSS_KERNEL_PROOF") {
            let proof_file = root.join(&proof_path);
            let profile = Value::object([
                ("proof_path", Value::str(&proof_path)),
                ("proof_sha256", Value::str(sha(&proof_file)?)),
                ("proof", json::read(&proof_file)?),
            ]);
            let (observer_sha, probe_sha) = (
                instrumentation[0].1.as_str().map(str::to_owned),
                instrumentation[1].1.as_str().map(str::to_owned),
            );
            if !linux::kernel_profile_current(
                root,
                &profile,
                observer_sha.as_deref(),
                probe_sha.as_deref(),
            ) {
                return Err("requested kernel RSS proof is unavailable or stale".into());
            }
            let bound = Value::object([
                ("proof_path", profile.get("proof_path").cloned().unwrap()),
                (
                    "proof_sha256",
                    profile.get("proof_sha256").cloned().unwrap(),
                ),
            ]);
            instrumentation.push(("rss_kernel_bound".into(), bound));
        }
    }
    let system_libraries = |name: &str, path: &Path| -> R<Value> {
        if linux {
            Ok(linux::library_identity(
                libraries
                    .iter()
                    .find(|(k, _)| k == name)
                    .map_or(&Value::Null, |(_, v)| v),
            ))
        } else {
            darwin_libraries(path)
        }
    };
    let worker_identity = |path: &Path| -> R<Value> {
        json::parse(&output(
            &path.display().to_string(),
            &["identity", "-"],
            &[],
        )?)
        .map_err(|e| e.to_string())
    };
    let Value::Object(mut identity) = worker_identity(&worker)? else {
        return Err("worker identity is not an object".into());
    };
    let metadata = cargo_metadata(root)?;
    let graph = selected_graph(root, &metadata, "media-worker", &shared_inputs(root)?)?;
    let candidates = candidate_records(root, &additional)?;
    let pinned_build = if pinned.exists() {
        Some(json::read(&pinned)?)
    } else {
        None
    };
    let files_of = |key: &str| -> Value {
        let mut map = Vec::new();
        if key == "source_hashes" {
            for package in graph
                .get("packages")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                if let Some(Value::Object(files)) = package.get("files") {
                    map.extend(files.iter().cloned());
                }
            }
        }
        Value::Object(map)
    };
    let archives = if pinned.exists() {
        let lib = root.join(".research/reference/install/lib");
        let mut entries: Vec<PathBuf> = fs::read_dir(&lib)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "a"))
            .collect();
        entries.sort();
        Value::Object(
            entries
                .iter()
                .map(|p| {
                    Ok((
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        Value::str(sha(p)?),
                    ))
                })
                .collect::<R<_>>()?,
        )
    } else {
        Value::Object(vec![])
    };
    let combined_record = Value::object([
        ("path", Value::str("target/harness/ffmpeg-combined-worker")),
        ("sha256", Value::str(sha(&combined)?)),
        (
            "kind",
            Value::str("pinned FFmpeg plus native scope-1 combined-encoding restoration"),
        ),
        ("compiler_command", strings(&combined_command)),
        ("worker_identity", worker_identity(&combined)?),
        (
            "system_libraries",
            system_libraries("combined_reference", &combined)?,
        ),
    ]);
    let mut profile_records = Vec::new();
    for (reference, path, build, command) in &profile_workers {
        profile_records.push((
            reference.record,
            Value::object([
                (
                    "path",
                    Value::str(format!("target/harness/{}", reference.binary)),
                ),
                ("sha256", Value::str(sha(path)?)),
                ("kind", Value::str(reference.kind)),
                ("instrumented", Value::Bool(reference.instrumented)),
                ("compiler_command", strings(command)),
                ("source", Value::str(reference.source)),
                (
                    "source_sha256",
                    Value::str(sha(&root.join(reference.source))?),
                ),
                ("reference_build", json::read(build)?),
                (
                    "linked_builds",
                    Value::Object(
                        reference
                            .linked
                            .iter()
                            .map(|profile| {
                                let build = root.join(profile).join("build.json");
                                Ok(((*profile).to_owned(), json::read(&build)?))
                            })
                            .collect::<R<_>>()?,
                    ),
                ),
                (
                    "worker_identity",
                    if reference.identity {
                        worker_identity(path)?
                    } else {
                        Value::Null
                    },
                ),
                (
                    "system_libraries",
                    system_libraries(reference.libraries, path)?,
                ),
            ]),
        ));
    }
    let ffmpeg = if pinned_build.is_some() {
        root.join(".research/reference/install/bin/ffmpeg")
    } else {
        PathBuf::from("ffmpeg")
    };
    let updates: Vec<(&str, Value)> = vec![
        ("schema", Value::int(2)),
        (
            "reference_kind",
            Value::str(if pinned.exists() {
                "pinned local source build"
            } else {
                "installed development reference; source build not yet qualified"
            }),
        ),
        ("candidate_command", strings(&candidate_command)),
        (
            "candidate_link_order",
            LinkOrder::record(candidate_order.as_ref()),
        ),
        ("selected_graph", graph.clone()),
        ("additional_candidates", candidates),
        (
            "source_revision",
            pinned_build
                .as_ref()
                .and_then(|b| b.get("revision").cloned())
                .unwrap_or(Value::Null),
        ),
        ("compiler_command", strings(&command)),
        (
            "comparison_rules",
            json::read(&root.join("harness/comparison-rules.json"))?,
        ),
        (
            "comparison_references",
            Value::object(
                [
                    (
                        "ffmpeg-native",
                        Value::object([
                            ("path", Value::str("target/harness/ffmpeg-worker")),
                            ("sha256", Value::str(sha(&worker)?)),
                            ("kind", Value::str("pinned native FFmpeg")),
                        ]),
                    ),
                    ("ffmpeg-combined-native", combined_record),
                ]
                .into_iter()
                .chain(profile_records)
                .collect::<Vec<_>>(),
            ),
        ),
        ("system_libraries", system_libraries("reference", &worker)?),
        ("instrumentation", Value::Object(instrumentation)),
        (
            "compiler",
            Value::str(output(&compiler, &["--version"], &[])?),
        ),
        (
            "ffmpeg_version",
            Value::str(output(&ffmpeg.to_string_lossy(), &["-version"], &[])?),
        ),
        (
            "binaries",
            Value::object([
                ("candidate", Value::str(sha(&candidate)?)),
                ("reference", Value::str(sha(&worker)?)),
            ]),
        ),
        ("source_hashes", files_of("source_hashes")),
        (
            "build_input_hashes",
            graph.get("shared_files").cloned().unwrap_or(Value::Null),
        ),
        (
            "workspace_provenance",
            Value::object([
                (
                    "manifest_sha256",
                    Value::str(sha(&root.join("Cargo.toml"))?),
                ),
                ("lock_sha256", Value::str(sha(&root.join("Cargo.lock"))?)),
            ]),
        ),
        ("reference_archives", archives),
        ("reference_build", pinned_build.unwrap_or(Value::Null)),
        (
            "reference_source_sha256",
            Value::str(sha(&root.join("harness/reference.c"))?),
        ),
        ("lock_sha256", Value::str(sha(&root.join("Cargo.lock"))?)),
        (
            "ffprobe_sha256",
            which("ffprobe").map_or(Ok(Value::Null), |p| sha(&p).map(Value::str))?,
        ),
    ];
    for (key, value) in updates {
        match identity.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => identity.push((key.to_owned(), value)),
        }
    }
    if linux {
        let getconf = |name: &str| {
            output("getconf", &[name], &[])
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .map_or(Value::Null, Value::int)
        };
        let tool_hashes = Value::object(["nm", "readelf", "ps"].map(|n| {
            (
                n,
                which(n).map_or(Value::Null, |p| sha(&p).map_or(Value::Null, Value::str)),
            )
        }));
        let platform = Value::object([
            ("system", Value::str("Linux")),
            ("machine", Value::str(linux::machine())),
            ("kernel_release", Value::str(linux::kernel_release())),
            (
                "kernel_version",
                Value::str(output("uname", &["-v"], &[])?.trim()),
            ),
            (
                "libc",
                Value::strings(
                    output("getconf", &["GNU_LIBC_VERSION"], &[])?
                        .split_whitespace()
                        .map(str::to_owned),
                ),
            ),
            ("page_size", getconf("PAGESIZE")),
            ("clock_ticks", getconf("CLK_TCK")),
            ("nm_version", Value::str(output("nm", &["--version"], &[])?)),
            (
                "readelf_version",
                Value::str(output("readelf", &["--version"], &[])?),
            ),
            ("ps_version", Value::str(output("ps", &["--version"], &[])?)),
            ("tool_hashes", tool_hashes),
        ]);
        identity.push(("host_platform".into(), platform));
        identity.push((
            "linked_libraries".into(),
            Value::Object(
                libraries
                    .iter()
                    .map(|(k, v)| (k.clone(), linux::library_identity(v)))
                    .collect(),
            ),
        ));
        identity.push((
            "linux_loader_observations_path".into(),
            Value::str("target/harness/linux-loader-observations.json"),
        ));
        // Every comparison reference binds the candidate and reference closures too.
        let bindings = Value::Object(
            libraries
                .iter()
                .filter(|(k, _)| {
                    matches!(k.as_str(), "candidate" | "reference" | "combined_reference")
                })
                .map(|(k, v)| (k.clone(), linux::library_identity(v)))
                .collect(),
        );
        if let Some((_, Value::Object(references))) = identity
            .iter_mut()
            .find(|(k, _)| k == "comparison_references")
        {
            for (_, record) in references.iter_mut() {
                if let Value::Object(fields) = record {
                    fields.push(("linked_library_bindings".into(), bindings.clone()));
                }
            }
        }
    }
    identity.extend(gate_build::current(root)?);
    let path = out.join("identity.json");
    json::write(&path, &Value::Object(identity))?;
    if linux {
        let observations = Value::object([
            ("identity_sha256", Value::str(sha(&path)?)),
            ("raw_loader_observations", Value::Object(libraries)),
        ]);
        json::write(&out.join("linux-loader-observations.json"), &observations)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests;
