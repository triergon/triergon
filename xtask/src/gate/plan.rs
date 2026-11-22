//! The plan: every item of the gate and its steps, for a target and its workers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::json::{self, Value};

use super::{Item, Placement, R, Stage, Step, lanes, owned, xtask};

/// Relative scheduling costs for report features, in historical plan order.
/// A newly registered report has a weight of one until it has a measured cost.
const REPORT_WEIGHTS: &[(&str, u32)] = &[
    ("mp4-forward", 2),
    ("mp4-range", 4),
    ("mpegts-access-units", 6),
    ("subtitle-pgs-capsules", 9),
    ("ts-program", 2),
    ("matroska-audio-clocks", 1),
    ("matroska-legacy-aac", 1),
    ("raw-annex-b", 1),
    ("mpegts-pes", 3),
    ("hls-segments", 5),
    ("matroska-startup-seek", 3),
    ("matroska-colour", 3),
    ("matroska-cues-preroll", 1),
    ("matroska-demux", 1),
    ("quicktime-pcm-samples", 1),
    ("mov-pcm-packets", 1),
    ("mov-run-recovery", 1),
    ("isobmff-dts-signalling", 1),
    ("mov-encoded-views", 1),
    ("matroska-header-removal", 1),
    ("matroska-segment-metadata", 5),
    ("matroska-zlib", 1),
    ("ass-render", 22),
    ("ass-render-host", 1),
    ("ass-render-settings", 1),
    ("subtitle-text", 16),
    ("subtitle-text-host", 2),
    ("subtitle-pgs", 18),
    ("subtitle-dvb", 13),
    ("subtitle-dvd", 9),
    ("subtitle-dvd-host", 1),
    ("subtitle-cc", 10),
    ("subtitle-teletext", 5),
    ("dts-core-frames", 1),
    ("dts-packed-frames", 1),
    ("dts-parser-trace", 1),
];

/// How the gate covers every other registered feature (`harness/features/ID`):
/// through the command that measures or compares it, or why it is not gated.
/// `gate plan` and `gate summary` fail on a registered feature that is neither here
/// nor registered with the report runner.
pub(super) const COVERED: &[(&str, Coverage)] = &[
    ("pgs-segments", Coverage::Item("oracle-compare")),
    ("text-extraction-oracle", Coverage::Item("oracle-compare")),
    ("tx3g-config-oracle", Coverage::Item("oracle-compare")),
    ("tx3g-semantics", Coverage::Item("oracle-compare")),
    ("ts-video-descriptors", Coverage::Item("oracle-compare")),
    ("ts-packet-fields", Coverage::Item("oracle-compare")),
    ("ts-bounded-time-oracle", Coverage::Item("oracle-compare")),
    ("dts-core-header", Coverage::Item("oracle-compare")),
    ("av1-obu-wire", Coverage::Item("oracle-compare")),
    ("bridge-av1-low-overhead", Coverage::Item("oracle-compare")),
    ("aac-adts-header", Coverage::Item("oracle-compare")),
    ("hevc-hvcc-prefix", Coverage::Item("oracle-compare")),
    ("vp9-config", Coverage::Item("oracle-compare")),
    ("eac3-bsi", Coverage::Item("oracle-compare")),
    ("dts-unpacking", Coverage::Item("oracle-compare")),
    (
        "isobmff-open-index",
        Coverage::Item("isobmff-bench, in parts"),
    ),
    ("isobmff-reader", Coverage::Item("codecs isobmff-reader")),
    ("isobmff-seek", Coverage::Item("isobmff-seek")),
    (
        "quicktime-sound-description",
        Coverage::Item("oracle-compare"),
    ),
    ("isobmff-dts-entry-source", Coverage::Item("oracle-compare")),
    ("isobmff-segment-indexes", Coverage::Item("oracle-compare")),
    ("mp4-source-clock", Coverage::Item("oracle-compare")),
    ("packet-framing", Coverage::Item("framing")),
    ("progressive-ts188", Coverage::Item("progressive")),
    ("font-outlines", Coverage::Item("fonts compare")),
    ("font-hinting-none", Coverage::Item("fonts compare")),
    ("font-hinting-default", Coverage::Item("fonts compare")),
    ("mkv-codec-state", Coverage::Item("oracle-compare")),
    ("mkv-metadata-oracle", Coverage::Item("oracle-compare")),
    (
        "mkv-track-extensions-oracle",
        Coverage::Item("oracle-compare"),
    ),
    ("font-store", Coverage::Item("oracle-compare")),
    ("bit-readers", Coverage::Item("oracle-compare")),
    ("rbsp-unescape", Coverage::Item("oracle-compare")),
    ("core-budgets", Coverage::Item("oracle-compare")),
    ("ac3-features", Coverage::Item("oracle-compare")),
    ("elementary-metadata-wire", Coverage::Item("oracle-compare")),
    ("dsp-windows", Coverage::Item("oracle-compare")),
    ("dsp-divide", Coverage::Item("oracle-compare")),
    ("opus-final-range", Coverage::Item("oracle-compare")),
    ("dsp-imdct", Coverage::Item("codecs dsp")),
    ("shape-libass", Coverage::Item("shape compare")),
    ("shape-text", Coverage::Item("shape compare")),
    ("shape-bidi", Coverage::Item("shape compare")),
    ("shape-simple", Coverage::Item("shape compare")),
    ("shape-unicode", Coverage::Item("shape compare")),
    ("shape-extents", Coverage::Item("shape compare")),
    ("raster-glyphs", Coverage::Item("raster compare --all")),
    ("raster-outlines", Coverage::Item("raster compare --all")),
    ("raster-bitmaps", Coverage::Item("raster compare --all")),
    ("raster-sweep", Coverage::Item("raster compare --all")),
    ("ass-parse-scripts", Coverage::Item("ass compare")),
    ("ass-parse-chunks", Coverage::Item("ass compare")),
    ("ass-parse-events", Coverage::Item("ass compare")),
    ("ass-parse-host", Coverage::Item("ass compare")),
    (
        "font-hinting-modes",
        Coverage::Excluded("on demand: measured, not gated (fonts compare --all)"),
    ),
    (
        "shape-text-all",
        Coverage::Excluded("on demand: coverage at units per em (shape compare --all)"),
    ),
    (
        "raster-simd",
        Coverage::Excluded("diagnostic: the host's SIMD engine, not a reference build"),
    ),
    (
        "aac-access-units",
        Coverage::Item("codecs aac-access-units"),
    ),
];

/// How many registered features a command other than `report` gates, and how many
/// the gate leaves out.
pub fn dedicated_coverage() -> (usize, usize) {
    let excluded = COVERED
        .iter()
        .filter(|(_, coverage)| matches!(coverage, Coverage::Excluded(_)))
        .count();
    (COVERED.len() - excluded, excluded)
}

/// How a registered feature with a specialized runner or exclusion is covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Coverage {
    /// By the gate step that runs this command.
    Item(&'static str),
    /// Not by the gate, for this reason.
    Excluded(&'static str),
}

/// Settings the generic report runner reads from one feature contract.
pub(super) struct ReportFeature {
    oracle: bool,
    parity_only: Vec<&'static str>,
    pub(super) id: String,
    corpus: Option<String>,
    manifest_directory: Option<&'static str>,
    pub(super) weight: u32,
}

/// Use only registered runners. Scheduling an unknown contract as a generic
/// report could skip the controls a dedicated runner needs.
pub(super) fn report_features(root: &Path) -> R<Vec<ReportFeature>> {
    report_contracts(root, crate::harness::features::discover(root)?)
}

pub(super) fn report_contracts(
    root: &Path,
    contracts: Vec<crate::harness::features::Contract>,
) -> R<Vec<ReportFeature>> {
    let mut special: BTreeSet<_> = COVERED.iter().map(|(id, _)| *id).collect();
    let mut weights: BTreeMap<_, _> = REPORT_WEIGHTS
        .iter()
        .enumerate()
        .map(|(index, &(id, weight))| (id, (index, weight)))
        .collect();
    let mut reports = Vec::new();
    for contract in contracts {
        let specialized = special.remove(contract.id.as_str());
        let oracle = contract.document.get("oracle").is_some();
        if oracle {
            crate::harness::oracle_compare::kind(&contract.document)?;
            if !COVERED.contains(&(contract.id.as_str(), Coverage::Item("oracle-compare")))
                || crate::harness::features::gate(&contract.id, &contract.document)?
                    != Some(crate::harness::features::Gate::Required)
            {
                return Err(format!(
                    "{}: a required oracle comparison needs oracle-compare coverage",
                    contract.id
                ));
            }
            reports.push((
                usize::MAX,
                ReportFeature {
                    id: contract.id,
                    corpus: None,
                    manifest_directory: None,
                    weight: 1,
                    oracle: true,
                    parity_only: Vec::new(),
                },
            ));
            continue;
        }
        if contract.document.get("runner").and_then(Value::as_str) != Some("cargo xtask report") {
            if specialized {
                continue;
            }
            return Err(format!("{}: no gate runner or exclusion", contract.id));
        }
        if specialized {
            return Err(format!("{}: scheduled twice", contract.id));
        }
        let corpus = contract
            .document
            .get("corpus_directory")
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{}: invalid corpus_directory", contract.id))
            })
            .transpose()?;
        let manifest_directory = crate::generators::for_contract(&contract.document)
            .filter(|_| contract.generated())
            .map(|g| g.manifest_directory);
        let (position, weight) = weights
            .remove(contract.id.as_str())
            .unwrap_or((usize::MAX, 1));
        let mut parity_only = Vec::new();
        if contract.document.get("open_unmeasured").is_some() {
            let (manifest, _) = crate::harness::manifest::read(root, &contract.manifest())?;
            for target in super::TARGETS {
                let unmeasured = crate::harness::bounds::Unmeasured::read(
                    &contract.document,
                    &manifest,
                    target,
                )?;
                if unmeasured.all_skip(&manifest, "timing")
                    && unmeasured.all_skip(&manifest, "memory")
                {
                    parity_only.push(target);
                }
            }
        }
        reports.push((
            position,
            ReportFeature {
                id: contract.id,
                corpus,
                manifest_directory,
                weight,
                oracle: false,
                parity_only,
            },
        ));
    }
    if !special.is_empty() {
        return Err(format!(
            "gate runner has no registered feature: {special:?}"
        ));
    }
    if let Some((id, _)) = weights.first_key_value() {
        return Err(format!("{id}: planned, but not registered"));
    }
    reports.sort_by_key(|(order, _)| *order);
    Ok(reports.into_iter().map(|(_, report)| report).collect())
}
/// Each capture case's planning weight, as [`REPORT_WEIGHTS`] gives the features'; a case
/// not listed weighs [`CAPTURE_WEIGHT`].
const CAPTURE_WEIGHTS: [(&str, u32); 15] = [
    ("c01-ui-static-960", 10),
    ("c02-ui-scroll-960", 10),
    ("c03-ui-transition-960", 10),
    ("c04-motion-960", 13),
    ("c05-noise-960", 31),
    ("c06-ui-scroll-960-bottomup", 10),
    ("c07-ui-static-480-bottomup", 3),
    ("c08-ui-scroll-480-bottomup", 3),
    ("c09-motion-480-bottomup", 4),
    ("c10-ui-transition-480-topdown", 3),
    ("c11-motion-960-rate1", 13),
    ("c12-motion-960-1m", 14),
    ("c13-motion-960-6m", 14),
    ("c14-motion-960-rate255", 15),
    ("c15-noise-480-bottomup", 8),
];
const CAPTURE_WEIGHT: u32 = 10;

/// How many parts the ISO BMFF suite's measurements are split into.
pub(super) const ISOBMFF_PARTS: usize = 3;
/// The open and index contract qualifies this target.
pub(super) const ISOBMFF_GRADE_TARGET: &str = "aarch64-unknown-linux-gnu";
/// The ISO BMFF seek suite's planning weight, until a gate run measures it: its
/// memory from a collection of six cases on four CPUs, its timing from the worker
/// cycles of each operation with every pair run, and the ARMv7 share from the open
/// and index suite's.
const ISOBMFF_SEEK_WEIGHT: u32 = 14;
/// The capture cases, which the capture items split between them.
const CAPTURE_CASES: &str = "harness/capture/cases.json";
/// The command of every capture step, and so of their memory batch.
pub(super) const CAPTURE_QUALIFY: [&str; 3] = ["xtask", "capture", "qualify"];
/// The batch of the capture items' memory steps.
pub(super) const CAPTURE_MEMORY: &str = "memory-capture";
/// The decode reference's programs, the codecs' and capture's independent decoder.
const FFMPEG_BIN: &str = ".research/reference-decode/install/bin";
/// The formats reference's `ffprobe`, which the neutral ISO BMFF reader is compared with.
const FORMATS_FFPROBE: &str = ".research/reference-formats/install/bin/ffprobe";
/// The reader comparison's inputs and seek cases, and the lanes the gate compares.
const READER_MANIFEST: &str = "harness/codecs/reader-manifest.json";
const READER_SEEKS: &str = "harness/codecs/reader-seeks.json";
const MOV_MANIFEST: &str = "harness/codecs/aac/mov-manifest.json";
const CORRECTNESS: &str = crate::codecs::derived::MANIFEST;
const DECODE_REFERENCE: &str = "target/harness/ffmpeg-decode-worker";

pub(super) fn step(
    name: impl Into<String>,
    stage: Stage,
    placement: Placement,
    command: Vec<String>,
) -> Step {
    Step {
        name: name.into(),
        stage,
        placement,
        command,
        environment: Vec::new(),
        batch: None,
    }
}

/// A gate collection: timing on CPU 1, then memory under the kernel bound.
fn collections(name: &str, timing: Vec<String>, memory: Vec<String>) -> [Step; 2] {
    let mut memory = step(
        format!("memory-{name}"),
        Stage::Memory,
        Placement::Memory,
        memory,
    );
    memory.environment.push(("TRIERGON_RSS_KERNEL_BOUND", "1"));
    [
        step(
            format!("benchmark-{name}"),
            Stage::Timing,
            Placement::Timed,
            timing,
        ),
        memory,
    ]
}

/// The codec workload lists. Each list's timing is one item and its memory is in parts,
/// each part every Nth workload: a list's memory took up to 108 minutes in one piece.
const WORKLOAD_LISTS: [WorkloadList; 4] = [
    WorkloadList {
        name: "codec-workloads",
        workloads: "harness/codecs/workloads.json",
        aac: true,
        timing_weight: 20,
        part_weights: &[36, 12, 20, 18, 31],
    },
    WorkloadList {
        name: "codec-file-workloads",
        workloads: "harness/codecs/file-workloads.json",
        aac: false,
        timing_weight: 4,
        part_weights: &[16, 29, 20],
    },
    WorkloadList {
        name: "aac-default-workloads",
        workloads: "harness/codecs/aac-default-workloads.json",
        aac: false,
        timing_weight: 5,
        part_weights: &[12, 14],
    },
    WorkloadList {
        name: "hevc-simd-workloads",
        workloads: "harness/codecs/hevc-simd-workloads.json",
        aac: false,
        timing_weight: 6,
        part_weights: &[10, 10, 40],
    },
];

/// One codec workload list and the planning weights of its items.
struct WorkloadList {
    name: &'static str,
    workloads: &'static str,
    /// Whether it has AAC workloads, which are measured from the AAC worker.
    aac: bool,
    /// The timing item's weight in minutes.
    timing_weight: u32,
    /// Each memory part's weight in minutes, one per part.
    part_weights: &'static [u32],
}

/// Every item of the gate, writing under `out`, with `full` passed to the
/// commands that reuse a kept result. `target` is the gate target, `release`
/// the workers' directory and `capture` the capture case ids.
pub(super) fn items(
    reports: &[ReportFeature],
    out: &str,
    full: bool,
    target: &str,
    release: &str,
    capture: &[String],
) -> Vec<Item> {
    let plan = Plan {
        out,
        full,
        target,
        release,
        pairs: crate::harness::timing::GATE_PAIRS.to_string(),
        lanes: lanes::planned(),
    };
    let mut items: Vec<Item> = reports
        .iter()
        .filter(|feature| !feature.oracle)
        .map(|feature| plan.report(feature))
        .collect();
    items.push(plan.external_vector());
    items.extend((1..=ISOBMFF_PARTS).map(|part| plan.isobmff_part(part)));
    items.extend([
        plan.isobmff_seek(),
        plan.framing(),
        plan.progressive(),
        plan.isobmff_reader(),
        plan.video_codecs(),
        plan.audio_codecs(),
        plan.ogg_readers(),
        plan.file_codecs(),
        plan.elementary_readers(),
        plan.codec_dsp(),
        plan.aac_access_units(),
        plan.nal_workloads(),
    ]);
    if let Some(session) = plan.session_codecs() {
        items.push(session);
    }
    if let Some(play) = plan.play_codecs() {
        items.push(play);
    }
    for list in &WORKLOAD_LISTS {
        items.extend(plan.workloads(list));
    }
    items.extend(capture.iter().map(|id| plan.capture(id)));
    items.extend([
        plan.release_tests(),
        plan.tests(),
        plan.comparisons(reports),
    ]);
    items
}

/// What every item of one plan shares.
struct Plan<'a> {
    /// The directory every step writes under.
    out: &'a str,
    /// Whether the commands that reuse a kept result redo it.
    full: bool,
    target: &'a str,
    /// The workers' directory.
    release: &'a str,
    /// The report pairs of every collection.
    pairs: String,
    lanes: Value,
}

impl Plan<'_> {
    /// `args` with `--full` when the plan redoes what a kept set holds.
    fn full(&self, mut args: Vec<String>) -> Vec<String> {
        if self.full {
            args.push("--full".into());
        }
        args
    }

    /// `--target TARGET` when the workers are not built for the host.
    fn cross_target(&self) -> Vec<String> {
        if self.release.starts_with("target/release") {
            Vec::new()
        } else {
            owned(&["--target", self.target])
        }
    }

    fn codec_worker(&self) -> String {
        format!("{}/codec-worker", self.release)
    }

    /// The AAC decode operations are measured from the AAC decoder's nightly build.
    fn aac_worker(&self) -> String {
        format!("audio-aac={}/codec-aac-worker", self.release)
    }

    /// A grade of a collection pair; with `full` it grades even what a kept set holds.
    fn grade(&self, name: &str, contract: &str, manifest: &str) -> Step {
        let out = self.out;
        step(
            format!("grade-{name}"),
            Stage::Memory,
            Placement::Grade,
            self.full(xtask(&[
                "grade",
                &format!("{out}/benchmark/{name}/report.json"),
                &format!("{out}/memory/{name}/report.json"),
                "--contract",
                &format!("harness/features/{contract}/contract.json"),
                "--manifest",
                manifest,
                "--output",
                &format!("{out}/graded/{name}"),
            ])),
        )
    }

    /// [`collections`] and their [`grade`](Self::grade) against `contract`.
    fn graded(
        &self,
        name: &str,
        timing: Vec<String>,
        memory: Vec<String>,
        contract: &str,
        manifest: &str,
    ) -> Vec<Step> {
        let mut steps = Vec::from(collections(name, timing, memory));
        steps.push(self.grade(name, contract, manifest));
        steps
    }

    /// A feature the generic report runner measures and grades.
    fn report(&self, feature: &ReportFeature) -> Item {
        let id = feature.id.as_str();
        let corpus = feature.corpus.as_deref();
        let manifest_directory = feature.manifest_directory.unwrap_or("harness/features");
        let manifest = format!("{manifest_directory}/{id}/corpus.json");
        let report = |mode: Option<&str>| {
            let mut args = xtask(&[
                "report",
                "--contract",
                &format!("harness/features/{id}/contract.json"),
                "--manifest",
                &manifest,
            ]);
            if let Some(corpus) = corpus {
                args.extend(owned(&["--corpus", corpus]));
            }
            if let Some(mode) = mode {
                args.extend(owned(&[&format!("--{mode}"), "--pairs", &self.pairs]));
            }
            args.extend(owned(&[
                "--output",
                &format!("{}/{}/{id}", self.out, mode.unwrap_or("parity")),
            ]));
            if mode.is_some() {
                self.full(args)
            } else {
                args
            }
        };
        let mut preflight = owned(&["--feature", id]);
        if let Some(corpus) = corpus {
            preflight.extend(owned(&["--corpus", &format!("{id}={corpus}")]));
        }
        if feature.manifest_directory.is_some() {
            preflight.extend(owned(&["--manifest", &format!("{id}={manifest}")]));
        }
        let mut steps = if feature.parity_only.contains(&self.target) {
            vec![step(
                format!("{id}-compare"),
                Stage::Timing,
                Placement::Free,
                report(None),
            )]
        } else {
            self.graded(
                id,
                report(Some("benchmark")),
                report(Some("memory")),
                id,
                &manifest,
            )
        };
        if id == "facade-seek" {
            steps.push(step(
                "facade-container-seeks",
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "play-views",
                    "seek",
                    &format!("{}/play-views-worker", self.release),
                    &format!("{}/parity/facade-container-seeks.json", self.out),
                ]),
            ));
            preflight.extend(owned(&[
                "--feature",
                "matroska-cues-preroll",
                "--isobmff-seek",
            ]));
        }
        Item {
            name: id.to_owned(),
            weight: feature.weight,
            steps,
            preflight,
        }
    }

    /// The mandatory external vector, graded against the segment metadata contract.
    fn external_vector(&self) -> Item {
        let vector = |mode: &str| {
            self.full(xtask(&[
                "external-vector",
                &format!("--{mode}"),
                "--pairs",
                &self.pairs,
                "--output",
                &format!("{}/{mode}/external-vector", self.out),
            ]))
        };
        Item {
            name: "external-vector".into(),
            weight: 1,
            steps: self.graded(
                "external-vector",
                vector("benchmark"),
                vector("memory"),
                "matroska-segment-metadata",
                "harness/features/matroska-segment-metadata/corpus.json",
            ),
            preflight: owned(&["--external-vector"]),
        }
    }

    /// Part `part` of the ISO BMFF suite: every [`ISOBMFF_PARTS`]-th operation measured
    /// and all of them compared, so a part is a third of the measurements and the whole
    /// comparison. Graded on [`ISOBMFF_GRADE_TARGET`] only.
    fn isobmff_part(&self, part: usize) -> Item {
        let out = self.out;
        let name = format!("isobmff-{part}");
        let part = format!("{part}/{ISOBMFF_PARTS}");
        let graded = self.target == ISOBMFF_GRADE_TARGET;
        let bench = |mode: &str, flag: &str| {
            let mut command = xtask(&[
                "isobmff-bench",
                &format!("--{mode}"),
                flag,
                &self.pairs,
                "--part",
                &part,
                "--output",
                &format!("{out}/{mode}/{name}"),
            ]);
            if self.full || graded {
                command.push("--full".into());
            }
            command
        };
        let mut steps = Vec::from(collections(
            &name,
            bench("benchmark", "--pairs"),
            bench("memory", "--memory-pairs"),
        ));
        if graded {
            steps.push(step(
                format!("grade-{name}"),
                Stage::Memory,
                Placement::Grade,
                xtask(&[
                    "grade",
                    &format!("{out}/benchmark/{name}/collection.json"),
                    &format!("{out}/memory/{name}/collection.json"),
                    "--contract",
                    "harness/features/isobmff-open-index/contract.json",
                    "--manifest",
                    "harness/features/isobmff-open-index/corpus.json",
                    "--output",
                    &format!("{out}/graded/{name}"),
                    "--isobmff-part",
                    &part,
                    "--full",
                ]),
            ));
        }
        Item {
            name,
            weight: 9,
            steps,
            preflight: owned(&["--isobmff"]),
        }
    }

    fn isobmff_seek(&self) -> Item {
        let seek = |mode: &str, flag: &str| {
            self.full(xtask(&[
                "isobmff-seek",
                &format!("--{mode}"),
                flag,
                &self.pairs,
                "--output",
                &format!("{}/{mode}/isobmff-seek", self.out),
            ]))
        };
        Item {
            name: "isobmff-seek".into(),
            weight: ISOBMFF_SEEK_WEIGHT,
            steps: collections(
                "isobmff-seek",
                seek("benchmark", "--pairs"),
                seek("memory", "--memory-pairs"),
            )
            .into(),
            preflight: owned(&["--isobmff-seek"]),
        }
    }

    fn framing(&self) -> Item {
        let framing = |mode: &str| {
            self.full(xtask(&[
                "framing",
                &format!("--{mode}"),
                "--pairs",
                &self.pairs,
                "--no-early-stop",
                "--output",
                &format!("{}/{mode}/framing", self.out),
            ]))
        };
        Item {
            name: "framing".into(),
            weight: 4,
            steps: self.graded(
                "framing",
                framing("benchmark"),
                framing("memory"),
                "packet-framing",
                "harness/features/packet-framing/corpus.json",
            ),
            preflight: owned(&[
                "--feature",
                "packet-framing",
                "--corpus",
                "packet-framing=.research/fixtures-packet-framing",
            ]),
        }
    }

    fn progressive(&self) -> Item {
        let progressive = |mode: &str| {
            self.full(xtask(&[
                "progressive",
                &format!("--{mode}"),
                "--pairs",
                &self.pairs,
                "--output",
                &format!("{}/{mode}/progressive", self.out),
            ]))
        };
        Item {
            name: "progressive".into(),
            weight: 2,
            steps: collections(
                "progressive",
                progressive("benchmark"),
                progressive("memory"),
            )
            .into(),
            preflight: owned(&[
                "--feature",
                "progressive-ts188",
                "--corpus",
                &format!(
                    "progressive-ts188={}/inputs",
                    crate::progressive_bench::FEATURE
                ),
            ]),
        }
    }

    /// The neutral ISO BMFF reader against the formats reference's MOV demuxer.
    fn isobmff_reader(&self) -> Item {
        Item {
            name: "isobmff-reader".into(),
            weight: 3,
            steps: vec![step(
                "isobmff-reader-compare",
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "codecs",
                    "isobmff-reader",
                    "--ffprobe",
                    FORMATS_FFPROBE,
                    "--codec-worker",
                    &self.codec_worker(),
                    "--out",
                    &format!("{}/parity/isobmff-reader.jsonl", self.out),
                ]),
            )],
            preflight: owned(&["--isobmff-reader", "--reference", "ffmpeg-formats-native"]),
        }
    }

    fn codec_example(
        &self,
        bin: &str,
        example: &str,
        name: &str,
        package: &str,
        feature: Option<&str>,
        nightly: bool,
    ) -> Vec<String> {
        let mut cargo = vec!["cargo".to_owned()];
        if nightly {
            cargo.push(format!("+{}", crate::NIGHTLY));
        }
        cargo.extend(owned(&[
            "build",
            "--release",
            "--locked",
            "-p",
            package,
            "--example",
            example,
        ]));
        cargo.extend(self.cross_target());
        let release = if nightly {
            cargo.extend(owned(&["--target-dir", "target/nightly"]));
            self.release.replacen("target/", "target/nightly/", 1)
        } else {
            self.release.to_owned()
        };
        if let Some(feature) = feature {
            cargo.extend(owned(&["--features", feature]));
        }
        let mut command = owned(&[
            "sh",
            "-c",
            r#"dir=$1 from=$2 to=$3; shift 3; mkdir -p -- "$dir" && "$@" && cp -- "$from" "$to""#,
            "codec-example",
            bin,
            &format!("{release}/examples/{example}"),
            &format!("{bin}/{name}"),
        ]);
        command.extend(cargo);
        command
    }

    fn codec_grade(
        &self,
        name: &str,
        manifest: &str,
        grade_lanes: &str,
        bin: &str,
        options: &[&str],
    ) -> Step {
        let run = format!("{}/{name}.jsonl", self.out);
        let mut command = xtask(&[
            "codecs",
            "grade",
            "--manifest",
            manifest,
            "--lanes",
            grade_lanes,
            "--root",
            ".",
            "--bin",
            bin,
            "--ffmpeg",
            &format!("{FFMPEG_BIN}/ffmpeg"),
            "--ffprobe",
            &format!("{FFMPEG_BIN}/ffprobe"),
            "--codec-worker",
            &self.codec_worker(),
            "--reference",
            DECODE_REFERENCE,
            "--out",
            &run,
        ]);
        command.extend(owned(options));
        step(name, Stage::Timing, Placement::Free, command)
    }

    fn codec_summary(&self, name: &str, run: &str) -> Step {
        step(
            name,
            Stage::Timing,
            Placement::Free,
            xtask(&[
                "codecs",
                "summarize",
                &format!("{}/{run}.jsonl", self.out),
                &format!("{}/{name}.json", self.out),
                "--require-pass",
            ]),
        )
    }

    /// The H.264 and HEVC decoders against FFmpeg's on the conformance vectors.
    fn video_codecs(&self) -> Item {
        let (out, release) = (self.out, self.release);
        let bin = format!("{out}/codec-bin");
        let cargo_target = self.cross_target().join(" ");
        let mut build = owned(&["sh", "-c"]);
        build.push(format!(
            "mkdir -p {bin} && \
             cargo build --release --locked {cargo_target} -p triergon-codec-h264 --example decode_h264 && \
             cargo build --release --locked {cargo_target} -p triergon-codec-hevc --example decode_hevc && \
             cp {release}/examples/decode_h264 {bin}/codec-h264-decode && \
             cp {release}/examples/decode_hevc {bin}/codec-hevc-decode"
        ));
        let stable = lanes::item(&self.lanes, "codecs")
            .filter(|lane| *lane != "hevc-simd")
            .collect::<Vec<_>>()
            .join(",");
        let mut steps = vec![
            step("codec-bin", Stage::Timing, Placement::Free, build),
            self.codec_grade("codecs-grade", CORRECTNESS, &stable, &bin, &[]),
            self.codec_summary("codecs-summary", "codecs-grade"),
        ];
        steps.push(self.codec_grade(
            "codecs-grade-workloads",
            CORRECTNESS,
            &stable,
            &bin,
            &["--only", &lanes::video_workloads()],
        ));
        steps.push(self.codec_summary("codecs-summary-workloads", "codecs-grade-workloads"));

        let simd_bin = format!("{out}/codec-bin-simd");
        steps.push(step(
            "codec-bin-simd",
            Stage::Timing,
            Placement::Free,
            self.codec_example(
                &simd_bin,
                "decode_hevc",
                "codec-hevc-decode",
                "triergon-codec-hevc",
                Some("simd"),
                true,
            ),
        ));
        steps.push(self.codec_grade(
            "codecs-grade-hevc-simd",
            CORRECTNESS,
            "hevc",
            &simd_bin,
            &[
                "--codec-worker-for",
                &format!("video-hevc={release}/codec-hevc-worker"),
            ],
        ));
        steps.push(self.codec_summary("codecs-summary-hevc-simd", "codecs-grade-hevc-simd"));
        Item {
            name: "codecs".into(),
            weight: 33,
            steps,
            preflight: owned(&[
                "--codecs",
                "harness/codecs/workloads.json",
                "--correctness",
                &stable,
            ]),
        }
    }

    /// The six video workloads with a per-NAL feed and the existing RGBA comparison.
    fn nal_workloads(&self) -> Item {
        let bin = format!("{}/nal-workload-bin", self.out);
        let worker = format!("{}/codec-views-worker", self.release);
        let h264 = format!("video-h264={worker}");
        let hevc = format!("video-hevc={worker}");
        let selected = lanes::video_workload_names(include_str!(
            "../../../harness/codecs/nal-workloads.json"
        ));
        let grade = "codecs-grade-nal-workloads";
        let mut steps = Vec::new();
        for (codec, package, example, name) in [
            (
                "h264",
                "triergon-codec-h264",
                "decode_h264",
                "codec-h264-decode",
            ),
            (
                "hevc",
                "triergon-codec-hevc",
                "decode_hevc",
                "codec-hevc-decode",
            ),
        ] {
            steps.push(step(
                format!("nal-workloads-{codec}-bin"),
                Stage::Timing,
                Placement::Free,
                self.codec_example(&bin, example, name, package, None, false),
            ));
        }
        steps.push(self.codec_grade(
            grade,
            CORRECTNESS,
            "h264,hevc",
            &bin,
            &[
                "--only",
                &selected,
                "--codec-worker-for",
                &h264,
                "--codec-worker-for",
                &hevc,
            ],
        ));
        steps.push(self.codec_summary("codecs-summary-nal-workloads", grade));
        Item {
            name: "nal-workloads".into(),
            weight: 1,
            steps,
            preflight: owned(&["--codecs", "harness/codecs/nal-workloads.json"]),
        }
    }

    /// The AAC, AC-3, E-AC-3 and DTS decoders against FFmpeg's on every vector of their
    /// correctness lanes, the derived streams among them included; the summary fails on
    /// any vector that fails and is not a listed open case. The AAC decoder is built as
    /// `codec-aac-worker` builds it: by the pinned nightly with its `simd` feature, in the
    /// nightly workers' target directory; the AAC lane's output comes from that worker.
    fn audio_codecs(&self) -> Item {
        let (out, release) = (self.out, self.release);
        let audio_bin = format!("{out}/codec-audio-bin");
        let nightly_release = release.replacen("target/", "target/nightly/", 1);
        let nightly = crate::NIGHTLY;
        let cargo_target = self.cross_target().join(" ");
        let mut build = owned(&["sh", "-c"]);
        build.push(format!(
            "mkdir -p {audio_bin} && \
             cargo +{nightly} build --release --locked {cargo_target} --target-dir target/nightly \
               -p triergon-codec-aac --features simd --example decode_aac && \
             cargo build --release --locked {cargo_target} -p triergon-codec-ac3 --example decode_ac3 && \
             cargo build --release --locked {cargo_target} -p triergon-codec-dca --example decode_dts && \
             cp {nightly_release}/examples/decode_aac {audio_bin}/codec-aac-decode && \
             cp {release}/examples/decode_ac3 {audio_bin}/codec-ac3-decode && \
             cp {release}/examples/decode_dts {audio_bin}/codec-dca-decode"
        ));
        let audio_run = format!("{out}/codecs-audio-grade.jsonl");
        let (variants, standard): (Vec<_>, Vec<_>) = lanes::item(&self.lanes, "codecs-audio")
            .partition(|lane| crate::codecs::correctness::variant_manifest(lane).is_some());
        let audio = standard
            .into_iter()
            .filter(|lane| *lane != "aac-default")
            .collect::<Vec<_>>()
            .join(",");
        let default_bin = format!("{out}/codec-audio-bin-default");
        let mut item = Item {
            name: "codecs-audio".into(),
            weight: 1,
            steps: vec![
                step("codec-audio-bin", Stage::Timing, Placement::Free, build),
                step(
                    "codecs-audio-grade",
                    Stage::Timing,
                    Placement::Free,
                    xtask(&[
                        "codecs",
                        "grade",
                        "--manifest",
                        crate::codecs::derived::MANIFEST,
                        "--lanes",
                        &audio,
                        "--root",
                        ".",
                        "--bin",
                        &audio_bin,
                        "--ffmpeg",
                        &format!("{FFMPEG_BIN}/ffmpeg"),
                        "--ffprobe",
                        &format!("{FFMPEG_BIN}/ffprobe"),
                        "--codec-worker",
                        &self.codec_worker(),
                        "--codec-worker-for",
                        &self.aac_worker(),
                        "--reference",
                        "target/harness/ffmpeg-decode-worker",
                        "--out",
                        &audio_run,
                    ]),
                ),
                step(
                    "codecs-audio-summary",
                    Stage::Timing,
                    Placement::Free,
                    xtask(&[
                        "codecs",
                        "summarize",
                        &audio_run,
                        &format!("{out}/codecs-audio-summary.json"),
                        "--require-pass",
                    ]),
                ),
                step(
                    "codec-audio-bin-default",
                    Stage::Timing,
                    Placement::Free,
                    self.codec_example(
                        &default_bin,
                        "decode_aac",
                        "codec-aac-decode",
                        "triergon-codec-aac",
                        None,
                        false,
                    ),
                ),
                self.codec_grade(
                    "codecs-audio-grade-default",
                    CORRECTNESS,
                    "aac",
                    &default_bin,
                    &[],
                ),
                self.codec_summary("codecs-audio-summary-default", "codecs-audio-grade-default"),
            ],
            preflight: owned(&["--correctness", &audio]),
        };
        if !variants.is_empty() {
            let variants = variants.join(",");
            item.preflight.extend(owned(&["--correctness", &variants]));
            item.steps.extend([
                step(
                    "codec-strip-dither-bin",
                    Stage::Timing,
                    Placement::Free,
                    self.codec_example(
                        &audio_bin,
                        "strip_dither",
                        "strip_dither",
                        "triergon-codec-ac3",
                        Some("reference-trace"),
                        false,
                    ),
                ),
                self.codec_grade(
                    "codecs-audio-variants-grade",
                    CORRECTNESS,
                    &variants,
                    &audio_bin,
                    &[],
                ),
                self.codec_summary(
                    "codecs-audio-variants-summary",
                    "codecs-audio-variants-grade",
                ),
            ]);
        }
        item
    }

    /// The Ogg reader's packets and seeks against ffprobe, in both views; the step fails
    /// on a case that neither passes nor differs only by its reviewed differences.
    fn ogg_readers(&self) -> Item {
        self.reader_item("ogg-readers", 1)
    }

    fn elementary_readers(&self) -> Item {
        self.reader_item("elementary-readers", 2)
    }

    fn reader_item(&self, name: &str, weight: u32) -> Item {
        let selected = lanes::item(&self.lanes, name).collect::<Vec<_>>().join(",");
        Item {
            name: name.into(),
            weight,
            steps: vec![step(
                name,
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "codecs",
                    "readers",
                    "--manifest",
                    READER_MANIFEST,
                    "--seeks",
                    READER_SEEKS,
                    "--only",
                    &selected,
                    "--root",
                    ".",
                    "--ffprobe",
                    &format!("{FFMPEG_BIN}/ffprobe"),
                    "--codec-worker",
                    &self.codec_worker(),
                    "--views-worker",
                    &format!("{}/codec-views-worker", self.release),
                    "--out",
                    &format!("{}/{name}.jsonl", self.out),
                ]),
            )],
            preflight: owned(&["--readers", &selected]),
        }
    }

    /// Audio decoders reached through the file reader and track configuration.
    fn file_codecs(&self) -> Item {
        let bin = format!("{}/codec-files-bin", self.out);
        let (mov, files): (Vec<_>, Vec<_>) =
            lanes::item(&self.lanes, "codecs-files").partition(|lane| *lane == "aac-mov");
        let all = files
            .iter()
            .chain(&mov)
            .copied()
            .collect::<Vec<_>>()
            .join(",");
        Item {
            name: "codecs-files".into(),
            weight: 6,
            steps: vec![
                step(
                    "codec-files-bin",
                    Stage::Timing,
                    Placement::Free,
                    self.codec_example(
                        &bin,
                        "decode_audio",
                        "audio-decode",
                        "triergon-session",
                        Some("codecs"),
                        false,
                    ),
                ),
                self.codec_grade(
                    "codecs-files-grade",
                    CORRECTNESS,
                    &files.join(","),
                    &bin,
                    &[],
                ),
                self.codec_grade(
                    "codecs-files-mov-grade",
                    MOV_MANIFEST,
                    &mov.join(","),
                    &bin,
                    &[],
                ),
                self.codec_summary("codecs-files-summary", "codecs-files-grade"),
                self.codec_summary("codecs-files-mov-summary", "codecs-files-mov-grade"),
            ],
            preflight: owned(&["--correctness", &all]),
        }
    }

    /// Prepare and grade configured AAC packets before collecting either bound.
    fn aac_access_units(&self) -> Item {
        let out = self.out;
        let prepared = format!("{out}/aac-access-units");
        let aac = format!("{}/codec-aac-worker", self.release);
        let collect = |mode: &str, destination: &str| {
            let mut args = xtask(&[
                "codecs",
                mode,
                "--candidate",
                &aac,
                "--reference",
                DECODE_REFERENCE,
                "--workloads",
                &format!("{prepared}/workloads.json"),
                "--root",
                &prepared,
                "--out",
                &format!("{out}/{destination}/aac-access-units"),
                "--candidate-for",
                &format!("audio-aac-access-units={aac}"),
                "--candidate-for",
                &format!("audio-aac-access-units-compat={aac}"),
            ]);
            if mode == "time" {
                args.extend(owned(&[
                    "--ledger",
                    &format!("{out}/aac-access-units-ledger.jsonl"),
                    "--label",
                    self.target,
                ]));
            }
            self.full(args)
        };
        let mut steps = vec![step(
            "aac-access-units-parity",
            Stage::Timing,
            Placement::Free,
            xtask(&["codecs", "aac-access-units", "--out", &prepared]),
        )];
        steps.extend(collections(
            "aac-access-units",
            collect("time", "benchmark"),
            collect("memory", "memory"),
        ));
        Item {
            name: "aac-access-units".into(),
            weight: 5,
            steps,
            preflight: owned(&["--aac-access-units"]),
        }
    }

    /// Grade the session API and reset behaviour, using the same decoded outputs.
    fn session_codecs(&self) -> Option<Item> {
        let selected = lanes::item(&self.lanes, "codecs-sessions")
            .collect::<Vec<_>>()
            .join(",");
        if selected.is_empty() {
            return None;
        }
        let bin = format!("{}/codec-session-bin", self.out);
        Some(Item {
            name: "codecs-sessions".into(),
            weight: 20,
            preflight: owned(&[
                "--correctness",
                &selected,
                "--codecs",
                "harness/codecs/session-workloads.json",
            ]),
            steps: vec![
                step(
                    "codec-session-bin",
                    Stage::Timing,
                    Placement::Free,
                    self.codec_example(
                        &bin,
                        "session_decode",
                        "session_decode",
                        "triergon-session",
                        Some("codecs"),
                        false,
                    ),
                ),
                step(
                    "codec-track-reset-bin",
                    Stage::Timing,
                    Placement::Free,
                    self.codec_example(
                        &bin,
                        "decode_audio",
                        "audio-decode",
                        "triergon-session",
                        Some("codecs"),
                        false,
                    ),
                ),
                step(
                    "codec-session-reference-bin",
                    Stage::Timing,
                    Placement::Free,
                    owned(&[
                        "sh",
                        "-c",
                        r#"set -e; export PKG_CONFIG_LIBDIR="$PWD/.research/reference-decode/install/lib/pkgconfig"; "${CC:-cc}" -O2 -Wall -Wextra -Werror harness/reference_session.c $(pkg-config --static --cflags --libs libavformat libavcodec libavfilter libavutil libswscale libswresample) -o "$1""#,
                        "build-session-reference",
                        &format!("{bin}/reference-session"),
                    ]),
                ),
                self.codec_grade("codecs-session-grade", CORRECTNESS, &selected, &bin, &[]),
                self.codec_summary("codecs-session-summary", "codecs-session-grade"),
            ],
        })
    }

    /// Grade the full playback output with the decode-profile reference.
    fn play_codecs(&self) -> Option<Item> {
        let selected = lanes::item(&self.lanes, "codecs-play")
            .collect::<Vec<_>>()
            .join(",");
        if selected.is_empty() {
            return None;
        }
        let grade = "codecs-play-grade";
        Some(Item {
            name: "codecs-play".into(),
            weight: 1,
            preflight: owned(&[
                "--correctness",
                &selected,
                "--codecs",
                "harness/codecs/play-workloads.json",
            ]),
            steps: vec![
                step(
                    grade,
                    Stage::Timing,
                    Placement::Free,
                    xtask(&[
                        "codecs",
                        "grade",
                        "--manifest",
                        "harness/codecs/play-manifest.json",
                        "--lanes",
                        &selected,
                        "--root",
                        ".",
                        "--bin",
                        self.release,
                        "--ffmpeg",
                        &format!("{FFMPEG_BIN}/ffmpeg"),
                        "--ffprobe",
                        &format!("{FFMPEG_BIN}/ffprobe"),
                        "--codec-worker",
                        &format!("{}/play-views-worker", self.release),
                        "--reference",
                        "target/harness/ffmpeg-play-views-worker",
                        "--out",
                        &format!("{}/{grade}.jsonl", self.out),
                    ]),
                ),
                self.codec_summary("codecs-play-summary", grade),
            ],
        })
    }

    /// A workload list's timing item, then its memory parts.
    fn workloads(&self, list: &WorkloadList) -> Vec<Item> {
        let (out, name, workloads) = (self.out, list.name, list.workloads);
        let parts = list.part_weights.len();
        let codec_worker = self.codec_worker();
        let codecs = |mode: &str, part: Option<usize>| {
            let mut args = xtask(&[
                "codecs",
                mode,
                "--candidate",
                &codec_worker,
                "--reference",
                "target/harness/ffmpeg-decode-worker",
                "--workloads",
                workloads,
                "--root",
                ".",
            ]);
            if list.aac {
                args.extend(owned(&["--candidate-for", &self.aac_worker()]));
            }
            if name == "hevc-simd-workloads" {
                let worker = format!("video-hevc={}/codec-hevc-worker", self.release);
                args.extend(owned(&["--candidate-for", &worker]));
            }
            args.push("--out".to_owned());
            if mode == "time" {
                args.push(format!("{out}/benchmark/{name}"));
                args.extend(owned(&[
                    "--ledger",
                    &format!("{out}/{name}-ledger.jsonl"),
                    "--label",
                    self.target,
                ]));
            } else if let Some(part) = part {
                args.push(format!("{out}/memory/{name}-{part}"));
                args.extend(["--part".to_owned(), format!("{part}/{parts}")]);
            } else {
                args.push(format!("{out}/memory/{name}"));
            }
            self.full(args)
        };
        let [timing, _] = collections(name, codecs("time", None), Vec::new());
        let mut items = vec![Item {
            name: name.into(),
            weight: list.timing_weight,
            steps: vec![timing],
            preflight: owned(&["--codecs", workloads]),
        }];
        for (part, &weight) in (1..=parts).zip(list.part_weights) {
            let part_name = format!("{name}-{part}");
            let [_, memory] = collections(&part_name, Vec::new(), codecs("memory", Some(part)));
            items.push(Item {
                name: part_name,
                weight,
                steps: vec![memory],
                preflight: owned(&["--codecs", workloads]),
            });
        }
        items
    }

    /// One capture case: one is up to a quarter of an hour on ARMv7. Its correctness
    /// and timing run in the timing stage, and its memory, from the records they wrote,
    /// under the kernel bound in the memory stage, in one collection with the other
    /// capture cases of its shard.
    fn capture(&self, id: &str) -> Item {
        let out = self.out;
        let name = format!("capture-{id}");
        let timing = format!("{out}/{name}-timing");
        let qualify = |stage: &[&str], output: &str| {
            let mut command = owned(&CAPTURE_QUALIFY);
            command.extend(owned(&[
                "--candidate",
                &format!("{}/capture-worker", self.release),
                "--reference",
                "target/harness/ffmpeg-capture-worker",
                "--reference-small",
                "target/harness/ffmpeg-capture-small-worker",
                "--oracle-bin",
                FFMPEG_BIN,
                "--observer",
                "target/harness/allocations.so",
                "--inputs",
                &format!("{out}/capture-inputs"),
                "--out",
                output,
            ]));
            command.extend(owned(stage));
            command.extend(owned(&["--only", id]));
            self.full(command)
        };
        let mut steps = collections(
            &name,
            qualify(&["--stage", "timing"], &timing),
            qualify(
                &["--stage", "memory", "--timing", &timing],
                &format!("{out}/{name}"),
            ),
        );
        for step in &mut steps {
            step.environment.push(("TRIERGON_PYTHON", "python3"));
        }
        steps[1].batch = Some(CAPTURE_MEMORY);
        Item {
            name,
            weight: CAPTURE_WEIGHTS
                .iter()
                .find(|(case, _)| *case == id)
                .map_or(CAPTURE_WEIGHT, |(_, weight)| *weight),
            steps: steps.into(),
            preflight: owned(&["--capture", CAPTURE_CASES]),
        }
    }

    /// The workspace tests in the gate build's release profile.
    fn release_tests(&self) -> Item {
        Item {
            name: "tests-release".into(),
            weight: 20,
            steps: vec![step(
                "test-release",
                Stage::Timing,
                Placement::Free,
                self.full(xtask(&[
                    "gate",
                    "test",
                    "--profile",
                    "release",
                    "--output",
                    &format!("{}/tests/release", self.out),
                ])),
            )],
            preflight: Vec::new(),
        }
    }

    /// The workspace tests in the checked profile, the worker tests and the core time
    /// oracle.
    fn tests(&self) -> Item {
        let out = self.out;
        let mut core_time = xtask(&[
            "core-time",
            "--toolchain",
            crate::NIGHTLY,
            "--output",
            &format!("{out}/core-time"),
        ]);
        core_time.extend(self.cross_target());
        Item {
            name: "tests".into(),
            weight: 8,
            steps: vec![
                step(
                    "test-checked",
                    Stage::Timing,
                    Placement::Free,
                    self.full(xtask(&[
                        "gate",
                        "test",
                        "--profile",
                        "checked",
                        "--output",
                        &format!("{out}/tests/checked"),
                    ])),
                ),
                step(
                    "worker-tests",
                    Stage::Timing,
                    Placement::Free,
                    xtask(&["worker-tests"]),
                ),
                step("core-time", Stage::Timing, Placement::Free, core_time),
            ],
            preflight: Vec::new(),
        }
    }

    /// Standalone transform parity with av_tx and the transform definition.
    fn codec_dsp(&self) -> Item {
        Item {
            name: "codec-dsp".into(),
            weight: 1,
            steps: vec![step(
                "imdct-compare",
                Stage::Timing,
                Placement::Free,
                xtask(&[
                    "codecs",
                    "dsp",
                    "--output",
                    &format!("{}/parity/dsp-imdct", self.out),
                ]),
            )],
            preflight: Vec::new(),
        }
    }

    /// The font, shaping, rasterising and ASS reference comparisons.
    fn comparisons(&self, reports: &[ReportFeature]) -> Item {
        let compare = |name: &str, args: &[&str]| {
            let mut command = xtask(args);
            command.extend(owned(&["--output", &format!("{}/parity/{name}", self.out)]));
            step(
                format!("{name}-compare"),
                Stage::Timing,
                Placement::Free,
                command,
            )
        };
        let mut steps = vec![
            compare("fonts", &["fonts", "compare"]),
            compare("shape", &["shape", "compare"]),
            compare("raster", &["raster", "compare", "--all"]),
            compare("ass", &["ass", "compare"]),
        ];
        for feature in reports.iter().filter(|feature| feature.oracle) {
            steps.push(compare(
                &feature.id,
                &[
                    "oracle-compare",
                    "--contract",
                    &format!("harness/features/{}/contract.json", feature.id),
                ],
            ));
        }
        Item {
            name: "font-shape-raster-ass".into(),
            weight: 11,
            steps,
            preflight: Vec::new(),
        }
    }
}

/// The capture case ids, in the order of `harness/capture/cases.json`.
pub(super) fn capture_cases(root: &Path) -> R<Vec<String>> {
    let cases = json::read(&root.join(CAPTURE_CASES))?;
    Ok(cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{CAPTURE_CASES} without cases"))?
        .iter()
        .filter_map(|case| case.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect())
}

#[cfg(test)]
mod views_tests {
    use super::*;

    #[test]
    fn parity_only_feature_and_oracle_run_without_observers() {
        let target = super::super::TARGETS[0];
        let feature = |id: &str, oracle, parity_only| ReportFeature {
            id: id.into(),
            corpus: None,
            manifest_directory: None,
            weight: 1,
            oracle,
            parity_only,
        };
        let reports = [
            feature("deferred", false, vec![target]),
            feature("measured", false, vec![]),
            feature("independent", true, vec![]),
        ];
        let plan = items(&reports, "OUT", false, target, "release", &[]);
        assert_eq!(plan[0].steps.len(), 1);
        assert_eq!(plan[0].steps[0].placement, Placement::Free);
        assert!(
            plan[0].steps[0]
                .command
                .iter()
                .all(|argument| argument != "--benchmark" && argument != "--memory")
        );
        assert_eq!(plan[1].steps.len(), 3);
        let oracle = plan.last().unwrap().steps.last().unwrap();
        assert_eq!(oracle.placement, Placement::Free);
        assert_eq!(oracle.command[1], "oracle-compare");
        assert_eq!(
            oracle.command[3],
            "harness/features/independent/contract.json"
        );
    }
}
