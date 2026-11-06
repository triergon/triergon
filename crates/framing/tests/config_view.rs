#![forbid(unsafe_code)]
//! Compact AVC/hvcC configuration views. The existing eager parsers are
//! the default-API oracle for differential sweeps; literal record arithmetic is
//! the independent oracle for selected records. No native or encoder output.
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceId, WorkBudget};
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
    AvcConfigViewParser => AvcConfigurationView,
    HvcConfigParser => HvcConfiguration,
    HvcConfigViewParser => HvcConfigurationView,
    NalObservedAvcConfigParser => NalObservedAvcConfiguration,
    NalObservedAvcConfigViewParser => NalObservedAvcConfigurationView,
    NalObservedHevcConfigParser => NalObservedHevcConfiguration,
    NalObservedHevcConfigViewParser => NalObservedHevcConfigurationView,
);

static NO: AtomicBool = AtomicBool::new(false);

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

fn avc_eager_facts(c: &AvcConfiguration) -> AvcFacts {
    AvcFacts {
        raw: c.source_bytes().to_vec(),
        source: c.source(),
        fields: c.fields(),
        width: c.sample_length_size().bytes(),
        understood_end: c.understood_end(),
        trailing: c.trailing_bytes().unwrap().to_vec(),
        arrays: c
            .arrays()
            .iter()
            .map(|a| {
                (
                    a.kind(),
                    a.source_range(),
                    a.nal_indices(),
                    a.count_offset(),
                )
            })
            .collect(),
        nals: c
            .nals()
            .iter()
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

fn avc_view_facts(v: &AvcConfigurationView) -> AvcFacts {
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

type Both<F> = (Trace, Outcome<F>, Trace, Outcome<F>);

fn avc_both(raw: &[u8], limits: AvcConfigLimits, quantum: u64) -> Both<AvcFacts> {
    let pool = Budget::new(4_000_000).unwrap();
    let baseline = pool.used();
    let (et, e) = AvcConfigParser::new(&pool, avc_source(), bytes(&pool, raw), limits)
        .map(|mut p| run(&mut p, quantum))
        .unwrap_or_else(|e| (Vec::new(), Err(e)));
    let e = e.map(|c| avc_eager_facts(&c));
    assert_eq!(pool.used(), baseline);
    let (vt, v) = AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, raw), limits)
        .map(|mut p| run(&mut p, quantum))
        .unwrap_or_else(|e| (Vec::new(), Err(e)));
    let v = v.map(|v| {
        // Parser and caller handle are gone: only the record handle and the
        // charged descriptor table remain live in the pool.
        assert_eq!(pool.used(), baseline + v.retained_storage_bytes().unwrap());
        avc_view_facts(&v)
    });
    assert_eq!(pool.used(), baseline);
    (et, e, vt, v)
}

fn assert_avc_equal(raw: &[u8], limits: AvcConfigLimits, quantum: u64, label: &str) -> bool {
    let (et, e, vt, v) = avc_both(raw, limits, quantum);
    assert_eq!(e, v, "{label} quantum{quantum}");
    assert_eq!(et, vt, "{label} quantum{quantum} work trace");
    e.is_ok()
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
            (33 | 128, vec![hvc_nal(33, 9), hvc_nal(33, 9)]),
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

fn hvc_eager_facts(c: &HvcConfiguration) -> HvcFacts {
    HvcFacts {
        raw: c.source_bytes().to_vec(),
        source: c.source(),
        fields: c.fields(),
        width: c.length_size().bytes(),
        understood_end: c.understood_end(),
        suffix: c.opaque_suffix().unwrap().to_vec(),
        syntax_issues: c.syntax_issues(),
        arrays: c
            .arrays()
            .iter()
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
        nals: c
            .nals()
            .iter()
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

fn hvc_view_facts(v: &HvcConfigurationView) -> HvcFacts {
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

fn hvc_both(raw: &[u8], limits: HvcConfigLimits, quantum: u64) -> Both<HvcFacts> {
    let pool = Budget::new(8_000_000).unwrap();
    let baseline = pool.used();
    let (et, e) = HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, raw), limits)
        .map(|mut p| run(&mut p, quantum))
        .unwrap_or_else(|e| (Vec::new(), Err(e)));
    let e = e.map(|c| hvc_eager_facts(&c));
    assert_eq!(pool.used(), baseline);
    let (vt, v) = HvcConfigViewParser::new(&pool, hvc_source(), bytes(&pool, raw), limits)
        .map(|mut p| run(&mut p, quantum))
        .unwrap_or_else(|e| (Vec::new(), Err(e)));
    let v = v.map(|v| {
        assert_eq!(pool.used(), baseline + v.retained_storage_bytes().unwrap());
        hvc_view_facts(&v)
    });
    assert_eq!(pool.used(), baseline);
    (et, e, vt, v)
}

fn assert_hvc_equal(raw: &[u8], limits: HvcConfigLimits, quantum: u64, label: &str) -> bool {
    let (et, e, vt, v) = hvc_both(raw, limits, quantum);
    assert_eq!(e, v, "{label} quantum{quantum}");
    assert_eq!(et, vt, "{label} quantum{quantum} work trace");
    e.is_ok()
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
                &[(64 | 39, vec![]), (128 | 64 | 40, vec![hvc_nal(40, 2)])],
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
fn g01_borrowed_inspection_equals_every_eager_fact_and_trace() {
    let mut ok = 0;
    let mut total = 0;
    for (label, raw) in avc_fixture_records() {
        for quantum in [1, 2, 7, 64, 4096] {
            total += 1;
            ok += usize::from(assert_avc_equal(&raw, avc_limits(), quantum, &label));
        }
    }
    assert!(total >= 82 * 5, "frozen AVC fixture rows {total}");
    assert!(
        ok > 0 && ok < total,
        "fixtures include accepted and refused rows"
    );
    let mut ok = 0;
    for (label, raw) in hvc_corpus() {
        for quantum in [1, 2, 7, 64, 4096] {
            ok += usize::from(assert_hvc_equal(&raw, hvc_limits(), quantum, label));
        }
    }
    assert!(ok > 0 && ok < hvc_corpus().len() * 5);
    // The borrowed getters return slices with the view lifetime, never Bytes
    // handles: these signatures only compile for `&'a [u8]` results.
    fn avc<'a>(n: AvcConfigNalRef<'a>, a: AvcConfigArrayRef<'a>) -> [&'a [u8]; 3] {
        [n.source_bytes(), n.length_field_bytes(), a.source_bytes()]
    }
    fn hvc<'a>(n: HvcConfigNalRef<'a>, a: HvcConfigArrayRef<'a>) -> [&'a [u8]; 4] {
        [
            n.source_bytes(),
            n.length_field_bytes(),
            a.source_bytes(),
            a.source_header_bytes(),
        ]
    }
    let pool = Budget::new(1 << 16).unwrap();
    let mut p =
        AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, &avc_rich()), avc_limits())
            .unwrap();
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
    let mut p = HvcConfigViewParser::new(
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
fn g01_mutation_and_truncation_sweeps_match_eager_outcomes() {
    let rich = avc_rich();
    assert!(assert_avc_equal(&rich, avc_limits(), 4096, "rich"));
    for cut in 0..=rich.len() {
        for quantum in [1, 4096] {
            assert_avc_equal(&rich[..cut], avc_limits(), quantum, &format!("cut{cut}"));
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
                assert_avc_equal(&mutated, avc_limits(), quantum, &format!("at{at}={value}"));
            }
        }
    }
    let example = hvc_example(&[0xde, 0xad]);
    for cut in 0..=example.len() {
        for quantum in [1, 4096] {
            assert_hvc_equal(&example[..cut], hvc_limits(), quantum, &format!("cut{cut}"));
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
                assert_hvc_equal(&mutated, hvc_limits(), quantum, &format!("at{at}={value}"));
            }
        }
    }
}

#[test]
fn g01_literal_record_arithmetic_is_an_independent_oracle() {
    // Plain AVC control: 01 42 a5 28 ff | e1 0002 6780 | 01 0002 6840.
    let raw = unhex("0142a528ffe1000267800100026840");
    let pool = Budget::new(65_536).unwrap();
    let mut parser =
        AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, &raw), avc_limits()).unwrap();
    let view = run(&mut parser, 1).1.unwrap();
    assert_eq!(view.fields().raw_prefix(), [1, 66, 165, 40, 255, 225]);
    assert_eq!(view.sample_length_size().bytes(), 4);
    assert_eq!(view.understood_end(), 15);
    assert!(view.trailing_bytes().is_empty());
    let arrays: Vec<_> = view
        .arrays()
        .map(|a| {
            (
                a.kind(),
                a.source_range(),
                a.count_offset(),
                a.nal_indices(),
            )
        })
        .collect();
    assert_eq!(
        arrays,
        [
            (AvcConfigArrayKind::Sequence, 5..10, 5, 0..1),
            (AvcConfigArrayKind::Picture, 10..15, 10, 1..2)
        ]
    );
    let n0 = view.nal(0).unwrap();
    assert_eq!(n0.source_range(), 8..10);
    assert_eq!(n0.length_field_bytes(), [0, 2]);
    assert_eq!(n0.source_bytes(), [0x67, 0x80]);
    assert_eq!(n0.header().nal_unit_type(), 7);
    let n1 = view.nal(1).unwrap();
    assert_eq!(n1.source_range(), 13..15);
    assert_eq!(n1.length_field_bytes(), [0, 2]);
    assert_eq!(n1.source_bytes(), [0x68, 0x40]);
    assert_eq!(n1.header().nal_unit_type(), 8);

    // hvcC example: 23-byte prefix, four arrays (one empty, one mismatched).
    let raw = hvc_example(&[0xde, 0xad, 0xbe]);
    let mut parser =
        HvcConfigViewParser::new(&pool, hvc_source(), bytes(&pool, &raw), hvc_limits()).unwrap();
    let view = run(&mut parser, 1).1.unwrap();
    let arrays: Vec<_> = view
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
    let nals: Vec<_> = view
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
    assert_eq!(view.understood_end(), 67);
    assert_eq!(view.opaque_suffix(), [0xde, 0xad, 0xbe]);
    assert_eq!(view.syntax_issues(), 1 << 7);
}

#[test]
fn g02_single_exports_outlive_parser_and_view_with_one_full_owner() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &hvc_example(&[0xde, 0xad]));
    let raw_charge = raw.retained_storage_bytes();
    let mut parser = HvcConfigViewParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    let view = run(&mut parser, 4096).1.unwrap();
    drop(parser);
    let table = view.descriptor_storage_bytes().unwrap();
    assert!(table > 0);
    assert_eq!(view.retained_storage_bytes().unwrap(), raw_charge + table);
    assert_eq!(pool.used(), baseline + raw_charge + table);
    let nal = view.nal(3).unwrap().retain();
    let array = view.array(1).unwrap().retain();
    let suffix = view.retain_opaque_suffix().unwrap();
    // Exports add no charged allocation: they alias the one backing owner.
    assert_eq!(pool.used(), baseline + raw_charge + table);
    assert!(suffix.shares_storage_with(view.source_bytes()));
    drop(view);
    // Descriptor tables released; the full raw backing stays charged once.
    assert_eq!(pool.used(), baseline + raw_charge);
    assert_eq!(nal.source_range(), 61..67);
    assert_eq!(nal.source_bytes(), [68, 1, 0, 0, 3, 6]);
    assert_eq!(nal.length_field_bytes(), [0, 6]);
    assert_eq!((nal.array_index(), nal.declared_nal_type()), (3, 33));
    assert!(!nal.type_matches_declaration());
    assert_eq!(nal.source(), hvc_source());
    assert_eq!(nal.view().source_bytes(), nal.source_bytes());
    assert_eq!(nal.retained_storage_bytes(), raw_charge);
    assert!(nal.belongs_to(&pool));
    assert_eq!(array.source_bytes(), [55, 0, 0]);
    assert_eq!(array.nal_indices(), 2..2);
    assert_eq!(array.declared_nal_type(), 55);
    assert_eq!(&suffix[..], [0xde, 0xad]);
    let copy = nal.clone();
    drop(nal);
    drop(array);
    drop(suffix);
    assert_eq!(pool.used(), baseline + raw_charge);
    assert_eq!(copy.source_bytes(), [68, 1, 0, 0, 3, 6]);
    drop(copy);
    assert_eq!(pool.used(), baseline);

    let raw = bytes(&pool, &avc_rich());
    let raw_charge = raw.retained_storage_bytes();
    let mut parser = AvcConfigViewParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    let view = run(&mut parser, 1).1.unwrap();
    drop(parser);
    let nal = view.nal(4).unwrap().retain_in(&pool).unwrap();
    let array = view.array(2).unwrap().retain_in(&pool).unwrap();
    let trailing = view.retain_trailing_bytes().unwrap();
    drop(view);
    assert_eq!(pool.used(), baseline + raw_charge);
    assert_eq!(nal.source_bytes(), [0x6d, 1, 2, 3]);
    assert_eq!(nal.length_field_bytes(), [0, 4]);
    assert_eq!(nal.source_range(), 29..33);
    assert_eq!(nal.array_kind(), AvcConfigArrayKind::SequenceExtension);
    assert_eq!(nal.header().nal_unit_type(), 13);
    assert_eq!(array.kind(), AvcConfigArrayKind::SequenceExtension);
    assert_eq!(array.nal_indices(), 4..5);
    assert_eq!(array.count_offset(), 26);
    assert_eq!(
        array.source_bytes(),
        [0xfd, 0xf8, 0xf8, 1, 0, 4, 0x6d, 1, 2, 3]
    );
    assert_eq!(&trailing[..], [0xaa, 0xbb]);
    drop((nal, array, trailing));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn g02_batch_export_charges_its_own_actual_capacity() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &hvc_example(&[]));
    let raw_charge = raw.retained_storage_bytes();
    let mut parser = HvcConfigViewParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    let view = run(&mut parser, 4096).1.unwrap();
    drop(parser);
    let table = view.descriptor_storage_bytes().unwrap();
    let mut batch = BudgetVec::new(&pool);
    batch.try_reserve_exact(view.nal_count()).unwrap();
    let batch_charge = batch.capacity() * size_of::<RetainedHvcConfigNal>();
    for nal in view.nals() {
        batch.try_push(nal.retain()).map_err(|e| e.error).unwrap();
    }
    assert_eq!(pool.used(), baseline + raw_charge + table + batch_charge);
    drop(view);
    // The batch keeps its own capacity charge plus one deduplicated backing.
    assert_eq!(pool.used(), baseline + raw_charge + batch_charge);
    let ranges: Vec<_> = batch.as_slice().iter().map(|n| n.source_range()).collect();
    assert_eq!(ranges, [28..34, 36..42, 50..56, 61..67]);
    drop(batch);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn g03_retained_exports_are_static_owners() {
    fn owned<T: 'static + Send + Sync>(_: &T) {}
    let pool = Budget::new(1 << 16).unwrap();
    let raw = bytes(&pool, &avc_rich());
    let mut parser = AvcConfigViewParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    let view = run(&mut parser, 4096).1.unwrap();
    let nal = view.nal(0).unwrap().retain();
    let array = view.array(0).unwrap().retain();
    drop(view);
    owned(&nal);
    owned(&array);
    let moved = std::thread::spawn(move || nal.source_bytes().to_vec())
        .join()
        .unwrap();
    assert_eq!(moved, [0x67, 0x64, 0x00]);
    assert_eq!(array.nal_indices(), 0..2);
}

#[test]
fn g04_hidden_parent_offsets_are_record_relative_and_fully_charged() {
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
        let other = Budget::new(1 << 16).unwrap();
        let retained = if codec == "avc" {
            let mut parser =
                AvcConfigViewParser::new(&pool, avc_source(), span, avc_limits()).unwrap();
            let view = run(&mut parser, 3).1.unwrap();
            assert_eq!(view.source_bytes().as_ref(), record);
            assert_eq!(view.nal(0).unwrap().source_range(), 8..11);
            assert_eq!(view.nal(0).unwrap().source_bytes(), [0x67, 0x64, 0x00]);
            assert_eq!(view.trailing_bytes(), [0xaa, 0xbb]);
            assert_eq!(
                view.retained_storage_bytes().unwrap(),
                full + view.descriptor_storage_bytes().unwrap()
            );
            assert_eq!(
                view.nal(0).unwrap().retain_in(&other).err(),
                Some(Error::ForeignPool)
            );
            assert_eq!(
                view.array(0).unwrap().retain_in(&other).err(),
                Some(Error::ForeignPool)
            );
            let leaf = view.nal(0).unwrap().retain();
            assert!(!leaf.belongs_to(&other));
            drop(view);
            leaf.retained_storage_bytes()
        } else {
            let mut parser =
                HvcConfigViewParser::new(&pool, hvc_source(), span, hvc_limits()).unwrap();
            let view = run(&mut parser, 3).1.unwrap();
            assert_eq!(view.source_bytes().as_ref(), record);
            assert_eq!(view.nal(0).unwrap().source_range(), 28..34);
            assert_eq!(view.opaque_suffix(), [9]);
            assert_eq!(
                view.nal(0).unwrap().retain_in(&other).err(),
                Some(Error::ForeignPool)
            );
            assert_eq!(
                view.array(0).unwrap().retain_in(&other).err(),
                Some(Error::ForeignPool)
            );
            let leaf = view.array(0).unwrap().retain();
            drop(view);
            leaf.retained_storage_bytes()
        };
        assert_eq!(retained, full);
        assert_eq!(pool.used(), baseline);
    }
    // Foreign records are refused at entry exactly like the eager parsers.
    let pool = Budget::new(1 << 16).unwrap();
    let other = Budget::new(1 << 16).unwrap();
    let raw = bytes(&other, &avc_rich());
    assert_eq!(
        AvcConfigViewParser::new(&pool, avc_source(), raw.clone(), avc_limits()).err(),
        Some(Error::ForeignPool)
    );
    assert_eq!(
        AvcConfigParser::new(&pool, avc_source(), raw, avc_limits()).err(),
        Some(Error::ForeignPool)
    );
    let raw = bytes(&other, &hvc_example(&[]));
    assert_eq!(
        HvcConfigViewParser::new(&pool, hvc_source(), raw.clone(), hvc_limits()).err(),
        Some(Error::ForeignPool)
    );
    assert_eq!(
        HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).err(),
        Some(Error::ForeignPool)
    );
}

#[test]
fn g05_full_width_offsets_wire_maxima_and_541_memberships() {
    // 65535-byte payloads put later spans far above u16.
    let mut avc = vec![1, 66, 0, 10, 255, 225, 255, 255, 0x67];
    avc.resize(8 + 65_535, 0);
    avc.extend_from_slice(&[1, 0, 3, 0x68, 1, 99]);
    let pool = Budget::new(1 << 20).unwrap();
    let source = AvcConfigSource {
        source: SourceId(7),
        generation: u64::MAX,
        offset: u64::MAX - avc.len() as u64,
    };
    for quantum in [1, 7, 4096] {
        let mut parser =
            AvcConfigViewParser::new(&pool, source, bytes(&pool, &avc), avc_limits()).unwrap();
        let view = run(&mut parser, quantum).1.unwrap();
        assert_eq!(view.source(), source);
        assert_eq!(view.nal(0).unwrap().source_range(), 8..65_543);
        assert_eq!(view.nal(0).unwrap().length_field_bytes(), [255, 255]);
        assert_eq!(view.nal(1).unwrap().source_range(), 65_546..65_549);
        assert_eq!(view.nal(1).unwrap().length_field_bytes(), [0, 3]);
        assert_eq!(view.nal(1).unwrap().source_bytes(), [0x68, 1, 99]);
        assert_eq!(view.array(1).unwrap().count_offset(), 65_543);
    }
    assert!(assert_avc_equal(&avc, avc_limits(), 7, "payload65535"));
    // One byte past the absolute u64 extent is refused at entry, unchanged.
    let raw = bytes(&pool, &avc);
    let late = AvcConfigSource {
        offset: u64::MAX - avc.len() as u64 + 1,
        ..source
    };
    assert_eq!(
        AvcConfigViewParser::new(&pool, late, raw.clone(), avc_limits()).err(),
        Some(Error::Overflow)
    );
    assert_eq!(
        AvcConfigParser::new(&pool, late, raw, avc_limits()).err(),
        Some(Error::Overflow)
    );

    let mut hvc = hvc_prefix(4, 2);
    hvc.extend_from_slice(&[32, 0, 1, 255, 255, 0x40, 1]);
    hvc.resize(28 + 65_535, 0x11);
    hvc.extend_from_slice(&[33, 0, 1, 0, 6]);
    hvc.extend_from_slice(&hvc_nal(33, 1));
    assert!(assert_hvc_equal(&hvc, hvc_limits(), 7, "hvc-payload65535"));
    let late = HvcConfigSource {
        source: SourceId(1),
        generation: u64::MAX,
        offset: u64::MAX - hvc.len() as u64,
    };
    let raw = bytes(&pool, &hvc);
    let mut parser = HvcConfigViewParser::new(&pool, late, raw.clone(), hvc_limits()).unwrap();
    let view = run(&mut parser, 4096).1.unwrap();
    assert_eq!(view.source(), late);
    assert_eq!(view.nal(0).unwrap().source_range(), 28..65_563);
    assert_eq!(view.nal(0).unwrap().length_field_bytes(), [255, 255]);
    assert_eq!(view.array(0).unwrap().source_range(), 23..65_563);
    assert_eq!(view.array(1).unwrap().source_range(), 65_563..65_574);
    assert_eq!(view.nal(1).unwrap().source_range(), 65_568..65_574);
    let after = HvcConfigSource {
        offset: late.offset + 1,
        ..late
    };
    assert_eq!(
        HvcConfigViewParser::new(&pool, after, raw, hvc_limits()).err(),
        Some(Error::Overflow)
    );

    // Literal maximum AVC group counts: 31 + 255 + 255 memberships.
    let mut raw = vec![1, 100, 0, 10, 255, 255];
    for _ in 0..31 {
        raw.extend_from_slice(&[0, 1, 0x67]);
    }
    raw.push(255);
    for _ in 0..255 {
        raw.extend_from_slice(&[0, 1, 0x68]);
    }
    raw.extend_from_slice(&[253, 248, 248, 255]);
    for _ in 0..255 {
        raw.extend_from_slice(&[0, 1, 0x6d]);
    }
    for quantum in [1, 7, 4096] {
        assert!(assert_avc_equal(&raw, avc_limits(), quantum, "avc541"));
        let pool = Budget::new(1 << 20).unwrap();
        let mut parser =
            AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, &raw), avc_limits())
                .unwrap();
        let view = run(&mut parser, quantum).1.unwrap();
        assert_eq!(view.nal_count(), 541);
        let groups: Vec<_> = view.arrays().map(|a| a.nal_indices()).collect();
        assert_eq!(groups, [0..31, 31..286, 286..541]);
        assert_eq!(view.array(2).unwrap().source_range().end, raw.len());
        assert!(view.nals().all(|n| n.length_field_bytes() == [0, 1]));
    }
}

#[test]
fn g06_empty_arrays_extensions_reserved_bits_mismatch_and_suffix() {
    // AVC: empty SPS and PPS groups, extension with no SPS-ext, suffix, and
    // a non-all-ones length-reserved pattern retained without repair.
    let raw = vec![1, 100, 0, 40, 0x7c, 0xe0, 0, 0xfc, 0xf8, 0xf8, 0, 1, 2];
    for quantum in [1, 4096] {
        assert!(assert_avc_equal(&raw, avc_limits(), quantum, "avc-empty"));
    }
    let pool = Budget::new(1 << 16).unwrap();
    let mut parser =
        AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, &raw), avc_limits()).unwrap();
    let view = run(&mut parser, 1).1.unwrap();
    let arrays: Vec<_> = view
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
    assert_eq!(view.sample_length_size().bytes(), 1);
    assert_eq!(view.fields().raw_extension(), Some([0xfc, 0xf8, 0xf8, 0]));
    assert_eq!(view.fields().syntax_issues(), 1);
    assert_eq!(view.trailing_bytes(), [1, 2]);
    // hvcC: every corpus record, reserved bits and declaration mismatch.
    for (label, raw) in hvc_corpus() {
        assert_hvc_equal(&raw, hvc_limits(), 1, label);
    }
    let raw = hvc_corpus()
        .into_iter()
        .find(|(l, _)| *l == "reserved-and-empty")
        .unwrap()
        .1;
    let mut parser =
        HvcConfigViewParser::new(&pool, hvc_source(), bytes(&pool, &raw), hvc_limits()).unwrap();
    let view = run(&mut parser, 1).1.unwrap();
    let facts: Vec<_> = view
        .arrays()
        .map(|a| (a.reserved_bit(), a.completeness(), a.nal_indices()))
        .collect();
    assert_eq!(facts, [(true, false, 0..0), (true, true, 0..1)]);
    assert_eq!(view.syntax_issues() & (1 << 6), 1 << 6);
}

#[test]
fn g07_quanta_traces_cancellation_and_complete_ordering() {
    let rich = avc_rich();
    let hvc = hvc_example(&[1]);
    for quantum in [1, 7, 4096] {
        assert!(assert_avc_equal(&rich, avc_limits(), quantum, "trace"));
        assert!(assert_hvc_equal(&hvc, hvc_limits(), quantum, "trace"));
    }
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let avc_eager =
        || AvcConfigParser::new(&pool, avc_source(), bytes(&pool, &rich), avc_limits()).unwrap();
    let avc_view = || {
        AvcConfigViewParser::new(&pool, avc_source(), bytes(&pool, &rich), avc_limits()).unwrap()
    };
    let hvc_eager =
        || HvcConfigParser::new(&pool, hvc_source(), bytes(&pool, &hvc), hvc_limits()).unwrap();
    let hvc_view =
        || HvcConfigViewParser::new(&pool, hvc_source(), bytes(&pool, &hvc), hvc_limits()).unwrap();

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
    zero_work(avc_eager());
    zero_work(avc_view());
    zero_work(hvc_eager());
    zero_work(hvc_view());
    assert_eq!(pool.used(), baseline);

    // After publication AVC checks cancellation first; hvcC reports Complete.
    fn after_ready<P: Drive>(mut parser: P) -> Outcome<bool> {
        let out = run(&mut parser, 4096).1.unwrap();
        drop(out);
        parser
            .drive(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .map(|step| matches!(step, BuildStep::Complete))
    }
    assert_eq!(after_ready(avc_eager()), Err(Error::Cancelled));
    assert_eq!(after_ready(avc_view()), Err(Error::Cancelled));
    assert_eq!(after_ready(hvc_eager()), Ok(true));
    assert_eq!(after_ready(hvc_view()), Ok(true));
    assert_eq!(pool.used(), baseline);

    // Cancellation at every step index stops both walks identically.
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
    let avc_steps = cancelled(avc_eager(), usize::MAX).len();
    let hvc_steps = cancelled(hvc_eager(), usize::MAX).len();
    assert!(avc_steps > 10 && hvc_steps > 10);
    for at in 0..40 {
        let e = cancelled(avc_eager(), at);
        assert_eq!(e, cancelled(avc_view(), at), "avc cancel{at}");
        assert_eq!(e.last() == Some(&201), at < avc_steps, "avc cancel{at}");
        let e = cancelled(hvc_eager(), at);
        assert_eq!(e, cancelled(hvc_view(), at), "hevc cancel{at}");
        assert_eq!(e.last() == Some(&201), at < hvc_steps, "hevc cancel{at}");
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn g08_permanent_failures_latch_release_and_match_eager() {
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
    for (label, raw) in &cases {
        for quantum in [1, 7, 4096] {
            let (et, e, vt, v) = avc_both(raw, avc_limits(), quantum);
            assert!(e.is_err(), "{label}");
            assert_eq!(e, v, "{label}");
            assert_eq!(et, vt, "{label}");
        }
    }
    // Record, NAL-count and NAL-size limits.
    for maximum_nals in 0..=6 {
        for maximum_nal_bytes in [1, 2, 3, 4, 65_535] {
            let limits = AvcConfigLimits {
                maximum_nals,
                maximum_nal_bytes,
                ..avc_limits()
            };
            assert_avc_equal(&rich, limits, 1, "avc-limits");
        }
    }
    for maximum_record_bytes in [0, rich.len() - 1, rich.len()] {
        let limits = AvcConfigLimits {
            maximum_record_bytes,
            ..avc_limits()
        };
        assert_avc_equal(&rich, limits, 4096, "avc-record-limit");
    }
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
                assert_hvc_equal(&hvc, limits, 1, "hvc-limits");
            }
        }
    }
    for maximum_record_bytes in [0, hvc.len() - 1, hvc.len()] {
        let limits = HvcConfigLimits {
            maximum_record_bytes,
            ..hvc_limits()
        };
        assert_hvc_equal(&hvc, limits, 4096, "hvc-record-limit");
    }
    // A latched failure released its raw handle: only the caller's remains.
    let pool = Budget::new(1 << 16).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &cases[0].1);
    let charge = raw.retained_storage_bytes();
    let mut parser =
        AvcConfigViewParser::new(&pool, avc_source(), raw.clone(), avc_limits()).unwrap();
    assert_eq!(run(&mut parser, 1).1.err(), Some(Error::Truncated));
    assert_eq!(pool.used(), baseline + charge);
    drop(raw);
    assert_eq!(pool.used(), baseline);
    drop(parser);
    let raw = bytes(&pool, &hvc[..hvc.len() - 3]);
    let mut parser =
        HvcConfigViewParser::new(&pool, hvc_source(), raw.clone(), hvc_limits()).unwrap();
    assert_eq!(run(&mut parser, 1).1.err(), Some(Error::Truncated));
    drop(raw);
    assert_eq!(pool.used(), baseline);
}

/// Smallest pool limit that publishes, with the caller's raw handle moved in.
fn minimum_limit(codec: &str, record: &[u8], view: bool) -> usize {
    (Budget::control_storage_bytes()..1 << 16)
        .find(|&limit| {
            let Ok(pool) = Budget::new(limit) else {
                return false;
            };
            let Ok(raw) = Bytes::copy_from_slice(&pool, record) else {
                return false;
            };
            match (codec, view) {
                ("avc", false) => AvcConfigParser::new(&pool, avc_source(), raw, avc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok()),
                ("avc", true) => AvcConfigViewParser::new(&pool, avc_source(), raw, avc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok()),
                (_, false) => HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok()),
                (_, true) => HvcConfigViewParser::new(&pool, hvc_source(), raw, hvc_limits())
                    .is_ok_and(|mut p| run(&mut p, 4096).1.is_ok()),
            }
        })
        .unwrap()
}

#[test]
fn g09_actual_capacity_minimum_budget_and_resumable_pressure() {
    let rich = avc_rich();
    let hvc = hvc_example(&[]);
    for (codec, record) in [("avc", &rich), ("hevc", &hvc)] {
        let eager = minimum_limit(codec, record, false);
        let view = minimum_limit(codec, record, true);
        // The compact table is smaller, so the view fits a pool that the
        // eager representation cannot; eager admission itself is unchanged.
        assert!(view < eager, "{codec} view{view} eager{eager}");
        let pool = Budget::new(view).unwrap();
        let raw = bytes(&pool, record);
        let charge = raw.retained_storage_bytes();
        let table = if codec == "avc" {
            let mut parser =
                AvcConfigViewParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
            let v = run(&mut parser, 4096).1.unwrap();
            drop(parser);
            assert_eq!(
                pool.used(),
                Budget::control_storage_bytes() + v.retained_storage_bytes().unwrap()
            );
            v.descriptor_storage_bytes().unwrap()
        } else {
            let mut parser =
                HvcConfigViewParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
            let v = run(&mut parser, 4096).1.unwrap();
            drop(parser);
            assert_eq!(
                pool.used(),
                Budget::control_storage_bytes() + v.retained_storage_bytes().unwrap()
            );
            v.descriptor_storage_bytes().unwrap()
        };
        // The view held only its record charge plus its actual table capacity.
        assert!(table > 0 && charge > 0);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
        let pool = Budget::new(view).unwrap();
        let raw = bytes(&pool, record);
        let result = if codec == "avc" {
            AvcConfigParser::new(&pool, avc_source(), raw, avc_limits())
                .map(|mut p| run(&mut p, 4096).1.map(drop))
        } else {
            HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits())
                .map(|mut p| run(&mut p, 4096).1.map(drop))
        };
        assert!(
            matches!(
                result,
                Ok(Err(Error::Backpressure { .. } | Error::Limit { .. }))
            ),
            "{codec} {result:?}"
        );
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
    let block = |pool: &Budget| {
        let size = pool.available() - 64 - Bytes::owner_overhead_bytes();
        bytes(pool, &vec![0; size])
    };
    let roomy = Budget::new(1 << 20).unwrap();
    let mut p =
        AvcConfigViewParser::new(&roomy, avc_source(), bytes(&roomy, &rich), avc_limits()).unwrap();
    let expected = avc_view_facts(&run(&mut p, 1).1.unwrap());
    let pool = Budget::new(1 << 16).unwrap();
    let raw = bytes(&pool, &rich);
    let blocker = block(&pool);
    let parser = AvcConfigViewParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    assert_eq!(avc_view_facts(&pressured(parser, blocker)), expected);

    let mut p =
        HvcConfigViewParser::new(&roomy, hvc_source(), bytes(&roomy, &hvc), hvc_limits()).unwrap();
    let expected = hvc_view_facts(&run(&mut p, 1).1.unwrap());
    let pool = Budget::new(1 << 16).unwrap();
    let raw = bytes(&pool, &hvc);
    let blocker = block(&pool);
    let parser = HvcConfigViewParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    assert_eq!(hvc_view_facts(&pressured(parser, blocker)), expected);
}

#[test]
fn g10_default_eager_api_and_owned_clone_behavior_are_unchanged() {
    // Owned eager leaves still clone their Bytes handles and keep the owner.
    let pool = Budget::new(1 << 16).unwrap();
    let baseline = pool.used();
    let raw = bytes(&pool, &hvc_example(&[]));
    let charge = raw.retained_storage_bytes();
    let mut parser = HvcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    let nal: HvcConfigNal = config.nals()[3].clone();
    let array: HvcConfigArray = config.arrays()[1].clone();
    let suffix = config.opaque_suffix().unwrap();
    let _: &Bytes = nal.source_bytes();
    let _: &Bytes = array.source_header_bytes();
    assert!(
        nal.source_bytes()
            .shares_storage_with(config.source_bytes())
    );
    drop((config, parser));
    assert_eq!(pool.used(), baseline + charge);
    assert_eq!(nal.source_bytes().as_ref(), [68, 1, 0, 0, 3, 6]);
    assert_eq!(array.source_header_bytes().as_ref(), [55, 0, 0]);
    drop((nal, array, suffix));
    assert_eq!(pool.used(), baseline);
    let raw = bytes(&pool, &avc_rich());
    let mut parser = AvcConfigParser::new(&pool, avc_source(), raw, avc_limits()).unwrap();
    let config = run(&mut parser, 1).1.unwrap();
    let nal: AvcConfigNal = config.nals()[4].clone();
    let _: &Bytes = nal.length_field_bytes();
    drop((config, parser));
    assert_eq!(nal.source_bytes().as_ref(), [0x6d, 1, 2, 3]);
    drop(nal);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn g11_strict_and_observed_views_keep_distinct_typed_refusals() {
    let mut avc3 = avc_rich();
    avc3[4] = 0xfe;
    let mut hvc3 = hvc_example(&[]);
    hvc3[21] = 0xb4 | 2;
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    // Strict source parsers refuse width three; the views stay strict.
    for quantum in [1, 4096] {
        let (_, e, _, v) = avc_both(&avc3, avc_limits(), quantum);
        let refusal = Error::Invalid("AVC three-byte sample length");
        assert_eq!((e.err(), v.err()), (Some(refusal), Some(refusal)));
        let (_, e, _, v) = hvc_both(&hvc3, hvc_limits(), quantum);
        let refusal = Error::Unsupported("hvcC three-byte sample length");
        assert_eq!((e.err(), v.err()), (Some(refusal), Some(refusal)));
    }
    // Explicit observed views accept the same bytes with the refusal attached.
    for quantum in [1, 7, 4096] {
        let mut eager =
            NalObservedAvcConfigParser::new(&pool, avc_source(), bytes(&pool, &avc3), avc_limits())
                .unwrap();
        let mut view = NalObservedAvcConfigViewParser::new(
            &pool,
            avc_source(),
            bytes(&pool, &avc3),
            avc_limits(),
        )
        .unwrap();
        let (et, e) = run(&mut eager, quantum);
        let (vt, v) = run(&mut view, quantum);
        assert_eq!(et, vt);
        let (e, v) = (e.unwrap(), v.unwrap());
        assert_eq!(v.observed_length_size().bytes(), 3);
        assert_eq!(v.observed_length_size(), e.observed_length_size());
        assert_eq!(v.strict_source_refusal(), e.strict_source_refusal());
        assert_eq!(
            v.strict_source_refusal(),
            Some(Error::Invalid("AVC three-byte sample length"))
        );
        assert_eq!((v.source(), v.fields()), (e.source(), e.fields()));
        assert_eq!(v.source_bytes().as_ref(), e.source_bytes().as_ref());
        let ranges: Vec<_> = v.nals().map(|n| n.source_range()).collect();
        let eager_ranges: Vec<_> = e.nals().iter().map(|n| n.source_range()).collect();
        assert_eq!(ranges, eager_ranges);
        assert_eq!(v.nal_count(), e.nals().len());
        assert_eq!(v.trailing_bytes(), [0xaa, 0xbb]);

        let mut eager = NalObservedHevcConfigParser::new(
            &pool,
            hvc_source(),
            bytes(&pool, &hvc3),
            hvc_limits(),
        )
        .unwrap();
        let mut view = NalObservedHevcConfigViewParser::new(
            &pool,
            hvc_source(),
            bytes(&pool, &hvc3),
            hvc_limits(),
        )
        .unwrap();
        let (et, e) = run(&mut eager, quantum);
        let (vt, v) = run(&mut view, quantum);
        assert_eq!(et, vt);
        let (e, v) = (e.unwrap(), v.unwrap());
        assert_eq!(v.observed_length_size().bytes(), 3);
        assert_eq!(v.strict_source_refusal(), e.strict_source_refusal());
        assert_eq!(
            v.strict_source_refusal(),
            Some(Error::Unsupported("hvcC three-byte sample length"))
        );
        let arrays: Vec<_> = v
            .arrays()
            .map(|a| (a.source_range(), a.nal_indices()))
            .collect();
        let eager_arrays: Vec<_> = e
            .arrays()
            .iter()
            .map(|a| (a.source_range(), a.nal_indices()))
            .collect();
        assert_eq!(arrays, eager_arrays);
        assert_eq!(v.syntax_issues() & (1 << 5), 1 << 5);
    }
    assert_eq!(pool.used(), baseline);

    // Short-update rejection receipts are identical, including refusals.
    let mut records: Vec<Vec<u8>> = (1..7).map(|n| avc3[..n].to_vec()).collect();
    records.extend((1..7).map(|n| avc_rich()[..n].to_vec()));
    records.push(vec![2, 66, 0, 10, 0xff]);
    records.push(avc_rich());
    type Receipt = (Vec<u8>, AvcConfigSource, Error, Option<Error>, i32);
    fn receipt(r: Option<NalAvcUpdateRejection>) -> Option<Receipt> {
        r.map(|r| {
            (
                r.source_bytes().to_vec(),
                r.source(),
                r.source_error(),
                r.strict_source_refusal(),
                r.native_receive_result(),
            )
        })
    }
    for raw in records {
        for update in [false, true] {
            let (mut eager, mut view) = if update {
                (
                    NalObservedAvcConfigParser::new_update(
                        &pool,
                        avc_source(),
                        bytes(&pool, &raw),
                        avc_limits(),
                    )
                    .unwrap(),
                    NalObservedAvcConfigViewParser::new_update(
                        &pool,
                        avc_source(),
                        bytes(&pool, &raw),
                        avc_limits(),
                    )
                    .unwrap(),
                )
            } else {
                (
                    NalObservedAvcConfigParser::new(
                        &pool,
                        avc_source(),
                        bytes(&pool, &raw),
                        avc_limits(),
                    )
                    .unwrap(),
                    NalObservedAvcConfigViewParser::new(
                        &pool,
                        avc_source(),
                        bytes(&pool, &raw),
                        avc_limits(),
                    )
                    .unwrap(),
                )
            };
            assert_eq!(view.source_bytes().as_ref(), raw);
            assert_eq!(view.strict_source_refusal(), eager.strict_source_refusal());
            let (et, e) = run(&mut eager, 1);
            let (vt, v) = run(&mut view, 1);
            assert_eq!(et, vt);
            assert_eq!(e.err(), v.err());
            let er = receipt(eager.take_update_rejection());
            let vr = receipt(view.take_update_rejection());
            assert_eq!(er, vr, "{raw:?} update{update}");
            assert_eq!(er.is_some(), update && raw.len() < 7 && raw[0] == 1);
            // A receipt is produced at most once.
            assert!(view.take_update_rejection().is_none());
        }
    }
    assert_eq!(pool.used(), baseline);
}
