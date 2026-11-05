//! Original public-API regression: one allocation can describe two physical origins.
mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::TimeBase;
use triergon_core::{Budget, Error, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_isobmff::{
    BoxView, BuildStatus, EditList, Limits, Mov9CursorStatus, Mov9EncodedEditContextV2,
    Mov9EncodedEditJobV2, Mov9EncodedEditLimitsV2, Mov9EncodedInitialV3, Mov9EncodedLimitsV1,
    Mov9EncodedSelectionV4, Mov9EncodedStreamJobV1, Mov9PacketCursor, Mov9PacketLimits,
    Mov9PrivateConfigurationJobV8, Mov9StreamView, Mov9Track, PinnedMovD32EncodedEditsV2,
    PinnedMovD32EncodedRegularV3, PinnedMovD32EncodedRegularV4,
    PinnedMovD32OpaquePrivateCatalogueV8, SampleStep, SampleTable, TrackHeader,
};
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
    ) -> triergon_isobmff::Mov9EncodedStreamV1 {
        let mut job = Mov9EncodedStreamJobV1::new_description_v4(
            &self.header,
            table,
            input,
            Mov9EncodedSelectionV4 {
                major_brand: Some(*b"isom"),
                description: 1,
                limits: Mov9EncodedLimitsV1::default(),
            },
            PinnedMovD32EncodedRegularV4,
        )
        .unwrap();
        for _ in 0..10000 {
            if job
                .poll(input, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == Mov9CursorStatus::Ready
            {
                return job.take(input, &AtomicBool::new(false)).unwrap();
            }
        }
        panic!("config progress");
    }
}
fn private(s: &Source, other: &SampleTable) -> triergon_isobmff::Mov9PrivateConfigurationV8 {
    let mut job = Mov9PrivateConfigurationJobV8::new(
        &other.descriptions()[0],
        &s.pool,
        1024 * 1024,
        Limits::default(),
        PinnedMovD32OpaquePrivateCatalogueV8,
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
#[test]
fn v8_same_bytes_relocated_physical_entry_is_rejected() {
    let s = Source::new(RAW);
    let shifted = s.shifted(4);
    let a = s.table.descriptions()[0].raw();
    let b = shifted.descriptions()[0].raw();
    assert_eq!(a.bytes().as_ref().as_ptr(), b.bytes().as_ref().as_ptr());
    assert_eq!(a.bytes().len(), b.bytes().len());
    assert_ne!(a.header().start, b.header().start);
    let config = private(&s, &shifted);
    let result = Mov9StreamView::for_source_private_v8(
        &s.header,
        &s.table,
        false,
        Some(*b"qt  "),
        &config,
        PinnedMovD32OpaquePrivateCatalogueV8,
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
fn v3_same_bytes_relocated_encoded_description_is_rejected() {
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
    let result = Mov9EncodedInitialV3::new(
        &s.table,
        &[config],
        &first(&s.table),
        &input,
        PinnedMovD32EncodedRegularV3,
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
fn v2_direct_edit_constructor_rejects_relocated_configuration() {
    let s = Source::new(AVC);
    let shifted = s.shifted(4);
    let mut input = s.input();
    let config = s.encoded(&shifted, &mut input);
    let result = Mov9EncodedEditJobV2::new(
        Mov9EncodedEditContextV2 {
            table: &s.table,
            edits: s.edits.as_ref().unwrap(),
            stream: &config,
            media_base: s.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &input,
        Mov9EncodedEditLimitsV2::default(),
        PinnedMovD32EncodedEditsV2,
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
    let mut job = Mov9EncodedEditJobV2::new_v4(
        Mov9EncodedEditContextV2 {
            table: &shifted,
            edits: s.edits.as_ref().unwrap(),
            stream: &config,
            media_base: s.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &configs,
        &input,
        Mov9EncodedEditLimitsV2::default(),
        PinnedMovD32EncodedRegularV4,
    )
    .unwrap();
    while job
        .poll(&mut input, &mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != Mov9CursorStatus::Ready
    {}
    let plan = job.take(&mut input, &AtomicBool::new(false)).unwrap();
    let tracks = [Mov9Track {
        track_id: s.header.track_id,
        table: &s.table,
        fragments: &[],
        media_base: s.header.time_base().unwrap(),
        edits: Some((s.edits.as_ref().unwrap(), TimeBase::new(1, 1000).unwrap())),
    }];
    let plans = [Some(plan)];
    let result = Mov9PacketCursor::new_encoded_regular_v2(
        &s.pool,
        &tracks,
        &s.media,
        Mov9PacketLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    );
    assert!(
        matches!(
            result,
            Err(Error::Invalid("encoded edit source association"))
        ),
        "relocated metadata plan was admitted"
    );
}
#[test]
fn v8_same_allocation_same_physical_start_stays_valid() {
    let s = Source::new(RAW);
    let reconstructed = s.shifted(0);
    let config = private(&s, &reconstructed);
    assert!(
        Mov9StreamView::for_source_private_v8(
            &s.header,
            &s.table,
            false,
            Some(*b"qt  "),
            &config,
            PinnedMovD32OpaquePrivateCatalogueV8
        )
        .is_ok()
    );
}
#[test]
fn v3_same_allocation_same_physical_start_stays_valid() {
    let s = Source::new(AVC);
    let reconstructed = s.shifted(0);
    let mut input = s.input();
    let config = s.encoded(&reconstructed, &mut input);
    assert!(
        Mov9EncodedInitialV3::new(
            &s.table,
            &[config],
            &first(&s.table),
            &input,
            PinnedMovD32EncodedRegularV3
        )
        .is_ok()
    );
}
