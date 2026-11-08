use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep, HevcAnnexBConfig, HevcAnnexBLimits, HvcConfigLimits, HvcConfigParser,
    HvcConfigSource, HvcConfiguration,
};

const LIMITS: HevcAnnexBLimits = HevcAnnexBLimits {
    maximum_packet_bytes: 128,
    maximum_output_bytes: 128,
    maximum_nals: 8,
};
static OFF: AtomicBool = AtomicBool::new(false);
const START_CODE: &[u8] = &[0, 0, 0, 1];

fn hvcc(width: u8, arrays: &[(u8, u8, u8)]) -> (HvcConfiguration, Bytes) {
    let pool = Budget::new(4096).unwrap();
    let mut record = vec![0; 23];
    record[0] = 1;
    record[13] = 0xf0;
    record[15..19].copy_from_slice(&[0xfc, 0xfd, 0xf8, 0xf8]);
    record[21] = 0x0c | (width - 1);
    record[22] = arrays.len() as u8;
    for &(declaration, nal_type, tag) in arrays {
        record.extend_from_slice(&[declaration | 0x80, 0, 1, 0, 3, nal_type << 1, 1, tag]);
    }
    let raw = Bytes::copy_from_slice(&pool, &record).unwrap();
    let mut parser = HvcConfigParser::new(
        &pool,
        HvcConfigSource {
            source: SourceId(1),
            generation: 0,
            offset: 0,
        },
        raw.clone(),
        HvcConfigLimits {
            maximum_record_bytes: 128,
            maximum_arrays: 8,
            maximum_nals: 8,
            maximum_nal_bytes: 8,
        },
    )
    .unwrap();
    let BuildStep::Ready(record) = parser.step(&mut WorkBudget::new(512), &OFF).unwrap() else {
        panic!("expected full hvcC record");
    };
    (record, raw)
}

#[test]
fn irap_types_prepend_only_parameter_sets_in_declaration_order() {
    let (record, raw) = hvcc(
        1,
        &[
            (34, 34, 11),
            (39, 39, 12),
            (32, 32, 13),
            (33, 33, 14),
            (32, 32, 15),
            (40, 40, 16),
        ],
    );
    let config = HevcAnnexBConfig::new(record).unwrap();
    for nal_type in 15..=24 {
        let sample = [
            3,
            39 << 1,
            1,
            21,
            3,
            nal_type << 1,
            1,
            22,
            3,
            34 << 1,
            1,
            23,
            3,
            40 << 1,
            1,
            24,
        ];
        let mut job = config.job(&sample, LIMITS).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &OFF).unwrap(),
            BuildStep::Yield
        ));
        for _ in 0..3 {
            assert!(matches!(
                job.step(&mut WorkBudget::new(1), &OFF).unwrap(),
                BuildStep::Yield
            ));
        }
        let BuildStep::Ready(unit) = job.step(&mut WorkBudget::new(1), &OFF).unwrap() else {
            panic!("expected full access unit");
        };
        let random_access = (16..=23).contains(&nal_type);
        let chunks: Vec<_> = unit.segments().collect();
        let mut expected: Vec<&[u8]> = Vec::new();
        if random_access {
            for array in [0, 2, 3, 4] {
                let start = 23 + array * 8 + 5;
                expected.push(&raw[start..start + 3]);
            }
        }
        expected.extend([
            &sample[1..4],
            &sample[5..8],
            &sample[9..12],
            &sample[13..16],
        ]);
        assert_eq!(unit.is_random_access(), random_access);
        assert_eq!(chunks.len(), expected.len() * 2);
        for (pair, original) in chunks.as_chunks::<2>().0.iter().zip(expected) {
            assert_eq!(pair[0], START_CODE);
            assert!(std::ptr::eq(pair[1], original));
        }
        assert_eq!(unit.byte_len(), chunks.iter().map(|part| part.len()).sum());
        assert!(matches!(
            job.step(&mut WorkBudget::new(0), &OFF).unwrap(),
            BuildStep::Complete
        ));
    }
}

#[test]
fn configuration_width_selects_sample_length_fields() {
    for width in [1, 2, 4] {
        let (record, _) = hvcc(width, &[]);
        let config = HevcAnnexBConfig::new(record).unwrap();
        let sample = [0, 0, 0, 2, 0x02, 1];
        let mut job = config
            .job(&sample[4 - usize::from(width)..], LIMITS)
            .unwrap();
        let BuildStep::Ready(unit) = job.step(&mut WorkBudget::new(1), &OFF).unwrap() else {
            panic!("expected checked access unit");
        };
        assert!(!unit.is_random_access());
        assert!(unit.segments().eq([START_CODE, &sample[4..]]));
    }
}

#[test]
fn bad_tails_headers_limits_and_cancel_do_not_publish() {
    let (record, _) = hvcc(4, &[(32, 32, 3)]);
    let config = HevcAnnexBConfig::new(record).unwrap();
    let truncated = [0, 0, 0, 2, 0x20, 1, 0, 0, 0, 2, 0x02];
    let mut job = config.job(&truncated, LIMITS).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &OFF).unwrap(),
        BuildStep::Yield
    ));
    for _ in 0..2 {
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &OFF).err(),
            Some(Error::Truncated)
        );
    }
    for (input, error) in [
        (&[0, 0, 0, 0][..], Error::Invalid("empty HEVC NAL")),
        (&[0, 0, 0, 1, 0x20][..], Error::Truncated),
        (
            &[0, 0, 0, 2, 0xa0, 1][..],
            Error::Invalid("NAL forbidden_zero_bit"),
        ),
        (
            &[0, 0, 0, 2, 0x20, 0][..],
            Error::Invalid("HEVC temporal_id_plus1 is zero"),
        ),
    ] {
        let mut job = config.job(input, LIMITS).unwrap();
        assert_eq!(job.step(&mut WorkBudget::new(1), &OFF).err(), Some(error));
    }

    let sample = [0, 0, 0, 2, 0x20, 1];
    let too_small = HevcAnnexBLimits {
        maximum_packet_bytes: 5,
        ..LIMITS
    };
    assert_eq!(
        config.job(&sample, too_small).err(),
        Some(Error::Limit {
            requested: 6,
            maximum: 5
        })
    );
    let too_small = HevcAnnexBLimits {
        maximum_output_bytes: 12,
        ..LIMITS
    };
    let mut job = config.job(&sample, too_small).unwrap();
    assert_eq!(
        job.step(&mut WorkBudget::new(1), &OFF).err(),
        Some(Error::Limit {
            requested: 13,
            maximum: 12
        })
    );

    let two = [sample, sample].concat();
    let one = HevcAnnexBLimits {
        maximum_nals: 1,
        ..LIMITS
    };
    let mut job = config.job(&two, one).unwrap();
    assert_eq!(
        job.step(&mut WorkBudget::new(2), &OFF).err(),
        Some(Error::Limit {
            requested: 2,
            maximum: 1
        })
    );
    let mut job = config.job(&two, LIMITS).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &OFF).unwrap(),
        BuildStep::Yield
    ));
    assert_eq!(
        job.step(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .err(),
        Some(Error::Cancelled)
    );
    assert_eq!(
        job.step(&mut WorkBudget::new(2), &OFF).err(),
        Some(Error::Cancelled)
    );
}

#[test]
fn parameter_set_array_mismatches_refuse_configuration() {
    for pair in [(32, 39, 7), (39, 34, 8), (32, 33, 9)] {
        let (record, _) = hvcc(4, &[pair]);
        assert_eq!(
            HevcAnnexBConfig::new(record).err(),
            Some(Error::Invalid("hvcC parameter-set declaration"))
        );
    }
}
