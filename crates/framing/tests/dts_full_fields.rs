//! Original primary-bit-authored controls for the isolated observer projection.
#[path = "../examples/support/dts24.rs"]
mod dts24;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, Step, WorkBudget};
use triergon_framing::{
    DtsCoreField, DtsHeaderInspector, DtsInspection, DtsInspectionIdentity, FieldObservation,
    probe_dts_core_header,
};

fn authored(cpf: u32, version: u32, rate: u32) -> Vec<u8> {
    let mut fields = vec![
        (32, 0x7ffe8001),
        (1, 1),
        (5, 31),
        (1, cpf),
        (7, 63),
        (14, 111),
        (6, 9),
        (4, 8),
        (5, rate),
        (1, 0),
        (1, 1),
        (1, 1),
        (1, 1),
        (1, 1),
        (3, 2),
        (1, 1),
        (1, 1),
        (2, 2),
        (1, 0),
    ];
    if cpf == 1 {
        fields.push((16, 0xbeef));
    }
    fields.extend([
        (1, 1),
        (4, version),
        (2, 2),
        (3, 5),
        (1, 1),
        (1, 1),
        (4, 11),
    ]);
    let bits: String = fields
        .into_iter()
        .map(|(width, value)| format!("{value:0width$b}"))
        .collect();
    assert_eq!(bits.len(), if cpf == 0 { 104 } else { 120 });
    bits.as_bytes()
        .chunks_exact(8)
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 2).unwrap())
        .collect()
}
fn inspect(source: &[u8]) -> DtsInspection {
    let pool = Budget::new(65536).unwrap();
    let mut job = DtsHeaderInspector::new(
        &pool,
        DtsInspectionIdentity {
            source: SourceId(24),
            generation: 1,
            offset: 0,
        },
        Bytes::copy_from_slice(&pool, source).unwrap(),
        60000,
    )
    .unwrap();
    for _ in 0..1024 {
        match job
            .step(&mut WorkBudget::new(31), &AtomicBool::new(false))
            .unwrap()
        {
            Step::Ready(value) => return value,
            Step::Yield => {}
            other => panic!("unexpected inspection: {other:?}"),
        }
    }
    panic!("bounded inspection");
}

#[test]
fn all24_values_follow_the_preregistered_literal_primary_field_domains() {
    let source = authored(1, 6, 22);
    let h = probe_dts_core_header(&source).unwrap();
    let expected = [
        1, 32, 1, 64, 112, 9, 8, 22, 1, 1, 1, 1, 2, 1, 1, 2, 0, 1, 6, 2, 5, 1, 1, 11,
    ];
    assert_eq!(dts24::from_core(&h), expected);
    assert_eq!(dts24::from_inspection(&inspect(&source)), Some(expected));
    assert_eq!(h.hcrc, Some(0xbeef));
    assert_eq!(h.fixed, 0);
    assert_eq!((h.short, h.nblks, h.fsize), (31, 63, 111));
}
#[test]
fn no_complete_projection_is_manufactured_for_any_missing_cpf1_prefix() {
    let source = authored(1, 6, 22);
    for end in 0..source.len() {
        assert!(matches!(
            probe_dts_core_header(&source[..end]),
            Err(Error::Truncated)
        ));
        assert!(dts24::from_inspection(&inspect(&source[..end])).is_none());
    }
}
#[test]
fn native_named_source_observation_does_not_convert_rejection_to_acceptance() {
    for (version, rate) in [(12, 22), (6, 31)] {
        let source = authored(1, version, rate);
        assert!(probe_dts_core_header(&source).is_err());
        let observation = inspect(&source);
        assert!(observation.syntax_complete());
        let view = dts24::from_inspection(&observation).unwrap();
        assert_eq!(view[18], version);
        assert_eq!(view[7], rate);
        assert!(!observation.issues().is_empty());
    }
}
#[test]
fn hcrc_absence_is_not_a_coded_zero_or_an_extra_native_member() {
    let source = authored(0, 6, 22);
    let h = probe_dts_core_header(&source).unwrap();
    assert_eq!(h.hcrc, None);
    assert_eq!(h.header_bits, 104);
    assert_eq!(
        inspect(&source).field(DtsCoreField::Hcrc),
        FieldObservation::NotInSyntax
    );
    assert_eq!(dts24::from_core(&h)[2], 0);
    assert_eq!(dts24::NAMES.len(), 24);
}
#[test]
fn serializer_emits_exactly24_unique_member_names_in_frozen_order() {
    let expected = [
        1, 32, 1, 64, 112, 9, 8, 22, 1, 1, 1, 1, 2, 1, 1, 2, 0, 1, 6, 2, 5, 1, 1, 11,
    ];
    let mut bytes = Vec::new();
    dts24::write(&mut bytes, Some(expected)).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text.matches(':').count(), 24);
    let names: std::collections::BTreeSet<_> = dts24::NAMES.into_iter().collect();
    assert_eq!(names.len(), 24);
    assert!(text.starts_with("{\"normal_frame\":1,\"deficit_samples\":32,"));
    assert!(text.ends_with("\"sumdiff_surround\":1,\"dn_code\":11}"));
    let mut bytes = Vec::new();
    dts24::write(&mut bytes, None).unwrap();
    assert_eq!(bytes, b"null");
}
#[test]
fn original396_source_inputs_stay_unchanged_and_strict_success_matches_inspection() {
    let mut count = 0;
    for line in include_str!("../fixtures/dts-inspect-source-v1.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let parts: Vec<_> = line.split('\t').collect();
        let source: Vec<u8> = if parts[1] == "~" {
            Vec::new()
        } else {
            parts[1]
                .as_bytes()
                .chunks_exact(2)
                .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
                .collect()
        };
        let observation = inspect(&source);
        assert_eq!(observation.source_bytes().as_ref(), source);
        if let Ok(h) = probe_dts_core_header(&source) {
            assert_eq!(
                dts24::from_inspection(&observation),
                Some(dts24::from_core(&h))
            );
        }
        if !observation.syntax_complete() {
            assert!(dts24::from_inspection(&observation).is_none());
        }
        count += 1;
    }
    assert_eq!(count, 396);
}
