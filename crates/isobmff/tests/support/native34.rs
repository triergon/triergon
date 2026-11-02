use crate::common::{self, child};
use std::ops::Range;
use triergon_core::{Budget, TimeBase, WorkBudget};
use triergon_isobmff::{
    BuildStatus, EditList, FragmentBuilder, FragmentContext, FragmentIndex, ImplicitBase, Limits,
    Mov9Track, MovieHeader, SampleTable, TrackHeader, Trex,
};
pub struct Track {
    pub id: u32,
    pub base: TimeBase,
    pub table: SampleTable,
    pub edits: Option<(EditList, TimeBase)>,
}
pub struct Movie {
    pub tracks: Vec<Track>,
    pub fragments: Vec<FragmentIndex>,
    pub media: Vec<Range<u64>>,
}
impl Movie {
    pub fn native(&self) -> Vec<Mov9Track<'_>> {
        self.tracks
            .iter()
            .map(|t| Mov9Track {
                track_id: t.id,
                table: &t.table,
                fragments: &self.fragments,
                media_base: t.base,
                edits: t.edits.as_ref().map(|(e, b)| (e, *b)),
            })
            .collect()
    }
}
pub fn parse(pool: &Budget, bytes: &[u8]) -> Movie {
    let roots = common::roots(pool, bytes).unwrap();
    let movie = roots.iter().find(|r| r.header().kind == *b"moov").unwrap();
    let movie_base = MovieHeader::parse(&child(movie, b"mvhd").unwrap())
        .unwrap()
        .time_base()
        .unwrap();
    let mut tracks = Vec::new();
    for raw in common::children(movie, 0)
        .unwrap()
        .iter()
        .filter(|r| r.header().kind == *b"trak")
    {
        let mdia = child(raw, b"mdia").unwrap();
        let h = TrackHeader::parse(
            &child(raw, b"tkhd").unwrap(),
            &child(&mdia, b"mdhd").unwrap(),
            &child(&mdia, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = common::table(
            child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap(),
            pool,
            1,
        )
        .unwrap();
        let edits = common::children(raw, 0)
            .unwrap()
            .into_iter()
            .find(|b| b.header().kind == *b"edts")
            .map(|e| {
                (
                    EditList::parse(child(&e, b"elst").unwrap(), Limits::default()).unwrap(),
                    movie_base,
                )
            });
        tracks.push(Track {
            id: h.track_id,
            base: h.time_base().unwrap(),
            table,
            edits,
        });
    }
    let mut fragments = Vec::new();
    if roots.iter().any(|r| r.header().kind == *b"moof") {
        let extends = child(movie, b"mvex").unwrap();
        let defaults: Vec<_> = common::children(&extends, 0)
            .unwrap()
            .iter()
            .filter(|r| r.header().kind == *b"trex")
            .map(|r| Trex::parse(r).unwrap())
            .collect();
        let mut next: Vec<_> = tracks.iter().map(|t| t.table.sample_count() + 1).collect();
        for moof in roots.iter().filter(|r| r.header().kind == *b"moof") {
            let mut previous = None;
            for traf in common::children(moof, 0)
                .unwrap()
                .iter()
                .filter(|r| r.header().kind == *b"traf")
            {
                let tfhd = child(traf, b"tfhd").unwrap();
                let p = tfhd.payload();
                let flags = u32::from_be_bytes(p[0..4].try_into().unwrap());
                let id = u32::from_be_bytes(p[4..8].try_into().unwrap());
                let at = tracks.iter().position(|t| t.id == id).unwrap();
                let default = *defaults.iter().find(|t| t.track_id == id).unwrap();
                let implicit_base = if flags & 0x020001 != 0 {
                    ImplicitBase::FirstTraf
                } else {
                    previous.map_or(ImplicitBase::FirstTraf, ImplicitBase::PreviousTraf)
                };
                let mut job = FragmentBuilder::new(
                    traf.clone(),
                    default,
                    FragmentContext {
                        moof_start: moof.header().start,
                        implicit_base,
                        decode_origin: None,
                        first_ordinal: next[at],
                        description_count: u32::try_from(tracks[at].table.descriptions().len())
                            .unwrap(),
                    },
                    pool.clone(),
                    Limits::default(),
                )
                .unwrap();
                while job.poll(&mut WorkBudget::new(1)).unwrap() != BuildStatus::Ready {}
                let index = job.finish().unwrap();
                next[at] += index.sample_count();
                previous = Some(index.implicit_data_end().unwrap());
                fragments.push(index);
            }
        }
    }
    let media = roots
        .iter()
        .filter(|r| r.header().kind == *b"mdat")
        .map(|r| r.header().payload_range().unwrap())
        .collect();
    Movie {
        tracks,
        fragments,
        media,
    }
}
