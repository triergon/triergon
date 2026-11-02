mod common;
use common::child;
use safe_media_core::{Budget, Time, TimeBase, WorkBudget};
use safe_media_isobmff::{
    EditList, Limits, Mov9Edit, Mov9PacketView, Mov9StreamView, Mov9View, SampleStep, TrackHeader,
};

fn source_sample(name: &str, ordinal: u64) -> (safe_media_isobmff::Sample, Option<u32>) {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == name)
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let top = common::roots(&budget, case.bytes).unwrap();
    let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let track = child(movie, b"trak").unwrap();
    let stbl = child(
        &child(&child(&track, b"mdia").unwrap(), b"minf").unwrap(),
        b"stbl",
    )
    .unwrap();
    let table = common::table(stbl, &budget, 1).unwrap();
    let mut locate = table.locate(ordinal).unwrap();
    let sample = loop {
        match locate.poll(&mut WorkBudget::new(1)).unwrap() {
            SampleStep::Sample(s) => break s,
            SampleStep::Yield => {}
            _ => panic!(),
        }
    };
    (sample, table.sync_entry_count())
}

#[test]
fn native_video_vendor_tag_uses_regular_sample_context_and_preserves_source_bytes() {
    for (name, expected) in [
        ("r-front-stco", Some([0; 4])),
        ("f-absolute-base", None),
        ("f-negative-data-offset", None),
    ] {
        let fixture = common::fixtures::CASES
            .iter()
            .find(|c| c.name == name)
            .unwrap();
        let budget = Budget::new(1024 * 1024).unwrap();
        let roots = common::roots(&budget, fixture.bytes).unwrap();
        let movie = roots.iter().find(|r| r.header().kind == *b"moov").unwrap();
        let track = child(movie, b"trak").unwrap();
        let media = child(&track, b"mdia").unwrap();
        let header = TrackHeader::parse(
            &child(&track, b"tkhd").unwrap(),
            &child(&media, b"mdhd").unwrap(),
            &child(&media, b"hdlr").unwrap(),
        )
        .unwrap();
        let table = common::table(
            child(&child(&media, b"minf").unwrap(), b"stbl").unwrap(),
            &budget,
            1,
        )
        .unwrap();
        let entry = &table.descriptions()[0];
        let source_entry = entry.raw().bytes().as_ref().to_vec();
        let has_fragments = roots.iter().any(|r| r.header().kind == *b"moof");
        let view = Mov9StreamView::for_sample_table(&header, &table, has_fragments).unwrap();
        assert_eq!(view.vendor_id, expected);
        assert_eq!(&entry.raw().payload()[12..16], &[0; 4]);
        assert_eq!(entry.raw().bytes().as_ref(), source_entry);
        assert_eq!(view.codec, "rawvideo");
    }
}

#[test]
fn unedited_native_projection_matches_pinned_observation_without_mutating_source() {
    let (sample, sync) = source_sample("r-ctts-v0", 1);
    let original = sample.clone();
    let view = Mov9View::new(TimeBase::new(1, 1000).unwrap(), sync)
        .project(&sample, None)
        .unwrap();
    assert_eq!(
        (view.pts, view.dts, view.duration, view.flags),
        (Some(2000), Some(0), 1000, 1)
    );
    assert_eq!(view.source_ordinal, 1);
    assert_eq!(sample, original);
}

#[test]
fn empty_sync_table_native_first_key_fallback_is_not_a_source_fact() {
    let (first, sync) = source_sample("r-stss-empty", 1);
    let (second, _) = source_sample("r-stss-empty", 2);
    assert_eq!(sync, Some(0));
    assert!(!first.is_container_sync());
    let view = Mov9View::new(TimeBase::new(1, 1000).unwrap(), sync);
    assert_eq!(view.project(&first, None).unwrap().flags, 1);
    assert_eq!(view.project(&second, None).unwrap().flags, 0);
    assert!(!first.is_container_sync());
}

#[test]
fn source_edit_points_produce_observed_native_clock_and_discard_views() {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == "r-edits-empty-trim")
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let top = common::roots(&budget, case.bytes).unwrap();
    let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let track = child(movie, b"trak").unwrap();
    let edits = EditList::parse(
        child(&child(&track, b"edts").unwrap(), b"elst").unwrap(),
        Limits::default(),
    )
    .unwrap();
    let edit = Mov9Edit {
        entry: edits.entry(1).unwrap(),
        movie_start: Time::new(1, 2).unwrap(),
        movie_base: TimeBase::new(1, 1000).unwrap(),
    };
    let (sample, sync) = source_sample("r-edits-empty-trim", 1);
    let original = sample.clone();
    let view = Mov9View::new(TimeBase::new(1, 48000).unwrap(), sync)
        .project(&sample, Some(edit))
        .unwrap();
    assert_eq!(
        (view.pts, view.dts, view.duration, view.flags),
        (Some(22976), Some(22976), 1024, 5)
    );
    assert_eq!(sample, original);
    let (tail, _) = source_sample("r-edits-empty-trim", 6);
    assert_eq!(
        Mov9View::new(TimeBase::new(1, 48000).unwrap(), sync)
            .project(&tail, Some(edit))
            .unwrap()
            .flags,
        4
    );
}

#[test]
fn native_duration_can_follow_occurrence_clock_without_changing_decode_delta() {
    let (sample, sync) = source_sample("r-edits-repeat", 6);
    let mapping = Mov9Edit {
        entry: safe_media_isobmff::EditEntry {
            segment_duration: 2000,
            media_time: 3000,
            rate_integer: 1,
            rate_fraction: 0,
        },
        movie_start: Time::new(2, 1).unwrap(),
        movie_base: TimeBase::new(1, 1000).unwrap(),
    };
    let view: Mov9PacketView = Mov9View::new(TimeBase::new(1, 1000).unwrap(), sync)
        .project(&sample, Some(mapping))
        .unwrap();
    assert_eq!(view.dts, Some(4000));
    assert_eq!(view.with_following_dts(Some(4000)).unwrap().duration, 0);
    assert_eq!(sample.duration_ticks, 1000);
}

#[test]
fn native_stream_projection_retains_tx3g_private_bytes_without_copy() {
    let case = common::fixtures::CASES
        .iter()
        .find(|c| c.name == "r-subtitle-tx3g")
        .unwrap();
    let budget = Budget::new(1024 * 1024).unwrap();
    let roots = common::roots(&budget, case.bytes).unwrap();
    let movie = roots.iter().find(|r| r.header().kind == *b"moov").unwrap();
    let track = child(movie, b"trak").unwrap();
    let media = child(&track, b"mdia").unwrap();
    let header = TrackHeader::parse(
        &child(&track, b"tkhd").unwrap(),
        &child(&media, b"mdhd").unwrap(),
        &child(&media, b"hdlr").unwrap(),
    )
    .unwrap();
    let table = common::table(
        child(&child(&media, b"minf").unwrap(), b"stbl").unwrap(),
        &budget,
        1,
    )
    .unwrap();
    let description = &table.descriptions()[0];
    let stream = Mov9StreamView::new(&header, description).unwrap();
    assert_eq!(stream.codec, "mov_text");
    assert_eq!(stream.media_type, 3);
    assert_eq!(&stream.language, b"und");
    assert_eq!(stream.handler_name.as_ref(), b"Authored M03");
    assert_eq!(stream.extradata.as_ref(), &description.raw().payload()[8..]);
    assert!(
        stream
            .extradata
            .shares_storage_with(description.raw().bytes())
    );
    assert_eq!(stream.vendor_id, None);
}
