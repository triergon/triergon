#![forbid(unsafe_code)]
//! AVC/hvcC configuration storage, work accounting and receiver acceptance.
//! Literal record arithmetic and pool accounting are the oracles; step traces
//! and outcomes are pinned to digests recorded before the eager and compact
//! forms were merged. No native or encoder output.
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::compat::nal::*;
use triergon_framing::*;
use triergon_harness_protocol::unhex;

type Outcome<T> = Result<T, Error>;
type Trace = Vec<(&'static str, u64)>;

trait Drive {
    type Out;
    fn drive(
        &mut self,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Outcome<BuildStep<Self::Out>>;
}
macro_rules! drive {
    ($($parser:ty => $out:ty),* $(,)?) => {$(
        impl Drive for $parser {
            type Out = $out;
            fn drive(
                &mut self,
                work: &mut WorkBudget,
                cancel: &AtomicBool,
            ) -> Outcome<BuildStep<$out>> {
                self.step(work, cancel)
            }
        }
    )*};
}
drive!(
    AvcConfigParser => AvcConfiguration,
    HvcConfigParser => HvcConfiguration,
    ReceiverAvcConfigParser => ReceiverAvcConfiguration,
    ReceiverHevcConfigParser => ReceiverHevcConfiguration,
);

static NO: AtomicBool = AtomicBool::new(false);
const QUANTA: [u64; 7] = [1, 2, 3, 5, 7, 16, 4096];

/// FNV-1a digest of each value's `Debug` text, compared with a value recorded
/// from the compact parsers on `main` before their two forms were merged.
fn pinned<T: std::fmt::Debug>(label: &str, values: impl IntoIterator<Item = T>) {
    let digest = values
        .into_iter()
        .flat_map(|v| format!("{v:?}\n").into_bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        });
    let expected = PINNED
        .iter()
        .find(|(l, _)| *l == label)
        .unwrap_or_else(|| panic!("nothing pinned for {label}"))
        .1;
    assert_eq!(digest, expected, "{label}");
}
/// Recorded on `main` at `0f154393`.
#[rustfmt::skip]
const PINNED: &[(&str, u64)] = &[
    ("avc cancellation", 9066978457836606541),
    ("avc failure traces", 14901974243396209317),
    ("avc quanta", 15985694055837125293),
    ("hevc cancellation", 10290735226527857491),
    ("hevc quanta", 9182612859336961706),
    ("limit outcomes", 13244593592555061840),
];

/// Every step's result kind and remaining work, then the terminal outcome.
fn run<P: Drive>(parser: &mut P, quantum: u64) -> (Trace, Outcome<P::Out>) {
    let mut trace = Vec::new();
    for _ in 0..2_000_000 {
        let mut work = WorkBudget::new(quantum);
        match parser.drive(&mut work, &NO) {
            Ok(BuildStep::Yield) => trace.push(("yield", work.remaining())),
            Ok(BuildStep::Ready(value)) => {
                trace.push(("ready", work.remaining()));
                let mut after = WorkBudget::new(0);
                assert!(matches!(
                    parser.drive(&mut after, &NO),
                    Ok(BuildStep::Complete)
                ));
                return (trace, Ok(value));
            }
            Ok(BuildStep::Complete) => panic!("completed without publication"),
            Err(error) => {
                trace.push(("error", work.remaining()));
                // Permanent failures latch without charging more work;
                // resource refusals are resumable and are not retried here.
                if !matches!(error, Error::Backpressure { .. } | Error::Allocation) {
                    let mut again = WorkBudget::new(quantum);
                    assert_eq!(parser.drive(&mut again, &NO).err(), Some(error));
                    assert_eq!(again.remaining(), quantum);
                }
                return (trace, Err(error));
            }
        }
    }
    panic!("bounded configuration walk did not finish")
}

fn bytes(pool: &Budget, raw: &[u8]) -> Bytes {
    Bytes::copy_from_slice(pool, raw).unwrap()
}

// ---------------------------------------------------------------- AVC facts

fn avc_source() -> AvcConfigSource {
    AvcConfigSource {
        source: SourceId(291),
        generation: 17,
        offset: 91,
    }
}
fn avc_limits() -> AvcConfigLimits {
    AvcConfigLimits {
        maximum_record_bytes: 200_000,
        maximum_nals: 600,
        maximum_nal_bytes: 65_535,
    }
}

type AvcArrayFact = (AvcConfigArrayKind, Range<usize>, Range<usize>, usize);
type AvcNalFact = (
    Range<usize>,
    Vec<u8>,
    Vec<u8>,
    AvcConfigArrayKind,
    NalHeader,
);

#[derive(Debug, PartialEq)]
struct AvcFacts {
    raw: Vec<u8>,
    source: AvcConfigSource,
    fields: AvcConfigFields,
    width: u8,
    understood_end: usize,
    trailing: Vec<u8>,
    arrays: Vec<AvcArrayFact>,
    nals: Vec<AvcNalFact>,
}

fn avc_facts(v: &AvcConfiguration) -> AvcFacts {
    assert_eq!(v.nals().len(), v.nal_count());
    assert_eq!(v.arrays().len(), v.array_count());
    assert!(v.nal(v.nal_count()).is_none());
    assert!(v.array(v.array_count()).is_none());
    for (index, n) in v.nals().enumerate() {
        assert_eq!(v.nal(index).unwrap().source_range(), n.source_range());
        assert_eq!(n.source(), v.source());
        // Borrowed bytes are exactly the record bytes at the stated extents.
        assert_eq!(n.source_bytes(), &v.source_bytes()[n.source_range()]);
        let field = n.source_range().start - 2..n.source_range().start;
        assert_eq!(n.length_field_bytes(), &v.source_bytes()[field]);
    }
    for a in v.arrays() {
        assert_eq!(a.source_bytes(), &v.source_bytes()[a.source_range()]);
        assert_eq!(a.source(), v.source());
    }
    assert_eq!(
        v.trailing_bytes(),
        v.retain_trailing_bytes().unwrap().as_ref()
    );
    AvcFacts {
        raw: v.source_bytes().to_vec(),
        source: v.source(),
        fields: v.fields(),
        width: v.sample_length_size().bytes(),
        understood_end: v.understood_end(),
        trailing: v.trailing_bytes().to_vec(),
        arrays: v
            .arrays()
            .map(|a| {
                (
                    a.kind(),
                    a.source_range(),
                    a.nal_indices(),
                    a.count_offset(),
                )
            })
            .collect(),
        nals: v
            .nals()
            .map(|n| {
                (
                    n.source_range(),
                    n.length_field_bytes().to_vec(),
                    n.source_bytes().to_vec(),
                    n.array_kind(),
                    n.header(),
                )
            })
            .collect(),
    }
}

/// Parses once; a published configuration holds exactly its retained storage
/// and everything returns to the pool afterwards.
fn avc_check(raw: &[u8], limits: AvcConfigLimits, quantum: u64) -> (Trace, Outcome<AvcFacts>) {
    let pool = Budget::new(4_000_000).unwrap();
    let baseline = pool.used();
    let (trace, out) = AvcConfigParser::new(&pool, avc_source(), bytes(&pool, raw), limits)
        .map_or_else(|e| (Vec::new(), Err(e)), |mut p| run(&mut p, quantum));
    let out = out.map(|v| {
        // Parser and caller handle are gone: only the record handle and the
        // charged descriptor table remain live in the pool.
        assert_eq!(pool.used(), baseline + v.retained_storage_bytes().unwrap());
        avc_facts(&v)
    });
    assert_eq!(pool.used(), baseline);
    (trace, out)
}

fn avc_fixture_records() -> Vec<(String, Vec<u8>)> {
    include_str!("../fixtures/avc-wire-v1.tsv")
        .lines()
        .filter(|v| !v.starts_with('#') && !v.is_empty())
        .map(|line| {
            let cells: Vec<_> = line.split('\t').collect();
            let raw = if cells[1] == "-" {
                Vec::new()
            } else {
                unhex(cells[1])
            };
            (cells[0].to_string(), raw)
        })
        .collect()
}

/// High profile: two SPS, two PPS, one SPS-extension NAL, trailing bytes.
fn avc_rich() -> Vec<u8> {
    let mut raw = vec![1, 100, 0, 40, 0xff, 0xe2];
    raw.extend_from_slice(&[0, 3, 0x67, 0x64, 0x00]);
    raw.extend_from_slice(&[0, 2, 0x67, 0x01]);
    raw.push(2);
    raw.extend_from_slice(&[0, 2, 0x68, 0xee]);
    raw.extend_from_slice(&[0, 1, 0x68]);
    raw.extend_from_slice(&[0xfd, 0xf8, 0xf8, 1]);
    raw.extend_from_slice(&[0, 4, 0x6d, 1, 2, 3]);
    raw.extend_from_slice(&[0xaa, 0xbb]);
    raw
}

// ---------------------------------------------------------------- HEVC facts

fn hvc_source() -> HvcConfigSource {
    HvcConfigSource {
        source: SourceId(342),
        generation: 9,
        offset: 37,
    }
}
fn hvc_limits() -> HvcConfigLimits {
    HvcConfigLimits {
        maximum_record_bytes: 1_000_000,
        maximum_arrays: 255,
        maximum_nals: 65_535,
        maximum_nal_bytes: 65_535,
    }
}
fn hvc_prefix(width: u8, arrays: u8) -> Vec<u8> {
    vec![
        1,
        0xa5,
        0x81,
        0x23,
        0x45,
        0x67,
        0x92,
        0x34,
        0x56,
        0x78,
        0x9a,
        0xbc,
        123,
        0xfa,
        0xbc,
        0xfd,
        0xfe,
        0xfa,
        0xfb,
        0x12,
        0x34,
        0xb4 | (width - 1),
        arrays,
    ]
}
fn hvc_record(width: u8, groups: &[(u8, Vec<Vec<u8>>)], suffix: &[u8]) -> Vec<u8> {
    let mut out = hvc_prefix(width, groups.len() as u8);
    for (header, nals) in groups {
        out.push(*header);
        out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            out.extend_from_slice(&(n.len() as u16).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    out.extend_from_slice(suffix);
    out
}
fn hvc_nal(kind: u8, tag: u8) -> Vec<u8> {
    vec![kind << 1, 1, 0, 0, 3, tag]
}
fn hvc_example(suffix: &[u8]) -> Vec<u8> {
    hvc_record(
        4,
        &[
            (0x80 | 0x21, vec![hvc_nal(33, 9), hvc_nal(33, 9)]),
            (55, vec![]),
            (32, vec![hvc_nal(32, 7)]),
            (33, vec![hvc_nal(34, 6)]),
        ],
        suffix,
    )
}

type HvcArrayFact = (Vec<u8>, Range<usize>, Range<usize>, bool, bool, u8);
type HvcNalFact = (Range<usize>, Vec<u8>, Vec<u8>, NalHeader, usize, u8, bool);

#[derive(Debug, PartialEq)]
struct HvcFacts {
    raw: Vec<u8>,
    source: HvcConfigSource,
    fields: HvcConfigFields,
    width: u8,
    understood_end: usize,
    suffix: Vec<u8>,
    syntax_issues: u32,
    arrays: Vec<HvcArrayFact>,
    nals: Vec<HvcNalFact>,
}

fn hvc_facts(v: &HvcConfiguration) -> HvcFacts {
    assert_eq!(v.nals().len(), v.nal_count());
    assert_eq!(v.arrays().len(), v.array_count());
    assert!(v.nal(v.nal_count()).is_none());
    assert!(v.array(v.array_count()).is_none());
    for (index, n) in v.nals().enumerate() {
        assert_eq!(v.nal(index).unwrap().source_range(), n.source_range());
        assert_eq!(n.source(), v.source());
        assert_eq!(n.source_bytes(), &v.source_bytes()[n.source_range()]);
    }
    for a in v.arrays() {
        assert_eq!(a.source_bytes(), &v.source_bytes()[a.source_range()]);
        assert_eq!(a.source_header_bytes(), &a.source_bytes()[..3]);
    }
    assert_eq!(
        v.opaque_suffix(),
        v.retain_opaque_suffix().unwrap().as_ref()
    );
    HvcFacts {
        raw: v.source_bytes().to_vec(),
        source: v.source(),
        fields: v.fields(),
        width: v.length_size().bytes(),
        understood_end: v.understood_end(),
        suffix: v.opaque_suffix().to_vec(),
        syntax_issues: v.syntax_issues(),
        arrays: v
            .arrays()
            .map(|a| {
                (
                    a.source_header_bytes().to_vec(),
                    a.source_range(),
                    a.nal_indices(),
                    a.completeness(),
                    a.reserved_bit(),
                    a.declared_nal_type(),
                )
            })
            .collect(),
        nals: v
            .nals()
            .map(|n| {
                (
                    n.source_range(),
                    n.length_field_bytes().to_vec(),
                    n.source_bytes().to_vec(),
                    n.header(),
                    n.array_index(),
                    n.declared_nal_type(),
                    n.type_matches_declaration(),
                )
            })
            .collect(),
    }
}

fn hvc_check(raw: &[u8], limits: HvcConfigLimits, quantum: u64) -> (Trace, Outcome<HvcFacts>) {
    let pool = Budget::new(8_000_000).unwrap();
    let baseline = pool.used();
    let (trace, out) = HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, raw), limits)
        .map_or_else(|e| (Vec::new(), Err(e)), |mut p| run(&mut p, quantum));
    let out = out.map(|v| {
        assert_eq!(pool.used(), baseline + v.retained_storage_bytes().unwrap());
        hvc_facts(&v)
    });
    assert_eq!(pool.used(), baseline);
    (trace, out)
}

fn hvc_corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("example", hvc_example(&[])),
        ("example-suffix", hvc_example(&[0xde, 0xad, 0xbe])),
        ("no-arrays", hvc_record(4, &[], &[])),
        ("no-arrays-suffix", hvc_record(4, &[], &[7])),
        ("width1", hvc_record(1, &[(32, vec![hvc_nal(32, 1)])], &[])),
        ("width2", hvc_record(2, &[(32, vec![hvc_nal(32, 1)])], &[])),
        ("width3", hvc_record(3, &[(32, vec![hvc_nal(32, 1)])], &[])),
        (
            "reserved-and-empty",
            hvc_record(
                4,
                &[
                    (0x40 | 0x27, vec![]),
                    (0x80 | 0x40 | 0x28, vec![hvc_nal(40, 2)]),
                ],
                &[],
            ),
        ),
        ("zero-nal-length", hvc_record(4, &[(32, vec![vec![]])], &[])),
        (
            "forbidden-bit",
            hvc_record(4, &[(32, vec![vec![0x80, 1, 9]])], &[]),
        ),
        (
            "one-byte-nal",
            hvc_record(4, &[(32, vec![vec![0x40]])], &[]),
        ),
        ("version0", {
            let mut r = hvc_example(&[]);
            r[0] = 0;
            r
        }),
    ]
}

// ================================================================== groups

#[test]
fn every_record_and_quantum_holds_exact_storage_and_record_extents() {
    let mut ok = 0;
    let mut total = 0;
    for (_, raw) in avc_fixture_records() {
        for quantum in [1, 2, 7, 64, 4096] {
            total += 1;
            ok += usize::from(avc_check(&raw, avc_limits(), quantum).1.is_ok());
        }
    }
    assert!(total >= 82 * 5, "frozen AVC fixture rows {total}");
    assert!(
        ok > 0 && ok < total,
        "fixtures include accepted and refused rows"
    );
    let mut ok = 0;
    for (_, raw) in hvc_corpus() {
        for quantum in [1, 2, 7, 64, 4096] {
            ok += usize::from(hvc_check(&raw, hvc_limits(), quantum).1.is_ok());
        }
    }
    assert!(ok > 0 && ok < hvc_corpus().len() * 5);
    // The borrowed getters return slices with the configuration lifetime,
    // never Bytes handles: these signatures only compile for `&'a [u8]`.
    fn avc<'a>(n: AvcConfigNal<'a>, a: AvcConfigArray<'a>) -> [&'a [u8]; 3] {
        [n.source_bytes(), n.length_field_bytes(), a.source_bytes()]
    }
    fn hvc<'a>(n: HvcConfigNal<'a>, a: HvcConfigArray<'a>) -> [&'a [u8]; 4] {
        [
            n.source_bytes(),
            n.length_field_bytes(),
            a.source_bytes(),
            a.source_header_bytes(),
        ]
    }
    let pool = Budget::new(1 << 16).unwrap();
    let mut p =
        AvcConfigParser::new(&pool, avc_source(), bytes(&pool, &avc_rich()), avc_limits()).unwrap();
    let v = run(&mut p, 4096).1.unwrap();
    let parts = avc(v.nal(4).unwrap(), v.array(2).unwrap());
    assert_eq!(
        parts,
        [
            &[0x6d, 1, 2, 3][..],
            &[0, 4],
            &[0xfd, 0xf8, 0xf8, 1, 0, 4, 0x6d, 1, 2, 3]
        ]
    );
    let _: &[u8] = v.trailing_bytes();
    let mut p = HvcConfigParser::new(
        &pool,
        hvc_source(),
        bytes(&pool, &hvc_example(&[])),
        hvc_limits(),
    )
    .unwrap();
    let v = run(&mut p, 4096).1.unwrap();
    let parts = hvc(v.nal(2).unwrap(), v.array(2).unwrap());
    assert_eq!(parts[0], [64, 1, 0, 0, 3, 7]);
    assert_eq!(parts[3], [32, 0, 1]);
    let _: &[u8] = v.opaque_suffix();
}

#[test]
fn truncations_refuse_physically_and_mutations_keep_exact_storage() {
    let rich = avc_rich();
    // Everything before the two trailing bytes is structure.
    let understood = rich.len() - 2;
    for cut in 0..=rich.len() {
        for quantum in [1, 4096] {
            let out = avc_check(&rich[..cut], avc_limits(), quantum).1;
            if cut < understood {
                assert_eq!(out.err(), Some(Error::Truncated), "cut{cut}");
            } else {
                assert_eq!(out.unwrap().trailing, rich[understood..cut], "cut{cut}");
            }
        }
    }
    for at in 0..rich.len() {
        let original = rich[at];
        for value in [
            0,
            1,
            2,
            0x7f,
            0xff,
            original ^ 0x80,
            original ^ 1,
            original ^ 0x1f,
        ] {
            let mut mutated = rich.clone();
            mutated[at] = value;
            for quantum in [1, 7] {
                let _ = avc_check(&mutated, avc_limits(), quantum);
            }
        }
    }
    let example = hvc_example(&[0xde, 0xad]);
    let understood = example.len() - 2;
    for cut in 0..=example.len() {
        for quantum in [1, 4096] {
            let out = hvc_check(&example[..cut], hvc_limits(), quantum).1;
            if cut < understood {
                assert_eq!(out.err(), Some(Error::Truncated), "cut{cut}");
            } else {
                assert_eq!(out.unwrap().suffix, example[understood..cut], "cut{cut}");
            }
        }
    }
    for at in 0..example.len() {
        let original = example[at];
        for value in [
            0,
            1,
            2,
            0x7f,
            0xff,
            original ^ 0x80,
            original ^ 0x40,
            original ^ 1,
        ] {
            let mut mutated = example.clone();
            mutated[at] = value;
            for quantum in [1, 7] {
                let _ = hvc_check(&mutated, hvc_limits(), quantum);
            }
        }
    }
}

#[test]
fn literal_hvcc_record_arithmetic() {
    // hvcC example: 23-byte prefix, four arrays (one empty, one mismatched).
    let pool = Budget::new(65_536).unwrap();
    let raw = hvc_example(&[0xde, 0xad, 0xbe]);
    let mut parser =
        HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, &raw), hvc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    let arrays: Vec<_> = config
        .arrays()
        .map(|a| {
            (
                a.source_header_bytes().to_vec(),
                a.source_range(),
                a.nal_indices(),
                a.completeness(),
                a.declared_nal_type(),
            )
        })
        .collect();
    assert_eq!(
        arrays,
        [
            (vec![161, 0, 2], 23..42, 0..2, true, 33),
            (vec![55, 0, 0], 42..45, 2..2, false, 55),
            (vec![32, 0, 1], 45..56, 2..3, false, 32),
            (vec![33, 0, 1], 56..67, 3..4, false, 33),
        ]
    );
    let nals: Vec<_> = config
        .nals()
        .map(|n| {
            (
                n.source_range(),
                n.length_field_bytes().to_vec(),
                n.source_bytes().last().copied(),
                n.array_index(),
                n.declared_nal_type(),
                n.header().nal_unit_type(),
            )
        })
        .collect();
    assert_eq!(
        nals,
        [
            (28..34, vec![0, 6], Some(9), 0, 33, 33),
            (36..42, vec![0, 6], Some(9), 0, 33, 33),
            (50..56, vec![0, 6], Some(7), 2, 32, 32),
            (61..67, vec![0, 6], Some(6), 3, 33, 34),
        ]
    );
    assert_eq!(config.understood_end(), 67);
    assert_eq!(config.opaque_suffix(), [0xde, 0xad, 0xbe]);
    assert_eq!(config.syntax_issues(), 1 << 7);
}
#[test]
fn single_exports_outlive_parser_and_configuration_with_one_full_owner() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &hvc_example(&[0xde, 0xad]));
    let raw_charge = raw.retained_storage_bytes();
    let mut parser = HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    let config = run(&mut parser, 4096).1.unwrap();
    drop(parser);
    let table = config.descriptor_storage_bytes().unwrap();
    assert!(table > 0);
    assert_eq!(config.retained_storage_bytes().unwrap(), raw_charge + table);
    assert_eq!(pool.used(), baseline + raw_charge + table);
    let suffix = config.retain_opaque_suffix().unwrap();
    // The export adds no charged allocation: it aliases the one backing owner.
    assert_eq!(pool.used(), baseline + raw_charge + table);
    assert!(suffix.shares_storage_with(config.source_bytes()));
    drop(config);
    // Descriptor tables released; the full raw backing stays charged once.
    assert_eq!(pool.used(), baseline + raw_charge);
    assert_eq!(&suffix[..], [0xde, 0xad]);
    drop(suffix);
    assert_eq!(pool.used(), baseline);

    let raw = bytes(&pool, &avc_rich());
    let raw_charge = raw.retained_storage_bytes();
    let mut parser = AvcConfigParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    drop(parser);
    let trailing = config.retain_trailing_bytes().unwrap();
    drop(config);
    assert_eq!(pool.used(), baseline + raw_charge);
    assert_eq!(&trailing[..], [0xaa, 0xbb]);
    drop(trailing);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn hidden_parent_offsets_are_record_relative_and_fully_charged() {
    for (codec, record) in [("avc", avc_rich()), ("hevc", hvc_example(&[9]))] {
        let pool = Budget::new(1 << 20).unwrap();
        let baseline = pool.used();
        let mut parent = vec![0xa5; record.len() + 4096];
        parent[127..127 + record.len()].copy_from_slice(&record);
        let root = Bytes::try_from_vec(&pool, parent)
            .map_err(|e| e.error())
            .unwrap();
        let full = root.retained_storage_bytes();
        let span = root.slice(127..127 + record.len()).unwrap();
        drop(root);
        if codec == "avc" {
            let mut parser = AvcConfigParser::new(&pool, avc_source(), span, avc_limits()).unwrap();
            let config = run(&mut parser, 3).1.unwrap();
            assert_eq!(config.source_bytes().as_ref(), record);
            assert_eq!(config.nal(0).unwrap().source_range(), 8..11);
            assert_eq!(config.nal(0).unwrap().source_bytes(), [0x67, 0x64, 0x00]);
            assert_eq!(config.trailing_bytes(), [0xaa, 0xbb]);
            assert_eq!(
                config.retained_storage_bytes().unwrap(),
                full + config.descriptor_storage_bytes().unwrap()
            );
        } else {
            let mut parser = HvcConfigParser::new(&pool, hvc_source(), span, hvc_limits()).unwrap();
            let config = run(&mut parser, 3).1.unwrap();
            assert_eq!(config.source_bytes().as_ref(), record);
            assert_eq!(config.nal(0).unwrap().source_range(), 28..34);
            assert_eq!(config.opaque_suffix(), [9]);
        }
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn hvcc_full_width_offsets_and_absolute_extent() {
    // A 65535-byte payload puts the later spans far above u16.
    let pool = Budget::new(1 << 20).unwrap();
    let mut hvc = hvc_prefix(4, 2);
    hvc.extend_from_slice(&[32, 0, 1, 255, 255, 0x40, 1]);
    hvc.resize(28 + 65_535, 0x11);
    hvc.extend_from_slice(&[33, 0, 1, 0, 6]);
    hvc.extend_from_slice(&hvc_nal(33, 1));
    let late = HvcConfigSource {
        source: SourceId(1),
        generation: u64::MAX,
        offset: u64::MAX - hvc.len() as u64,
    };
    let raw = bytes(&pool, &hvc);
    let mut parser = HvcConfigParser::new(&pool, late, raw.clone(), hvc_limits()).unwrap();
    let config = run(&mut parser, 4096).1.unwrap();
    assert_eq!(config.source(), late);
    assert_eq!(config.nal(0).unwrap().source_range(), 28..65_563);
    assert_eq!(config.nal(0).unwrap().length_field_bytes(), [255, 255]);
    assert_eq!(config.array(0).unwrap().source_range(), 23..65_563);
    assert_eq!(config.array(1).unwrap().source_range(), 65_563..65_574);
    assert_eq!(config.nal(1).unwrap().source_range(), 65_568..65_574);
    let after = HvcConfigSource {
        offset: late.offset + 1,
        ..late
    };
    assert_eq!(
        HvcConfigParser::new(&pool, after, raw, hvc_limits()).err(),
        Some(Error::Overflow)
    );
}

#[test]
fn empty_arrays_extensions_reserved_bits_mismatch_and_suffix() {
    // AVC: empty SPS and PPS groups, extension with no SPS-ext, suffix, and
    // a non-all-ones length-reserved pattern retained without repair.
    let raw = vec![1, 100, 0, 40, 0x7c, 0xe0, 0, 0xfc, 0xf8, 0xf8, 0, 1, 2];
    let pool = Budget::new(1 << 16).unwrap();
    let mut parser =
        AvcConfigParser::new(&pool, avc_source(), bytes(&pool, &raw), avc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    let arrays: Vec<_> = config
        .arrays()
        .map(|a| {
            (
                a.kind(),
                a.source_range(),
                a.nal_indices(),
                a.source_bytes().to_vec(),
            )
        })
        .collect();
    assert_eq!(
        arrays,
        [
            (AvcConfigArrayKind::Sequence, 5..6, 0..0, vec![0xe0]),
            (AvcConfigArrayKind::Picture, 6..7, 0..0, vec![0]),
            (
                AvcConfigArrayKind::SequenceExtension,
                7..11,
                0..0,
                vec![0xfc, 0xf8, 0xf8, 0]
            ),
        ]
    );
    assert_eq!(config.sample_length_size().bytes(), 1);
    assert_eq!(config.fields().raw_extension(), Some([0xfc, 0xf8, 0xf8, 0]));
    assert_eq!(config.fields().syntax_issues(), 1);
    assert_eq!(config.trailing_bytes(), [1, 2]);
    // hvcC: reserved bits and an empty array.
    let raw = hvc_corpus()
        .into_iter()
        .find(|(l, _)| *l == "reserved-and-empty")
        .unwrap()
        .1;
    let mut parser =
        HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, &raw), hvc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    let facts: Vec<_> = config
        .arrays()
        .map(|a| (a.reserved_bit(), a.completeness(), a.nal_indices()))
        .collect();
    assert_eq!(facts, [(true, false, 0..0), (true, true, 0..1)]);
    assert_eq!(config.syntax_issues() & (1 << 6), 1 << 6);
}

#[test]
fn cancellation_quanta_and_complete_ordering() {
    let rich = avc_rich();
    let hvc = hvc_example(&[1]);
    // Every step and outcome at each quantum.
    pinned(
        "avc quanta",
        QUANTA.map(|q| avc_check(&rich, avc_limits(), q)),
    );
    pinned(
        "hevc quanta",
        QUANTA.map(|q| hvc_check(&hvc, hvc_limits(), q)),
    );
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let avc =
        || AvcConfigParser::new(&pool, avc_source(), bytes(&pool, &rich), avc_limits()).unwrap();
    let hvc =
        || HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, &hvc), hvc_limits()).unwrap();

    // Zero work: an active parse yields without charging; cancellation
    // precedes that Yield and is sticky.
    fn zero_work<P: Drive>(mut parser: P) {
        assert!(matches!(
            parser.drive(&mut WorkBudget::new(0), &NO),
            Ok(BuildStep::Yield)
        ));
        let mut work = WorkBudget::new(0);
        assert_eq!(
            parser.drive(&mut work, &AtomicBool::new(true)).err(),
            Some(Error::Cancelled)
        );
        assert_eq!(work.remaining(), 0);
        let mut work = WorkBudget::new(9);
        assert_eq!(parser.drive(&mut work, &NO).err(), Some(Error::Cancelled));
        assert_eq!(work.remaining(), 9);
    }
    zero_work(avc());
    zero_work(hvc());
    assert_eq!(pool.used(), baseline);

    // After publication AVC checks cancellation first; hvcC reports Complete.
    fn after_ready<P: Drive>(mut parser: P) -> Outcome<bool> {
        let out = run(&mut parser, 4096).1.unwrap();
        drop(out);
        parser
            .drive(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .map(|step| matches!(step, BuildStep::Complete))
    }
    assert_eq!(after_ready(avc()), Err(Error::Cancelled));
    assert_eq!(after_ready(hvc()), Ok(true));
    assert_eq!(pool.used(), baseline);

    // Cancellation at any step index before publication stops the walk
    // with the step's whole work unit unspent.
    fn cancelled<P: Drive>(mut parser: P, at: usize) -> Vec<u64> {
        let mut trace = Vec::new();
        for step in 0.. {
            let mut work = WorkBudget::new(1);
            match parser.drive(&mut work, &AtomicBool::new(step == at)) {
                Ok(BuildStep::Yield) => trace.push(work.remaining()),
                Ok(BuildStep::Ready(_)) => {
                    trace.push(100);
                    break;
                }
                Ok(BuildStep::Complete) => unreachable!(),
                Err(error) => {
                    assert_eq!(error, Error::Cancelled);
                    trace.push(200 + work.remaining());
                    break;
                }
            }
        }
        trace
    }
    let avc_steps = cancelled(avc(), usize::MAX).len();
    let hvc_steps = cancelled(hvc(), usize::MAX).len();
    assert!(avc_steps > 10 && hvc_steps > 10);
    let (mut avc_traces, mut hvc_traces) = (Vec::new(), Vec::new());
    for at in 0..40 {
        let trace = cancelled(avc(), at);
        assert_eq!(trace.last() == Some(&201), at < avc_steps, "avc cancel{at}");
        avc_traces.push(trace);
        let trace = cancelled(hvc(), at);
        assert_eq!(
            trace.last() == Some(&201),
            at < hvc_steps,
            "hevc cancel{at}"
        );
        hvc_traces.push(trace);
        assert_eq!(pool.used(), baseline);
    }
    pinned("avc cancellation", avc_traces);
    pinned("hevc cancellation", hvc_traces);
}

#[test]
fn avc_cached_fill_boundary_matches_fallback_output_and_work() {
    let raw = avc_fixture_records()
        .into_iter()
        .find(|(label, _)| label == "plain-control")
        .unwrap()
        .1;
    // Two groups and their ends, two NALs, then the end event.
    let fill_work = 7;
    let before_fill_work = 1 + fill_work + 1; // Prefix, Count and Reserve.
    let mut previous = None;
    for remaining in [fill_work - 1, fill_work] {
        let quantum = before_fill_work + remaining;
        let (trace, facts) = avc_check(&raw, avc_limits(), quantum);
        let expected_trace = if remaining == fill_work {
            vec![("ready", 0)]
        } else {
            vec![("yield", 0), ("ready", quantum - 1)]
        };
        assert_eq!(trace, expected_trace);
        let spent = trace.iter().map(|(_, left)| quantum - left).sum::<u64>();
        assert_eq!(spent, before_fill_work + fill_work);
        let facts = facts.unwrap();
        if let Some(previous) = &previous {
            assert_eq!(&facts, previous);
        }
        previous = Some(facts);
    }
}

#[test]
fn permanent_failures_and_limits_latch_and_release() {
    let rich = avc_rich();
    // Late physical truncation, zero NAL length and a bad fixed NAL header.
    let mut zero = rich.clone();
    zero[6..8].copy_from_slice(&[0, 0]);
    let mut forbidden = rich.clone();
    forbidden[8] |= 0x80;
    let cases = [
        ("late-truncation", rich[..rich.len() - 3].to_vec()),
        ("zero-length", zero),
        ("forbidden-bit", forbidden),
    ];
    let errors = [
        Error::Truncated,
        Error::Invalid("zero AVC record NAL length"),
        Error::Invalid("NAL forbidden_zero_bit"),
    ];
    let mut traces = Vec::new();
    for ((label, raw), error) in cases.iter().zip(errors) {
        for quantum in QUANTA {
            let (trace, out) = avc_check(raw, avc_limits(), quantum);
            assert_eq!(out.err(), Some(error), "{label} quantum {quantum}");
            traces.push(trace);
        }
    }
    pinned("avc failure traces", traces);
    let mut outcomes = Vec::new();
    // Record, NAL-count and NAL-size limits: five NALs, the largest four bytes.
    for maximum_nals in 0..=6 {
        for maximum_nal_bytes in [1, 2, 3, 4, 65_535] {
            let limits = AvcConfigLimits {
                maximum_nals,
                maximum_nal_bytes,
                ..avc_limits()
            };
            let out = avc_check(&rich, limits, 1).1;
            outcomes.push(format!("{limits:?} {out:?}"));
            if maximum_nals >= 5 && maximum_nal_bytes >= 4 {
                assert!(out.is_ok(), "{limits:?}");
            } else {
                assert!(matches!(out, Err(Error::Limit { .. })), "{limits:?}");
            }
        }
    }
    for maximum_record_bytes in [0, rich.len() - 1, rich.len()] {
        let limits = AvcConfigLimits {
            maximum_record_bytes,
            ..avc_limits()
        };
        let out = avc_check(&rich, limits, 4096).1;
        outcomes.push(format!("{limits:?} {out:?}"));
        assert_eq!(out.is_ok(), maximum_record_bytes == rich.len());
    }
    // Four arrays and four six-byte NALs.
    let hvc = hvc_example(&[5]);
    for maximum_arrays in 0..=5 {
        for maximum_nals in [0, 3, 4, 5] {
            for maximum_nal_bytes in [5, 6, 65_535] {
                let limits = HvcConfigLimits {
                    maximum_arrays,
                    maximum_nals,
                    maximum_nal_bytes,
                    ..hvc_limits()
                };
                let out = hvc_check(&hvc, limits, 1).1;
                outcomes.push(format!("{limits:?} {out:?}"));
                if maximum_arrays >= 4 && maximum_nals >= 4 && maximum_nal_bytes >= 6 {
                    assert!(out.is_ok(), "{limits:?}");
                } else {
                    assert!(matches!(out, Err(Error::Limit { .. })), "{limits:?}");
                }
            }
        }
    }
    for maximum_record_bytes in [0, hvc.len() - 1, hvc.len()] {
        let limits = HvcConfigLimits {
            maximum_record_bytes,
            ..hvc_limits()
        };
        let out = hvc_check(&hvc, limits, 4096).1;
        outcomes.push(format!("{limits:?} {out:?}"));
        assert_eq!(out.is_ok(), maximum_record_bytes == hvc.len());
    }
    pinned("limit outcomes", outcomes);
    // A latched failure released its raw handle: only the caller's remains.
    let pool = Budget::new(1 << 16).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &cases[0].1);
    let charge = raw.retained_storage_bytes();
    let mut parser = AvcConfigParser::new(&pool, avc_source(), raw.clone(), avc_limits()).unwrap();
    assert_eq!(run(&mut parser, 1).1.err(), Some(Error::Truncated));
    assert_eq!(pool.used(), baseline + charge);
    drop(raw);
    assert_eq!(pool.used(), baseline);
    drop(parser);
    let raw = bytes(&pool, &hvc[..hvc.len() - 3]);
    let mut parser = HvcConfigParser::new(&pool, hvc_source(), raw.clone(), hvc_limits()).unwrap();
    assert_eq!(run(&mut parser, 1).1.err(), Some(Error::Truncated));
    drop(raw);
    assert_eq!(pool.used(), baseline);
}

/// Smallest pool limit that publishes, with the caller's raw handle moved in.
fn minimum_limit(codec: &str, record: &[u8]) -> usize {
    (Budget::control_storage_bytes()..1 << 16)
        .find(|&limit| {
            let Ok(pool) = Budget::new(limit) else {
                return false;
            };
            let Ok(raw) = Bytes::copy_from_slice(&pool, record) else {
                return false;
            };
            if codec == "avc" {
                AvcConfigParser::new(&pool, avc_source(), raw, avc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok())
            } else {
                HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok())
            }
        })
        .unwrap()
}

#[test]
fn actual_capacity_minimum_budget_and_resumable_pressure() {
    let rich = avc_rich();
    let hvc = hvc_example(&[]);
    for (codec, record) in [("avc", &rich), ("hevc", &hvc)] {
        let minimum = minimum_limit(codec, record);
        let pool = Budget::new(minimum).unwrap();
        let raw = bytes(&pool, record);
        let charge = raw.retained_storage_bytes();
        let table = if codec == "avc" {
            let mut parser = AvcConfigParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
            let v = run(&mut parser, 4096).1.unwrap();
            drop(parser);
            assert_eq!(
                pool.used(),
                Budget::control_storage_bytes() + v.retained_storage_bytes().unwrap()
            );
            v.descriptor_storage_bytes().unwrap()
        } else {
            let mut parser = HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
            let v = run(&mut parser, 4096).1.unwrap();
            drop(parser);
            assert_eq!(
                pool.used(),
                Budget::control_storage_bytes() + v.retained_storage_bytes().unwrap()
            );
            v.descriptor_storage_bytes().unwrap()
        };
        // The configuration held only its record charge plus its actual
        // table capacity.
        assert!(table > 0 && charge > 0);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }

    // Backpressure while reserving is resumable and commits no event twice.
    fn pressured<P: Drive>(mut parser: P, blocker: Bytes) -> P::Out {
        let mut blocker = Some(blocker);
        let mut refusals = 0;
        loop {
            match parser.drive(&mut WorkBudget::new(1), &NO) {
                Ok(BuildStep::Yield) => (),
                Ok(BuildStep::Ready(v)) => {
                    assert!(blocker.is_none() && refusals == 1);
                    return v;
                }
                Ok(BuildStep::Complete) => unreachable!(),
                Err(Error::Backpressure { .. }) => {
                    refusals += 1;
                    // Retrying under the same pressure stays resumable.
                    assert!(matches!(
                        parser.drive(&mut WorkBudget::new(1), &NO),
                        Err(Error::Backpressure { .. })
                    ));
                    blocker = None;
                }
                Err(e) => panic!("pressure {e:?}"),
            }
        }
    }
    // Hold everything the parser has not yet charged, so its first reservation
    // meets backpressure whatever the storage headers cost on the target.
    let block = |pool: &Budget| {
        let size = pool.available() - Bytes::owner_overhead_bytes();
        bytes(pool, &vec![0; size])
    };
    let expected = avc_check(&rich, avc_limits(), 1).1.unwrap();
    let pool = Budget::new(1 << 16).unwrap();
    let raw = bytes(&pool, &rich);
    let parser = AvcConfigParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    let blocker = block(&pool);
    assert_eq!(avc_facts(&pressured(parser, blocker)), expected);

    let expected = hvc_check(&hvc, hvc_limits(), 1).1.unwrap();
    let pool = Budget::new(1 << 16).unwrap();
    let raw = bytes(&pool, &hvc);
    let parser = HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    let blocker = block(&pool);
    assert_eq!(hvc_facts(&pressured(parser, blocker)), expected);
}

#[test]
fn strict_and_observed_configurations_keep_distinct_typed_refusals() {
    let mut avc3 = avc_rich();
    avc3[4] = 0xfe;
    let mut hvc3 = hvc_example(&[]);
    hvc3[21] = 0xb4 | 2;
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    // Strict source parsers refuse width three.
    for quantum in [1, 4096] {
        assert_eq!(
            avc_check(&avc3, avc_limits(), quantum).1.err(),
            Some(Error::Invalid("AVC three-byte sample length"))
        );
        assert_eq!(
            hvc_check(&hvc3, hvc_limits(), quantum).1.err(),
            Some(Error::Unsupported("hvcC three-byte sample length"))
        );
    }
    // Explicit observed receivers accept the same bytes with the refusal attached.
    for quantum in [1, 7, 4096] {
        let mut parser =
            ReceiverAvcConfigParser::new(&pool, avc_source(), bytes(&pool, &avc3), avc_limits())
                .unwrap();
        let v = run(&mut parser, quantum).1.unwrap();
        assert_eq!(v.observed_length_size().bytes(), 3);
        assert_eq!(
            v.strict_source_refusal(),
            Some(Error::Invalid("AVC three-byte sample length"))
        );
        assert_eq!(v.source(), avc_source());
        assert_eq!(v.source_bytes().as_ref(), avc3);
        assert_eq!(v.nal_count(), 5);
        assert_eq!(v.trailing_bytes(), [0xaa, 0xbb]);

        let mut parser =
            ReceiverHevcConfigParser::new(&pool, hvc_source(), bytes(&pool, &hvc3), hvc_limits())
                .unwrap();
        let v = run(&mut parser, quantum).1.unwrap();
        assert_eq!(v.observed_length_size().bytes(), 3);
        assert_eq!(
            v.strict_source_refusal(),
            Some(Error::Unsupported("hvcC three-byte sample length"))
        );
        assert_eq!(v.syntax_issues() & (1 << 5), 1 << 5);
    }
    assert_eq!(pool.used(), baseline);

    // Only a short version-one update header produces a rejection, once.
    let mut records: Vec<Vec<u8>> = (1..7).map(|n| avc3[..n].to_vec()).collect();
    records.extend((1..7).map(|n| avc_rich()[..n].to_vec()));
    records.push(vec![2, 66, 0, 10, 0xff]);
    records.push(avc_rich());
    for raw in records {
        for update in [false, true] {
            let new = if update {
                ReceiverAvcConfigParser::new_update
            } else {
                ReceiverAvcConfigParser::new
            };
            let mut parser = new(&pool, avc_source(), bytes(&pool, &raw), avc_limits()).unwrap();
            assert_eq!(parser.source_bytes().as_ref(), raw);
            let _ = run(&mut parser, 1);
            let rejection = parser.take_update_rejection();
            assert_eq!(
                rejection.is_some(),
                update && raw.len() < 7 && raw[0] == 1,
                "{raw:?} update{update}"
            );
            if let Some(r) = rejection {
                assert_eq!(r.source_bytes().as_ref(), raw);
                assert_eq!(r.source(), avc_source());
                assert_eq!(r.source_error(), Error::Truncated);
                assert_eq!(r.strict_source_refusal(), parser.strict_source_refusal());
                #[cfg(feature = "reference-trace")]
                assert_eq!(r.native_receive_result(), -1_094_995_529);
            }
            // A rejection is produced at most once.
            assert!(parser.take_update_rejection().is_none());
        }
    }
    assert_eq!(pool.used(), baseline);
}
