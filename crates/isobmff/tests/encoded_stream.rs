mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_isobmff::{
    BoxView, ConfigurationLimits, SampleTable, TrackHeader,
    compat::mov::{
        encoded::{EncodedEntryKind, EncodedStream, EncodedStreamJob, EncodedStreamProfile},
        packets::CursorStatus,
    },
};
const AVC: &[u8] = include_bytes!("fixtures/encoded-v1/control-avc-aac-front.mp4");
const HEVC: &[u8] = include_bytes!("fixtures/encoded-v1/control-hevc-aac-front.mp4");
const AAC: &[u8] = include_bytes!("fixtures/encoded-v1/control-aac-front.mp4");
const AC3: &[u8] = include_bytes!("fixtures/encoded-v1/control-ac3-front.mp4");
const EAC3: &[u8] = include_bytes!("fixtures/encoded-v1/control-eac3-front.mp4");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    length: u64,
}
impl Source {
    fn new(data: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, data).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let trak = children(movie, 0)
            .unwrap()
            .into_iter()
            .find(|x| x.header().kind == *b"trak")
            .unwrap();
        let mdia = child(&trak, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&trak, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = table(
            child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
            &pool,
            1,
        )
        .unwrap();
        Self {
            pool,
            header,
            table,
            length: data.len() as u64,
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(812),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn job(&self, input: &InputCursor) -> EncodedStreamJob<'_> {
        EncodedStreamJob::new(
            &self.header,
            &self.table,
            input,
            Some(*b"isom"),
            ConfigurationLimits::default(),
            EncodedStreamProfile,
        )
        .unwrap()
    }
}
fn complete(
    job: &mut EncodedStreamJob<'_>,
    input: &mut InputCursor,
    q: u64,
) -> Result<EncodedStream, Error> {
    for _ in 0..20000 {
        let mut work = WorkBudget::new(q);
        let state = job.poll(input, &mut work, &AtomicBool::new(false))?;
        assert!(work.spent() <= q);
        if state == CursorStatus::Ready {
            return job.take(input, &AtomicBool::new(false));
        }
    }
    panic!("bounded encoded metadata did not progress")
}
fn config(data: &Source, kind: &[u8; 4], skip: usize) -> BoxView {
    let mut scan = data.table.descriptions()[0]
        .raw()
        .children(skip, triergon_isobmff::Limits::default())
        .unwrap();
    loop {
        if let triergon_isobmff::BoxStep::Box(b) = scan.poll(&mut WorkBudget::new(16)).unwrap()
            && b.header().kind == *kind
        {
            return b;
        }
    }
}
#[test]
fn avc_catalogue_borrows_exact_avcc_and_preserves_source_entry() {
    let p = Source::new(AVC);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.kind(), EncodedEntryKind::Avc);
    assert_eq!(r.demux().codec, "h264");
    assert_eq!(
        r.demux().extradata.as_ref(),
        config(&p, b"avcC", 78).payload()
    );
    assert_eq!(
        r.source_description().raw().bytes().as_ref(),
        p.table.descriptions()[0].raw().bytes().as_ref()
    );
}
#[test]
fn hevc_keeps_entire_opaque_record_without_reparsing_or_stripping() {
    let p = Source::new(HEVC);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.kind(), EncodedEntryKind::Hevc);
    assert_eq!(r.demux().codec, "hevc");
    assert_eq!(
        r.demux().extradata.as_ref(),
        config(&p, b"hvcC", 78).payload()
    );
    assert!(r.demux().extradata.len() > 100);
    assert!(r.demux().extradata.retained_capacity() > r.demux().extradata.len());
}
#[test]
fn aac_uses_esds_object_namespace_and_retains_whole_wrapper() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.kind(), EncodedEntryKind::Aac);
    assert_eq!(r.demux().codec, "aac");
    assert_eq!(r.demux().media_type, 1);
    assert_eq!(r.demux().extradata.as_ref(), [0x11, 0x88, 0x56, 0xe5, 0]);
    assert_eq!(
        r.configuration_box().bytes().as_ref(),
        config(&p, b"esds", 28).bytes().as_ref()
    );
}
#[test]
fn ac3_source_dac3_is_not_silently_returned_as_demux_extradata() {
    let p = Source::new(AC3);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.demux().codec, "ac3");
    assert!(r.demux().extradata.is_empty());
    assert_eq!(
        r.configuration_box().payload(),
        config(&p, b"dac3", 28).payload()
    );
    assert_eq!(r.configuration_box().payload().len(), 3);
}
#[test]
fn eac3_source_dec3_stays_observable_with_demux_empty_extradata() {
    let p = Source::new(EAC3);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.demux().codec, "eac3");
    assert!(r.demux().extradata.is_empty());
    assert_eq!(
        r.configuration_box().payload(),
        config(&p, b"dec3", 28).payload()
    );
}
#[test]
fn compressor_name_is_source_metadata_not_a_codec_implementation_guess() {
    let p = Source::new(HEVC);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    assert_eq!(r.encoder_name().unwrap().as_ref(), b"Lavc63.1.102 libx264");
    assert_eq!(r.demux().codec, "hevc");
}
#[test]
fn zero_work_does_not_scan_allocate_move_input_or_publish() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let mut j = p.job(&i);
    let pos = i.position();
    let used = p.pool.used();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(i.position(), pos);
    assert_eq!(p.pool.used(), used);
}
#[test]
fn work_one_and_bulk_have_identical_complete_config_and_metadata() {
    let p = Source::new(AAC);
    let mut a = p.input();
    let one = complete(&mut p.job(&a), &mut a, 1).unwrap();
    let mut b = p.input();
    let bulk = complete(&mut p.job(&b), &mut b, 4096).unwrap();
    assert_eq!(
        one.demux().extradata.as_ref(),
        bulk.demux().extradata.as_ref()
    );
    assert_eq!(one.demux().codec, bulk.demux().codec);
    assert_eq!(
        one.configuration_box().bytes().as_ref(),
        bulk.configuration_box().bytes().as_ref()
    );
}
#[test]
fn cancellation_precedes_zero_work_and_is_sticky() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let mut j = p.job(&i);
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(9), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn stale_source_generation_is_not_a_config_change() {
    let p = Source::new(AVC);
    let mut i = p.input();
    let mut j = p.job(&i);
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn legitimate_range_requests_do_not_reseed_owned_metadata() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let mut j = p.job(&i);
    let generation = i.generation();
    i.request_range(4).unwrap();
    let r = complete(&mut j, &mut i, 1).unwrap();
    assert_eq!(i.generation(), generation);
    assert_eq!(i.position(), 4);
    assert_eq!(r.demux().codec, "aac");
}
#[test]
fn transport_failure_is_not_eof_or_a_zero_work_yield() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let mut j = p.job(&i);
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn foreign_pool_is_rejected_before_config_scanning() {
    let p = Source::new(AAC);
    let other = Budget::new(1 << 20).unwrap();
    let i = InputCursor::new(
        &other,
        SourceId(812),
        SourceCapabilities {
            length: Some(p.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        EncodedStreamJob::new(
            &p.header,
            &p.table,
            &i,
            Some(*b"isom"),
            ConfigurationLimits::default(),
            EncodedStreamProfile
        ),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn config_limit_is_a_resource_error_with_literal_source_untouched() {
    let p = Source::new(HEVC);
    let mut i = p.input();
    let limits = ConfigurationLimits {
        max_config_bytes: 1,
        ..ConfigurationLimits::default()
    };
    let mut j = EncodedStreamJob::new(
        &p.header,
        &p.table,
        &i,
        Some(*b"isom"),
        limits,
        EncodedStreamProfile,
    )
    .unwrap();
    assert!(matches!(
        complete(&mut j, &mut i, 1),
        Err(Error::Limit { maximum: 1, .. })
    ));
    assert_eq!(p.table.descriptions()[0].format, *b"hvc1");
}
#[test]
fn descriptor_limit_bounds_actual_nested_records() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let limits = ConfigurationLimits {
        max_descriptors: 1,
        ..ConfigurationLimits::default()
    };
    let mut j = EncodedStreamJob::new(
        &p.header,
        &p.table,
        &i,
        Some(*b"isom"),
        limits,
        EncodedStreamProfile,
    )
    .unwrap();
    assert_eq!(
        complete(&mut j, &mut i, 1).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
}
#[test]
fn retained_config_and_raw_headers_keep_original_charge_after_parser_drop() {
    let p = Source::new(HEVC);
    let mut i = p.input();
    let r = complete(&mut p.job(&i), &mut i, 1).unwrap();
    let pool = p.pool.clone();
    drop(i);
    drop(p);
    assert!(!r.demux().extradata.is_empty());
    assert_eq!(r.raw_headers()[1].header().kind, *b"mdhd");
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(r);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn public_header_values_cannot_replace_raw_source_facts() {
    let mut p = Source::new(AAC);
    p.header.timescale += 1;
    let mut i = p.input();
    assert!(matches!(
        complete(&mut p.job(&i), &mut i, 1),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn unproven_brand_remains_refused_without_source_rewrite() {
    let p = Source::new(AAC);
    let i = p.input();
    assert!(matches!(
        EncodedStreamJob::new(
            &p.header,
            &p.table,
            &i,
            Some(*b"qt  "),
            ConfigurationLimits::default(),
            EncodedStreamProfile
        ),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(p.table.descriptions()[0].format, *b"mp4a");
}
#[test]
fn completed_job_cannot_repeat_the_configuration_publication() {
    let p = Source::new(AVC);
    let mut i = p.input();
    let mut j = p.job(&i);
    let r = complete(&mut j, &mut i, 1).unwrap();
    assert_eq!(r.demux().codec, "h264");
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        CursorStatus::End
    );
}
#[test]
fn cancellation_of_ready_config_cannot_be_undone_by_retry() {
    let p = Source::new(AAC);
    let mut i = p.input();
    let mut j = p.job(&i);
    while j
        .poll(&mut i, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != CursorStatus::Ready
    {}
    assert_eq!(
        j.take(&mut i, &AtomicBool::new(true)).unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.take(&mut i, &AtomicBool::new(false)).unwrap_err(),
        Error::Cancelled
    );
}
