mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, WorkBudget,
};
use triergon_isobmff::{
    ConfigurationLimits, Sample, SampleStep, SampleTable, TrackHeader,
    compat::mov::{
        encoded::{
            EncodedConfigProfile, EncodedConfigSequence, EncodedConfigStep, EncodedCreation,
            EncodedInitial, EncodedStream, EncodedStreamJob,
        },
        packets::CursorStatus,
    },
};
const BASE: &[u8] = include_bytes!("fixtures/encoded-v1/control-avc-aac-front.mp4");
const DIFFERENT: &[u8] = include_bytes!("fixtures/encoded-config-v3/different121.mp4");
const INITIAL2: &[u8] = include_bytes!("fixtures/encoded-config-v3/initial21.mp4");
const EQUAL: &[u8] = include_bytes!("fixtures/encoded-config-v3/equal121.mp4");
const CREATED: &[u8] = include_bytes!("fixtures/encoded-config-v3/creation-all.mp4");
const TRACK_ONLY: &[u8] = include_bytes!("fixtures/encoded-config-v3/creation-track.mp4");
struct Source {
    pool: Budget,
    table: SampleTable,
    header: TrackHeader,
    length: u64,
}
impl Source {
    fn new(b: &[u8]) -> Self {
        Self::in_pool(b, Budget::new(1 << 22).unwrap())
    }
    fn in_pool(b: &[u8], pool: Budget) -> Self {
        let top = roots(&pool, b).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let t = children(movie, 0)
            .unwrap()
            .into_iter()
            .find(|x| x.header().kind == *b"trak")
            .unwrap();
        let md = child(&t, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&t, b"tkhd").unwrap(),
            &child(&md, b"mdhd").unwrap(),
            &child(&md, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = table(
            child(&child(&md, b"minf").unwrap(), b"stbl").unwrap(),
            &pool,
            1,
        )
        .unwrap();
        Self {
            pool,
            table,
            header,
            length: b.len() as u64,
        }
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(1019),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn configs(&self, i: &mut InputCursor) -> Vec<EncodedStream> {
        (1..=self.table.descriptions().len())
            .map(|n| {
                let mut j = EncodedStreamJob::new_description(
                    &self.header,
                    &self.table,
                    i,
                    Some(*b"isom"),
                    ConfigurationLimits::default(),
                    n as u32,
                    EncodedConfigProfile,
                )
                .unwrap();
                for _ in 0..100000 {
                    let mut w = WorkBudget::new(1);
                    if j.poll(i, &mut w, &AtomicBool::new(false)).unwrap() == CursorStatus::Ready {
                        return j.take(i, &AtomicBool::new(false)).unwrap();
                    }
                    assert!(w.spent() <= 1);
                }
                panic!("configuration work1 progress")
            })
            .collect()
    }
    fn samples(&self) -> Vec<Sample> {
        let mut c = self.table.cursor();
        let mut out = Vec::new();
        for _ in 0..100000 {
            match c.poll(&mut WorkBudget::new(1)).unwrap() {
                SampleStep::Yield => {}
                SampleStep::Sample(s) => out.push(s),
                SampleStep::End => return out,
            }
        }
        panic!("sample progress")
    }
}
fn observed(
    seq: &mut EncodedConfigSequence<'_>,
    sample: &Sample,
    input: &mut InputCursor,
) -> Option<triergon_core::Bytes> {
    let mut w = WorkBudget::new(1);
    let result = seq
        .observe(sample, input, &mut w, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(w.spent(), 1);
    match result {
        EncodedConfigStep::Ready(b) => b,
        EncodedConfigStep::Yield => panic!("one source event must progress"),
    }
}
#[test]
fn creation_uses_media_header_not_distinct_movie_or_track_fields() {
    let p = Source::new(CREATED);
    let c = EncodedCreation::from_source(&p.header, EncodedConfigProfile).unwrap();
    assert_eq!(c.source_seconds(), 2082844803);
    assert_eq!(c.demux().unwrap(), b"1970-01-01T00:00:03.000000Z");
    assert_eq!(c.raw().bytes().as_ref(), p.header.raw_mdhd.bytes().as_ref());
}
#[test]
fn track_only_creation_does_not_fill_absent_media_creation() {
    let p = Source::new(TRACK_ONLY);
    let c = EncodedCreation::from_source(&p.header, EncodedConfigProfile).unwrap();
    assert_eq!(c.source_seconds(), 0);
    assert!(c.demux().is_none());
}
#[test]
fn calendar_handles_gregorian_leap_day_from_source_seconds() {
    let mut b = CREATED.to_vec();
    let p = Source::new(&b);
    let at = p.header.raw_mdhd.header().start as usize + 12;
    drop(p);
    let seconds = 2082844800u32 + 951782400;
    b[at..at + 4].copy_from_slice(&seconds.to_be_bytes());
    let p = Source::new(&b);
    assert_eq!(
        EncodedCreation::from_source(&p.header, EncodedConfigProfile)
            .unwrap()
            .demux()
            .unwrap(),
        b"2000-02-29T00:00:00.000000Z"
    );
}
#[test]
fn ambiguous_old_epoch_is_refused_without_raw_rewrite() {
    let mut b = CREATED.to_vec();
    let p = Source::new(&b);
    let at = p.header.raw_mdhd.header().start as usize + 12;
    drop(p);
    b[at..at + 4].copy_from_slice(&1u32.to_be_bytes());
    let p = Source::new(&b);
    assert!(matches!(
        EncodedCreation::from_source(&p.header, EncodedConfigProfile),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(&p.header.raw_mdhd.payload()[4..8], &1u32.to_be_bytes());
}
#[test]
fn second_configuration_job_borrows_complete_original_opaque_record() {
    let p = Source::new(DIFFERENT);
    let mut i = p.input();
    let c = p.configs(&mut i);
    assert_eq!(c.len(), 2);
    assert_ne!(
        c[0].demux().extradata.as_ref(),
        c[1].demux().extradata.as_ref()
    );
    assert_eq!(c[1].source_description().index, 2);
    assert_eq!(
        c[1].demux().extradata.as_ref(),
        [
            1, 66, 0, 10, 255, 225, 0, 6, 103, 66, 0, 10, 218, 121, 1, 0, 4, 104, 206, 60, 128
        ]
    );
}
#[test]
fn initial_extradata_uses_first_used_description_but_metadata_stays_first_declared() {
    let p = Source::new(INITIAL2);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let samples = p.samples();
    assert_eq!(samples[0].description_index, 2);
    let view = EncodedInitial::new(&p.table, &c, &samples[0], &i, EncodedConfigProfile).unwrap();
    assert_eq!(view.source_initial_description(), 2);
    assert_eq!(
        view.demux().extradata.as_ref(),
        c[1].demux().extradata.as_ref()
    );
    assert_eq!(
        view.metadata_description().bytes().as_ref(),
        c[0].source_description().raw().bytes().as_ref()
    );
    assert!(c[0].encoder_name().is_some());
    assert!(c[1].encoder_name().is_none());
}
#[test]
fn different121_emits_real_second_and_return_configuration_once_each() {
    let p = Source::new(DIFFERENT);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
    assert_eq!(
        observed(&mut seq, &s[1], &mut i).unwrap().as_ref(),
        c[1].demux().extradata.as_ref()
    );
    assert_eq!(
        observed(&mut seq, &s[2], &mut i).unwrap().as_ref(),
        c[0].demux().extradata.as_ref()
    );
    assert_eq!(
        s.iter().map(|x| x.description_index).collect::<Vec<_>>(),
        vec![1, 2, 1]
    );
}
#[test]
fn equal_byte_descriptions_still_retain_two_distinct_source_transition_events() {
    let p = Source::new(EQUAL);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    assert_eq!(
        c[0].demux().extradata.as_ref(),
        c[1].demux().extradata.as_ref()
    );
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
    assert!(observed(&mut seq, &s[1], &mut i).is_some());
    assert!(observed(&mut seq, &s[2], &mut i).is_some());
}
#[test]
fn zero_work_cannot_consume_source_event_or_allocate() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    let used = p.pool.used();
    let position = i.position();
    let mut w = WorkBudget::new(0);
    assert!(matches!(
        seq.observe(&s[0], &mut i, &mut w, &AtomicBool::new(false))
            .unwrap(),
        EncodedConfigStep::Yield
    ));
    assert_eq!(w.spent(), 0);
    assert_eq!(p.pool.used(), used);
    assert_eq!(i.position(), position);
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
}
#[test]
fn cancelled_config_event_stays_failed() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    assert_eq!(
        seq.observe(
            &s[0],
            &mut i,
            &mut WorkBudget::new(0),
            &AtomicBool::new(true)
        )
        .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        seq.observe(
            &s[0],
            &mut i,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn source_reset_rejects_config_even_at_zero_work() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    i.reset(0).unwrap();
    assert!(matches!(
        seq.observe(
            &s[0],
            &mut i,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        ),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn transport_failure_precedes_zero_work_config_event() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        seq.observe(
            &s[0],
            &mut i,
            &mut WorkBudget::new(0),
            &AtomicBool::new(false)
        )
        .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn range_request_keeps_same_source_configuration_history() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    i.request_range(4).unwrap();
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
    assert_eq!(i.position(), 4);
}
#[test]
fn duplicate_source_ordinal_is_an_error_not_an_extra_packet() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
    assert!(matches!(
        seq.observe(
            &s[0],
            &mut i,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        seq.observe(
            &s[1],
            &mut i,
            &mut WorkBudget::new(1),
            &AtomicBool::new(false)
        ),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn foreign_pool_does_not_admit_owned_source_configurations() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let q = Budget::new(1 << 20).unwrap();
    let foreign = InputCursor::new(
        &q,
        SourceId(1019),
        SourceCapabilities {
            length: Some(p.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        EncodedConfigSequence::new(&p.table, &c, &foreign, EncodedConfigProfile),
        Err(Error::ForeignPool)
    ));
}
#[test]
fn equal_bytes_from_another_source_backing_do_not_rebind_configuration() {
    let p = Source::new(BASE);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let other = Source::in_pool(BASE, p.pool.clone());
    assert!(matches!(
        EncodedConfigSequence::new(&other.table, &c, &i, EncodedConfigProfile),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn emitted_span_keeps_full_backing_charge_after_jobs_and_tables_drop() {
    let p = Source::new(DIFFERENT);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let span = {
        let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
        assert!(observed(&mut seq, &s[0], &mut i).is_none());
        observed(&mut seq, &s[1], &mut i).unwrap()
    };
    let pool = p.pool.clone();
    drop(c);
    drop(i);
    drop(p);
    assert!(span.retained_capacity() > span.len());
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(span);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn source_transition_borrows_under_full_pool_without_new_allocation() {
    let p = Source::new(DIFFERENT);
    let mut i = p.input();
    let c = p.configs(&mut i);
    let s = p.samples();
    let mut seq = EncodedConfigSequence::new(&p.table, &c, &i, EncodedConfigProfile).unwrap();
    assert!(observed(&mut seq, &s[0], &mut i).is_none());
    let hold = p.pool.reserve(p.pool.limit() - p.pool.used()).unwrap();
    let before = p.pool.used();
    let span = observed(&mut seq, &s[1], &mut i).unwrap();
    assert_eq!(p.pool.used(), before);
    assert_eq!(span.as_ref(), c[1].demux().extradata.as_ref());
    drop(hold);
}
