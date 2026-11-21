//! One encoded track of an authored file with an edit list, its encoded stream and the
//! packets of an edit plan, for the tests of plans read with optional source views.
use crate::common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceId, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    CursorStatus, Limits,
    compat::mov::{
        edits::{EncodedEditContext, EncodedEditJob, EncodedEditPlan, EncodedEditProfile},
        encoded::{EncodedMetadataProfile, EncodedSelection, EncodedStream, EncodedStreamJob},
        packets::{DemuxTrack, PacketCursor, PacketLimits, PacketOccurrence},
    },
    config::ConfigurationLimits,
    movie::{EditList, TrackHeader},
    table::SampleTable,
};

pub(crate) struct Source {
    pub(crate) pool: Budget,
    pub(crate) header: TrackHeader,
    pub(crate) table: SampleTable,
    pub(crate) edits: EditList,
    pub(crate) media: Vec<Range<u64>>,
    pub(crate) length: u64,
}
impl Source {
    pub(crate) fn new(b: &[u8]) -> Self {
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
        let edits = EditList::parse(
            child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
            Limits::default(),
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
            edits,
            media,
            length: b.len() as u64,
        }
    }
    pub(crate) fn input(&self) -> InputCursor {
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
    pub(crate) fn configs(&self, i: &mut InputCursor) -> Vec<EncodedStream> {
        let mut j = EncodedStreamJob::new_description_with_metadata(
            &self.header,
            &self.table,
            i,
            EncodedSelection {
                major_brand: Some(*b"isom"),
                description: 1,
                limits: ConfigurationLimits::default(),
            },
            EncodedMetadataProfile,
        )
        .unwrap();
        for _ in 0..20000 {
            if j.poll(i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == CursorStatus::Ready
            {
                return vec![j.take(i, &AtomicBool::new(false)).unwrap()];
            }
        }
        panic!("config progress");
    }
    pub(crate) fn context<'a>(&'a self, s: &'a EncodedStream) -> EncodedEditContext<'a> {
        EncodedEditContext {
            table: &self.table,
            edits: &self.edits,
            stream: s,
            media_base: self.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        }
    }
    pub(crate) fn track(&self) -> DemuxTrack<'_> {
        DemuxTrack {
            track_id: self.header.track_id,
            table: &self.table,
            fragments: &[],
            media_base: self.header.time_base().unwrap(),
            edits: Some((&self.edits, TimeBase::new(1, 1000).unwrap())),
        }
    }
}
pub(crate) fn finish(
    j: &mut EncodedEditJob<'_>,
    i: &mut InputCursor,
    q: u64,
) -> Result<EncodedEditPlan, Error> {
    for _ in 0..20000 {
        let mut w = WorkBudget::new(q);
        let state = j.poll(i, &mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if state == CursorStatus::Ready {
            return j.take(i, &AtomicBool::new(false));
        }
    }
    panic!("group/sample actual-work progress");
}
pub(crate) fn rows(s: &Source, plan: EncodedEditPlan) -> Vec<PacketOccurrence> {
    let plans = [Some(plan)];
    let mut c = PacketCursor::new_encoded_regular(
        &s.pool,
        &[s.track()],
        &s.media,
        PacketLimits::default(),
        &plans,
        EncodedEditProfile,
    )
    .unwrap();
    let mut rows = Vec::new();
    for _ in 0..20000 {
        let mut w = WorkBudget::new(1);
        match c.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            CursorStatus::Yield => {}
            CursorStatus::End => return rows,
            CursorStatus::Ready => rows.push(c.take_occurrence().unwrap()),
        }
        assert!(w.spent() <= 1);
    }
    panic!("packet work1 progress");
}
