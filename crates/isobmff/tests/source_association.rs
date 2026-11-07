//! Original public-API regression: one allocation can describe two physical origins.
mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::TimeBase;
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_isobmff::{
    BoxView, ConfigurationLimits, EditList, Limits, SampleStep, SampleTable, TrackHeader,
    compat::mov::{
        edits::{EncodedEditContext, EncodedEditJob, EncodedEditLimits, EncodedEditProfile},
        encoded::{
            EncodedConfigProfile, EncodedInitial, EncodedMetadataProfile, EncodedSelection,
            EncodedStreamJob,
        },
        packets::{CursorStatus, DemuxTrack, PacketCursor, PacketLimits},
    },
};
#[cfg(feature = "reference-trace")]
use triergon_isobmff::{
    BuildStatus,
    compat::mov::packets::StreamView,
    reference_trace::mov::sample_entry::{PrivateCatalogueProfile, PrivateConfigurationJob},
};
#[cfg(feature = "reference-trace")]
const RAW: &[u8] = include_bytes!("fixtures/private-catalogue-v8/dOps-only-size-11.mov");
const AVC: &[u8] = include_bytes!("fixtures/encoded-dependency-v4/dep-unreferenced.mp4");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    edits: Option<EditList>,
    media: Vec<std::ops::Range<u64>>,
    length: u64,
}
impl Source {
    fn new(data: &[u8]) -> Self {
        let pool = Budget::new(1 << 22).unwrap();
        let top = roots(&pool, data).unwrap();
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
        let edits = children(&track, 0)
            .unwrap()
            .into_iter()
            .find(|b| b.header().kind == *b"edts")
            .map(|b| EditList::parse(child(&b, b"elst").unwrap(), Limits::default()).unwrap());
        let media = top
            .iter()
            .filter(|b| b.header().kind == *b"mdat")
            .map(|b| b.header().payload_range().unwrap())
            .collect();
        Self {
            pool,
            header,
            table,
            edits,
            media,
            length: data.len() as u64,
        }
    }
    fn shifted(&self, delta: u64) -> SampleTable {
        table(
            BoxView::new(
                self.table.raw().bytes().clone(),
                self.table.raw().header().start + delta,
            )
            .unwrap(),
            &self.pool,
            1,
        )
        .unwrap()
    }
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(2233),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn encoded(
        &self,
        table: &SampleTable,
        input: &mut InputCursor,
    ) -> triergon_isobmff::compat::mov::encoded::EncodedStream {
        let mut job = EncodedStreamJob::new_description_with_metadata(
            &self.header,
            table,
            input,
            EncodedSelection {
                major_brand: Some(*b"isom"),
                description: 1,
                limits: ConfigurationLimits::default(),
            },
            EncodedMetadataProfile,
        )
        .unwrap();
        for _ in 0..10000 {
            if job
                .poll(input, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == CursorStatus::Ready
            {
                return job.take(input, &AtomicBool::new(false)).unwrap();
            }
        }
        panic!("config progress");
    }
}
#[cfg(feature = "reference-trace")]
fn private(
    s: &Source,
    other: &SampleTable,
) -> triergon_isobmff::reference_trace::mov::sample_entry::PrivateConfiguration {
    let mut job = PrivateConfigurationJob::new(
        &other.descriptions()[0],
        &s.pool,
        1024 * 1024,
        Limits::default(),
        PrivateCatalogueProfile,
    )
    .unwrap();
    for _ in 0..10000 {
        if job
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == BuildStatus::Ready
        {
            return job.try_finish(&AtomicBool::new(false)).unwrap();
        }
    }
    panic!("private progress");
}
fn first(table: &SampleTable) -> triergon_isobmff::Sample {
    let mut c = table.cursor();
    loop {
        match c.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(v) => return v,
            SampleStep::Yield => {}
            SampleStep::End => panic!("no sample"),
        }
    }
}
#[cfg(feature = "reference-trace")]
#[test]
fn same_bytes_relocated_physical_entry_is_rejected() {
    let s = Source::new(RAW);
    let shifted = s.shifted(4);
    let a = s.table.descriptions()[0].raw();
    let b = shifted.descriptions()[0].raw();
    assert_eq!(a.bytes().as_ref().as_ptr(), b.bytes().as_ref().as_ptr());
    assert_eq!(a.bytes().len(), b.bytes().len());
    assert_ne!(a.header().start, b.header().start);
    let config = private(&s, &shifted);
    let result = StreamView::for_source_private(
        &s.header,
        &s.table,
        false,
        Some(*b"qt  "),
        &config,
        PrivateCatalogueProfile,
    );
    assert!(
        matches!(
            result,
            Err(Error::Invalid("private catalogue source association"))
        ),
        "relocated physical configuration was admitted"
    );
}
#[test]
fn same_bytes_relocated_encoded_description_is_rejected() {
    let s = Source::new(AVC);
    let shifted = s.shifted(4);
    let mut input = s.input();
    let config = s.encoded(&shifted, &mut input);
    assert_eq!(
        config.source_description().raw().bytes().as_ref().as_ptr(),
        s.table.descriptions()[0].raw().bytes().as_ref().as_ptr()
    );
    assert_ne!(
        config.source_description().raw().header().start,
        s.table.descriptions()[0].raw().header().start
    );
    let result = EncodedInitial::new(
        &s.table,
        &[config],
        &first(&s.table),
        &input,
        EncodedConfigProfile,
    );
    assert!(
        matches!(
            result,
            Err(Error::Invalid("encoded description source association"))
        ),
        "relocated encoded source was admitted"
    );
}
#[test]
fn direct_edit_constructor_rejects_relocated_configuration() {
    let s = Source::new(AVC);
    let shifted = s.shifted(4);
    let mut input = s.input();
    let config = s.encoded(&shifted, &mut input);
    let result = EncodedEditJob::new(
        EncodedEditContext {
            table: &s.table,
            edits: s.edits.as_ref().unwrap(),
            stream: &config,
            media_base: s.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &input,
        EncodedEditLimits::default(),
        EncodedEditProfile,
    );
    assert!(
        matches!(
            result,
            Err(Error::Invalid("encoded edit configuration association"))
        ),
        "direct edit accepted relocated description"
    );
}
#[test]
fn packet_cursor_rejects_plan_at_relocated_metadata_origin() {
    let s = Source::new(AVC);
    let shifted = s.shifted(4);
    let mut input = s.input();
    let config = s.encoded(&shifted, &mut input);
    let configs = [config.clone()];
    let mut job = EncodedEditJob::new_with_dependency_table(
        EncodedEditContext {
            table: &shifted,
            edits: s.edits.as_ref().unwrap(),
            stream: &config,
            media_base: s.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &configs,
        &input,
        EncodedEditLimits::default(),
        EncodedMetadataProfile,
    )
    .unwrap();
    while job
        .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != CursorStatus::Ready
    {}
    let plan = job.take(&mut input, &AtomicBool::new(false)).unwrap();
    let tracks = [DemuxTrack {
        track_id: s.header.track_id,
        table: &s.table,
        fragments: &[],
        media_base: s.header.time_base().unwrap(),
        edits: Some((s.edits.as_ref().unwrap(), TimeBase::new(1, 1000).unwrap())),
    }];
    let plans = [Some(plan)];
    let result = PacketCursor::new_encoded_regular(
        &s.pool,
        &tracks,
        &s.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    );
    assert!(
        matches!(
            result,
            Err(Error::Invalid("encoded edit source association"))
        ),
        "relocated metadata plan was admitted"
    );
}
#[cfg(feature = "reference-trace")]
#[test]
fn private_same_allocation_same_physical_start_stays_valid() {
    let s = Source::new(RAW);
    let reconstructed = s.shifted(0);
    let config = private(&s, &reconstructed);
    assert!(
        StreamView::for_source_private(
            &s.header,
            &s.table,
            false,
            Some(*b"qt  "),
            &config,
            PrivateCatalogueProfile
        )
        .is_ok()
    );
}
#[test]
fn encoded_same_allocation_same_physical_start_stays_valid() {
    let s = Source::new(AVC);
    let reconstructed = s.shifted(0);
    let mut input = s.input();
    let config = s.encoded(&reconstructed, &mut input);
    assert!(
        EncodedInitial::new(
            &s.table,
            &[config],
            &first(&s.table),
            &input,
            EncodedConfigProfile
        )
        .is_ok()
    );
}
