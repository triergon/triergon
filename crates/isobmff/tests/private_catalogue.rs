mod common;
use common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Limits, SampleTable, TrackHeader,
    compat::mov::packets::StreamView,
    reference_trace::mov::sample_entry::{
        PrivateCatalogueProfile, PrivateConfigCause, PrivateConfiguration, PrivateConfigurationJob,
    },
};
const BASE: &[u8] = include_bytes!("fixtures/private-catalogue-v8/dOps-only-size-11.mov");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    media: Vec<Range<u64>>,
    length: u64,
}
impl Source {
    fn new(b: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, b).unwrap();
        let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
        let track = children(movie, 0)
            .unwrap()
            .into_iter()
            .find(|b| b.header().kind == *b"trak")
            .unwrap();
        let mdia = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
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
        let media = top
            .iter()
            .filter(|b| b.header().kind == *b"mdat")
            .map(|b| b.header().payload_range().unwrap())
            .collect();
        Self {
            pool,
            header,
            table,
            media,
            length: b.len() as u64,
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(1997),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
}
fn fixture(name: &str) -> &'static [u8] {
    match name {
        "dOps-only-size-0" => include_bytes!("fixtures/private-catalogue-v8/dOps-only-size-0.mov"),
        "dOps-only-size-11" => {
            include_bytes!("fixtures/private-catalogue-v8/dOps-only-size-11.mov")
        }
        "dOps-valid-after-ddts" => {
            include_bytes!("fixtures/private-catalogue-v8/dOps-valid-after-ddts.mov")
        }
        "ddts-layout-ff00" => include_bytes!("fixtures/private-catalogue-v8/ddts-layout-ff00.mov"),
        "ddts-only-size-19" => {
            include_bytes!("fixtures/private-catalogue-v8/ddts-only-size-19.mov")
        }
        "ddts-only-size-20" => {
            include_bytes!("fixtures/private-catalogue-v8/ddts-only-size-20.mov")
        }
        "ddts-only-size-7" => include_bytes!("fixtures/private-catalogue-v8/ddts-only-size-7.mov"),
        "drop-dOps" => include_bytes!("fixtures/private-catalogue-v8/drop-dOps.mov"),
        "fiel-av1C" => include_bytes!("fixtures/private-catalogue-v8/fiel-av1C.mov"),
        "fiel-avcC" => include_bytes!("fixtures/private-catalogue-v8/fiel-avcC.mov"),
        "fiel-hvcC" => include_bytes!("fixtures/private-catalogue-v8/fiel-hvcC.mov"),
        "no-private-boxes" => include_bytes!("fixtures/private-catalogue-v8/no-private-boxes.mov"),
        _ => panic!("unknown frozen original control"),
    }
}
fn job(s: &Source, maximum: usize) -> PrivateConfigurationJob {
    PrivateConfigurationJob::new(
        &s.table.descriptions()[0],
        &s.pool,
        maximum,
        Limits::default(),
        PrivateCatalogueProfile,
    )
    .unwrap()
}
fn finish(j: &mut PrivateConfigurationJob, q: u64) -> Result<PrivateConfiguration, Error> {
    for _ in 0..10000 {
        let mut w = WorkBudget::new(q);
        let status = j.poll(&mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if status == BuildStatus::Ready {
            return j.try_finish(&AtomicBool::new(false));
        }
    }
    panic!("configuration work1 progress");
}
fn view(s: &Source, c: &PrivateConfiguration) -> Result<StreamView, Error> {
    StreamView::for_source_private(
        &s.header,
        &s.table,
        false,
        Some(*b"qt  "),
        c,
        PrivateCatalogueProfile,
    )
}
const OPUS_HEAD: [u8; 19] = [
    79, 112, 117, 115, 72, 101, 97, 100, 1, 2, 56, 1, 128, 187, 0, 0, 0, 0, 0,
];
#[test]
fn complete_opus_private_catalogue_keeps_source_rawvideo_codec_and_exact_header() {
    let s = Source::new(BASE);
    let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
    let demux = view(&s, &result).unwrap();
    assert_eq!(demux.extradata.as_ref(), OPUS_HEAD);
    assert_eq!(demux.codec, "rawvideo");
    assert_eq!(demux.media_type, 0);
    assert_eq!(
        result.cause(),
        PrivateConfigCause::OpusIdentificationMappingZero
    );
    assert_eq!(result.source().description.format, *b"raw ");
    assert_eq!(
        result
            .source()
            .installed_configuration_source
            .as_ref()
            .unwrap()
            .payload(),
        &[0, 2, 1, 56, 0, 0, 187, 128, 0, 0, 0]
    );
    assert_eq!(demux.time_base, s.header.time_base().unwrap());
}
#[test]
fn opaque_config_is_borrowed_with_original_backing_and_literal_seven_bytes() {
    let s = Source::new(fixture("drop-dOps"));
    let before = s.pool.used();
    let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
    let demux = view(&s, &result).unwrap();
    assert_eq!(demux.extradata.as_ref(), &[0, 255, 1, 2, 3, 0, 0]);
    assert_eq!(result.cause(), PrivateConfigCause::OpaqueDirectBody);
    let raw = result
        .source()
        .installed_configuration_source
        .as_ref()
        .unwrap();
    assert_eq!(demux.extradata.as_ref().as_ptr(), raw.payload().as_ptr());
    assert_eq!(s.pool.used(), before);
    assert_eq!(
        demux.extradata.retained_capacity(),
        raw.bytes().retained_capacity()
    );
}
#[test]
fn legacy_field_only_envelopes_do_not_block_later_actual_dops_backing() {
    for name in ["fiel-avcC", "fiel-hvcC", "fiel-av1C"] {
        let s = Source::new(fixture(name));
        let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
        assert_eq!(view(&s, &result).unwrap().extradata.as_ref(), OPUS_HEAD);
        assert_eq!(
            result
                .source()
                .installed_configuration_source
                .as_ref()
                .unwrap()
                .header()
                .kind,
            *b"dOps"
        );
    }
}
#[test]
fn absent_private_bytes_keep_absence_and_complete_original_source_entry() {
    let s = Source::new(fixture("no-private-boxes"));
    let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
    assert_eq!(result.cause(), PrivateConfigCause::Absent);
    assert!(result.demux_bytes().is_none());
    assert!(view(&s, &result).unwrap().extradata.is_empty());
    assert_eq!(
        result.source().description.raw().bytes().as_ref(),
        s.table.descriptions()[0].raw().bytes().as_ref()
    );
}
#[test]
fn empty_dops_body_does_not_manufacture_an_identification_header() {
    let s = Source::new(fixture("dOps-only-size-0"));
    let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
    assert!(result.demux_bytes().is_none());
    assert!(result.source().installed_configuration_source.is_none());
}
#[test]
fn dts_private_side_fields_do_not_imply_opus_or_new_source_timestamps() {
    for name in [
        "ddts-only-size-7",
        "ddts-only-size-19",
        "ddts-only-size-20",
        "ddts-layout-ff00",
    ] {
        let s = Source::new(fixture(name));
        let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
        let demux = view(&s, &result).unwrap();
        assert_eq!(demux.codec, "rawvideo");
        assert!(demux.extradata.is_empty());
        assert_eq!(demux.time_base, s.header.time_base().unwrap());
        assert_eq!(s.table.sample_count(), 6);
        assert_eq!(s.media.len(), 1);
    }
}
#[test]
fn transformed_output_extent_is_limited_as_well_as_source_body() {
    let s = Source::new(BASE);
    assert_eq!(
        finish(&mut job(&s, 18), 1).unwrap_err(),
        Error::Limit {
            requested: 19,
            maximum: 18
        }
    );
}
#[test]
fn zero_copy_opaque_body_still_obeys_declared_configuration_limit() {
    let s = Source::new(fixture("drop-dOps"));
    assert_eq!(
        finish(&mut job(&s, 6), 1).unwrap_err(),
        Error::Limit {
            requested: 7,
            maximum: 6
        }
    );
}
#[test]
fn zero_work_does_not_inspect_or_allocate_configuration() {
    let s = Source::new(BASE);
    let mut j = job(&s, 1 << 20);
    let before = s.pool.used();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut w, &AtomicBool::new(false)).unwrap(),
        BuildStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(s.pool.used(), before);
    assert!(matches!(
        j.try_finish(&AtomicBool::new(false)),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        finish(&mut j, 1).unwrap().demux_bytes().unwrap().as_ref(),
        OPUS_HEAD
    );
}
#[test]
fn work_one_and_bulk_keep_same_owned_header_and_configuration() {
    let s = Source::new(BASE);
    let a = finish(&mut job(&s, 1 << 20), 1).unwrap();
    let b = finish(&mut job(&s, 1 << 20), 4096).unwrap();
    assert_eq!(
        a.demux_bytes().unwrap().as_ref(),
        b.demux_bytes().unwrap().as_ref()
    );
    assert_eq!(
        a.source().description.raw().header(),
        b.source().description.raw().header()
    );
    assert_eq!(a.source().inspected_children, b.source().inspected_children);
}
#[test]
fn allocation_pressure_retries_real_pending_backing_without_reparsing_headers() {
    let s = Source::new(BASE);
    let mut j = job(&s, 1 << 20);
    let hold = s.pool.reserve(s.pool.available()).unwrap();
    let mut inspected = 0;
    for _ in 0..10000 {
        let mut w = WorkBudget::new(1);
        match j.poll(&mut w, &AtomicBool::new(false)) {
            Ok(BuildStatus::Yield) => inspected += w.spent(),
            Err(Error::Backpressure { .. }) => break,
            other => panic!("unexpected under pressure {other:?}"),
        }
    }
    assert!(inspected > 0);
    drop(hold);
    let mut w = WorkBudget::new(1);
    assert_eq!(
        j.poll(&mut w, &AtomicBool::new(false)).unwrap(),
        BuildStatus::Ready
    );
    assert_eq!(w.spent(), 1);
    let c = j.try_finish(&AtomicBool::new(false)).unwrap();
    assert_eq!(c.demux_bytes().unwrap().as_ref(), OPUS_HEAD);
}
#[test]
fn cancellation_before_work_is_sticky_and_has_no_result() {
    let s = Source::new(BASE);
    let mut j = job(&s, 1 << 20);
    assert_eq!(
        j.poll(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    );
    assert_eq!(
        j.poll(&mut WorkBudget::new(1000), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    );
    assert!(matches!(
        j.try_finish(&AtomicBool::new(false)),
        Err(Error::Cancelled)
    ));
}
#[test]
fn cancellation_at_publication_releases_transformed_output() {
    let s = Source::new(BASE);
    let mut j = job(&s, 1 << 20);
    while j
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    let live = s.pool.used();
    assert!(matches!(
        j.try_finish(&AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(s.pool.used() < live);
    assert_eq!(
        j.poll(&mut WorkBudget::new(100), &AtomicBool::new(false)),
        Err(Error::Cancelled)
    );
}
#[test]
fn foreign_pool_is_rejected_before_source_extension_interpretation() {
    let s = Source::new(BASE);
    let other = Budget::new(1 << 20).unwrap();
    assert!(matches!(
        PrivateConfigurationJob::new(
            &s.table.descriptions()[0],
            &other,
            1 << 20,
            Limits::default(),
            PrivateCatalogueProfile
        ),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn byte_identical_other_source_description_cannot_be_substituted() {
    let a = Source::new(BASE);
    let b = Source::new(BASE);
    let result = finish(&mut job(&a, 1 << 20), 1).unwrap();
    assert!(matches!(
        view(&b, &result),
        Err(Error::Invalid("private catalogue source association"))
    ));
}
#[test]
fn owned_metadata_remains_valid_when_an_unrelated_io_generation_resets() {
    let s = Source::new(BASE);
    let mut i = s.input();
    let mut j = job(&s, 1 << 20);
    assert_eq!(
        j.poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap(),
        BuildStatus::Yield
    );
    i.reset(0).unwrap();
    let result = finish(&mut j, 1).unwrap();
    assert_eq!(view(&s, &result).unwrap().extradata.as_ref(), OPUS_HEAD);
}
#[test]
fn full_source_and_transformed_output_charges_survive_producer_teardown() {
    let (pool, result) = {
        let s = Source::new(BASE);
        let result = finish(&mut job(&s, 1 << 20), 1).unwrap();
        (s.pool.clone(), result)
    };
    assert!(pool.used() > Budget::control_storage_bytes() + 19);
    assert_eq!(result.demux_bytes().unwrap().as_ref(), OPUS_HEAD);
    assert_eq!(result.source().description.format, *b"raw ");
    drop(result);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn prior_rejection_predicate_remains_independent_of_new_catalogue_view() {
    let s = Source::new(BASE);
    let mut old = triergon_isobmff::reference_trace::mov::sample_entry::SampleEntryHeaderJob::new(
        &s.table.descriptions()[0],
        triergon_isobmff::EntryLayout::Visual,
        Limits::default(),
        triergon_isobmff::reference_trace::mov::sample_entry::SampleEntryHeaderProfile,
    )
    .unwrap();
    while old
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    let header = old.try_finish(&AtomicBool::new(false)).unwrap();
    assert_eq!(header.demux_open_result(), None);
    assert_eq!(
        header
            .installed_configuration_source
            .as_ref()
            .unwrap()
            .header()
            .kind,
        *b"dOps"
    );
}

#[test]
fn reversed_source_order_selects_first_real_backing() {
    let s = Source::new(include_bytes!(
        "fixtures/private-catalogue-v8/reversed-opaque-order.mov"
    ));
    let c = finish(&mut job(&s, 1 << 20), 1).unwrap();
    let demux = view(&s, &c).unwrap();
    assert_eq!(
        c.source()
            .installed_configuration_source
            .as_ref()
            .unwrap()
            .header()
            .kind,
        *b"av1C"
    );
    assert_eq!(demux.extradata.as_ref(), &[0, 255, 1, 2, 3, 0, 2]);
}
#[test]
fn one_byte_prefix_is_replaced_by_the_next_nontrivial_source_backing() {
    let s = Source::new(include_bytes!(
        "fixtures/private-catalogue-v8/one-byte-prefix-then-real.mov"
    ));
    let c = finish(&mut job(&s, 1 << 20), 1).unwrap();
    let demux = view(&s, &c).unwrap();
    assert_eq!(
        c.source()
            .installed_configuration_source
            .as_ref()
            .unwrap()
            .header()
            .kind,
        *b"hvcC"
    );
    assert_eq!(demux.extradata.as_ref(), &[0, 255, 1, 2, 3, 0, 1]);
}
