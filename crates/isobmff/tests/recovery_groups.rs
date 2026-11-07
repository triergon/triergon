mod common;
#[path = "support/recovery.rs"]
mod wire;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus, RecoveryDefinitionOrigin as Definition, RecoveryDescription, RecoveryDescriptions,
    RecoveryKind as Kind, RecoveryLimits, RecoveryLookup, RecoveryMappingOrigin as Mapping,
    RecoveryMappings, RecoveryScope, RecoverySelection, RecoverySyntax as Syntax,
};

fn entry(
    table: &RecoveryDescriptions,
    index: u32,
    quantum: u64,
) -> Result<RecoveryDescription, Error> {
    let mut lookup = table.locate(index)?;
    for _ in 0..128 {
        if lookup.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?
            == BuildStatus::Ready
        {
            return lookup.finish();
        }
    }
    panic!("recovery entry lookup stalled within fixed poll bound");
}
fn resolve(
    scope: RecoveryScope<'_>,
    ordinal: u64,
    limits: RecoveryLimits,
    quantum: u64,
) -> Result<RecoverySelection, Error> {
    let mut lookup = RecoveryLookup::new(scope, ordinal, limits)?;
    for _ in 0..128 {
        if lookup.poll(&mut WorkBudget::new(quantum), &AtomicBool::new(false))?
            == BuildStatus::Ready
        {
            return lookup.finish();
        }
    }
    panic!("recovery association stalled within fixed poll bound");
}
fn scope<'a>(
    kind: Kind,
    track: Option<&'a RecoveryDescriptions>,
    local: Option<&'a RecoveryDescriptions>,
    mappings: Option<&'a RecoveryMappings>,
    fragment: Option<u64>,
    first: u64,
    count: u64,
) -> RecoveryScope<'a> {
    RecoveryScope {
        kind,
        first_ordinal: first,
        sample_count: count,
        fragment_ordinal: fragment,
        track_descriptions: track,
        local_descriptions: local,
        mappings,
    }
}

#[test]
fn roll_prol_versions_signed_extremes_and_variable_entries_preserve_wire_bits() {
    let pool = Budget::new(65536).unwrap();
    for kind in [Kind::Roll, Kind::Prol] {
        for (version, length, value_start, stride) in [
            (0, 2, 20, 2),
            (1, 2, 24, 2),
            (1, 0, 28, 6),
            (2, 2, 28, 2),
            (2, 0, 32, 6),
        ] {
            let raw =
                wire::descriptions(&pool, kind, version, length, 1, &[i16::MIN, -2, 0, 3], 4096);
            let table = RecoveryDescriptions::parse(
                raw.clone(),
                Syntax::Mpeg2026Draft,
                RecoveryLimits::default(),
            )
            .unwrap();
            assert_eq!(
                (table.kind(), table.version(), table.entry_count()),
                (kind, version, 4)
            );
            assert_eq!(
                table.default_index(),
                if version >= 2 { Some(1) } else { None }
            );
            assert_eq!(table.raw().bytes().as_ref(), raw.bytes().as_ref());
            let before = pool.used();
            for (i, distance) in [i16::MIN, -2, 0, 3].into_iter().enumerate() {
                let index = u32::try_from(i + 1).unwrap();
                let description = entry(&table, index, 1).unwrap();
                assert_eq!(description.distance, distance);
                assert_eq!(
                    description.raw_bits,
                    u16::from_be_bytes(distance.to_be_bytes())
                );
                let start = 4096 + value_start + u64::try_from(i).unwrap() * stride;
                assert_eq!(description.source_range, start..start + 2);
                let selection = RecoverySelection {
                    kind,
                    raw_mapping_index: Some(index),
                    mapping_origin: Mapping::Explicit,
                    definition_origin: Some(Definition::Track),
                    description: Some(description),
                };
                match distance {
                    i16::MIN => assert_eq!(selection.preceding_samples(), Ok(32768)),
                    -2 => assert_eq!(selection.preceding_samples(), Ok(2)),
                    0 => assert_eq!(selection.preceding_samples(), Ok(0)),
                    _ => assert_eq!(
                        selection.preceding_samples(),
                        Err(Error::Unsupported(
                            "positive recovery distance needs postroll"
                        ))
                    ),
                }
            }
            assert_eq!(
                pool.used(),
                before,
                "recovery lookups must borrow charged metadata"
            );
        }
    }
}

#[test]
fn sbgp_versions_keep_parameters_and_compact_uint32_max_sample_runs() {
    let pool = Budget::new(65536).unwrap();
    let descriptions = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-1, -2], 0),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    for version in [0, 1] {
        let raw = wire::mappings(
            &pool,
            Kind::Roll,
            version,
            0x123456,
            &[(u32::MAX, 1), (1, 2)],
        );
        let table = RecoveryMappings::parse(raw.clone(), RecoveryLimits::default()).unwrap();
        assert_eq!(table.run_count(), 2);
        assert_eq!(
            table.grouping_parameter(),
            if version == 1 { Some(0x123456) } else { None }
        );
        assert_eq!(table.raw().bytes().as_ref(), raw.bytes().as_ref());
    }
    let mappings = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 1, 0, &[(u32::MAX, 1), (1, 2)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    let before = pool.used();
    let selected = resolve(
        scope(
            Kind::Roll,
            Some(&descriptions),
            None,
            Some(&mappings),
            None,
            1,
            u64::from(u32::MAX) + 1,
        ),
        u64::from(u32::MAX) + 1,
        RecoveryLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(selected.raw_mapping_index, Some(2));
    assert_eq!(selected.preceding_samples(), Ok(2));
    assert_eq!(pool.used(), before);
}

#[test]
fn fragment_indexes_select_track_or_current_local_definitions_without_shadowing_or_leakage() {
    let pool = Budget::new(65536).unwrap();
    let track = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-1], 0),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    let local = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-3], 256),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    let map = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 0, 0, &[(1, 1), (1, 0x10001)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    let current = scope(
        Kind::Roll,
        Some(&track),
        Some(&local),
        Some(&map),
        Some(9),
        100,
        2,
    );
    let a = resolve(current, 100, RecoveryLimits::default(), 1).unwrap();
    let b = resolve(current, 101, RecoveryLimits::default(), 1).unwrap();
    assert_eq!(
        (a.definition_origin, a.preceding_samples()),
        (Some(Definition::Track), Ok(1))
    );
    assert_eq!(
        (b.definition_origin, b.preceding_samples()),
        (Some(Definition::Fragment(9)), Ok(3))
    );
    assert_eq!(b.raw_mapping_index, Some(0x10001));
    let later = scope(Kind::Roll, Some(&track), None, Some(&map), Some(10), 102, 2);
    assert!(matches!(
        resolve(later, 103, RecoveryLimits::default(), 1),
        Err(Error::Invalid("missing fragment recovery description"))
    ));
}

#[test]
fn absent_maps_resolve_explicitly_tagged_track_fragment_and_inherited_defaults() {
    let pool = Budget::new(65536).unwrap();
    let track = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Prol, 2, 2, 1, &[-2], 0),
        Syntax::Mpeg2026Draft,
        RecoveryLimits::default(),
    )
    .unwrap();
    let local = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Prol, 2, 2, 0x10001, &[-4], 256),
        Syntax::Mpeg2026Draft,
        RecoveryLimits::default(),
    )
    .unwrap();
    for (scope, expected, mapping, definition) in [
        (
            scope(Kind::Prol, Some(&track), None, None, None, 1, 6),
            2,
            Mapping::TrackDefault,
            Definition::Track,
        ),
        (
            scope(Kind::Prol, Some(&track), Some(&local), None, Some(1), 7, 3),
            4,
            Mapping::FragmentDefault,
            Definition::Fragment(1),
        ),
        (
            scope(Kind::Prol, Some(&track), None, None, Some(2), 10, 3),
            2,
            Mapping::InheritedTrackDefault,
            Definition::Track,
        ),
    ] {
        let result = resolve(scope, scope.first_ordinal, RecoveryLimits::default(), 1).unwrap();
        assert_eq!(result.raw_mapping_index, None);
        assert_eq!(result.mapping_origin, mapping);
        assert_eq!(result.definition_origin, Some(definition));
        assert_eq!(result.preceding_samples(), Ok(expected));
    }
    for default in [0, 0x10000] {
        let none = RecoveryDescriptions::parse(
            wire::descriptions(&pool, Kind::Prol, 2, 2, default, &[-4], 256),
            Syntax::Mpeg2026Draft,
            RecoveryLimits::default(),
        )
        .unwrap();
        let result = resolve(
            scope(Kind::Prol, Some(&track), Some(&none), None, Some(3), 13, 3),
            13,
            RecoveryLimits::default(),
            1,
        )
        .unwrap();
        assert_eq!(result.mapping_origin, Mapping::FragmentDefault);
        assert!(result.description.is_none());
        assert_eq!(result.preceding_samples(), Ok(0));
    }
}

#[test]
fn explicit_zero_empty_and_absent_maps_remain_distinct_without_default_guessing() {
    let pool = Budget::new(65536).unwrap();
    let track = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-1], 0),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    let zero = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 0, 0, &[(1, 0)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    let explicit = resolve(
        scope(Kind::Roll, Some(&track), None, Some(&zero), None, 1, 1),
        1,
        RecoveryLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(explicit.raw_mapping_index, Some(0));
    assert_eq!(explicit.mapping_origin, Mapping::Explicit);
    assert!(explicit.description.is_none());
    assert_eq!(explicit.preceding_samples(), Ok(0));
    let absent = resolve(
        scope(Kind::Roll, Some(&track), None, None, None, 1, 1),
        1,
        RecoveryLimits::default(),
        1,
    )
    .unwrap();
    assert_eq!(absent.raw_mapping_index, None);
    assert_eq!(absent.mapping_origin, Mapping::Absent);
    let empty = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 0, 0, &[]),
        RecoveryLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        resolve(
            scope(Kind::Roll, Some(&track), None, Some(&empty), None, 1, 1),
            1,
            RecoveryLimits::default(),
            1
        ),
        Err(Error::Unsupported("partial recovery map coverage"))
    ));
    let defaulted = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 2, 2, 1, &[-2], 0),
        Syntax::Mpeg2026Draft,
        RecoveryLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        resolve(
            scope(Kind::Roll, Some(&defaulted), None, Some(&zero), None, 1, 1),
            1,
            RecoveryLimits::default(),
            1
        ),
        Err(Error::Unsupported(
            "explicit zero recovery index with default remains unresolved"
        ))
    ));
}

#[test]
fn unknown_recovery_syntax_lengths_parameters_and_dangling_indexes_are_not_repaired() {
    let pool = Budget::new(65536).unwrap();
    let valid = wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-1], 0);
    let table = RecoveryDescriptions::parse(
        valid.clone(),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    let modern = wire::descriptions(&pool, Kind::Roll, 2, 2, 1, &[-1], 0);
    assert!(matches!(
        RecoveryDescriptions::parse(modern, Syntax::Versions0And1, RecoveryLimits::default()),
        Err(Error::Unsupported(
            "recovery version2 requires explicit draft profile"
        ))
    ));
    let mut unknown = valid.payload().to_vec();
    unknown[0] = 3;
    assert!(matches!(
        RecoveryDescriptions::parse(
            wire::boxed(&pool, b"sgpd", &unknown, 0),
            Syntax::Mpeg2026Draft,
            RecoveryLimits::default()
        ),
        Err(Error::Unsupported(_))
    ));
    let mut flags = valid.payload().to_vec();
    flags[3] = 4;
    assert!(matches!(
        RecoveryDescriptions::parse(
            wire::boxed(&pool, b"sgpd", &flags, 0),
            Syntax::Mpeg2026Draft,
            RecoveryLimits::default()
        ),
        Err(Error::Unsupported(_))
    ));
    let mut short = valid.payload().to_vec();
    short.pop();
    assert!(matches!(
        RecoveryDescriptions::parse(
            wire::boxed(&pool, b"sgpd", &short, 0),
            Syntax::Versions0And1,
            RecoveryLimits::default()
        ),
        Err(Error::Truncated)
    ));
    let variable = wire::descriptions(&pool, Kind::Roll, 1, 0, 0, &[-1], 0);
    let mut extended = variable.payload().to_vec();
    extended[16..20].copy_from_slice(&3u32.to_be_bytes());
    extended.push(0);
    let opaque = RecoveryDescriptions::parse(
        wire::boxed(&pool, b"sgpd", &extended, 0),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        entry(&opaque, 1, 1),
        Err(Error::Unsupported("recovery entry extension"))
    ));
    let map = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 0, 0, &[(1, 2)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        resolve(
            scope(Kind::Roll, Some(&table), None, Some(&map), None, 1, 1),
            1,
            RecoveryLimits::default(),
            1
        ),
        Err(Error::Invalid("recovery description index"))
    ));
    let map = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 1, 123, &[(1, 1)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        resolve(
            scope(Kind::Roll, Some(&table), None, Some(&map), None, 1, 1),
            1,
            RecoveryLimits::default(),
            1
        ),
        Err(Error::Unsupported("recovery grouping parameter semantics"))
    ));
    assert_eq!(table.raw().bytes().as_ref(), valid.bytes().as_ref());
}

#[test]
fn recovery_work_yields_cancel_is_sticky_and_run_limit_is_cumulative() {
    let pool = Budget::new(65536).unwrap();
    let table = RecoveryDescriptions::parse(
        wire::descriptions(&pool, Kind::Roll, 1, 2, 0, &[-1], 0),
        Syntax::Versions0And1,
        RecoveryLimits::default(),
    )
    .unwrap();
    let map = RecoveryMappings::parse(
        wire::mappings(&pool, Kind::Roll, 0, 0, &[(1, 1), (1, 1)]),
        RecoveryLimits::default(),
    )
    .unwrap();
    let context = scope(Kind::Roll, Some(&table), None, Some(&map), None, 1, 2);
    let mut lookup = RecoveryLookup::new(context, 1, RecoveryLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(0), &cancel),
        Ok(BuildStatus::Yield)
    );
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        lookup.poll(&mut WorkBudget::new(4096), &cancel),
        Err(Error::Cancelled)
    );
    assert!(matches!(
        resolve(
            context,
            1,
            RecoveryLimits {
                max_runs: 1,
                ..RecoveryLimits::default()
            },
            1
        ),
        Err(Error::Limit {
            requested: 2,
            maximum: 1
        })
    ));
}
