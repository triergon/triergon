//! Original bit-authored controls; the production implementation is not an oracle.
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Budget, Bytes, Error, SourceId, Time, WorkBudget};
use triergon_framing::{
    DtsCoreField as F, DtsHeaderInspector, DtsInspection, DtsInspectionIdentity,
    DtsInspectionIssues as I, DtsInspectionSourceStatus as S, DtsInspectionStep, FieldObservation,
};

fn identity() -> DtsInspectionIdentity {
    DtsInspectionIdentity {
        source: SourceId(4201),
        generation: 19,
        offset: 73,
    }
}

// This sequential MSB-first bit writer is independent of the inspector's table.
fn authored(cpf: bool, changes: &[(&str, u32)]) -> Vec<u8> {
    let mut bits = String::new();
    let base = [
        ("sync", 32, 0x7ffe8001),
        ("normal", 1, 1),
        ("short", 5, 31),
        ("cpf", 1, u32::from(cpf)),
        ("nblks", 7, 15),
        ("fsize", 14, 95),
        ("amode", 6, 2),
        ("sfreq", 4, 13),
        ("rate", 5, 15),
        ("fixed", 1, 0),
        ("dynamic", 1, 0),
        ("timestamp", 1, 0),
        ("auxiliary", 1, 0),
        ("hdcd", 1, 0),
        ("extension_id", 3, 0),
        ("extension", 1, 0),
        ("audio_sync", 1, 0),
        ("lfe", 2, 0),
        ("history", 1, 1),
        ("hcrc", if cpf { 16 } else { 0 }, 0x1234),
        ("filter", 1, 0),
        ("version", 4, 7),
        ("copy_history", 2, 0),
        ("pcm_resolution", 3, 0),
        ("sum_front", 1, 0),
        ("sum_surround", 1, 0),
        ("dialog_word", 4, 11),
    ];
    for (name, width, default) in base {
        if width == 0 {
            continue;
        }
        let value = changes
            .iter()
            .find(|(key, _)| *key == name)
            .map_or(default, |(_, value)| *value);
        if width != 32 {
            assert!(value < (1 << width));
        }
        bits.push_str(&format!("{value:0width$b}", width = width as usize));
    }
    assert_eq!(bits.len(), if cpf { 120 } else { 104 });
    bits.as_bytes()
        .chunks_exact(8)
        .map(|chunk| u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 2).unwrap())
        .collect()
}

fn finish(job: &mut DtsHeaderInspector, quantum: u64) -> DtsInspection {
    for _ in 0..512 {
        match job
            .step(&mut WorkBudget::new(quantum), &AtomicBool::new(false))
            .unwrap()
        {
            DtsInspectionStep::Yield => {}
            DtsInspectionStep::Ready(value) => return value,
            DtsInspectionStep::End(_) => panic!("publication missing"),
            DtsInspectionStep::NeedInput(_) => panic!("owned record requested input"),
        }
    }
    panic!("bounded progress failed");
}

fn inspect(raw: &[u8], quantum: u64) -> DtsInspection {
    let pool = Budget::new(65536).unwrap();
    let source = Bytes::copy_from_slice(&pool, raw).unwrap();
    let mut job = DtsHeaderInspector::new(&pool, identity(), source, 256).unwrap();
    finish(&mut job, quantum)
}

#[test]
fn canonical_cpf_absence_and_coded_zero_are_distinct() {
    let plain = inspect(&authored(false, &[]), 1);
    assert!(plain.syntax_complete());
    assert_eq!(plain.field(F::Hcrc), FieldObservation::NotInSyntax);
    assert_eq!(plain.required_header_bits(), Some(104));
    assert!(plain.issues().is_empty());
    let crc = inspect(&authored(true, &[("hcrc", 0)]), 1);
    assert_eq!(crc.field(F::Hcrc).value(), Some(0));
    assert_eq!(crc.required_header_bits(), Some(120));
    assert_eq!(crc.field(F::Hcrc).bit_range(), Some(88..104));
    assert_eq!(crc.field(F::Version).bit_range(), Some(105..109));
}

#[test]
fn all_prefix_lengths_preserve_missing_bits_without_zero_padding() {
    for cpf in [false, true] {
        let raw = authored(
            cpf,
            &[("dynamic", 1), ("copy_history", 3), ("dialog_word", 13)],
        );
        for length in 0..=raw.len() {
            let value = inspect(&raw[..length], 1);
            assert_eq!(value.source_bytes().as_ref(), &raw[..length]);
            assert_eq!(value.syntax_complete(), length == raw.len());
            assert_eq!(value.issues().contains(I::TRUNCATED), length < raw.len());
            for field in F::ALL {
                if let FieldObservation::Bits {
                    offset,
                    observed_bits,
                    ..
                } = value.field(field)
                {
                    assert!(usize::from(offset) + usize::from(observed_bits) <= length * 8);
                }
            }
        }
    }
    let raw = authored(false, &[]);
    let value = inspect(&raw[..9], 1);
    assert_eq!(value.field(F::Rate).value(), None);
    assert!(matches!(
        value.field(F::Rate),
        FieldObservation::Bits {
            observed_bits: 2,
            ..
        }
    ));
    assert_eq!(value.field(F::Version), FieldObservation::NotReached);
}

#[test]
fn all_codes_in_native_acceptance_disagreements_remain_explicit_source_facts() {
    for rate in [25, 26, 27, 28, 30, 31] {
        let value = inspect(&authored(false, &[("rate", rate)]), 1);
        assert_eq!(value.field(F::Rate).value(), Some(rate));
        assert!(value.issues().contains(I::BITRATE_CODE));
        assert_eq!(value.views().nominal_bitrate, None);
    }
    for sfreq in [14, 15] {
        let value = inspect(&authored(false, &[("sfreq", sfreq)]), 1);
        assert_eq!(value.field(F::SampleRate).value(), Some(sfreq));
        assert!(value.issues().contains(I::SAMPLE_RATE_CODE));
        assert_eq!(value.views().core_rate_hz, None);
        assert_eq!(value.views().normal_window, None);
    }
}

#[test]
fn all_channel_codes_preserve_known_and_user_defined_distinctions() {
    let known = [1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8];
    for mode in 0..64 {
        let value = inspect(&authored(false, &[("amode", mode)]), 2);
        assert_eq!(value.field(F::AudioMode).value(), Some(mode));
        assert_eq!(
            value.views().full_bandwidth_channels,
            known.get(mode as usize).copied()
        );
        assert!(value.issues().is_empty());
    }
}

#[test]
fn future_versions_and_extension_descriptors_do_not_become_decoder_support() {
    for version in 8..16 {
        let value = inspect(&authored(false, &[("version", version)]), 3);
        assert_eq!(value.field(F::Version).value(), Some(version));
        assert!(value.issues().contains(I::FUTURE_VERSION));
        assert_eq!(value.views().dialog_gain_db, None);
    }
    for extension_id in [1, 3, 4, 5, 7] {
        let value = inspect(
            &authored(false, &[("extension", 1), ("extension_id", extension_id)]),
            3,
        );
        assert_eq!(value.field(F::ExtensionId).value(), Some(extension_id));
        assert!(value.issues().contains(I::UNSUPPORTED_EXTENSION));
    }
}

#[test]
fn validation_collects_independent_issues_instead_of_hiding_later_codes() {
    let raw = authored(
        false,
        &[
            ("short", 0),
            ("nblks", 0),
            ("fsize", 0),
            ("sfreq", 0),
            ("rate", 31),
            ("fixed", 1),
            ("lfe", 3),
            ("pcm_resolution", 4),
            ("version", 9),
        ],
    );
    let value = inspect(&raw, 1);
    for issue in [
        I::NORMAL_SHORT,
        I::BLOCK_COUNT,
        I::FRAME_SIZE,
        I::SAMPLE_RATE_CODE,
        I::BITRATE_CODE,
        I::FIXED_BIT,
        I::LFE_CODE,
        I::PCM_RESOLUTION,
        I::FUTURE_VERSION,
    ] {
        assert!(value.issues().contains(issue));
    }
    assert_eq!(value.field(F::DialogWord).value(), Some(11));
    assert_eq!(value.views().declared_frame_bytes, Some(1));
}

#[test]
fn normal_count_time_gain_and_open_bitrate_views_are_exact() {
    for (version, expected) in [(5, None), (6, Some(-27)), (7, Some(-11)), (8, None)] {
        let value = inspect(&authored(false, &[("version", version), ("rate", 29)]), 7);
        assert_eq!(value.views().declared_frame_bytes, Some(96));
        assert_eq!(value.views().coded_window_samples, Some(512));
        assert_eq!(
            value.views().normal_window,
            if version <= 7 {
                Some(Time::new(4, 375).unwrap())
            } else {
                None
            }
        );
        assert_eq!(value.views().open_bitrate, Some(true));
        assert_eq!(value.views().nominal_bitrate, None);
        assert_eq!(value.views().dialog_gain_db, expected);
    }
}

#[test]
fn termination_is_raw_metadata_with_no_guessed_presentation_duration() {
    let value = inspect(&authored(false, &[("normal", 0), ("short", 10)]), 1);
    assert!(value.issues().contains(I::TERMINATION));
    assert_eq!(value.field(F::Short).value(), Some(10));
    assert_eq!(value.views().normal_window, None);
}

#[test]
fn noncanonical_sync_is_not_interpreted_as_a_canonical_header() {
    for sync in [0, 0x64582025, 0xffffffff] {
        let raw = authored(false, &[("sync", sync)]);
        let value = inspect(&raw, 1);
        assert!(value.issues().contains(I::NONCANONICAL_SYNC));
        assert_eq!(value.field(F::Sync).value(), Some(sync));
        assert_eq!(value.field(F::Normal), FieldObservation::NotReached);
        assert_eq!(value.views().declared_frame_bytes, None);
        assert_eq!(value.source_bytes().as_ref(), raw);
    }
}

#[test]
fn arbitrary_work_quanta_and_zero_work_keep_the_same_source_facts() {
    let pool = Budget::new(65536).unwrap();
    let raw = authored(true, &[]);
    let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let before = pool.used();
    let mut job = DtsHeaderInspector::new(&pool, identity(), source, 256).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap(),
        DtsInspectionStep::Yield
    ));
    assert_eq!(pool.used(), before);
    let expected = finish(&mut job, 1);
    match job
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        DtsInspectionStep::End(end) => {
            assert_eq!(end.source, identity().source);
            assert_eq!(end.offset, identity().offset + raw.len() as u64);
        }
        _ => panic!("record completion missing"),
    }
    for quantum in [2, 3, 7, 31, 1024] {
        let other = inspect(&raw, quantum);
        for field in F::ALL {
            assert_eq!(other.field(field), expected.field(field));
        }
        assert_eq!(other.issues(), expected.issues());
        assert_eq!(other.views(), expected.views());
    }
}

#[test]
fn inspection_requires_no_new_pool_allocation_even_with_full_budget_pressure() {
    let pool = Budget::new(65536).unwrap();
    let raw = authored(true, &[]);
    let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let blocker = pool.reserve(pool.available()).unwrap();
    let used = pool.used();
    let mut job = DtsHeaderInspector::new(&pool, identity(), source, 256).unwrap();
    let output = finish(&mut job, 1);
    assert_eq!(pool.used(), used);
    assert_eq!(output.source_bytes().as_ref(), raw);
    drop(output);
    drop(job);
    drop(blocker);
}

#[test]
fn cancellation_at_every_bit_releases_the_owned_source_and_remains_sticky() {
    let raw = authored(true, &[]);
    for units in 0..=122 {
        let pool = Budget::new(65536).unwrap();
        let baseline = pool.used();
        let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = DtsHeaderInspector::new(&pool, identity(), source, 256).unwrap();
        let step = job
            .step(&mut WorkBudget::new(units), &AtomicBool::new(false))
            .unwrap();
        drop(step);
        let cancel = AtomicBool::new(true);
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(Error::Cancelled)
        );
        cancel.store(false, Ordering::Relaxed);
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(Error::Cancelled)
        );
        assert_eq!(pool.used(), baseline);
    }
}

#[test]
fn source_identity_limit_foreign_pool_and_retained_capacity_are_checked() {
    let pool = Budget::new(65536).unwrap();
    let baseline = pool.used();
    let mut raw = authored(false, &[]);
    raw.extend([0xa5; 32]);
    let source = Bytes::copy_from_slice(&pool, &raw).unwrap();
    let same = source.clone();
    let mut job = DtsHeaderInspector::new(&pool, identity(), source, 256).unwrap();
    let output = finish(&mut job, 1);
    assert_eq!(output.identity(), identity());
    assert_eq!(output.source_bytes().as_ref(), raw);
    assert!(output.source_bytes().shares_storage_with(&same));
    let retained = output.source_bytes().slice(0..1).unwrap();
    drop(same);
    drop(job);
    drop(output);
    assert!(pool.used() > baseline);
    drop(retained);
    assert_eq!(pool.used(), baseline);
    let foreign = Budget::new(65536).unwrap();
    assert!(matches!(
        DtsHeaderInspector::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&foreign, &[0]).unwrap(),
            256
        ),
        Err(Error::ForeignPool)
    ));
    assert!(matches!(
        DtsHeaderInspector::new(
            &pool,
            identity(),
            Bytes::copy_from_slice(&pool, &[0; 16]).unwrap(),
            15
        ),
        Err(Error::Limit {
            requested: 16,
            maximum: 15
        })
    ));
    let overflow = DtsInspectionIdentity {
        offset: u64::MAX,
        ..identity()
    };
    assert!(matches!(
        DtsHeaderInspector::new(
            &pool,
            overflow,
            Bytes::copy_from_slice(&pool, &[0]).unwrap(),
            256
        ),
        Err(Error::Overflow)
    ));
}

#[test]
fn all_frozen396_inputs_keep_every_independent_complete_raw_field() {
    let mut rows = 0;
    let mut compared = 0;
    for line in include_str!("../fixtures/dts-inspect-source-v1.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let parts: Vec<_> = line.split('\t').collect();
        assert_eq!(parts.len(), 4);
        let raw: Vec<u8> = if parts[1] == "~" {
            Vec::new()
        } else {
            parts[1]
                .as_bytes()
                .chunks_exact(2)
                .map(|s| u8::from_str_radix(std::str::from_utf8(s).unwrap(), 16).unwrap())
                .collect()
        };
        let value = inspect(&raw, 1);
        assert_eq!(value.source_bytes().as_ref(), raw, "{}", parts[0]);
        let status = match value.source_status() {
            S::NormalHeader => "ok",
            S::Truncated => "truncated",
            S::Invalid => "invalid",
            S::Unsupported => "unsupported",
        };
        assert_eq!(status, parts[2], "{}", parts[0]);
        if parts[3] != "~" {
            let fields: Vec<_> = parts[3].split(',').collect();
            assert_eq!(fields.len(), 27);
            for (field, expected) in F::ALL.into_iter().zip(fields) {
                if expected == "-" {
                    assert_eq!(value.field(field), FieldObservation::NotInSyntax);
                } else {
                    assert_eq!(
                        value.field(field).value(),
                        Some(expected.parse().unwrap()),
                        "{} {:?}",
                        parts[0],
                        field
                    );
                }
                compared += 1;
            }
        }
        rows += 1;
    }
    assert_eq!(rows, 396);
    assert_eq!(compared, 9882);
}
