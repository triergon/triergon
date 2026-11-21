use crate::support::players::*;
use crate::support::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{Budget, Error as C, InputCursor, SourceCapabilities, WorkBudget};
use triergon_playback::mp4::{Mp4Event, Mp4Packet};
use triergon_playback::{
    SourceOwner,
    mp4::{Mp4Open, OpenLimits, PlanLimits},
    probe::SelectedInput,
    *,
};

#[test]
fn bounded_timing_accepts_forward_index_with_finite_media() {
    use triergon_isobmff::table::TimingRunPolicy;

    let bytes = include_bytes!("../../../isobmff/tests/fixtures/bad-stts-count-mismatch.mov");
    let (opened, _, _) =
        open_with_timing_policy(bytes, TimingRunPolicy::BoundedPrefix, false).unwrap();
    let mut job = compat::mov::index_front(&opened.movie, PlanLimits::default()).unwrap();
    assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
    let plan = job.finish().unwrap();
    assert_eq!(plan.demux_rows().len(), 6);
    for (index, row) in plan.demux_rows().iter().enumerate() {
        assert_eq!(row.source.dts_ticks, Some(index as i128 * 1000));
        assert!(row.source.range.end <= bytes.len() as u64);
    }
}

#[test]
fn forward_encoded_edits_keep_aac_preroll_and_trim_in_source_order() {
    use triergon_playback::mp4::ProgressiveLimits;

    let raw = include_bytes!("../fixtures/mp4/forward-aac-priming.mp4");
    let (selected, budget, calls) = probe(raw, 37, true);
    let mut open = Mp4Open::new_progressive(selected, OpenLimits::default()).unwrap();
    run_open(&mut open, 17).unwrap();
    let mut opened = open.finish().unwrap();
    let plans = encoded_plans(&mut opened);
    let limits = PlanLimits::default();
    let mut index = compat::mov::forward_encoded_edits(&opened.movie, &plans, limits).unwrap();
    while !index.poll(&mut WorkBudget::new(17), &off()).unwrap() {}
    let engine = compat::mov::open_front(
        opened.input,
        index.finish().unwrap(),
        1 << 20,
        ProgressiveLimits::default(),
    )
    .unwrap();
    let mut player = Player::new_direct(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut player, 17).unwrap();
    let packets = mp4_packets(&out);
    assert!(
        packets
            .windows(2)
            .all(|rows| { rows[0].source.sample.range.end <= rows[1].source.sample.range.start })
    );
    let audio: Vec<_> = packets
        .iter()
        .filter(|packet| packet.configuration.track.handler == *b"soun")
        .collect();
    let trim = |packet: &Mp4Packet| {
        let data = packet.encoded_skip_samples.unwrap();
        (
            u32::from_le_bytes(data[..4].try_into().unwrap()),
            u32::from_le_bytes(data[4..8].try_into().unwrap()),
        )
    };
    assert_eq!(
        audio
            .first()
            .unwrap()
            .occurrence
            .as_ref()
            .unwrap()
            .demux
            .pts,
        Some(-1024)
    );
    assert_eq!(trim(audio.first().unwrap()), (1024, 0));
    assert_eq!(trim(audio.last().unwrap()), (0, 949));
    for packet in packets {
        let range = &packet.source.sample.range;
        assert_eq!(
            packet.source.payload.as_ref(),
            &raw[range.start as usize..range.end as usize]
        );
    }
    let offsets: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|call| {
            if let Call::Poll(offset) = call {
                Some(*offset)
            } else {
                None
            }
        })
        .collect();
    assert!(offsets.windows(2).all(|rows| rows[0] < rows[1]));
    drop(out);
    drop(player);
    drop(plans);
    drop(opened.movie);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn forward_fragment_edits_keep_source_clocks_and_trim_once() {
    use triergon_playback::mp4::ProgressiveLimits;

    for raw in [
        include_bytes!("../fixtures/mp4/forward-fragment-aac-edit.mp4").as_slice(),
        include_bytes!("../fixtures/mp4/forward-fragment-aac-late-track.mp4").as_slice(),
    ] {
        let (selected, budget, calls) = probe(raw, 37, true);
        let mut open = Mp4Open::new_progressive(selected, OpenLimits::default()).unwrap();
        run_open(&mut open, 17).unwrap();
        let opened = open.finish().unwrap();
        let mut index = compat::mov::index_front(&opened.movie, PlanLimits::default()).unwrap();
        assert!((0..100_000).any(|_| index.poll(&mut WorkBudget::new(17), &off()).unwrap()));
        let engine = compat::mov::open_front(
            opened.input,
            index.finish().unwrap(),
            1 << 20,
            ProgressiveLimits::default(),
        )
        .unwrap();
        let mut player = Player::new_direct(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let out = collect(&mut player, 17).unwrap();
        let packets = mp4_packets(&out);
        assert_eq!(packets.len(), 21);
        assert!(
            packets.windows(2).all(|rows| {
                rows[0].source.sample.range.end <= rows[1].source.sample.range.start
            })
        );
        let mut audio_count = 0;
        for packet in packets {
            let source = &packet.source.sample;
            let range = &source.range;
            assert_eq!(
                packet.source.payload.as_ref(),
                &raw[range.start as usize..range.end as usize]
            );
            if packet.configuration.track.handler == *b"soun" {
                let expected = 1024 * audio_count;
                let occurrence = packet.occurrence.as_ref().unwrap();
                assert_eq!(source.dts_ticks, Some(i128::from(expected)));
                assert_eq!(occurrence.demux.dts, Some(expected - 1024));
                assert_eq!(occurrence.demux.pts, Some(expected - 1024));
                let mut leading_trim = [0; 10];
                leading_trim[..4].copy_from_slice(&1024u32.to_le_bytes());
                assert_eq!(
                    packet.encoded_skip_samples,
                    (audio_count == 0).then_some(leading_trim)
                );
                audio_count += 1;
            }
        }
        assert_eq!(audio_count, 17);
        let calls = calls.lock().unwrap();
        let offsets: Vec<_> = calls
            .iter()
            .filter_map(|call| match call {
                Call::Poll(offset) => Some(*offset),
                _ => None,
            })
            .collect();
        assert!(offsets.windows(2).all(|rows| rows[0] < rows[1]));
        drop(calls);
        drop(out);
        drop(player);
        drop(opened.movie);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn forward_fragment_ranges_are_checked_before_initialization_edit_support() {
    let mut raw = include_bytes!("../fixtures/mp4/forward-fragment-aac-edit.mp4").to_vec();
    let edit = raw.windows(4).rposition(|kind| kind == b"elst").unwrap();
    assert_eq!(&raw[edit + 12..edit + 16], &0u32.to_be_bytes());
    raw[edit + 12..edit + 16].copy_from_slice(&1u32.to_be_bytes());
    let index_error = |bytes: &[u8]| {
        let (selected, _, _) = probe(bytes, 37, true);
        let mut open = Mp4Open::new_progressive(selected, OpenLimits::default()).unwrap();
        run_open(&mut open, 17).unwrap();
        let opened = open.finish().unwrap();
        let mut index = compat::mov::index_front(&opened.movie, PlanLimits::default()).unwrap();
        for _ in 0..100_000 {
            match index.poll(&mut WorkBudget::new(17), &off()) {
                Err(error) => return error,
                Ok(false) => {}
                Ok(true) => panic!("the edited source was accepted"),
            }
        }
        panic!("the index did not finish")
    };
    assert_eq!(
        index_error(&raw),
        C::Unsupported("fragment edit needs open-ended rate-one media")
    );

    let run = raw.windows(4).position(|kind| kind == b"trun").unwrap();
    assert_eq!(raw[run + 7] & 1, 1);
    raw[run + 12..run + 16].copy_from_slice(&0i32.to_be_bytes());
    assert_eq!(
        index_error(&raw),
        C::Invalid("sample outside self-contained mdat")
    );
}

#[test]
fn forward_encoded_edit_drains_samples_outside_the_presentation() {
    let mut raw = include_bytes!("../fixtures/mp4/forward-aac-priming.mp4").to_vec();
    let audio_edit = raw.windows(4).rposition(|kind| kind == b"elst").unwrap();
    let duration = &mut raw[audio_edit + 12..audio_edit + 16];
    assert_eq!(duration, &15_435_u32.to_be_bytes());
    duration.copy_from_slice(&4_410_u32.to_be_bytes());

    for truncate in [0, 11] {
        let budget = pool();
        let bytes = &raw[..raw.len() - truncate];
        let (mut source, _, _) = observed(&budget, bytes, 37, true);
        let release = Arc::new(AtomicBool::new(false));
        source.end = Some((bytes.len() as u64, release.clone()));
        let selected = SelectedInput {
            input: InputCursor::new(
                &budget,
                ID,
                SourceCapabilities {
                    length: None,
                    random_access: false,
                },
                4096,
            )
            .unwrap(),
            source: SourceOwner::new(source),
            format: Format::Mp4,
        };
        let mut open = Mp4Open::new_progressive(selected, OpenLimits::default()).unwrap();
        run_open(&mut open, 17).unwrap();
        let mut opened = open.finish().unwrap();
        let plans = encoded_plans(&mut opened);
        let mut index =
            compat::mov::forward_encoded_edits(&opened.movie, &plans, PlanLimits::default())
                .unwrap();
        run_index(&mut index, 17).unwrap();
        let prepared = index.finish().unwrap();
        assert_eq!(prepared.source_rows().len(), 21);
        assert_eq!(prepared.rows(), 10);
        let engine = compat::mov::open_front(
            opened.input,
            prepared,
            1 << 20,
            mp4::ProgressiveLimits::default(),
        )
        .unwrap();
        let mut player = Player::new_direct(
            engine,
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let mut output = None;
        let mut packets = 0;
        let request = (0..10_000)
            .find_map(|_| {
                let progress = player
                    .read_output(&mut output, &mut WorkBudget::new(17), &off())
                    .unwrap();
                if let Some(event) = output.take() {
                    packets +=
                        usize::from(matches!(event.event(), Event::Mp4(Mp4Event::Packet(_))));
                }
                assert!(!matches!(progress, Progress::Ended));
                match progress {
                    Progress::NeedInput(request) if request.offset == bytes.len() as u64 => {
                        Some(request)
                    }
                    _ => None,
                }
            })
            .unwrap();
        assert_eq!(packets, 10);
        let Progress::NeedInput(retry) = player
            .read_output(&mut output, &mut WorkBudget::new(17), &off())
            .unwrap()
        else {
            panic!("expected the pending end request to be retried");
        };
        assert_eq!(retry.token, request.token);
        assert_eq!(retry.offset, request.offset);
        assert!(output.is_none());
        release.store(true, Ordering::Relaxed);
        let result = bounded("edited source end", || {
            let progress = player.read_output(&mut output, &mut WorkBudget::new(17), &off())?;
            assert!(output.is_none());
            Ok(matches!(progress, Progress::Ended))
        });
        assert_eq!(
            result,
            if truncate == 0 {
                Ok(())
            } else {
                Err(C::Truncated)
            }
        );
    }
}

#[test]
fn forward_encoded_edit_order_refusal_survives_repeated_poll() {
    let mut raw = include_bytes!("../fixtures/mp4/forward-aac-priming.mp4").to_vec();
    let audio_offsets = raw.windows(4).rposition(|kind| kind == b"stco").unwrap();
    // Place the first AAC sample one byte after the first video sample starts.
    let audio_start = &mut raw[audio_offsets + 12..audio_offsets + 16];
    assert_eq!(audio_start, &1572_u32.to_be_bytes());
    audio_start.copy_from_slice(&1749_u32.to_be_bytes());

    let (selected, _, _) = probe(&raw, 37, true);
    let mut open = Mp4Open::new_progressive(selected, OpenLimits::default()).unwrap();
    run_open(&mut open, 17).unwrap();
    let mut opened = open.finish().unwrap();
    let plans = encoded_plans(&mut opened);
    let mut index =
        compat::mov::forward_encoded_edits(&opened.movie, &plans, PlanLimits::default()).unwrap();
    let expected = C::Unsupported("forward encoded edits revisit media");
    assert_eq!(run_index(&mut index, 17), Err(expected));
    assert_eq!(index.poll(&mut WorkBudget::new(17), &off()), Err(expected));
}
