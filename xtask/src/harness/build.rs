//! `cargo xtask build-workers`: build the candidate worker, the FFmpeg reference
//! workers and the memory instrumentation, and record the build identity every
//! collection is later checked against (`target/harness/identity.json`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::gate_build;
use super::linux;
use super::provenance::{cargo_metadata, selected_graph, shared_inputs};
use crate::checks::read_json;
use crate::corpus::write_json;
use crate::json::Value;

type R<T> = Result<T, String>;

const USAGE: &str = "usage: cargo xtask build-workers [--comparison-build] [--only WORKER]";

fn options(args: &[String]) -> R<(bool, Option<&str>)> {
    let mut comparison_build = false;
    let mut only = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--comparison-build" if !comparison_build => comparison_build = true,
            "--only" if only.is_none() => {
                only = Some(args.next().ok_or(USAGE)?.as_str());
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok((comparison_build, only))
}

pub fn main(root: &Path, args: &[String]) -> R<PathBuf> {
    let (comparison_build, only) = options(args)?;
    match only {
        None => build(root, comparison_build),
        Some(name) => build_selected(root, comparison_build, Some(name)),
    }
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

/// Measurement workers a contract may name as its candidate besides
/// `media-worker`, with the command that builds each.
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    name: &'static str,
    package: &'static str,
    example: Option<&'static str>,
    features: &'static [&'static str],
}

const ADDITIONAL_CANDIDATES: [Candidate; 12] = [
    Candidate {
        name: "playback-worker",
        package: "playback-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "transport-worker",
        package: "transport-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "codec-worker",
        package: "codec-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "capture-worker",
        package: "capture-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "font-worker",
        package: "font-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "shape-worker",
        package: "shape-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "raster-worker",
        package: "raster-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "ass-worker",
        package: "ass-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "subtitle-worker",
        package: "subtitle-worker",
        example: None,
        features: &[],
    },
    // A second binary of the subtitle-worker package.
    Candidate {
        name: "subtitle-capsule-worker",
        package: "subtitle-worker",
        example: None,
        features: &[],
    },
    Candidate {
        name: "isobmff-worker",
        package: "triergon-isobmff",
        example: Some("isobmff_consumer_worker"),
        features: &["reference-trace"],
    },
    Candidate {
        name: "framing-worker",
        package: "triergon-framing",
        example: Some("framing_worker"),
        features: &["reference-trace"],
    },
];

pub(super) fn candidate_graph(root: &Path, name: &str) -> R<Value> {
    let spec = ADDITIONAL_CANDIDATES
        .iter()
        .find(|spec| spec.name == name)
        .ok_or_else(|| format!("unknown measurement worker {name}"))?;
    let features: Vec<String> = spec
        .features
        .iter()
        .map(|feature| format!("{}/{feature}", spec.package))
        .collect();
    let metadata = super::provenance::cargo_metadata_with_features(root, &features)?;
    let mut inputs = shared_inputs(root)?;
    if name == "isobmff-worker" {
        inputs.extend(crate::isobmff_bench::source_inputs(root)?);
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
        Some(name) => ADDITIONAL_CANDIDATES
            .iter()
            .find(|spec| spec.name == name)
            .map(|spec| vec![spec])
            .ok_or_else(|| format!("unknown measurement worker {name}")),
    }
}

fn additional_candidates(
    root: &Path,
    selected: &[&Candidate],
) -> R<Vec<(&'static str, Vec<String>)>> {
    let mut built = Vec::new();
    for spec in selected {
        let mut command: Vec<String> = [
            "cargo",
            "build",
            "-p",
            spec.package,
            "--release",
            "--locked",
        ]
        .map(String::from)
        .to_vec();
        command.extend([
            "--target-dir".into(),
            root.join("target").display().to_string(),
        ]);
        command.extend(gate_build::target_args()?);
        if let Some(example) = spec.example {
            command.extend(["--example".into(), example.into()]);
        }
        if !spec.features.is_empty() {
            command.extend(["--features".into(), spec.features.join(",")]);
        }
        let status = Command::new("cargo")
            .args(&command[1..])
            .env("CARGO_INCREMENTAL", "0")
            .current_dir(root)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("{} build failed: {status}", spec.name));
        }
        if let Some(example) = spec.example {
            fs::copy(
                root.join(gate_build::release_dir()?)
                    .join("examples")
                    .join(example),
                root.join(gate_build::release_dir()?).join(spec.name),
            )
            .map_err(|error| format!("{}: {error}", spec.name))?;
        }
        built.push((spec.name, command));
    }
    Ok(built)
}

fn candidate_records(root: &Path, built: &[(&str, Vec<String>)]) -> R<Value> {
    let mut candidates = Vec::new();
    for (name, command) in built {
        let path = format!("{}/{name}", gate_build::release_dir()?);
        candidates.push((
            *name,
            Value::object([
                ("path", Value::str(&path)),
                ("sha256", Value::str(sha(&root.join(&path))?)),
                ("command", strings(command)),
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

const PROFILE_REFERENCES: [ProfileReference; 14] = [
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
        profile: ".research/reference-subtitles",
        binary: "ffmpeg-subtitles-worker",
        define: Some("-DTRIERGON_SUBTITLE_REFERENCE=1"),
        record: "ffmpeg-subtitles-native",
        kind: "pinned FFmpeg subtitle decoders with libzvbi and the video decoders closed captions need; subtitle decode operations",
        libraries: "subtitles_reference",
        packages: CONTAINER_PACKAGES,
        options: CONTAINER_OPTIONS,
        // The calls whose result can differ between the targets, so that the
        // worker flags the folds that reach such a result
        // (`harness/reference_subtitles_host.h`).
        wrapped: &[
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
        ],
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
        packages: &[
            "libavformat",
            "libavfilter",
            "libswscale",
            "libswresample",
            "libavcodec",
            "libavutil",
        ],
        options: RELEASE_OPTIONS,
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

pub fn build(root: &Path, comparison_build: bool) -> R<PathBuf> {
    build_selected(root, comparison_build, None)
}

fn build_selected(root: &Path, comparison_build: bool, only: Option<&str>) -> R<PathBuf> {
    let selected = selected_candidates(only)?;
    gate_build::admit(root, comparison_build)?;
    let linux = cfg!(target_os = "linux");
    let mut candidate_command: Vec<String> = [
        "cargo",
        "build",
        "-p",
        "media-worker",
        "--release",
        "--locked",
    ]
    .map(String::from)
    .to_vec();
    candidate_command.extend([
        "--target-dir".into(),
        root.join("target").display().to_string(),
    ]);
    candidate_command.extend(gate_build::target_args()?);
    let status = Command::new("cargo")
        .args(&candidate_command[1..])
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(root)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("candidate build failed: {status}"));
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
    command.extend(shell_words(&flags)?);
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
    for reference in PROFILE_REFERENCES {
        let path = out.join(reference.binary);
        let build = root.join(reference.profile).join("build.json");
        let profiles =
            || std::iter::once(reference.profile).chain(reference.linked.iter().copied());
        if !profiles().all(|profile| root.join(profile).join("build.json").exists()) {
            let _ = fs::remove_file(&path);
            continue;
        }
        if let Some(prepared) = reference.prepared {
            prepared(&read_json(&build)?)?;
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
        command.extend(shell_words(&flags)?);
        command.extend(["-o".into(), path.display().to_string()]);
        run(&command)?;
        profile_workers.push((reference, path, build, command));
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
                ("proof", read_json(&proof_file)?),
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
        crate::json::parse(&output(
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
        Some(read_json(&pinned)?)
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
                ("reference_build", read_json(build)?),
                (
                    "linked_builds",
                    Value::Object(
                        reference
                            .linked
                            .iter()
                            .map(|profile| {
                                let build = root.join(profile).join("build.json");
                                Ok(((*profile).to_owned(), read_json(&build)?))
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
            read_json(&root.join("harness/comparison-rules.json"))?,
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
    write_json(&path, &Value::Object(identity))?;
    if linux {
        let observations = Value::object([
            ("identity_sha256", Value::str(sha(&path)?)),
            ("raw_loader_observations", Value::Object(libraries)),
        ]);
        write_json(&out.join("linux-loader-observations.json"), &observations)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn only_an_uninstrumented_build_of_the_same_libraries_measures_for_a_reference() {
        let contract = |reference: &str, measurement: &str| {
            crate::json::parse(&format!(
                r#"{{"reference":"{reference}","measurement_reference":"{measurement}"}}"#
            ))
            .unwrap()
        };
        let error =
            |reference, measurement| measurement_reference_error(&contract(reference, measurement));
        assert_eq!(error("libass-ass-native", "libass-player-native"), None);
        assert_eq!(
            error("ffmpeg-subtitles-native", "ffmpeg-subtitles-plain-native"),
            None
        );
        assert!(error("ffmpeg-subtitles-plain-native", "ffmpeg-subtitles-native").is_some());
        assert!(error("libass-ass-native", "libass-ass-native").is_some());
        assert!(error("libass-native", "libass-player-native").is_some());
        assert!(error("libass-ass-native", "freetype-native").is_some());
        assert!(error("libass-ass-native", "unbuilt").is_some());
        let plain = crate::json::parse(r#"{"reference":"libass-ass-native"}"#).unwrap();
        assert_eq!(measurement_reference_error(&plain), None);
    }

    #[test]
    fn worker_options_preserve_defaults_and_accept_either_flag_order() {
        assert_eq!(options(&[]).unwrap(), (false, None));
        assert_eq!(
            options(&args(&["--comparison-build"])).unwrap(),
            (true, None)
        );
        for name in std::iter::once("media-worker")
            .chain(ADDITIONAL_CANDIDATES.iter().map(|spec| spec.name))
        {
            assert_eq!(
                options(&args(&["--only", name])).unwrap(),
                (false, Some(name))
            );
            for arguments in [
                args(&["--only", name, "--comparison-build"]),
                args(&["--comparison-build", "--only", name]),
            ] {
                assert_eq!(options(&arguments).unwrap(), (true, Some(name)));
            }
        }
    }

    #[test]
    fn worker_selection_uses_the_registry_and_keeps_default_order() {
        let all = selected_candidates(None).unwrap();
        assert_eq!(all.len(), ADDITIONAL_CANDIDATES.len());
        for (actual, registered) in all.iter().zip(&ADDITIONAL_CANDIDATES) {
            assert_eq!(*actual, registered);
            let selected = selected_candidates(Some(registered.name)).unwrap();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0], registered);
        }
        assert!(
            selected_candidates(Some("media-worker"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_worker_options_are_refused_before_build_admission() {
        let root = Path::new("/nonexistent-worker-build-root");
        for arguments in [
            args(&["--only"]),
            args(&["--only", "framing-worker", "--only", "framing-worker"]),
            args(&["--only", "framing-worker", "--only", "codec-worker"]),
            args(&["--comparison-build", "--comparison-build"]),
            args(&["--unknown"]),
            args(&["framing-worker"]),
        ] {
            assert_eq!(main(root, &arguments).unwrap_err(), USAGE);
        }
        for name in ["unknown-worker", "--comparison-build", ""] {
            let error = main(root, &args(&["--only", name])).unwrap_err();
            assert_eq!(error, format!("unknown measurement worker {name}"));
        }
    }

    #[test]
    fn selected_workers_still_require_the_gate_build() {
        let root = std::env::temp_dir().join(format!("worker-build-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.99.1\"\n",
        )
        .unwrap();
        fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cfg=worker_selection_guard\"]\n",
        )
        .unwrap();
        for arguments in [
            Vec::new(),
            args(&["--only", "media-worker"]),
            args(&["--only", "framing-worker"]),
        ] {
            let error = main(&root, &arguments).unwrap_err();
            assert!(error.contains("not the gate build"), "{error}");
            assert!(error.contains("build.rustflags"), "{error}");
            assert!(!root.join("target").exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn primary_only_identity_does_not_adopt_stale_additional_binaries() {
        let root = std::env::temp_dir().join(format!("worker-build-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let release = root.join(gate_build::release_dir().unwrap());
        fs::create_dir_all(&release).unwrap();
        for spec in &ADDITIONAL_CANDIDATES {
            fs::write(release.join(spec.name), b"stale").unwrap();
        }
        let candidates = candidate_records(&root, &[]).unwrap();
        assert!(matches!(&candidates, Value::Object(records) if records.is_empty()));
        let identity = Value::object([("additional_candidates", candidates)]);
        for spec in &ADDITIONAL_CANDIDATES {
            let error = candidate_worker(&root, &identity, spec.name).unwrap_err();
            assert_eq!(
                error,
                format!("{} is not in the build identity; rebuild", spec.name)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isobmff_comparison_graph_enables_trace_without_changing_defaults() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let comparison = candidate_graph(root, "isobmff-worker").unwrap();
        let production = selected_graph(
            root,
            &cargo_metadata(root).unwrap(),
            "triergon-isobmff",
            &[],
        )
        .unwrap();
        for (graph, enabled) in [(&comparison, true), (&production, false)] {
            let package = graph
                .get("packages")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .find(|package| {
                    package.get("name").and_then(Value::as_str) == Some("triergon-isobmff")
                })
                .unwrap();
            let features = package.get("features").and_then(Value::as_array).unwrap();
            assert_eq!(
                features
                    .iter()
                    .any(|f| f.as_str() == Some("reference-trace")),
                enabled
            );
        }
    }

    #[test]
    fn splits_pkg_config_output_like_a_shell() {
        assert_eq!(
            shell_words("-I/x/include  -L'/a b/lib' -lz \"-DX=\\\"1\\\"\" a\\ b\n").unwrap(),
            ["-I/x/include", "-L/a b/lib", "-lz", "-DX=\"1\"", "a b"]
        );
        assert!(shell_words("'open").is_err());
    }

    #[test]
    fn instrumentation_commands_follow_the_platform() {
        let (root, out) = (Path::new("/r"), Path::new("/r/target/harness"));
        let darwin = instrumentation_commands("cc", root, out, false);
        assert!(
            darwin[0].contains(&"-dynamiclib".to_owned())
                && darwin[0].contains(&"/r/harness/allocations.c".to_owned())
        );
        assert!(darwin[1].contains(&"/r/harness/memory-check.c".to_owned()));
        let linux = instrumentation_commands("cc", root, out, true);
        assert!(
            linux[0].contains(&"-Wl,-z,now".to_owned())
                && linux[0].ends_with(&["-ldl".to_owned(), "-pthread".to_owned()])
        );
        assert!(linux[1].contains(&"/r/harness/portable-memory-check.c".to_owned()));
    }
}
