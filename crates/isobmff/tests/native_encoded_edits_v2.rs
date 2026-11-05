mod common;
use common::{child, children, roots, table};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, SourceCapabilities, SourceFailure, SourceId, TimeBase, WorkBudget,
};
use triergon_isobmff::{
    EditList, Limits, Mov9CursorStatus, Mov9EditCursor, Mov9EditLimits, Mov9EncodedEditContextV2,
    Mov9EncodedEditJobV2, Mov9EncodedEditLimitsV2, Mov9EncodedEditPlanV2, Mov9EncodedLimitsV1,
    Mov9EncodedStreamJobV1, Mov9EncodedStreamV1, Mov9NativeIndexCursor, Mov9NativeIndexLimits,
    Mov9PacketCursor, Mov9PacketLimits, Mov9Track, PinnedMovD32EncodedEditsV2,
    PinnedMovD32EncodedRegularV1, SampleTable, TrackHeader,
};
const ORIGINAL: &[u8] = include_bytes!("fixtures/encoded-edit-v2/original.mp4");
const AUDIO0: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio0.mp4");
const AUDIO1024: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio1024.mp4");
const AUDIO2048: &[u8] = include_bytes!("fixtures/encoded-edit-v2/audio2048.mp4");
const VIDEO0: &[u8] = include_bytes!("fixtures/encoded-edit-v2/video0.mp4");
const VIDEO2673: &[u8] = include_bytes!("fixtures/encoded-edit-v2/video2673.mp4");
struct Source {
    pool: Budget,
    header: TrackHeader,
    table: SampleTable,
    edits: EditList,
    media: Vec<Range<u64>>,
    length: u64,
}
impl Source {
    fn new(data: &[u8], handler: &[u8; 4]) -> Self {
        Self::in_pool(data, handler, Budget::new(1 << 22).unwrap())
    }
    fn in_pool(data: &[u8], handler: &[u8; 4], pool: Budget) -> Self {
        let top = roots(&pool, data).unwrap();
        let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
        let track = children(movie, 0)
            .unwrap()
            .into_iter()
            .filter(|x| x.header().kind == *b"trak")
            .find(|x| {
                child(&child(x, b"mdia").unwrap(), b"hdlr")
                    .unwrap()
                    .payload()
                    .get(8..12)
                    == Some(handler.as_slice())
            })
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
            .filter(|x| x.header().kind == *b"mdat")
            .map(|x| x.header().payload_range().unwrap())
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
    fn input(&self) -> InputCursor {
        InputCursor::new(
            &self.pool,
            SourceId(917),
            SourceCapabilities {
                length: Some(self.length),
                random_access: true,
            },
            64,
        )
        .unwrap()
    }
    fn stream(&self, i: &mut InputCursor) -> Mov9EncodedStreamV1 {
        let mut j = Mov9EncodedStreamJobV1::new(
            &self.header,
            &self.table,
            i,
            Some(*b"isom"),
            Mov9EncodedLimitsV1::default(),
            PinnedMovD32EncodedRegularV1,
        )
        .unwrap();
        for _ in 0..100000 {
            if j.poll(i, &mut WorkBudget::new(1), &AtomicBool::new(false))
                .unwrap()
                == Mov9CursorStatus::Ready
            {
                return j.take(i, &AtomicBool::new(false)).unwrap();
            }
        }
        panic!("stream progress")
    }
    fn job<'a>(
        &'a self,
        s: &'a Mov9EncodedStreamV1,
        i: &InputCursor,
        maximum: u64,
    ) -> Result<Mov9EncodedEditJobV2<'a>, Error> {
        Mov9EncodedEditJobV2::new(
            Mov9EncodedEditContextV2 {
                table: &self.table,
                edits: &self.edits,
                stream: s,
                media_base: self.header.time_base().unwrap(),
                movie_base: TimeBase::new(1, 1000).unwrap(),
            },
            i,
            Mov9EncodedEditLimitsV2 {
                max_samples: maximum,
            },
            PinnedMovD32EncodedEditsV2,
        )
    }
    fn plan(&self, q: u64) -> Result<Mov9EncodedEditPlanV2, Error> {
        let mut i = self.input();
        let s = self.stream(&mut i);
        complete(&mut self.job(&s, &i, 1000000)?, &mut i, q)
    }
    fn track(&self) -> Mov9Track<'_> {
        Mov9Track {
            track_id: self.header.track_id,
            table: &self.table,
            fragments: &[],
            media_base: self.header.time_base().unwrap(),
            edits: Some((&self.edits, TimeBase::new(1, 1000).unwrap())),
        }
    }
}
fn complete(
    j: &mut Mov9EncodedEditJobV2<'_>,
    i: &mut InputCursor,
    q: u64,
) -> Result<Mov9EncodedEditPlanV2, Error> {
    for _ in 0..100000 {
        let mut w = WorkBudget::new(q);
        let state = j.poll(i, &mut w, &AtomicBool::new(false))?;
        assert!(w.spent() <= q);
        if state == Mov9CursorStatus::Ready {
            return j.take(i, &AtomicBool::new(false));
        }
    }
    panic!("edit scan must progress with real work1")
}
fn rows(
    p: &Source,
    plan: Mov9EncodedEditPlanV2,
) -> Vec<(triergon_isobmff::Mov9PacketOccurrence, Option<[u8; 10]>)> {
    let plans = [Some(plan)];
    let mut c = Mov9PacketCursor::new_encoded_regular_v2(
        &p.pool,
        &[p.track()],
        &p.media,
        Mov9PacketLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    let mut out = Vec::new();
    for _ in 0..100000 {
        let mut w = WorkBudget::new(1);
        match c.poll(&mut w, &AtomicBool::new(false)).unwrap() {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::Ready => {
                let side = c.ready_encoded_skip_v2().unwrap();
                out.push((c.take_occurrence().unwrap(), side));
            }
            Mov9CursorStatus::End => return out,
        };
        assert!(w.spent() <= 1);
    }
    panic!("occurrence progress")
}
#[test]
fn original_video_all48_source_ranges_raw_cto_and_shift_are_separate() {
    let p = Source::new(ORIGINAL, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.native_anchor_ticks(), 2672);
    let r = rows(&p, plan);
    assert_eq!(r.len(), 48);
    assert_eq!(r[0].0.source.dts_ticks, Some(0));
    assert_eq!(r[0].0.source.cts_ticks, Some(2672));
    assert_eq!(r[0].0.native.dts, Some(-2672));
    assert_eq!(r[0].0.native.pts, Some(0));
    assert_eq!(r[0].0.source.range, 48..3128);
    assert!(r.iter().all(|x| x.1.is_none()));
}
#[test]
fn video_boundary2673_chooses_next_real_presentation_point_not_a_constant() {
    let p = Source::new(VIDEO2673, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.native_anchor_ticks(), 4000);
    let r = rows(&p, plan);
    assert_eq!(r.len(), 48);
    assert_eq!(r[0].0.native.pts, Some(-1328));
    assert_eq!(r[0].0.native.flags, 5);
    assert_eq!(r[0].0.source.cts_ticks, Some(2672));
}
#[test]
fn video_zero_edit_still_uses_first_real_composition_anchor() {
    let p = Source::new(VIDEO0, b"vide");
    let plan = p.plan(1).unwrap();
    assert_eq!(plan.source_entry().media_time, 0);
    assert_eq!(plan.native_anchor_ticks(), 2672);
    assert_eq!(rows(&p, plan)[0].0.native.dts, Some(-2672));
}
#[test]
fn original_audio189_keeps_source_delta_and_both_skip_recommendations() {
    let p = Source::new(ORIGINAL, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 189);
    assert_eq!(r[0].0.source.dts_ticks, Some(0));
    assert_eq!(r[0].0.native.dts, Some(-1008));
    assert_eq!(r[0].0.native.flags, 1);
    assert_eq!(r[0].1, Some([0xf0, 3, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert_eq!(r[188].0.source.duration_ticks, 528);
    assert_eq!(r[188].0.native.duration, 528);
    assert_eq!(r[188].1, Some([0, 0, 0, 0, 0xf0, 1, 0, 0, 0, 0]));
}
#[test]
fn encoded_cursor_rejects_plan_with_wrong_movie_timebase() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut input = p.input();
    let stream = p.stream(&mut input);
    let mut job = Mov9EncodedEditJobV2::new(
        Mov9EncodedEditContextV2 {
            table: &p.table,
            edits: &p.edits,
            stream: &stream,
            media_base: p.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 2000).unwrap(),
        },
        &input,
        Mov9EncodedEditLimitsV2::default(),
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    let plans = [Some(complete(&mut job, &mut input, 1).unwrap())];
    let result = Mov9PacketCursor::new_encoded_regular_v2(
        &p.pool,
        &[p.track()],
        &p.media,
        Mov9PacketLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .map(|_| ());
    assert_eq!(
        result,
        Err(Error::Invalid("encoded edit source association"))
    );
}
#[test]
fn zero_audio_edit_selects188_but_preserves_original189_source_table() {
    let p = Source::new(AUDIO0, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(p.table.sample_count(), 189);
    assert_eq!(r.len(), 188);
    assert_eq!(r[0].0.native.dts, Some(0));
    assert_eq!(r[0].1, None);
    assert_eq!(r[187].0.source.ordinal, 188);
    assert_eq!(r[187].1, Some([0, 0, 0, 0, 0xc8, 1, 0, 0, 0, 0]));
}
#[test]
fn complete_preroll_packet_is_discard_but_source_key_and_duration_remain() {
    let p = Source::new(AUDIO1024, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r[0].0.native.flags, 5);
    assert!(r[0].0.source.is_container_sync());
    assert_eq!(r[0].0.source.duration_ticks, 1024);
    assert_eq!(r[0].1, Some([0, 4, 0, 0, 0, 0, 0, 0, 0, 0]));
}
#[test]
fn two_audio_preroll_packets_keep_order_and_one_start_recommendation() {
    let p = Source::new(AUDIO2048, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 189);
    assert_eq!(r[0].0.native.dts, Some(-2048));
    assert_eq!(r[0].0.native.flags, 5);
    assert_eq!(r[1].0.native.flags, 5);
    assert_eq!(r[2].0.native.flags, 1);
    assert_eq!(r[0].1, Some([0, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert!(r[1].1.is_none());
}
#[test]
fn plan_work1_and_bulk_are_identical_and_keep_literal_elst() {
    let p = Source::new(ORIGINAL, b"vide");
    let a = p.plan(1).unwrap();
    let b = p.plan(4096).unwrap();
    assert_eq!(a.native_anchor_ticks(), b.native_anchor_ticks());
    assert_eq!(a.selected_end_ticks(), b.selected_end_ticks());
    assert_eq!(
        a.source_edit().bytes().as_ref(),
        p.edits.raw().bytes().as_ref()
    );
}
#[test]
fn zero_work_has_no_scan_allocation_position_or_ready_progress() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    let used = p.pool.used();
    let position = i.position();
    let mut w = WorkBudget::new(0);
    assert_eq!(
        j.poll(&mut i, &mut w, &AtomicBool::new(false)).unwrap(),
        Mov9CursorStatus::Yield
    );
    assert_eq!(w.spent(), 0);
    assert_eq!(i.position(), position);
    assert_eq!(p.pool.used(), used);
}
#[test]
fn cancelled_plan_is_sticky_even_at_zero_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(100), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Cancelled
    );
}
#[test]
fn ready_plan_cannot_be_taken_after_cancel_then_resurrected() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    while j
        .poll(&mut i, &mut WorkBudget::new(7), &AtomicBool::new(false))
        .unwrap()
        != Mov9CursorStatus::Ready
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
#[test]
fn transport_failure_precedes_zero_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    i.fail(i.source(), i.token(), SourceFailure::Transport)
        .unwrap();
    assert_eq!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false))
            .unwrap_err(),
        Error::Transport
    );
}
#[test]
fn logical_generation_change_rejects_even_without_work() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    i.reset(0).unwrap();
    assert!(matches!(
        j.poll(&mut i, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
}
#[test]
fn range_request_change_preserves_logical_source_plan() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 10000).unwrap();
    let generation = i.generation();
    i.request_range(32).unwrap();
    let plan = complete(&mut j, &mut i, 1).unwrap();
    assert_eq!(i.generation(), generation);
    assert_eq!(i.position(), 32);
    assert_eq!(plan.source_entry().media_time, 1008);
}
#[test]
fn foreign_pool_is_not_a_codec_epoch() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let other = Budget::new(1 << 20).unwrap();
    let f = InputCursor::new(
        &other,
        SourceId(917),
        SourceCapabilities {
            length: Some(p.length),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(p.job(&s, &f, 1000), Err(Error::ForeignPool)));
}
#[test]
fn retained_plan_keeps_full_hidden_source_owner_charged() {
    let p = Source::new(ORIGINAL, b"soun");
    let plan = p.plan(1).unwrap();
    let pool = p.pool.clone();
    drop(p);
    assert_eq!(plan.source_edit().header().kind, *b"elst");
    assert!(plan.source_table().bytes().retained_capacity() > plan.source_table().bytes().len());
    assert!(pool.used() > Budget::control_storage_bytes());
    drop(plan);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
#[test]
fn source_association_cannot_rebind_equal_bytes_from_different_owner() {
    let p = Source::new(ORIGINAL, b"soun");
    let plan = p.plan(1).unwrap();
    let other = Source::in_pool(ORIGINAL, b"soun", p.pool.clone());
    let plans = [Some(plan)];
    assert!(matches!(
        Mov9PacketCursor::new_encoded_regular_v2(
            &p.pool,
            &[other.track()],
            &other.media,
            Mov9PacketLimits::default(),
            &plans,
            PinnedMovD32EncodedEditsV2
        ),
        Err(Error::Invalid(_))
    ));
}
#[test]
fn source_sample_limit_counts_actual_scan_and_keeps_source_intact() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut i = p.input();
    let s = p.stream(&mut i);
    let mut j = p.job(&s, &i, 1).unwrap();
    assert_eq!(
        complete(&mut j, &mut i, 1).unwrap_err(),
        Error::Limit {
            requested: 2,
            maximum: 1
        }
    );
    assert_eq!(p.table.sample_count(), 189);
}
#[test]
fn old_strict_reordered_edit_cursor_stays_refused_after_new_admission() {
    let p = Source::new(ORIGINAL, b"vide");
    let _plan = p.plan(1).unwrap();
    let mut old = Mov9EditCursor::new(
        &p.table,
        &p.edits,
        p.header.time_base().unwrap(),
        TimeBase::new(1, 1000).unwrap(),
        Mov9EditLimits::default(),
    )
    .unwrap();
    for _ in 0..10000 {
        match old.poll(&mut WorkBudget::new(1)) {
            Err(Error::Unsupported(_)) => return,
            Ok(Mov9CursorStatus::Yield) => {}
            value => panic!("old strict policy changed {value:?}"),
        }
    }
    panic!("strict refusal did not progress")
}
#[test]
fn new_index_uses_same_real_source_positions_and_decode_ticks() {
    let p = Source::new(ORIGINAL, b"vide");
    let plans = [Some(p.plan(1).unwrap())];
    let mut c = Mov9NativeIndexCursor::new_encoded_regular_v2(
        &p.pool,
        p.track(),
        &p.media,
        Mov9NativeIndexLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    let mut count = 0;
    for _ in 0..100000 {
        match c
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
        {
            Mov9CursorStatus::Yield => {}
            Mov9CursorStatus::Ready => {
                let e = c.take_entry().unwrap();
                if count == 0 {
                    assert_eq!(e.position, 48);
                    assert_eq!(e.timestamp, -2672);
                    assert_eq!(e.size, 3080);
                    assert_eq!(e.flags, 1);
                }
                count += 1;
            }
            Mov9CursorStatus::End => {
                assert_eq!(count, 48);
                return;
            }
        }
    }
    panic!("index progress")
}

#[test]
fn charged_cursor_pressure_preserves_plan_for_bounded_retry() {
    let p = Source::new(ORIGINAL, b"soun");
    let plans = [Some(p.plan(1).unwrap())];
    let hold = p.pool.reserve(p.pool.limit() - p.pool.used()).unwrap();
    assert!(matches!(
        Mov9PacketCursor::new_encoded_regular_v2(
            &p.pool,
            &[p.track()],
            &p.media,
            Mov9PacketLimits::default(),
            &plans,
            PinnedMovD32EncodedEditsV2
        ),
        Err(Error::Backpressure { .. })
    ));
    drop(hold);
    let mut cursor = Mov9PacketCursor::new_encoded_regular_v2(
        &p.pool,
        &[p.track()],
        &p.media,
        Mov9PacketLimits::default(),
        &plans,
        PinnedMovD32EncodedEditsV2,
    )
    .unwrap();
    for _ in 0..100000 {
        if cursor
            .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
            .unwrap()
            == Mov9CursorStatus::Ready
        {
            assert_eq!(cursor.take_occurrence().unwrap().native.dts, Some(-1008));
            return;
        }
    }
    panic!("pressure retry progress")
}

#[test]
fn video_zero_edit_tail_discard_follows_declared_window_not_shifted_anchor() {
    let p = Source::new(VIDEO0, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    let flags: Vec<_> = r
        .iter()
        .filter(|(row, _)| row.native.flags & 4 != 0)
        .map(|(row, _)| row.source.ordinal)
        .collect();
    assert_eq!(flags, vec![45, 48]);
    assert_eq!(r[44].0.source.cts_ticks, Some(64000));
    assert_eq!(r[44].0.native.pts, Some(61328));
    assert_eq!(p.edits.entry(0).unwrap().media_time, 0);
}
#[test]
fn original_video_tail_keeps_declared_window_separate_from_zero_edit_control() {
    let p = Source::new(ORIGINAL, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 48);
    assert!(r.iter().all(|(row, _)| row.native.flags & 4 == 0));
    assert_eq!(p.edits.entry(0).unwrap().media_time, 2672);
}

#[test]
fn encoded_occurrence_provenance_video_retains_real_edit_ordinal_and_local_sequence() {
    let p = Source::new(ORIGINAL, b"vide");
    let r = rows(&p, p.plan(1).unwrap());
    for (row, _) in r {
        let e = row
            .edit
            .expect("known applied single edit must have identity");
        assert_eq!(e.edit_ordinal, 0);
        assert_eq!(e.sequence, row.source.ordinal);
        assert_eq!(
            e.duration_basis,
            triergon_isobmff::Mov9DurationBasis::OriginalDecodeTableOrdinal
        );
        assert_eq!(row.source.composition_offset.unwrap().version(), 0);
    }
}
#[test]
fn encoded_occurrence_provenance_audio_prefix_omits_no_source_fact() {
    let p = Source::new(AUDIO0, b"soun");
    let r = rows(&p, p.plan(1).unwrap());
    assert_eq!(r.len(), 188);
    let e = r[187].0.edit.expect("selected edit identity");
    assert_eq!(e.edit_ordinal, 0);
    assert_eq!(e.sequence, 188);
    assert_eq!(r[187].0.source.ordinal, 188);
    assert_eq!(p.table.sample_count(), 189);
    assert_eq!(p.edits.entry(0).unwrap().media_time, 0);
}

#[test]
fn v4_without_sdtp_keeps_existing_aac_roll_observation() {
    let p = Source::new(ORIGINAL, b"soun");
    let mut source = p.table.cursor();
    match source.poll(&mut WorkBudget::new(1)).unwrap() {
        triergon_isobmff::SampleStep::Sample(s) => assert_eq!(s.dependency, None),
        _ => panic!("original source first sample absent"),
    }
    let expected = rows(&p, p.plan(1).unwrap());
    let mut input = p.input();
    let configs = [p.stream(&mut input)];
    let mut job = Mov9EncodedEditJobV2::new_v4(
        Mov9EncodedEditContextV2 {
            table: &p.table,
            edits: &p.edits,
            stream: &configs[0],
            media_base: p.header.time_base().unwrap(),
            movie_base: TimeBase::new(1, 1000).unwrap(),
        },
        &configs,
        &input,
        Mov9EncodedEditLimitsV2::default(),
        triergon_isobmff::PinnedMovD32EncodedRegularV4,
    )
    .unwrap();
    let actual = rows(&p, complete(&mut job, &mut input, 1).unwrap());
    assert_eq!(actual.len(), 189);
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.0.source, expected.0.source);
        assert_eq!(actual.0.native, expected.0.native);
        assert_eq!(actual.1, expected.1);
    }
}
