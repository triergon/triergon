use super::*;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId};

const OPUS: [u8; 11] = [0, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0];

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}
fn entry(pool: &Budget, extensions: &[u8], at: u64) -> SampleDescription {
    let mut body = vec![0; 78];
    body[7] = 1;
    body.extend_from_slice(extensions);
    let raw = BoxView::new(
        Bytes::copy_from_slice(pool, &atom(b"raw ", &body)).unwrap(),
        at,
    )
    .unwrap();
    SampleDescription::parse(raw, 1).unwrap()
}
fn job(d: &SampleDescription, limits: Limits) -> SampleEntryHeaderJob {
    SampleEntryHeaderJob::new(d, EntryLayout::Visual, limits, SampleEntryHeaderProfile).unwrap()
}
fn finish(mut j: SampleEntryHeaderJob, quantum: u64) -> Result<SampleEntryHeader> {
    let cancel = AtomicBool::new(false);
    for _ in 0..1000 {
        if j.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Ready {
            return j.try_finish(&cancel);
        }
    }
    panic!("bounded owned-entry inspection stalled")
}
fn cause(data: &[u8]) -> Option<HeaderCause> {
    let pool = Budget::new(1 << 20).unwrap();
    let description = entry(&pool, data, 0);
    finish(job(&description, Limits::default()), 1)
        .unwrap()
        .rejection
        .map(|x| x.cause)
}

#[test]
fn nonempty_short_opus_is_demux_rejected_without_source_rejection() {
    for size in 1..11 {
        assert_eq!(
            cause(&atom(b"dOps", &OPUS[..size])),
            Some(HeaderCause::OpusBodyLength { declared: size })
        );
    }
}
#[test]
fn empty_demux_dispatch_and_complete_version_zero_are_distinct_controls() {
    assert_eq!(cause(&atom(b"dOps", &[])), None);
    assert_eq!(cause(&atom(b"dOps", &OPUS)), None);
}
#[test]
fn opus_version_bits_are_retained_not_normalized() {
    for version in [1, 128, 255] {
        let mut raw = OPUS;
        raw[0] = version;
        assert_eq!(
            cause(&atom(b"dOps", &raw)),
            Some(HeaderCause::OpusVersion { raw: version })
        );
    }
}
#[test]
fn earlier_configuration_causes_collision_and_order_remains_source_order() {
    for kind in [b"avcC", b"hvcC", b"av1C", b"dOps"] {
        let first = if kind == b"dOps" {
            OPUS.as_slice()
        } else {
            &[7, 8, 9]
        };
        let mut raw = atom(kind, first);
        raw.extend(atom(b"dOps", &OPUS));
        assert_eq!(
            cause(&raw),
            Some(HeaderCause::OpusConfigurationCollision { earlier: *kind })
        );
    }
    let mut reversed = atom(b"dOps", &OPUS);
    reversed.extend(atom(b"avcC", &[7, 8, 9]));
    assert_eq!(cause(&reversed), None);
}
#[test]
fn empty_or_opaque_configuration_does_not_invent_installed_extradata() {
    let mut raw = atom(b"avcC", &[]);
    raw.extend(atom(b"zzzz", &[7, 8, 9]));
    raw.extend(atom(b"dOps", &OPUS));
    assert_eq!(cause(&raw), None);
}
#[test]
fn demux_dts_signed_frequency_failure_keeps_unsigned_bits() {
    for rate in [0u32, 1 << 31, u32::MAX] {
        let mut body = [0u8; 20];
        body[..4].copy_from_slice(&rate.to_be_bytes());
        assert_eq!(
            cause(&atom(b"ddts", &body)),
            Some(HeaderCause::DtsSampleRate { raw: rate })
        );
    }
    let mut body = [0xff; 20];
    body[..4].copy_from_slice(&48000u32.to_be_bytes());
    assert_eq!(cause(&atom(b"ddts", &body)), None);
}
#[test]
fn short_dts_is_opaque_not_an_invented_demux_fatal_overread() {
    for size in [0, 1, 7, 19] {
        assert_eq!(cause(&atom(b"ddts", &vec![0; size])), None);
    }
}
#[test]
fn all_raw_bytes_and_backing_survive_at_zero_extra_budget() {
    let pool = Budget::new(4096).unwrap();
    let raw = atom(b"dOps", &[255, 3, 9]);
    let description = entry(&pool, &raw, 101);
    let used = pool.used();
    let backing = description.raw().bytes().retained_storage_bytes();
    let blocker = pool.reserve(pool.available()).unwrap();
    let j = job(&description, Limits::default());
    drop(description);
    let out = finish(j, 1).unwrap();
    assert_eq!(pool.available(), 0);
    assert_eq!(out.description.raw().bytes().len(), 86 + raw.len());
    assert_eq!(&out.description.raw().payload()[78..], raw);
    assert_eq!(out.rejection.as_ref().unwrap().raw.payload(), &[255, 3, 9]);
    assert!(
        out.description
            .raw()
            .bytes()
            .shares_storage_with(out.rejection.as_ref().unwrap().raw.bytes())
    );
    drop(blocker);
    assert_eq!(pool.used(), used);
    drop(out);
    assert_eq!(pool.used(), used - backing);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn zero_work_yields_then_one_unit_steps_make_progress_without_reparse() {
    let pool = Budget::new(4096).unwrap();
    let d = entry(&pool, &atom(b"dOps", &[1]), 0);
    let mut j = job(&d, Limits::default());
    let mut work = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut work, &AtomicBool::new(false)).unwrap(),
        BuildStatus::Yield
    );
    assert_eq!(work.spent(), 0);
    let out = finish(j, 1).unwrap();
    assert_eq!(out.inspected_children, 1);
    assert_eq!(out.demux_open_result(), Some(-1_094_995_529));
}
#[test]
fn cancellation_precedes_zero_work_and_releases_job_backings_stickily() {
    let pool = Budget::new(4096).unwrap();
    let d = entry(&pool, &atom(b"dOps", &[1]), 0);
    let mut j = job(&d, Limits::default());
    drop(d);
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(pool.used(), Budget::control_storage_bytes());
    assert_eq!(
        j.poll(&mut WorkBudget::new(4), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn cancellation_at_publication_does_not_publish_retained_demux_result() {
    let pool = Budget::new(4096).unwrap();
    let d = entry(&pool, &atom(b"dOps", &[1]), 0);
    let mut j = job(&d, Limits::default());
    assert_eq!(
        j.poll(&mut WorkBudget::new(10), &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Ready
    );
    assert_eq!(
        j.try_finish(&AtomicBool::new(true)).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.try_finish(&AtomicBool::new(false)).unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn later_malformed_source_header_wins_over_earlier_demux_rejection() {
    let pool = Budget::new(4096).unwrap();
    let mut raw = atom(b"dOps", &[1]);
    raw.extend([0, 0, 0, 100, b'z', b'z', b'z', b'z']);
    let d = entry(&pool, &raw, 0);
    assert_eq!(
        finish(job(&d, Limits::default()), 1).unwrap_err(),
        Error::Truncated
    );
}
#[test]
fn count_limit_stays_source_error_after_a_demux_rejection() {
    let pool = Budget::new(4096).unwrap();
    let mut raw = atom(b"dOps", &[1]);
    raw.extend(atom(b"zzzz", &[]));
    let d = entry(&pool, &raw, 0);
    assert_eq!(
        finish(
            job(
                &d,
                Limits {
                    max_boxes: 1,
                    ..Limits::default()
                }
            ),
            1
        )
        .unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
}
#[test]
fn source_coordinates_above_four_gib_remain_checked_u64() {
    let pool = Budget::new(4096).unwrap();
    let at = (1u64 << 32) + 97;
    let d = entry(&pool, &atom(b"dOps", &[7]), at);
    let out = finish(job(&d, Limits::default()), 1).unwrap();
    assert_eq!(out.description.raw().header().start, at);
    assert_eq!(out.rejection.unwrap().raw.header().start, at + 86);
}
#[test]
fn description_boundaries_do_not_inherit_prior_configuration() {
    let pool = Budget::new(4096).unwrap();
    let first = entry(&pool, &atom(b"avcC", &[7, 8, 9]), 0);
    let second = entry(&pool, &atom(b"dOps", &OPUS), 0);
    assert_eq!(
        finish(job(&first, Limits::default()), 1)
            .unwrap()
            .demux_open_result(),
        None
    );
    assert_eq!(
        finish(job(&second, Limits::default()), 1)
            .unwrap()
            .demux_open_result(),
        None
    );
}
#[test]
fn a_taken_result_is_not_published_twice() {
    let pool = Budget::new(4096).unwrap();
    let d = entry(&pool, &[], 0);
    let mut j = job(&d, Limits::default());
    assert_eq!(
        j.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Ready
    );
    j.try_finish(&AtomicBool::new(false)).unwrap();
    assert_eq!(
        j.try_finish(&AtomicBool::new(false)).unwrap_err(),
        Error::Invalid("v7 header not ready or already taken")
    );
}
#[test]
fn owned_source_snapshot_does_not_consume_or_depend_on_live_cursor_reset() {
    let pool = Budget::new(4096).unwrap();
    let d = entry(&pool, &atom(b"dOps", &[1]), 0);
    let j = job(&d, Limits::default());
    let mut input = InputCursor::new(
        &pool,
        SourceId(5),
        SourceCapabilities {
            length: Some(10),
            random_access: true,
        },
        1,
    )
    .unwrap();
    let old = input.token();
    input.reset(9).unwrap();
    let changed = input.token();
    assert_ne!(old, changed);
    let out = finish(j, 1).unwrap();
    assert_eq!(
        out.description.raw().bytes().as_ref(),
        d.raw().bytes().as_ref()
    );
    assert_eq!(input.token(), changed);
    assert_eq!(input.position(), 9);
}

#[test]
fn whole_field_only_envelope_does_not_install_codec_private_configuration() {
    for kind in [b"avcC", b"hvcC", b"av1C"] {
        let mut raw = atom(kind, &atom(b"fiel", &[1, 0]));
        raw.extend(atom(b"dOps", &OPUS));
        assert_eq!(cause(&raw), None);
    }
}
#[test]
fn field_lookalikes_and_eight_byte_headers_remain_direct_configurations() {
    let empty = atom(b"fiel", &[]);
    let mut wrong_size = atom(b"fiel", &[1, 0]);
    wrong_size[..4].copy_from_slice(&9u32.to_be_bytes());
    for field in [empty, wrong_size] {
        let mut raw = atom(b"avcC", &field);
        raw.extend(atom(b"dOps", &OPUS));
        assert_eq!(
            cause(&raw),
            Some(HeaderCause::OpusConfigurationCollision { earlier: *b"avcC" })
        );
    }
}
#[test]
fn field_only_envelope_does_not_clear_an_earlier_real_configuration() {
    let mut raw = atom(b"avcC", &[7, 8, 9]);
    raw.extend(atom(b"hvcC", &atom(b"fiel", &[1, 0])));
    raw.extend(atom(b"dOps", &OPUS));
    assert_eq!(
        cause(&raw),
        Some(HeaderCause::OpusConfigurationCollision { earlier: *b"avcC" })
    );
}
