use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    AacAdtsParameters, AvcAnnexBConfig, AvcAnnexBJob, AvcAnnexBLimits, AvcConfigLimits,
    AvcConfigSource, AvcConfigViewParser, BuildStep, inspect_aac_adts_header,
};

const LIMITS: AvcAnnexBLimits = AvcAnnexBLimits {
    maximum_packet_bytes: 64,
    maximum_output_bytes: 64,
    maximum_nals: 8,
};

fn avc_config(width: u8) -> AvcAnnexBConfig {
    let pool = Budget::new(4096).unwrap();
    let raw = [
        1,
        66,
        0,
        30,
        0xfc | (width - 1),
        0xe1,
        0,
        2,
        0x67,
        42,
        1,
        0,
        2,
        0x68,
        43,
    ];
    let mut parser = AvcConfigViewParser::new(
        &pool,
        AvcConfigSource {
            source: SourceId(1),
            generation: 0,
            offset: 0,
        },
        Bytes::copy_from_slice(&pool, &raw).unwrap(),
        AvcConfigLimits {
            maximum_record_bytes: 32,
            maximum_nals: 8,
            maximum_nal_bytes: 8,
        },
    )
    .unwrap();
    let BuildStep::Ready(record) = parser
        .step(&mut WorkBudget::new(512), &AtomicBool::new(false))
        .unwrap()
    else {
        panic!("expected complete AVC configuration");
    };
    AvcAnnexBConfig::new(record).unwrap()
}

#[test]
fn borrowed_avc_output_preserves_every_nal_and_prepends_parameters_only_for_idr() {
    let config = avc_config(4);
    let cancel = AtomicBool::new(false);
    for nal in [0x41, 0x65] {
        let sample = [0, 0, 0, 1, 0x06, 0, 0, 0, 2, nal, 5];
        let mut job = AvcAnnexBJob::new(&config, &sample, LIMITS).unwrap();
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel).unwrap(),
            BuildStep::Yield
        ));
        let BuildStep::Ready(unit) = job.step(&mut WorkBudget::new(1), &cancel).unwrap() else {
            panic!("expected a bounded AVC access unit");
        };
        let chunks: Vec<_> = unit.segments().collect();
        assert!(std::ptr::eq(chunks.last().copied().unwrap(), &sample[9..]));
        assert_eq!(unit.is_idr(), nal == 0x65);
        let mut expected = Vec::new();
        if nal == 0x65 {
            expected.extend_from_slice(&[0, 0, 0, 1, 0x67, 42, 0, 0, 0, 1, 0x68, 43]);
        }
        expected.extend_from_slice(&[0, 0, 0, 1, 0x06, 0, 0, 0, 1, nal, 5]);
        assert_eq!(chunks.concat(), expected);
        assert_eq!(unit.byte_len(), expected.len());
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel).unwrap(),
            BuildStep::Complete
        ));
    }
}

#[test]
fn avc_malformed_tail_limits_and_cancel_refuse_without_partial_publication() {
    let config = avc_config(4);
    let cancel = AtomicBool::new(false);
    let malformed = [0, 0, 0, 1, 0x65, 0, 0, 0, 2, 0x06];
    let mut job = AvcAnnexBJob::new(&config, &malformed, LIMITS).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &cancel).unwrap(),
        BuildStep::Yield
    ));
    for _ in 0..2 {
        assert!(matches!(
            job.step(&mut WorkBudget::new(1), &cancel),
            Err(Error::Truncated)
        ));
    }

    for (input, error) in [
        (&[0, 0, 0, 0][..], Error::Invalid("empty AVC NAL")),
        (
            &[0, 0, 0, 1, 0xe5][..],
            Error::Invalid("NAL forbidden_zero_bit"),
        ),
    ] {
        let mut job = AvcAnnexBJob::new(&config, input, LIMITS).unwrap();
        assert_eq!(
            job.step(&mut WorkBudget::new(1), &cancel).err(),
            Some(error)
        );
    }

    let sample = [0, 0, 0, 1, 0x65];
    let limits = AvcAnnexBLimits {
        maximum_output_bytes: 16,
        ..LIMITS
    };
    let mut job = AvcAnnexBJob::new(&config, &sample, limits).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Limit {
            requested: 17,
            maximum: 16
        })
    ));
    let mut job = AvcAnnexBJob::new(&config, &sample, LIMITS).unwrap();
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        job.step(&mut WorkBudget::new(1), &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn adts_header_for_a_borrowed_access_unit_checks_the_thirteen_bit_wire_limit() {
    let params = AacAdtsParameters {
        mpeg2_id: false,
        frequency_index: 3,
        channel_configuration: 2,
        buffer_fullness: 0x7ff,
        private_bit: false,
        original_copy: false,
        home: false,
        copyright_id_bit: false,
        copyright_id_start: false,
    };
    let header = params.header_for_payload(8184).unwrap();
    assert_eq!(
        inspect_aac_adts_header(&header).unwrap().frame_length(),
        8191
    );
    assert_eq!(
        params.header_for_payload(8185),
        Err(Error::Limit {
            requested: 8192,
            maximum: 8191
        })
    );
}

#[test]
fn access_unit_uses_the_configuration_length_width() {
    let cancel = AtomicBool::new(false);
    for width in [1, 2, 4] {
        let config = avc_config(width);
        let wire = [0, 0, 0, 2, 0x41, 6];
        let mut job = AvcAnnexBJob::new(&config, &wire[4 - usize::from(width)..], LIMITS).unwrap();
        let BuildStep::Ready(unit) = job.step(&mut WorkBudget::new(1), &cancel).unwrap() else {
            panic!("valid selected AVC length width failed");
        };
        assert_eq!(
            unit.segments().collect::<Vec<_>>().concat(),
            [0, 0, 0, 1, 0x41, 6]
        );
    }
}
