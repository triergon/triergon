use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use triergon_core::{
    Budget, Bytes, Error as C, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceId, Time, WorkBudget,
};
use triergon_mpegts::{
    PacketFormat, TraceEvent,
    clock::{SourceTimePolicy, SourceTimeStatus},
};
use triergon_playback::{ByteSource, Source, SourceReply};
use triergon_playback::{
    io::{Probe, ProbeProgress, SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    *,
};
mod gold {
    include!("fixtures/mp4_golds.rs");
}
const ID: SourceId = SourceId(771);
fn off() -> AtomicBool {
    AtomicBool::new(false)
}
fn pool() -> Budget {
    Budget::new(64 * 1024 * 1024).unwrap()
}
fn open(
    raw: &[u8],
    quantum: usize,
    units: u64,
) -> Result<(mp4_open::OpenedMp4<ByteSource>, Budget)> {
    let b = pool();
    let source = SourceOwner::new(ByteSource::new(
        ID,
        Bytes::copy_from_slice(&b, raw)?,
        quantum,
    )?);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )?;
    let mut job = Mp4Open::new(
        SelectedInput {
            input,
            source,
            format: Format::Mp4,
        },
        OpenLimits {
            metadata_bytes: 1 << 20,
            ..OpenLimits::default()
        },
    )?;
    for _ in 0..2_000_000 {
        if matches!(
            job.advance(&mut WorkBudget::new(units), &off())?,
            OpenProgress::Ready
        ) {
            return Ok((job.finish()?, b));
        }
    }
    panic!("real MP4 open did not make bounded progress");
}
fn mp4(
    raw: &[u8],
    view: Mp4View,
    quantum: usize,
    units: u64,
) -> Result<(Player<ByteSource>, Budget)> {
    let (opened, b) = open(raw, quantum, units)?;
    let mut job = Mp4IndexJob::new(&opened.movie, view, PlanLimits::default())?;
    for _ in 0..2_000_000 {
        if job.poll(&mut WorkBudget::new(units), &off())? {
            let plan = job.finish()?;
            let player = Player::new(
                Engine::mp4(opened.input, plan, 1 << 20)?,
                opened.source,
                Selection::All,
                PlayerPolicy::default(),
            )?;
            return Ok((player, b));
        }
    }
    panic!("real MP4 index did not finish");
}
fn ts(
    raw: &[u8],
    format: PacketFormat,
    quantum: usize,
    policy: SourceTimePolicy,
) -> Result<(Player<ByteSource>, Budget)> {
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        4096,
    )?;
    let source = SourceOwner::new(ByteSource::new(
        ID,
        Bytes::copy_from_slice(&b, raw)?,
        quantum,
    )?);
    Ok((
        Player::new(
            Engine::ts(
                input,
                format,
                1,
                triergon_mpegts::ParseLimits::default(),
                policy,
            )?,
            source,
            Selection::All,
            PlayerPolicy::default(),
        )?,
        b,
    ))
}
fn collect<S: Source>(p: &mut Player<S>, units: u64) -> Result<Vec<Output>> {
    let mut out = Vec::new();
    for _ in 0..2_000_000 {
        let progress = p.advance(&mut WorkBudget::new(units), &off())?;
        while let Some(o) = p.pop() {
            out.push(o);
        }
        if matches!(progress, Progress::Ended) {
            return Ok(out);
        }
    }
    panic!("fixture reader stalled");
}
fn first_packet<S: Source>(p: &mut Player<S>) -> Output {
    for _ in 0..2_000_000 {
        p.advance(&mut WorkBudget::new(17), &off()).unwrap();
        while let Some(o) = p.pop() {
            if o.event().track().is_some() {
                return o;
            }
        }
    }
    panic!("no source packet");
}
fn mp4_packets(v: &[Output]) -> Vec<&Mp4Packet> {
    v.iter()
        .filter_map(|o| {
            if let RawEvent::Mp4(Mp4Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}
fn pes(v: &[Output]) -> Vec<&TsEvent> {
    v.iter()
        .filter_map(|o| {
            if let RawEvent::Ts(e) = o.event()
                && matches!(e.source, TraceEvent::Pes(_))
            {
                return Some(e);
            }
            None
        })
        .collect()
}

#[test]
fn all17_frozen_mp4_controls_keep_independent_original_source_gold() {
    assert_eq!(gold::CASES.len(), 17);
    for case in gold::CASES {
        println!(
            "SOURCE_CASE {} fragmented={} gap={} malformed={}",
            case.name, case.fragmented, case.gap, case.malformed
        );
        let result = mp4(case.bytes, Mp4View::SourcePhysical, 7, 1);
        if case.malformed {
            assert!(
                result.is_err(),
                "{} must refuse malformed source",
                case.name
            );
            continue;
        }
        let (mut p, b) = result.unwrap_or_else(|e| panic!("{} {e:?}", case.name));
        let out = collect(&mut p, 1).unwrap();
        let packets = mp4_packets(&out);
        assert_eq!(packets.len(), case.samples.len(), "{}", case.name);
        for expected in case.samples {
            let p = packets
                .iter()
                .find(|p| {
                    p.configuration.track.track_id == expected.track
                        && p.source.sample.ordinal == expected.ordinal
                })
                .unwrap();
            let sample = &p.source.sample;
            assert_eq!(
                sample.description_index, expected.description,
                "{}",
                case.name
            );
            assert_eq!(sample.range.start, expected.offset);
            assert_eq!(p.source.payload.as_ref(), expected.payload);
            assert_eq!(sample.duration_ticks, expected.delta);
            assert_eq!(sample.is_container_sync(), expected.sync);
            if let Some(dts) = sample.dts_ticks {
                assert_eq!(dts, expected.dts);
                assert_eq!(sample.cts_ticks, Some(expected.cts));
            } else {
                assert_eq!(case.name, "f-no-second-tfdt");
                assert!(sample.cts_ticks.is_none());
            }
            assert_eq!(
                p.source.payload.as_ref(),
                &case.bytes[sample.range.start as usize..sample.range.end as usize]
            );
        }
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn actual_mp4_native_order_stays_distinct_from_all_source_facts() {
    let raw = include_bytes!("fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, Mp4View::PinnedMov9NoParseNoFillin, 7, 1).unwrap();
    if let Some(Engine::Mp4(engine)) = p.engine() {
        assert_eq!(engine.plan.source_rows().len(), 12);
        assert_eq!(engine.plan.native_rows().len(), 12);
    } else {
        panic!()
    }
    let out = collect(&mut p, 1).unwrap();
    let packets = mp4_packets(&out);
    let order: Vec<_> = packets
        .iter()
        .map(|p| (p.configuration.track.track_id, p.source.sample.ordinal))
        .collect();
    assert_eq!(
        order,
        [
            (1, 1),
            (1, 2),
            (2, 1),
            (2, 2),
            (1, 3),
            (2, 3),
            (1, 4),
            (1, 5),
            (2, 4),
            (1, 6),
            (2, 5),
            (2, 6)
        ]
    );
    for p in packets {
        let n = p.native.as_ref().unwrap();
        assert_eq!(n.source, p.source.sample);
        assert_eq!(n.native.source_range, p.source.sample.range);
    }
}
#[test]
fn description_changes_publish_raw_config_before_each_matching_packet() {
    for raw in [
        include_bytes!("fixtures/mp4/r-description-change.mov").as_slice(),
        include_bytes!("fixtures/mp4/f-description-change.mp4").as_slice(),
    ] {
        let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 1, 1).unwrap();
        let out = collect(&mut p, 1).unwrap();
        let mut last = None;
        let mut seen = Vec::new();
        for o in &out {
            match o.event() {
                RawEvent::Mp4(Mp4Event::Configuration(c)) => {
                    last = Some((c.track.track_id, c.description.index, c.revision));
                    seen.push(c.description.index);
                }
                RawEvent::Mp4(Mp4Event::Packet(p)) => {
                    assert_eq!(
                        last,
                        Some((
                            p.configuration.track.track_id,
                            p.source.sample.description_index,
                            p.configuration.revision
                        ))
                    );
                    assert_eq!(
                        p.configuration.description.index,
                        p.source.sample.description_index
                    );
                }
                _ => {}
            }
        }
        assert!(seen.contains(&1) && seen.contains(&2));
    }
}
#[test]
fn multitrack_packet_configuration_uses_its_track_revision_not_latest_other_track() {
    let (mut p, _) = mp4(
        include_bytes!("fixtures/mp4/r-multitrack.mov"),
        Mp4View::SourcePhysical,
        7,
        1,
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let mut revisions = std::collections::BTreeMap::new();
    for o in out {
        match o.event() {
            RawEvent::Mp4(Mp4Event::Configuration(c)) => {
                revisions.insert(c.track.track_id, c.revision);
            }
            RawEvent::Mp4(Mp4Event::Packet(p)) => assert_eq!(
                Some(&p.configuration.revision),
                revisions.get(&p.configuration.track.track_id)
            ),
            _ => {}
        }
    }
}
#[test]
fn source_unknown_fragment_clock_is_not_filled_from_native_projection() {
    let raw = include_bytes!("fixtures/mp4/f-no-second-tfdt.mp4");
    let (mut p, _) = mp4(raw, Mp4View::PinnedMov9NoParseNoFillin, 1, 1).unwrap();
    let out = collect(&mut p, 1).unwrap();
    let unknown: Vec<_> = mp4_packets(&out)
        .into_iter()
        .filter(|p| p.source.sample.dts_ticks.is_none())
        .collect();
    assert!(!unknown.is_empty());
    assert!(unknown.iter().all(|p| p.source.sample.cts_ticks.is_none()));
    assert!(
        unknown
            .iter()
            .any(|p| p.native.as_ref().is_some_and(|n| n.native.dts.is_some()))
    );
}
#[test]
fn source_and_native_clock_delivery_are_explicit_independent_views() {
    let raw = include_bytes!("fixtures/ts/pts-negative-against-dts.ts");
    let (mut strict, _) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
    let source = collect(&mut strict, 1).unwrap();
    let e = pes(&source)[0];
    assert_eq!(e.source_pts.unwrap().raw33, Some((1_u64 << 33) - 2));
    assert_eq!(
        e.source_pts.unwrap().status,
        SourceTimeStatus::AnchorBoundMissing
    );
    assert!(e.source_pts.unwrap().exact.is_none());
    assert_eq!(e.native_clock.unwrap().pts90, Some((1_i128 << 33) - 2));
    assert_eq!(e.native_clock.unwrap().dts90, Some(3));
    assert!(
        source
            .iter()
            .filter(|o| o.event().track().is_some())
            .all(|o| o.delivery() == Delivery::UnknownTime)
    );
    let (mut native, _) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
    native
        .set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let out = collect(&mut native, 1).unwrap();
    assert!(
        out.iter()
            .filter(|o| o.event().track().is_some())
            .all(|o| o.delivery() == Delivery::Present)
    );
    assert!(native.set_clock_view(ClockChoice::SourceExact).is_err());
}
#[test]
fn ts188_and_m2ts_real_readers_preserve_complete_carriage_and_pes() {
    for (raw, format) in [
        (
            include_bytes!("fixtures/ts/ts188-basic.ts").as_slice(),
            PacketFormat::Ts188,
        ),
        (
            include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts").as_slice(),
            PacketFormat::M2ts192,
        ),
    ] {
        for q in [1, 7, 4096] {
            let (mut p, b) = ts(raw, format, q, SourceTimePolicy::RelativeEpochZero).unwrap();
            let out = collect(&mut p, 1).unwrap();
            let events = pes(&out);
            assert_eq!(events.len(), 1);
            let TraceEvent::Pes(pes) = &events[0].source else {
                panic!()
            };
            assert_eq!(pes.pid, 256);
            assert_eq!(pes.header.pts_raw33, Some(90_000));
            assert_eq!(
                pes.payload().flatten().copied().collect::<Vec<_>>(),
                b"lane-ts-original-payload\x00\x47\xff"
            );
            assert_eq!(events[0].native_clock.unwrap().pts90, Some(90_000));
            assert_eq!(
                events[0].source_pts.unwrap().exact,
                Some(Time::new(1, 1).unwrap())
            );
            let carriers: Vec<_> = out
                .iter()
                .filter_map(|o| {
                    if let RawEvent::Ts(TsEvent {
                        source: TraceEvent::Transport(t),
                        ..
                    }) = o.event()
                    {
                        Some(t)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(carriers.len(), 3);
            for (i, c) in carriers.iter().enumerate() {
                assert_eq!(
                    c.source.as_ref(),
                    &raw[i * format.bytes()..(i + 1) * format.bytes()]
                );
                if format == PacketFormat::M2ts192 {
                    assert_eq!(
                        c.prefix.as_ref().unwrap().as_ref(),
                        &raw[i * 192..i * 192 + 4]
                    );
                }
            }
            drop(out);
            drop(p);
            assert_eq!(b.used(), Budget::control_storage_bytes());
        }
    }
}
#[test]
fn actual_pmt_version_change_preserves_both_maps_and_epoch_order() {
    let (mut p, _) = ts(
        include_bytes!("fixtures/ts/pmt-version-wrap-retypes-pid.ts"),
        PacketFormat::Ts188,
        1,
        SourceTimePolicy::Unanchored,
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let maps: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let RawEvent::Ts(TsEvent {
                source:
                    TraceEvent::ProgramMap {
                        map,
                        active: true,
                        configuration_epoch,
                    },
                ..
            }) = o.event()
            {
                Some((
                    map.header.version,
                    *configuration_epoch,
                    map.streams.as_slice()[0].stream_type,
                    map.source.clone(),
                ))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(maps.len(), 2);
    assert!(maps[1].1 > maps[0].1);
    assert_ne!(maps[0].2, maps[1].2);
    assert_ne!(maps[0].3.as_ref(), maps[1].3.as_ref());
    assert!(pes(&out).is_empty());
}
#[test]
fn source_ts_errors_are_retained_diagnostics_not_fabricated_pes() {
    for (raw, expected_prefix_pes) in [
        (
            include_bytes!("fixtures/ts/psi-crc-invalid.ts").as_slice(),
            0,
        ),
        (
            include_bytes!("fixtures/ts/pes-timestamp-marker.ts").as_slice(),
            0,
        ),
        (
            include_bytes!("fixtures/ts/transport-partial-final-packet.ts").as_slice(),
            1,
        ),
    ] {
        let (mut p, _) = ts(raw, PacketFormat::Ts188, 1, SourceTimePolicy::Unanchored).unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert!(out.iter().any(|o| matches!(
            o.event(),
            RawEvent::Ts(TsEvent {
                source: TraceEvent::Diagnostic(_),
                ..
            })
        )));
        assert_eq!(pes(&out).len(), expected_prefix_pes);
        if expected_prefix_pes == 1 {
            let TraceEvent::Pes(packet) = &pes(&out)[0].source else {
                panic!()
            };
            assert_eq!(packet.header.pts_raw33, Some(90_000));
            assert_eq!(packet.payload_len, 27);
        }
    }
}
#[test]
fn all_remaining_frozen_ts_cases_have_actual_terminal_owned_outcomes() {
    for raw in [
        include_bytes!("fixtures/ts/pmt-opaque-language-descriptors.ts").as_slice(),
        include_bytes!("fixtures/ts/pts-wrap-forward.ts").as_slice(),
        include_bytes!("fixtures/ts/pts-dts-common-epoch.ts").as_slice(),
        include_bytes!("fixtures/ts/carriage-hevc-direct.ts").as_slice(),
        include_bytes!("fixtures/ts/carriage-aac-adts-direct.ts").as_slice(),
    ] {
        let (mut p, b) = ts(raw, PacketFormat::Ts188, 7, SourceTimePolicy::Unanchored).unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert!(out.iter().any(|o| matches!(
            o.event(),
            RawEvent::Ts(TsEvent {
                source: TraceEvent::Finished,
                ..
            })
        )));
        for e in pes(&out) {
            assert!(e.source_pts.unwrap().exact.is_none());
            assert!(e.native_clock.is_some());
        }
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Poll(u64),
    Invalidate,
    Abort,
    Close,
}
struct Observed {
    inner: ByteSource,
    calls: Arc<Mutex<Vec<Call>>>,
    fail: Arc<AtomicBool>,
    sequential: bool,
    next: u64,
    pending: usize,
}
impl Source for Observed {
    fn poll(
        &mut self,
        r: &InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.calls.lock().unwrap().push(Call::Poll(r.offset));
        if self.pending > 0 {
            self.pending -= 1;
            return Ok(SourceReply::Pending);
        }
        if self.fail.load(Ordering::Relaxed) {
            return Ok(SourceReply::Failed {
                source: r.source,
                token: r.token.clone(),
                failure: triergon_core::SourceFailure::Transport,
            });
        }
        if self.sequential && r.offset != self.next {
            return Err(C::Invalid(
                "test sequential source was asked to rewind or skip",
            ));
        }
        let out = self.inner.poll(r, w, c)?;
        if let SourceReply::Data(win) = &out {
            self.next = win.offset + win.bytes().len() as u64;
        }
        Ok(out)
    }
    fn invalidate(&mut self) {
        self.calls.lock().unwrap().push(Call::Invalidate);
        self.inner.invalidate();
    }
    fn abort(&mut self) {
        self.calls.lock().unwrap().push(Call::Abort);
        self.inner.abort();
    }
    fn close(&mut self) {
        self.calls.lock().unwrap().push(Call::Close);
        self.inner.close();
    }
}
type Calls = Arc<Mutex<Vec<Call>>>;
fn observed(
    b: &Budget,
    raw: &[u8],
    quantum: usize,
    sequential: bool,
) -> (Observed, Calls, Arc<AtomicBool>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(false));
    (
        Observed {
            inner: ByteSource::new(ID, Bytes::copy_from_slice(b, raw).unwrap(), quantum).unwrap(),
            calls: calls.clone(),
            fail: fail.clone(),
            sequential,
            next: 0,
            pending: 0,
        },
        calls,
        fail,
    )
}
fn probe(
    raw: &[u8],
    q: usize,
    sequential: bool,
) -> (io::SelectedInput<io::ReplaySource<Observed>>, Budget, Calls) {
    let b = pool();
    let (source, calls, _) = observed(&b, raw, q, sequential);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: !sequential,
        },
        4096,
        source,
    )
    .unwrap();
    for _ in 0..2_000_000 {
        if let ProbeProgress::Selected(_) = probe.advance(&mut WorkBudget::new(1), &off()).unwrap()
        {
            return (probe.finish().unwrap(), b, calls);
        }
    }
    panic!("probe stalled");
}
fn observed_ts() -> (Player<Observed>, Budget, Calls, Arc<AtomicBool>) {
    let raw = include_bytes!("fixtures/ts/ts188-basic.ts");
    let b = pool();
    let (source, calls, fail) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let p = Player::new(
        Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
        )
        .unwrap(),
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    (p, b, calls, fail)
}
#[test]
fn actual_signature_probe_selects_mp4_ts_and_m2ts_without_file_names() {
    for (raw, want) in [
        (
            include_bytes!("fixtures/mp4/r-front-stco.mov").as_slice(),
            Format::Mp4,
        ),
        (
            include_bytes!("fixtures/ts/ts188-basic.ts").as_slice(),
            Format::Ts(PacketFormat::Ts188),
        ),
        (
            include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts").as_slice(),
            Format::Ts(PacketFormat::M2ts192),
        ),
    ] {
        for q in [1, 7, 4096] {
            let (selected, _, _) = probe(raw, q, false);
            assert_eq!(selected.format, want);
            assert_eq!(selected.input.position(), 0);
        }
    }
}
#[test]
fn sequential_ts_probe_replays_owned_prefix_without_transport_rewind() {
    let raw = include_bytes!("fixtures/ts/ts188-basic.ts");
    for q in [1, 7, 4096] {
        let (selected, b, calls) = probe(raw, q, true);
        let input = selected.input;
        let engine = Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
        )
        .unwrap();
        let mut player = Player::new(
            engine,
            selected.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let out = collect(&mut player, 1).unwrap();
        assert_eq!(pes(&out).len(), 1);
        let polls: Vec<_> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                if let Call::Poll(at) = c {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect();
        assert!(polls.windows(2).all(|pair| pair[0] < pair[1]));
        drop(out);
        drop(player);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn probe_work_zero_and_cancellation_have_no_source_io_or_false_selection() {
    let b = pool();
    let (source, calls, _) = observed(&b, b"not a movie", 7, false);
    let mut p = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: None,
            random_access: true,
        },
        64,
        source,
    )
    .unwrap();
    let before = b.used();
    for _ in 0..10 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            ProbeProgress::Yield
        ));
    }
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(b.used(), before);
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(C::Cancelled))
    ));
    drop(p);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[Call::Abort, Call::Close]
    );
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn mp4_work_zero_after_open_and_index_construction_neither_polls_nor_allocates() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut o = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    let before = b.used();
    for _ in 0..10 {
        assert!(matches!(
            o.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            OpenProgress::Yield
        ));
    }
    assert_eq!(b.used(), before);
    assert!(calls.lock().unwrap().is_empty());
    drop(o);
    assert_eq!(b.used(), Budget::control_storage_bytes());
    let (opened, b) = open(raw, 7, 1).unwrap();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    let before = b.used();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    assert_eq!(b.used(), before);
}
#[test]
fn live_player_work_zero_has_no_queue_reader_or_transport_progress() {
    let (mut p, b, calls, _) = observed_ts();
    let before = (b.used(), calls.lock().unwrap().len());
    for _ in 0..10 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            Progress::Yield
        ));
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    assert_eq!(p.queued(), 0);
}
#[test]
fn pause_resume_retains_scope_and_prevents_read_pop_and_delivery() {
    let (mut p, b, calls, _) = observed_ts();
    let first = first_packet(&mut p);
    p.pause(true).unwrap();
    let before = (b.used(), calls.lock().unwrap().len());
    assert!(p.current(&first));
    assert!(!p.may_deliver(&first));
    for _ in 0..4 {
        assert!(matches!(
            p.advance(&mut WorkBudget::new(100), &off()).unwrap(),
            Progress::Paused
        ));
        assert!(p.pop().is_none());
    }
    assert_eq!((b.used(), calls.lock().unwrap().len()), before);
    p.pause(false).unwrap();
    assert!(p.may_deliver(&first));
    collect(&mut p, 1).unwrap();
}
#[test]
fn transport_failure_is_terminal_distinct_from_native_and_source_eof() {
    let (mut p, _, calls, fail) = observed_ts();
    fail.store(true, Ordering::Relaxed);
    let mut failed = false;
    for _ in 0..20 {
        if let Err(Error::Core(C::Transport)) = p.advance(&mut WorkBudget::new(7), &off()) {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert_eq!(p.state(), State::Failed);
    let l = calls.lock().unwrap();
    assert_eq!(&l[l.len() - 2..], &[Call::Abort, Call::Close]);
    assert!(p.pop().is_none());
}
#[test]
fn empty_and_pending_input_are_retryable_starvation_not_eof() {
    let (mut p, b, _, _) = observed_ts();
    let r = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
    let empty = SourceReply::Data(
        InputWindow::new(
            ID,
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&b, &[]).unwrap(),
        )
        .unwrap(),
    );
    assert!(!p.provide(&SourceReply::Pending).unwrap());
    assert!(!p.provide(&empty).unwrap());
    assert_eq!(p.state(), State::Running);
    assert!(p.pending_request().is_some());
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(pes(&out).len(), 1);
}
#[test]
fn stale_foreign_source_and_foreign_pool_windows_preserve_live_request() {
    let (mut p, b, _, _) = observed_ts();
    let r = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
    let foreign = pool();
    let wrong = SourceReply::Data(
        InputWindow::new(
            ID,
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&foreign, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(
        p.provide(&wrong),
        Err(Error::Core(C::ForeignPool))
    ));
    let wrong = SourceReply::Data(
        InputWindow::new(
            SourceId(900),
            r.token.clone(),
            r.offset,
            Bytes::copy_from_slice(&b, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(
        p.provide(&wrong),
        Err(Error::Core(C::SourceMismatch))
    ));
    assert!(p.pending_request().is_some());
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}
#[test]
fn eof_mp4_seek_replays_all_source_preroll_without_editing_packet_time() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 1, 1).unwrap();
    let old = collect(&mut p, 1).unwrap();
    let first = &old[0];
    assert!(p.current(first));
    assert!(
        p.seek_and_select(
            TrackKey::Mp4(1),
            Time::new(3, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    assert!(!p.current(first));
    let replay = collect(&mut p, 1).unwrap();
    assert_eq!(mp4_packets(&old).len(), mp4_packets(&replay).len());
    for o in replay.iter().filter(|o| o.event().track().is_some()) {
        let pts = o.event().source_pts().unwrap().unwrap();
        if pts < Time::new(3, 1).unwrap() {
            assert_eq!(
                o.delivery(),
                Delivery::DecodeOnly {
                    until: Time::new(3, 1).unwrap()
                }
            );
        } else {
            assert_eq!(o.delivery(), Delivery::Present);
        }
    }
}
#[test]
fn source_ts_seek_replay_has_new_generation_and_keeps_complete_psi_before_pes() {
    let (mut p, _, _, _) = observed_ts();
    let old = first_packet(&mut p);
    assert!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        )
        .unwrap()
    );
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    let map = out
        .iter()
        .position(|o| {
            matches!(
                o.event(),
                RawEvent::Ts(TsEvent {
                    source: TraceEvent::ProgramMap { active: true, .. },
                    ..
                })
            )
        })
        .unwrap();
    let packet = out
        .iter()
        .position(|o| o.event().track().is_some())
        .unwrap();
    assert!(map < packet);
    assert_eq!(pes(&out).len(), 1);
    assert!(out.iter().all(|o| p.current(o)));
}
#[test]
fn paused_seek_changes_scope_now_and_waits_for_resume_before_any_io() {
    let (mut p, _, calls, _) = observed_ts();
    let old = first_packet(&mut p);
    p.pause(true).unwrap();
    p.seek_and_select(
        TrackKey::Ts {
            program: 1,
            pid: 256,
        },
        Time::new(1, 1).unwrap(),
        Selection::All,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    let before = calls.lock().unwrap().len();
    assert!(!p.current(&old));
    assert!(matches!(
        p.advance(&mut WorkBudget::new(100), &off()).unwrap(),
        Progress::Paused
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
    p.pause(false).unwrap();
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}
#[test]
fn track_switch_keeps_all_mp4_source_packets_and_marks_unselected() {
    let raw = include_bytes!("fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 1, 1).unwrap();
    let old = first_packet(&mut p);
    let selection = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: None,
    };
    p.seek_and_select(
        TrackKey::Mp4(1),
        Time::new(0, 1).unwrap(),
        selection,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(mp4_packets(&out).len(), 12);
    for o in out.iter().filter(|o| o.event().track().is_some()) {
        assert_eq!(
            o.delivery() == Delivery::Unselected,
            o.event().track() == Some(TrackKey::Mp4(2))
        );
    }
}
#[test]
fn invalid_track_switch_and_work_zero_leave_current_owners_and_transport_unchanged() {
    let (mut p, _, calls, _) = observed_ts();
    let old = first_packet(&mut p);
    let before = calls.lock().unwrap().len();
    let bad = Selection::Tracks {
        video: Some(TrackKey::Ts {
            program: 1,
            pid: 999,
        }),
        audio: None,
        subtitle: None,
    };
    assert!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            bad,
            &mut WorkBudget::new(1),
            &off()
        )
        .is_err()
    );
    assert!(p.current(&old));
    assert!(
        !p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(1, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(0),
            &off()
        )
        .unwrap()
    );
    assert_eq!(calls.lock().unwrap().len(), before);
    assert!(p.current(&old));
}
#[test]
fn late_preseek_range_reply_is_rejected_before_current_source_advance() {
    let (mut p, b, _, _) = observed_ts();
    first_packet(&mut p);
    p.seek_and_select(
        TrackKey::Ts {
            program: 1,
            pid: 256,
        },
        Time::new(1, 1).unwrap(),
        Selection::All,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    let old = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while p.pop().is_some() {}
        if let Some(r) = p.pending_request() {
            break r.clone();
        }
    };
    p.seek_and_select(
        TrackKey::Ts {
            program: 1,
            pid: 256,
        },
        Time::new(1, 1).unwrap(),
        Selection::All,
        &mut WorkBudget::new(1),
        &off(),
    )
    .unwrap();
    for _ in 0..20 {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while p.pop().is_some() {}
        if p.pending_request().is_some() {
            break;
        }
    }
    let bad = SourceReply::Data(
        InputWindow::new(
            ID,
            old.token,
            old.offset,
            Bytes::copy_from_slice(&b, &[0x47]).unwrap(),
        )
        .unwrap(),
    );
    assert!(matches!(
        p.provide(&bad),
        Err(Error::Core(C::StaleGeneration { .. }))
    ));
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}
#[test]
fn retained_real_mp4_packet_config_and_native_view_outlive_player() {
    let (mut p, b) = mp4(
        include_bytes!("fixtures/mp4/r-front-stco.mov"),
        Mp4View::PinnedMov9NoParseNoFillin,
        7,
        1,
    )
    .unwrap();
    let output = first_packet(&mut p);
    p.stop();
    assert!(!p.current(&output));
    drop(p);
    let RawEvent::Mp4(Mp4Event::Packet(packet)) = output.event() else {
        panic!()
    };
    assert!(!packet.source.payload.is_empty());
    assert!(!packet.configuration.description.raw().bytes().is_empty());
    assert!(packet.native.is_some());
    assert!(b.used() > Budget::control_storage_bytes());
    drop(output);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn retained_ts_packet_spans_and_header_outlive_stop_without_leaks() {
    let (mut p, b, _, _) = observed_ts();
    let output = first_packet(&mut p);
    p.stop();
    drop(p);
    let RawEvent::Ts(TsEvent {
        source: TraceEvent::Pes(pes),
        ..
    }) = output.event()
    else {
        panic!()
    };
    assert_eq!(pes.header.pts_raw33, Some(90_000));
    assert_eq!(
        pes.payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\x00\x47\xff"
    );
    drop(output);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn cancellation_while_paused_pending_is_quiescent_before_close_once() {
    let (mut p, b, calls, _) = observed_ts();
    p.pause(true).unwrap();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(C::Cancelled))
    ));
    p.cancel();
    p.stop();
    drop(p);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[Call::Abort, Call::Close]
    );
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn actual_queue_pressure_stops_read_and_release_resumes_whole_operation() {
    let raw = include_bytes!("fixtures/ts/ts188-basic.ts");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut p = Player::new(
        Engine::ts(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
        )
        .unwrap(),
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy {
            slots: 1,
            ..PlayerPolicy::default()
        },
    )
    .unwrap();
    let mut pressure = false;
    for _ in 0..10000 {
        if matches!(
            p.advance(&mut WorkBudget::new(32), &off()).unwrap(),
            Progress::Backpressure { .. }
        ) {
            pressure = true;
            break;
        }
    }
    assert!(pressure, "a one-slot queue must stop real reader progress");
    let before = calls.lock().unwrap().len();
    assert!(matches!(
        p.advance(&mut WorkBudget::new(32), &off()).unwrap(),
        Progress::Backpressure { .. }
    ));
    assert_eq!(calls.lock().unwrap().len(), before);
    let mut out = Vec::new();
    while let Some(o) = p.pop() {
        out.push(o);
    }
    out.extend(collect(&mut p, 1).unwrap());
    assert_eq!(pes(&out).len(), 1);
}
#[test]
fn real_mp4_avc_packets_flow_through_existing_stateful_bsf_with_all_raw_metadata() {
    use triergon_framing::{AvcConfigLimits, BuildStep, NalPacketLimits};
    let raw = include_bytes!("fixtures/mp4/encoded-avc-aac-front.mp4");
    let (mut p, b) = mp4(raw, Mp4View::SourcePhysical, 7, 17).unwrap();
    let out = collect(&mut p, 17).unwrap();
    let packets: Vec<_> = mp4_packets(&out)
        .into_iter()
        .filter(|p| p.configuration.description.format == *b"avc1")
        .cloned()
        .collect();
    assert!(!packets.is_empty());
    let mut setup = nal_bridge::AvcBridgeSetup::new(
        &b,
        ID,
        packets[0].configuration.clone(),
        AvcConfigLimits {
            maximum_record_bytes: 1 << 20,
            maximum_nals: 256,
            maximum_nal_bytes: 1 << 20,
        },
        NalPacketLimits {
            maximum_packet_bytes: 1 << 20,
            maximum_output_bytes: 2 << 20,
            maximum_nals: 1024,
            maximum_segments: 2048,
        },
    )
    .unwrap();
    let mut bridge = loop {
        if let Some(bridge) = setup.poll(&mut WorkBudget::new(1), &off()).unwrap() {
            break bridge;
        }
    };
    for packet in packets {
        let source = packet.source.payload.clone();
        let clock = packet
            .source
            .sample
            .source_times(packet.configuration.track.base.require().unwrap())
            .unwrap();
        let generation = packet.source.token.generation();
        let mut job = bridge.packet(packet).unwrap();
        let result = loop {
            match job.step(&mut WorkBudget::new(1), &off()).unwrap() {
                BuildStep::Ready(o) => break o,
                BuildStep::Yield => {}
                BuildStep::Complete => panic!(),
            }
        };
        assert_eq!(result.source().bytes.as_ref(), source.as_ref());
        assert_eq!(result.source().generation, generation);
        let original = &result.source().fields;
        assert_eq!(
            original
                .source
                .sample
                .source_times(original.configuration.track.base.require().unwrap())
                .unwrap(),
            clock
        );
        assert!(
            result.bytes().starts_with(&[0, 0, 0, 1]) || result.bytes().starts_with(&[0, 0, 1])
        );
    }
}
#[test]
fn cancelled_opener_and_probe_cannot_later_publish_ready() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, _, _) = observed(&b, raw, 7, false);
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let mut job = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    assert!(matches!(
        job.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Core(C::Cancelled))
    ));
    assert!(matches!(
        job.advance(&mut WorkBudget::new(100), &off()),
        Err(Error::Closed)
    ));
    assert!(job.finish().is_err());
    assert_eq!(b.used(), Budget::control_storage_bytes());
    let (source, _, _) = observed(&b, raw, 7, false);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
        source,
    )
    .unwrap();
    assert!(
        probe
            .advance(&mut WorkBudget::new(0), &AtomicBool::new(true))
            .is_err()
    );
    assert!(matches!(
        probe.advance(&mut WorkBudget::new(100), &off()),
        Err(Error::Closed)
    ));
    assert!(probe.finish().is_err());
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn mp4_plan_cannot_attach_to_another_entity_or_generation() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let (opened, b) = open(raw, 7, 1).unwrap();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !job.poll(&mut WorkBudget::new(11), &off()).unwrap() {}
    let plan = job.finish().unwrap();
    let other = InputCursor::new(
        &b,
        SourceId(999),
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::mp4(other, plan, 1 << 20),
        Err(Error::Core(C::SourceMismatch))
    ));
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !job.poll(&mut WorkBudget::new(11), &off()).unwrap() {}
    let plan = job.finish().unwrap();
    let other = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::mp4(other, plan, 1 << 20),
        Err(Error::Core(C::StaleGeneration { .. }))
    ));
}
#[test]
fn actual_bounded_source_index_refuses_resource_exhaustion_and_releases_owners() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let (opened, b) = open(raw, 7, 1).unwrap();
    let before = b.used();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits {
            samples: 2,
            ..PlanLimits::default()
        },
    )
    .unwrap();
    let mut refused = false;
    for _ in 0..10_000 {
        match job.poll(&mut WorkBudget::new(1), &off()) {
            Err(Error::Core(C::Limit { .. })) => {
                refused = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong resource refusal {e:?}"),
        }
    }
    assert!(refused);
    drop(job);
    assert_eq!(b.used(), before);
    drop(opened);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn finite_prefix_ambiguity_is_rejected_from_bytes_not_corpus_identity() {
    let raw = [0x47; 580];
    let b = pool();
    let (source, _, _) = observed(&b, &raw, 4096, false);
    let mut probe = Probe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(580),
            random_access: true,
        },
        4096,
        source,
    )
    .unwrap();
    let mut refused = false;
    for _ in 0..5000 {
        match probe.advance(&mut WorkBudget::new(11), &off()) {
            Err(Error::Core(C::Invalid("ambiguous transport signature"))) => {
                refused = true;
                break;
            }
            Ok(_) => {}
            Err(e) => panic!("wrong ambiguity {e:?}"),
        }
    }
    assert!(refused);
    assert!(probe.finish().is_err());
}
#[test]
fn full_native_ts_packets_are_distinct_from_source_pes_and_keep_literal_fields() {
    use triergon_mpegts::native_pes::{
        NativePesBoundary, NativePesPayloadKind, NativeRegisteredCarriage as Carriage,
    };
    for (raw, pid, carriage, stream_id) in [
        (
            include_bytes!("fixtures/ts/ts188-basic.ts").as_slice(),
            256,
            Carriage::Avc,
            0xe0,
        ),
        (
            include_bytes!("fixtures/ts/carriage-hevc-direct.ts").as_slice(),
            257,
            Carriage::Hevc,
            0xe0,
        ),
        (
            include_bytes!("fixtures/ts/carriage-aac-adts-direct.ts").as_slice(),
            257,
            Carriage::AacAdts,
            0xc0,
        ),
    ] {
        let b = pool();
        let input = InputCursor::new(
            &b,
            ID,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            64,
        )
        .unwrap();
        let engine = Engine::ts_native(
            input,
            PacketFormat::Ts188,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::Unanchored,
            TsNativeSelection { pid, carriage },
        )
        .unwrap();
        let mut p = Player::new(
            engine,
            SourceOwner::new(
                ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 1).unwrap(),
            ),
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        p.set_clock_view(ClockChoice::ExplicitNativeProjection)
            .unwrap();
        let out = collect(&mut p, 1).unwrap();
        assert_eq!(pes(&out).len(), 1);
        assert!(
            out.iter()
                .filter(|o| matches!(
                    o.event(),
                    RawEvent::Ts(TsEvent {
                        source: TraceEvent::Pes(_),
                        ..
                    })
                ))
                .all(|o| o.delivery() == Delivery::Source)
        );
        let native: Vec<_> = out
            .iter()
            .filter_map(|o| {
                if let RawEvent::NativeTs { packet, clock } = o.event() {
                    assert_eq!(o.delivery(), Delivery::Present);
                    Some((packet, clock))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(native.len(), 1);
        let (packet, clock) = native[0];
        assert_eq!(packet.pid, pid);
        assert_eq!(packet.native_position, 376);
        assert_eq!(packet.native_flags, 0);
        assert_eq!(packet.native_duration90_raw, 0);
        assert_eq!(packet.native_stream_id, stream_id);
        assert_eq!(packet.boundary, NativePesBoundary::FiniteLength);
        assert_eq!(packet.payload_kind, NativePesPayloadKind::DeclaredComplete);
        assert_eq!(
            packet.payload().flatten().copied().collect::<Vec<_>>(),
            b"lane-ts-original-payload\x00\x47\xff"
        );
        assert_eq!(clock.pts90, Some(90_000));
        assert_eq!(clock.dts90, Some(90_000));
        assert_eq!(packet.source_header.dts_raw33, None);
        assert_eq!(packet.header_source_ranges.as_slice()[0].offset, 523);
        assert_eq!(packet.spans.as_slice()[0].source.offset, 537);
        assert_eq!(packet.spans.as_slice()[0].context.source_event_ordinal, 8);
        drop(out);
        drop(p);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn native_carriage_refusal_keeps_source_and_is_not_a_native_codec_error() {
    use triergon_mpegts::native_pes::NativeRegisteredCarriage;
    let raw = include_bytes!("fixtures/ts/ts188-basic.ts");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        64,
    )
    .unwrap();
    let engine = Engine::ts_native(
        input,
        PacketFormat::Ts188,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        TsNativeSelection {
            pid: 256,
            carriage: NativeRegisteredCarriage::Hevc,
        },
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 1).unwrap();
    assert_eq!(pes(&out).len(), 1);
    assert!(out.iter().any(|o| matches!(
        o.event(),
        RawEvent::NativeRefusal {
            error: C::Unsupported(_),
            ..
        }
    )));
    assert!(
        out.iter()
            .all(|o| !matches!(o.event(), RawEvent::NativeTs { .. }))
    );
}
#[test]
fn full_native_m2ts_position_is_refused_without_inventing_startup_evidence() {
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(576),
            random_access: true,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::ts_native(
            input,
            PacketFormat::M2ts192,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::Unanchored,
            TsNativeSelection {
                pid: 256,
                carriage: triergon_mpegts::native_pes::NativeRegisteredCarriage::Avc
            }
        ),
        Err(Error::Core(C::Unsupported(_)))
    ));
    let (mut p, _) = ts(
        include_bytes!("fixtures/ts/m2ts192-opaque-prefix.m2ts"),
        PacketFormat::M2ts192,
        7,
        SourceTimePolicy::Unanchored,
    )
    .unwrap();
    assert_eq!(pes(&collect(&mut p, 1).unwrap()).len(), 1);
}
#[test]
fn real_encoded_matroska_uses_old183_reader_after_sequential_probe() {
    let raw = include_bytes!("fixtures/mkv/observed-11.mkv");
    let (selected, b, _) = probe(raw, 7, true);
    assert_eq!(selected.format, Format::Matroska);
    let engine = Engine::matroska(
        &b,
        ID,
        selected.input.capabilities(),
        triergon::Limits::default(),
        4096,
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        selected.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let out = collect(&mut p, 17).unwrap();
    let packets: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let RawEvent::Matroska(triergon::typed::Event::Packet(p)) = o.event() {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    assert!(!packets.is_empty());
    assert!(packets.iter().any(|p| {
        p.configuration
            .tracks()
            .iter()
            .any(|t| t.codec.as_ref() == "V_MPEG4/ISO/AVC")
    }));
    assert!(packets.iter().all(|p| !p.packet.data.is_empty()));
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}
#[test]
fn real_movie_edit_seek_maps_exact_source_point_before_playback_fence() {
    let raw = include_bytes!("fixtures/mp4/r-edits-repeat.mov");
    let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 7, 1).unwrap();
    let old = first_packet(&mut p);
    let Some(Engine::Mp4(engine)) = p.engine() else {
        panic!()
    };
    let mut job = engine
        .plan
        .movie_seek(1, Time::new(1, 2).unwrap(), 1024)
        .unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    let triergon_isobmff::MoviePoint::Media(point) = command.point() else {
        panic!("frozen edit fixture must contain this movie point")
    };
    assert_eq!(point.requested_movie_time, Time::new(1, 2).unwrap());
    assert!(command.raw_edits().is_some());
    assert!(command.raw_media_clock().payload().len() >= 20);
    let target = point.source_time;
    assert!(matches!(
        p.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(0), &off())
            .unwrap(),
        mp4_plan::MovieSeekOutcome::Yield
    ));
    assert!(p.current(&old));
    assert!(matches!(
        p.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(1), &off())
            .unwrap(),
        mp4_plan::MovieSeekOutcome::Applied(_)
    ));
    assert!(!p.current(&old));
    let out = collect(&mut p, 1).unwrap();
    assert!(!mp4_packets(&out).is_empty());
    for o in out.iter().filter(|o| o.event().track().is_some()) {
        if o.event().source_pts().unwrap().is_some_and(|t| t < target) {
            assert_eq!(o.delivery(), Delivery::DecodeOnly { until: target });
        }
    }
}
#[test]
fn movie_seek_command_cannot_cross_source_owner_pools() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let (a, _) = mp4(raw, Mp4View::SourcePhysical, 7, 1).unwrap();
    let (mut b, _) = mp4(raw, Mp4View::SourcePhysical, 7, 1).unwrap();
    let Some(Engine::Mp4(engine)) = a.engine() else {
        panic!()
    };
    let mut job = engine
        .plan
        .movie_seek(1, Time::new(0, 1).unwrap(), 16)
        .unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    assert!(matches!(
        b.apply_movie_seek(&command, Selection::All, &mut WorkBudget::new(1), &off()),
        Err(Error::Core(C::ForeignPool))
    ));
    assert_eq!(b.state(), State::Running);
}
#[test]
fn front_moov_mp4_streams_real_samples_without_source_seek_or_preloading_mdat() {
    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    for q in [1, 7, 4096] {
        let (selected, b, calls) = probe(raw, q, true);
        let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
        for _ in 0..100_000 {
            if matches!(
                open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
                OpenProgress::Ready
            ) {
                break;
            }
        }
        let opened = open.finish().unwrap();
        assert!(matches!(
            opened.movie.read_scope(),
            mp4_open::MetadataReadScope::FrontMetadataBeforeMdat(_)
        ));
        let at = opened.input.position();
        assert!(at < raw.len() as u64);
        let mut index = Mp4IndexJob::new(
            &opened.movie,
            Mp4View::SourcePhysical,
            PlanLimits::default(),
        )
        .unwrap();
        while !index.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
        let plan = index.finish().unwrap();
        let mut player = Player::new(
            Engine::mp4(opened.input, plan, 1 << 20).unwrap(),
            opened.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        drop(opened.movie);
        let out = collect(&mut player, 1).unwrap();
        assert_eq!(mp4_packets(&out).len(), 6);
        for p in mp4_packets(&out) {
            assert_eq!(
                p.source.payload.as_ref(),
                &raw[p.source.sample.range.start as usize..p.source.sample.range.end as usize]
            );
        }
        let offsets: Vec<_> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                if let Call::Poll(at) = c {
                    Some(*at)
                } else {
                    None
                }
            })
            .collect();
        assert!(offsets.windows(2).all(|p| p[0] < p[1]));
        drop(out);
        drop(player);
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}
#[test]
fn real_ts_entity_transition_preserves_partial_pes_and_requires_explicit_logical_eof() {
    let a = include_bytes!("fixtures/ts/hls-contiguous-part-a.ts");
    let bbytes = include_bytes!("fixtures/ts/hls-contiguous-part-b.ts");
    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(a.len() as u64),
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = Engine::ts_segmented(
        input,
        PacketFormat::Ts188,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
    )
    .unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(ByteSource::new(ID, Bytes::copy_from_slice(&b, a).unwrap(), 7).unwrap()),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let mut out = Vec::new();
    let mut boundaries = 0;
    for _ in 0..200_000 {
        let step = player.advance(&mut WorkBudget::new(1), &off()).unwrap();
        while let Some(o) = player.pop() {
            out.push(o);
        }
        if matches!(step, Progress::AwaitEntity) {
            boundaries += 1;
            if boundaries == 1 {
                assert!(pes(&out).is_empty());
                let id = SourceId(ID.0 + 1);
                let mut next = Some(
                    ByteSource::new(id, Bytes::copy_from_slice(&b, bbytes).unwrap(), 7).unwrap(),
                );
                assert!(
                    !player
                        .continue_entity(
                            id,
                            SourceCapabilities {
                                length: Some(bbytes.len() as u64),
                                random_access: false
                            },
                            &mut next,
                            false,
                            &mut WorkBudget::new(0),
                            &off()
                        )
                        .unwrap()
                );
                assert!(next.is_some());
                assert!(
                    player
                        .continue_entity(
                            id,
                            SourceCapabilities {
                                length: Some(bbytes.len() as u64),
                                random_access: false
                            },
                            &mut next,
                            false,
                            &mut WorkBudget::new(1),
                            &off()
                        )
                        .unwrap()
                );
                assert!(next.is_none());
            } else {
                assert!(
                    player
                        .finish_logical_source(&mut WorkBudget::new(1), &off())
                        .unwrap()
                );
            }
        }
        if matches!(step, Progress::Ended) {
            break;
        }
    }
    assert_eq!(boundaries, 2);
    let packets = pes(&out);
    assert_eq!(packets.len(), 1);
    let TraceEvent::Pes(p) = &packets[0].source else {
        panic!()
    };
    assert_eq!(
        p.payload().flatten().copied().collect::<Vec<_>>(),
        (0_u8..=255).cycle().take(512).collect::<Vec<_>>()
    );
    assert!(p.source_ranges().any(|r| r.entity == ID));
    assert!(p.source_ranges().any(|r| r.entity == SourceId(ID.0 + 1)));
    assert_eq!(
        out.iter()
            .filter(|o| matches!(o.event(), RawEvent::EntityEnd(_)))
            .count(),
        2
    );
    drop(out);
    drop(player);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn native_ts_projection_only_replaces_source_pes_for_its_selected_pid() {
    use triergon_mpegts::native_pes::NativeRegisteredCarriage;

    let video = include_bytes!("fixtures/ts/ts188-basic.ts");
    let audio = include_bytes!("fixtures/ts/carriage-aac-adts-direct.ts");
    let mut raw = video[..188].to_vec();
    // Build one active PMT from the two authored one-stream controls. Their
    // elementary entries keep the original types, PIDs and descriptor bytes.
    let mut section = [
        0x02, 0xb0, 0x17, 0, 1, 0xc1, 0, 0, 0xe1, 0, 0xf0, 0, 0x1b, 0xe1, 0, 0xf0, 0, 0x0f, 0xe1,
        1, 0xf0, 0, 0, 0, 0, 0,
    ];
    let mut crc = u32::MAX;
    for &byte in &section[..22] {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc >> 31 == 1 { 0x04c1_1db7 } else { 0 };
        }
    }
    section[22..].copy_from_slice(&crc.to_be_bytes());
    let mut pmt = [0xff; 188];
    pmt[..5].copy_from_slice(&[0x47, 0x50, 0x00, 0x10, 0]);
    pmt[5..31].copy_from_slice(&section);
    raw.extend_from_slice(&pmt);
    raw.extend_from_slice(&video[376..]);
    raw.extend_from_slice(&audio[376..]);

    let b = pool();
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = Engine::ts_native(
        input,
        PacketFormat::Ts188,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        TsNativeSelection {
            pid: 256,
            carriage: NativeRegisteredCarriage::Avc,
        },
    )
    .unwrap();
    let mut p = Player::new(
        engine,
        SourceOwner::new(
            ByteSource::new(ID, Bytes::copy_from_slice(&b, &raw).unwrap(), 7).unwrap(),
        ),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    p.set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let out = collect(&mut p, 1).unwrap();
    let source_packets: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let RawEvent::Ts(TsEvent {
                source: TraceEvent::Pes(pes),
                ..
            }) = o.event()
            {
                Some((pes.pid, o.delivery()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        source_packets,
        [(256, Delivery::Source), (257, Delivery::Present)]
    );
    assert_eq!(
        out.iter()
            .filter(|o| matches!(o.event(), RawEvent::NativeTs { .. }))
            .count(),
        1
    );
}

fn assert_source_pes_match(left: &[Output], right: &[Output]) {
    let left = pes(left);
    let right = pes(right);
    assert_eq!(left.len(), right.len());
    for (a, b) in left.into_iter().zip(right) {
        let (TraceEvent::Pes(x), TraceEvent::Pes(y)) = (&a.source, &b.source) else {
            panic!("source PES expected");
        };
        assert_eq!(a.clock_key, b.clock_key);
        assert_eq!(a.source_pts, b.source_pts);
        assert_eq!(a.source_dts, b.source_dts);
        assert_eq!(x.pid, y.pid);
        assert_eq!(x.epochs, y.epochs);
        assert_eq!(x.byte_run_epoch, y.byte_run_epoch);
        assert_eq!(x.elementary_offset, y.elementary_offset);
        assert_eq!(x.header.as_bytes(), y.header.as_bytes());
        assert_eq!(
            x.header_source_ranges.as_slice(),
            y.header_source_ranges.as_slice()
        );
        assert!(x.source_ranges().eq(y.source_ranges()));
        assert!(x.payload().flatten().eq(y.payload().flatten()));
    }
}

fn private_audio_player(
    raw: &[u8],
    carriage: triergon_mpegts::native_pes::NativePrivateAudioCarriage,
    selection: Selection,
    quantum: usize,
) -> (Player<ByteSource>, Budget) {
    let budget = pool();
    let input = InputCursor::new(
        &budget,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        64,
    )
    .unwrap();
    let engine = Engine::ts_native_private_audio_v1(
        input,
        PacketFormat::Ts188,
        1,
        triergon_mpegts::ParseLimits::default(),
        SourceTimePolicy::RelativeEpochZero,
        TsNativePrivateAudioSelection { pid: 257, carriage },
    )
    .unwrap();
    let source = SourceOwner::new(
        ByteSource::new(ID, Bytes::copy_from_slice(&budget, raw).unwrap(), quantum).unwrap(),
    );
    let player = Player::new(engine, source, selection, PlayerPolicy::default()).unwrap();
    (player, budget)
}

#[test]
fn private_audio_playback_matches_all_four_archived_packet_controls() {
    use triergon_mpegts::native_pes::{
        NativePesBoundary, NativePesPayloadKind, NativePrivateAudioCarriage as Carriage,
    };

    for (raw, carriage) in [
        (
            include_bytes!("../../mpegts/tests/data/carriage-ac3-dvb.ts"),
            Carriage::DvbAc3Tag,
        ),
        (
            include_bytes!("../../mpegts/tests/data/carriage-eac3-dvb.ts"),
            Carriage::DvbEac3Tag,
        ),
        (
            include_bytes!("../../mpegts/tests/data/carriage-ac3-direct.ts"),
            Carriage::PrivateType81Ac3,
        ),
        (
            include_bytes!("../../mpegts/tests/data/carriage-eac3-direct.ts"),
            Carriage::PrivateType87Eac3,
        ),
    ] {
        for quantum in [1, 64] {
            let (mut p, budget) = private_audio_player(
                raw,
                carriage,
                Selection::Tracks {
                    video: None,
                    audio: Some(TrackKey::Ts {
                        program: 1,
                        pid: 257,
                    }),
                    subtitle: None,
                },
                quantum,
            );
            p.set_clock_view(ClockChoice::ExplicitNativeProjection)
                .unwrap();
            let out = collect(&mut p, 1).unwrap();
            let (mut strict, _) = ts(
                raw,
                PacketFormat::Ts188,
                quantum,
                SourceTimePolicy::RelativeEpochZero,
            )
            .unwrap();
            let strict_out = collect(&mut strict, 1).unwrap();
            assert_source_pes_match(&out, &strict_out);
            assert!(
                out.iter()
                    .all(|o| !matches!(o.event(), RawEvent::NativeRefusal { .. }))
            );
            let mut native = out.iter().filter_map(|o| {
                if let RawEvent::NativeTs { packet, clock } = o.event() {
                    assert_eq!(o.delivery(), Delivery::Present);
                    Some((packet, clock))
                } else {
                    None
                }
            });
            let (packet, clock) = native.next().expect("admitted private audio");
            assert!(native.next().is_none());
            assert_eq!((packet.program, packet.pid), (1, 257));
            assert_eq!(packet.native_stream_id, 0xbd);
            assert_eq!(
                (
                    packet.native_position,
                    packet.native_duration90_raw,
                    packet.native_flags
                ),
                (376, 0, 0)
            );
            assert_eq!(packet.boundary, NativePesBoundary::FiniteLength);
            assert_eq!(packet.payload_kind, NativePesPayloadKind::DeclaredComplete);
            assert_eq!(packet.source_header.as_bytes(), &raw[523..537]);
            assert_eq!(packet.source_header.pts_raw33, Some(90_000));
            assert_eq!(packet.source_header.dts_raw33, None);
            assert_eq!((clock.pts90, clock.dts90), (Some(90_000), Some(90_000)));
            assert_eq!(packet.spans.as_slice()[0].source.offset, 537);
            assert_eq!(packet.spans.as_slice()[0].context.source_event_ordinal, 8);
            drop(p);
            assert!(
                packet
                    .payload()
                    .flatten()
                    .copied()
                    .eq(raw[537..].iter().copied())
            );
            drop(out);
            assert_eq!(budget.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn private_audio_mismatch_reports_refusal_and_retains_source_packet() {
    use triergon_mpegts::native_pes::NativePrivateAudioCarriage as Carriage;
    let raw = include_bytes!("../../mpegts/tests/data/carriage-ac3-dvb.ts");
    let (mut p, _) = private_audio_player(raw, Carriage::DvbEac3Tag, Selection::All, 7);
    let out = collect(&mut p, 1).unwrap();
    let (mut strict, _) = ts(
        raw,
        PacketFormat::Ts188,
        7,
        SourceTimePolicy::RelativeEpochZero,
    )
    .unwrap();
    assert_source_pes_match(&out, &collect(&mut strict, 1).unwrap());
    assert!(
        out.iter()
            .all(|o| !matches!(o.event(), RawEvent::NativeTs { .. }))
    );
    assert_eq!(
        out.iter()
            .filter(|o| matches!(
                o.event(),
                RawEvent::NativeRefusal {
                    track: TrackKey::Ts {
                        program: 1,
                        pid: 257
                    },
                    error: C::Invalid("native private audio carriage mismatch"),
                    ..
                },
            ))
            .count(),
        1
    );
}

#[test]
fn private_audio_does_not_admit_m2ts_by_construction() {
    use triergon_mpegts::native_pes::NativePrivateAudioCarriage as Carriage;
    let budget = pool();
    let input = InputCursor::new(
        &budget,
        ID,
        SourceCapabilities {
            length: None,
            random_access: false,
        },
        64,
    )
    .unwrap();
    assert!(matches!(
        Engine::ts_native_private_audio_v1(
            input,
            PacketFormat::M2ts192,
            1,
            triergon_mpegts::ParseLimits::default(),
            SourceTimePolicy::RelativeEpochZero,
            TsNativePrivateAudioSelection {
                pid: 257,
                carriage: Carriage::DvbAc3Tag
            },
        ),
        Err(Error::Core(C::Unsupported(_)))
    ));
}

#[test]
fn native_pressure_retries_instead_of_disabling_the_observer() {
    use triergon_mpegts::native_pes::NativePrivateAudioCarriage as Carriage;
    let raw = include_bytes!("../../mpegts/tests/data/carriage-ac3-dvb.ts");
    let (mut player, budget) = private_audio_player(raw, Carriage::DvbAc3Tag, Selection::All, 7);
    for _ in 0..4096 {
        player.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(event) = player.pop()
            && matches!(
                event.event(),
                RawEvent::Ts(TsEvent { source: TraceEvent::Transport(packet), .. })
                    if packet.pid == 257
            )
        {
            let pressure = budget.reserve(budget.available()).unwrap();
            let mut blocked = false;
            for _ in 0..512 {
                match player.advance(&mut WorkBudget::new(1), &off()).unwrap() {
                    Progress::Backpressure { .. } => {
                        blocked = true;
                        break;
                    }
                    Progress::Queued => panic!("native observer escaped allocation pressure"),
                    _ => {}
                }
            }
            assert!(blocked, "native observer pressure must be retryable");
            drop(pressure);
            let out = collect(&mut player, 1).unwrap();
            assert!(
                out.iter()
                    .any(|o| matches!(o.event(), RawEvent::NativeTs { packet, .. }
                    if packet.payload().flatten().copied().eq(raw[537..].iter().copied())))
            );
            assert!(
                out.iter()
                    .all(|o| !matches!(o.event(), RawEvent::NativeRefusal { .. }))
            );
            return;
        }
    }
    panic!("missing selected carrier");
}

#[test]
fn av1_bridge_matches_delimiter_framing_from_real_mp4_and_webm_packets() {
    use triergon_framing::{Av1ConfigLimits, Av1ObuLimits, Av1TemporalUnitLimits, BuildStep};
    use triergon_playback::av1_bridge::{Av1BridgeSetup, Av1Input};

    let mp4_raw = include_bytes!("fixtures/av1/six-frames.mp4");
    let webm_raw = include_bytes!("fixtures/av1/six-frames.webm");
    let expected = include_bytes!("fixtures/av1/six-frames.obu");
    let obu_limits = Av1ObuLimits {
        maximum_obu_bytes: 16_384,
        maximum_obus: 64,
        maximum_input_window: 128,
    };
    let config_limits = Av1ConfigLimits {
        maximum_record_bytes: 16_384,
        obu: obu_limits,
    };
    let packet_limits = Av1TemporalUnitLimits {
        maximum_sample_bytes: 16_384,
        maximum_output_bytes: 32_768,
        maximum_obus: 64,
    };

    for quantum in [7, 4096] {
        let (mut mp4, mp4_pool) = mp4(mp4_raw, Mp4View::SourcePhysical, quantum, 3).unwrap();
        let (selected, mkv_pool, _) = probe(webm_raw, quantum, false);
        let engine = Engine::matroska(
            &mkv_pool,
            ID,
            selected.input.capabilities(),
            triergon::Limits::default(),
            4096,
        )
        .unwrap();
        let mut mkv = Player::new(
            engine,
            selected.source,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
        let mp4_packets = collect(&mut mp4, 7).unwrap();
        let mkv_packets = collect(&mut mkv, 7).unwrap();

        for (outputs, pool) in [(&mp4_packets, &mp4_pool), (&mkv_packets, &mkv_pool)] {
            let mut bridge = None;
            let mut bytes = Vec::new();
            let mut packets = 0;
            for output in outputs {
                let (packet, payload) = match output.event() {
                    RawEvent::Mp4(Mp4Event::Packet(packet)) => {
                        (Av1Input::Mp4(packet), packet.source.payload.as_ref())
                    }
                    RawEvent::Matroska(triergon::typed::Event::Packet(packet)) => {
                        (Av1Input::Matroska(packet), packet.packet.data.as_ref())
                    }
                    _ => continue,
                };
                if bridge.is_none() {
                    let mut setup =
                        Av1BridgeSetup::new(pool, ID, packet, config_limits, packet_limits)
                            .unwrap();
                    for _ in 0..4096 {
                        match setup.step(&mut WorkBudget::new(1), &off()).unwrap() {
                            BuildStep::Yield => {}
                            BuildStep::Ready(bound) => {
                                bridge = Some(bound);
                                assert!(matches!(
                                    setup.step(&mut WorkBudget::new(1), &off()).unwrap(),
                                    BuildStep::Complete
                                ));
                                break;
                            }
                            BuildStep::Complete => panic!("bridge setup lost its configuration"),
                        }
                    }
                }
                let bound = bridge.as_ref().expect("bridge setup made bounded progress");
                if let Av1Input::Mp4(packet) = packet {
                    let mut stale = packet.clone();
                    stale.configuration.revision += 1;
                    assert!(matches!(
                        bound.packet(Av1Input::Mp4(&stale)),
                        Err(Error::Core(C::Invalid(_)))
                    ));
                }
                let mut job = bound.packet(packet).unwrap();
                for _ in 0..128 {
                    match job.step(&mut WorkBudget::new(1), &off()).unwrap() {
                        BuildStep::Yield => {}
                        BuildStep::Ready(unit) => {
                            assert!(std::ptr::eq(unit.source(), payload));
                            let before = bytes.len();
                            for span in unit.segments() {
                                bytes.extend_from_slice(span);
                            }
                            assert_eq!(bytes.len() - before, unit.byte_len());
                            packets += 1;
                            break;
                        }
                        BuildStep::Complete => panic!("unit was not delivered"),
                    }
                }
            }
            assert_eq!(packets, 6);
            assert_eq!(bytes.as_slice(), expected);

            if std::ptr::eq(pool, &mkv_pool) {
                let track = outputs.iter().find_map(|o| o.event().track()).unwrap();
                assert!(
                    mkv.seek_and_select(
                        track,
                        Time::new(0, 1).unwrap(),
                        Selection::All,
                        &mut WorkBudget::new(4096),
                        &off(),
                    )
                    .unwrap()
                );
                let replay = first_packet(&mut mkv);
                let RawEvent::Matroska(triergon::typed::Event::Packet(packet)) = replay.event()
                else {
                    panic!("seek did not return an AV1 packet");
                };
                let packet = Av1Input::Matroska(packet);
                assert!(matches!(
                    bridge.unwrap().packet(packet),
                    Err(Error::Core(C::Invalid(_)))
                ));
                let mut setup =
                    Av1BridgeSetup::new(pool, ID, packet, config_limits, packet_limits).unwrap();
                let BuildStep::Ready(bound) =
                    setup.step(&mut WorkBudget::new(4096), &off()).unwrap()
                else {
                    panic!("could not rebind the seek generation");
                };
                assert!(matches!(
                    bound
                        .packet(packet)
                        .unwrap()
                        .step(&mut WorkBudget::new(4096), &off(),)
                        .unwrap(),
                    BuildStep::Ready(_)
                ));
            }
        }
    }
}

#[test]
fn indexed_mp4_seek_starts_at_late_sync_and_republishes_its_configuration() {
    let raw = include_bytes!("fixtures/mp4/r-description-change.mov");
    let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 7, 1).unwrap();
    let old = first_packet(&mut p);
    let target = Time::new(7, 2).unwrap();
    let mut unsafe_transition = p
        .prepare_mp4_sync_seek(
            indexed_seek_request(Time::new(9, 2).unwrap()),
            Selection::All,
        )
        .unwrap();
    assert!(matches!(
        unsafe_transition.poll(&mut WorkBudget::new(64), &off()),
        Err(Error::Core(C::Unsupported(
            "no MP4 sync in target configuration"
        )))
    ));
    assert!(p.current(&old));
    let request = indexed_seek_request(target);
    let mut job = p.prepare_mp4_sync_seek(request, Selection::All).unwrap();
    assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    assert_eq!(command.sync_ordinal(), 4);
    assert_eq!(command.start_offset(), 815);
    assert!(
        !p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(0), &off())
            .unwrap()
    );
    assert!(p.current(&old));
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    assert!(!p.current(&old));
    assert!(matches!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off()),
        Err(Error::Core(C::StaleGeneration { .. }))
    ));
    let events = collect(&mut p, 1).unwrap();
    let packets = mp4_packets(&events);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.source.sample.ordinal)
            .collect::<Vec<_>>(),
        [4, 5, 6]
    );
    assert_eq!(packets[0].source.sample.range.start, 815);
    assert_eq!(packets[0].source.payload.as_ref(), &raw[815..833]);
    assert_eq!(packets[0].configuration.description.index, 2);
    assert!(matches!(
        events[0].event(),
        RawEvent::Seek(SeekIntent {
            replay_from_start: false,
            ..
        })
    ));
    assert!(matches!(
        events[1].event(),
        RawEvent::Mp4(Mp4Event::Configuration(_))
    ));
    let delivered = events
        .iter()
        .filter(|e| matches!(e.event(), RawEvent::Mp4(Mp4Event::Packet(_))))
        .map(Output::delivery)
        .collect::<Vec<_>>();
    assert_eq!(
        delivered,
        [
            Delivery::DecodeOnly { until: target },
            Delivery::Present,
            Delivery::Present
        ]
    );
}

fn indexed_seek_request(source_time: Time) -> Mp4SyncSeekRequest {
    Mp4SyncSeekRequest {
        video: 1,
        source_time,
        audio_preroll: Time::new(0, 1).unwrap(),
        maximum_scanned_rows: 1000,
    }
}

#[test]
fn indexed_mp4_seek_keeps_selected_subtitle_prefix_and_only_late_video() {
    let raw = include_bytes!("fixtures/mp4/r-multitrack.mov");
    let (mut p, _) = mp4(raw, Mp4View::SourcePhysical, 7, 1).unwrap();
    let before = collect(&mut p, 7).unwrap();
    let all = mp4_packets(&before);
    let expected: Vec<_> = all
        .iter()
        .filter(|p| p.configuration.track.track_id == 2 || p.source.sample.ordinal >= 4)
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    let target = Time::new(9, 2).unwrap();
    let selection = Selection::Tracks {
        video: Some(TrackKey::Mp4(1)),
        audio: None,
        subtitle: Some(TrackKey::Mp4(2)),
    };
    let request = indexed_seek_request(target);

    let mut cancelled = p.prepare_mp4_sync_seek(request, selection).unwrap();
    assert!(matches!(
        cancelled.poll(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Core(C::Cancelled))
    ));
    assert!(matches!(cancelled.finish(), Err(Error::Core(C::Cancelled))));

    let mut limited = p
        .prepare_mp4_sync_seek(
            Mp4SyncSeekRequest {
                maximum_scanned_rows: 2,
                ..request
            },
            selection,
        )
        .unwrap();
    assert!(matches!(
        limited.poll(&mut WorkBudget::new(3), &off()),
        Err(Error::Core(C::Limit {
            requested: 3,
            maximum: 2
        }))
    ));
    assert!(matches!(
        limited.finish(),
        Err(Error::Core(C::Limit { .. }))
    ));
    assert!(p.current(before.last().unwrap()));
    let mut job = p.prepare_mp4_sync_seek(request, selection).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    assert_eq!(command.sync_ordinal(), 4);
    assert_eq!(command.start_offset(), 1175);
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let after = collect(&mut p, 1).unwrap();
    let actual: Vec<_> = mp4_packets(&after)
        .into_iter()
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn indexed_mp4_seek_reads_aac_preroll_and_matches_the_encoded_suffix() {
    let raw = include_bytes!("fixtures/mp4/encoded-avc-aac-indexed-seek.mp4");
    let (selected, budget, calls) = probe(raw, 37, false);
    let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
    while !matches!(
        open.advance(&mut WorkBudget::new(64), &off()).unwrap(),
        OpenProgress::Ready
    ) {}
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    while !index.poll(&mut WorkBudget::new(64), &off()).unwrap() {}
    let engine = Engine::mp4(opened.input, index.finish().unwrap(), 1 << 20).unwrap();
    let mut p = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy {
            seek_input_bytes: 8192,
            ..PlayerPolicy::default()
        },
    )
    .unwrap();
    let before = collect(&mut p, 64).unwrap();
    let packets = mp4_packets(&before);
    let sync = packets
        .iter()
        .find(|p| p.configuration.track.track_id == 1 && p.source.sample.ordinal == 17)
        .unwrap();
    assert_eq!(sync.source.sample.range.start, 13038);
    assert!(sync.source.sample.is_container_sync());
    let sync_time = sync
        .source
        .sample
        .source_times(sync.configuration.track.base.require().unwrap())
        .unwrap()
        .cts
        .unwrap();
    let preroll = Time::new(1, 2).unwrap();
    let audio_from = sync_time.checked_sub(preroll).unwrap();
    let target = sync_time.checked_add(Time::new(1, 8).unwrap()).unwrap();
    let first_audio = packets
        .iter()
        .find(|p| {
            if p.configuration.track.track_id != 2 {
                return false;
            }
            let times = p
                .source
                .sample
                .source_times(p.configuration.track.base.require().unwrap())
                .unwrap();
            times.cts.unwrap().checked_add(times.decode_delta).unwrap() > audio_from
        })
        .unwrap();
    let first_offset = first_audio.source.sample.range.start;
    assert_eq!(first_offset, 12886);
    let expected: Vec<_> = packets
        .iter()
        .filter(|p| {
            p.source.sample.range.start >= first_offset
                && (p.configuration.track.track_id == 2 || p.source.sample.ordinal >= 17)
        })
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();

    let request = Mp4SyncSeekRequest {
        audio_preroll: preroll,
        ..indexed_seek_request(target)
    };
    let mut job = p.prepare_mp4_sync_seek(request, Selection::All).unwrap();
    while !job.poll(&mut WorkBudget::new(1), &off()).unwrap() {}
    let command = job.finish().unwrap();
    assert_eq!(command.sync_ordinal(), 17);
    assert_eq!(command.start_offset(), first_offset);
    calls.lock().unwrap().clear();
    assert!(
        p.apply_mp4_sync_seek(&command, &mut WorkBudget::new(1), &off())
            .unwrap()
    );
    let after = collect(&mut p, 11).unwrap();
    let actual: Vec<_> = mp4_packets(&after)
        .into_iter()
        .map(|p| (&p.source.sample, p.source.payload.as_ref()))
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 41);
    assert_eq!(
        actual
            .iter()
            .map(|(_, payload)| payload.len())
            .sum::<usize>(),
        4373
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0], Call::Invalidate);
    let offsets: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(offsets.first(), Some(&first_offset));
    assert!(offsets.iter().all(|offset| *offset >= first_offset));
    assert!(
        after
            .iter()
            .any(|o| matches!(o.delivery(), Delivery::DecodeOnly { .. }))
    );
    assert!(after.iter().any(|o| o.delivery() == Delivery::Present));
    drop(calls);
    drop(command);
    drop(p);
    drop(after);
    drop(before);
    drop(opened.movie);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn combined_playback_probe_opens_real_sequential_mp4() {
    use triergon_framing::RawAnnexBLimits;
    use triergon_playback::io::{
        PlaybackInput, PlaybackInputKind, PlaybackProbe, PlaybackProbeStep, RawVideoProbeLimits,
    };

    let raw = include_bytes!("fixtures/mp4/r-front-stco.mov");
    let b = pool();
    let (source, calls, _) = observed(&b, raw, 7, true);
    let mut probe = PlaybackProbe::new(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: false,
        },
        4096,
        source,
        RawVideoProbeLimits::default(),
    )
    .unwrap();
    let detected =
        (0..1000).find_map(
            |_| match probe.advance(&mut WorkBudget::new(1), &off()).unwrap() {
                PlaybackProbeStep::Detected(kind) => Some(kind),
                PlaybackProbeStep::Yield | PlaybackProbeStep::NeedInput(_) => None,
            },
        );
    assert_eq!(detected, Some(PlaybackInputKind::Container(Format::Mp4)));
    let PlaybackInput::Container(selected) = probe.finish(RawAnnexBLimits::default()).unwrap()
    else {
        panic!("combined probe did not hand off MP4");
    };
    drop(probe);
    let mut open = Mp4Open::new(selected, OpenLimits::default()).unwrap();
    let ready = (0..100_000).any(|_| {
        matches!(
            open.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            OpenProgress::Ready
        )
    });
    assert!(ready);
    let opened = open.finish().unwrap();
    let mut index = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::SourcePhysical,
        PlanLimits::default(),
    )
    .unwrap();
    assert!((0..100_000).any(|_| index.poll(&mut WorkBudget::new(1), &off()).unwrap()));
    let mut player = Player::new(
        Engine::mp4(opened.input, index.finish().unwrap(), 1 << 20).unwrap(),
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    drop(opened.movie);
    let out = collect(&mut player, 1).unwrap();
    let packets = mp4_packets(&out);
    assert_eq!(packets.len(), 6);
    for packet in packets {
        let range = &packet.source.sample.range;
        assert_eq!(
            packet.source.payload.as_ref(),
            &raw[range.start as usize..range.end as usize]
        );
    }
    let calls = calls.lock().unwrap();
    let offsets: Vec<_> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        })
        .collect();
    assert!(offsets.windows(2).all(|offsets| offsets[0] < offsets[1]));
    assert!(!calls.contains(&Call::Invalidate));
    drop(calls);
    drop(out);
    drop(player);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

fn native_range_player(bytes: &[u8]) -> (Player<Observed>, mp4_open::Movie, Calls, Budget) {
    let pool = pool();
    let (source, calls, _) = observed(&pool, bytes, 23, false);
    let input = InputCursor::new(
        &pool,
        ID,
        SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        },
        4096,
    )
    .unwrap();
    let mut opener = Mp4Open::new(
        SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Mp4,
        },
        OpenLimits::default(),
    )
    .unwrap();
    assert!((0..100_000).any(|_| matches!(
        opener.advance(&mut WorkBudget::new(17), &off()).unwrap(),
        OpenProgress::Ready
    )));
    let opened = opener.finish().unwrap();
    let mut job = Mp4IndexJob::new(
        &opened.movie,
        Mp4View::PinnedMov9NoParseNoFillin,
        PlanLimits::default(),
    )
    .unwrap();
    assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(17), &off()).unwrap()));
    let engine = Engine::mp4(opened.input, job.finish().unwrap(), 1 << 20).unwrap();
    let mut player = Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    player
        .set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    (player, opened.movie, calls, pool)
}

#[test]
fn native_range_seek_matches_ffprobe_packet_order_bytes_and_clocks() {
    let cases = [
        (
            include_bytes!("fixtures/mp4/r-multitrack.mov").as_slice(),
            3500,
            3000,
            include_str!("fixtures/mp4/r-multitrack.mov.native-seek"),
        ),
        (
            include_bytes!("fixtures/mp4/f-multitrack.mp4").as_slice(),
            3500,
            3000,
            include_str!("fixtures/mp4/f-multitrack.mp4.native-seek"),
        ),
        (
            include_bytes!("fixtures/mp4/r-edits-repeat.mov").as_slice(),
            1500,
            0,
            include_str!("fixtures/mp4/r-edits-repeat.mov.native-seek"),
        ),
        (
            include_bytes!("fixtures/mp4/r-description-change.mov").as_slice(),
            3500,
            3000,
            include_str!("fixtures/mp4/r-description-change.mov.native-seek"),
        ),
        (
            include_bytes!("fixtures/mp4/encoded-avc-aac-native-seek.mp4").as_slice(),
            28672,
            16384,
            include_str!("fixtures/mp4/encoded-avc-aac-native-seek.mp4.native-seek"),
        ),
    ];
    for (bytes, target, selected, expected) in cases {
        let (mut player, movie, calls, pool) = native_range_player(bytes);
        let old = first_packet(&mut player);
        calls.lock().unwrap().clear();
        let request = Mp4NativeSeekRequest {
            video: 1,
            timestamp: target,
            limits: triergon_isobmff::Mov9NativeSeekLimits::default(),
        };
        let mut job = player
            .prepare_mp4_native_seek(&movie, request, Selection::All)
            .unwrap();
        assert!(!job.poll(&mut WorkBudget::new(0), &off()).unwrap());
        assert!((0..100_000).any(|_| job.poll(&mut WorkBudget::new(1), &off()).unwrap()));
        let command = job.finish().unwrap();
        assert_eq!(command.receipt().requested_timestamp, target);
        assert_eq!(command.receipt().selected.timestamp, selected);
        assert!(
            calls.lock().unwrap().is_empty(),
            "native planning read media"
        );
        assert!(
            !player
                .apply_mp4_native_seek(&command, &mut WorkBudget::new(0), &off())
                .unwrap()
        );
        assert!(player.current(&old));
        assert!(
            player
                .apply_mp4_native_seek(&command, &mut WorkBudget::new(1), &off())
                .unwrap()
        );
        assert!(!player.current(&old));
        assert!(matches!(
            player.apply_mp4_native_seek(&command, &mut WorkBudget::new(1), &off()),
            Err(Error::Core(C::StaleGeneration { .. }))
        ));

        let events = collect(&mut player, 1).unwrap();
        assert!(matches!(
            events[0].event(),
            RawEvent::Seek(SeekIntent {
                clock: ClockChoice::ExplicitNativeProjection,
                replay_from_start: false,
                ..
            })
        ));
        let mut configurations = [None; 2];
        let mut wanted = expected.lines();
        for event in &events[1..] {
            match event.event() {
                RawEvent::Mp4(Mp4Event::Configuration(config)) => {
                    configurations[(config.track.track_id - 1) as usize] =
                        Some((config.revision, config.description.index));
                }
                RawEvent::Mp4(Mp4Event::Packet(packet)) => {
                    let fields: Vec<_> = wanted
                        .next()
                        .expect("extra packet after native seek")
                        .split_ascii_whitespace()
                        .collect();
                    let track: u32 = fields[0].parse().unwrap();
                    assert_eq!(packet.configuration.track.track_id, track);
                    assert_eq!(
                        configurations[(track - 1) as usize],
                        Some((
                            packet.configuration.revision,
                            packet.configuration.description.index
                        ))
                    );
                    let native = packet.native.as_ref().unwrap();
                    assert_eq!(native.track_id, track);
                    assert_eq!(native.native.pts, fields[1].parse().ok());
                    assert_eq!(native.native.dts, fields[2].parse().ok());
                    assert_eq!(native.native.duration, fields[3].parse::<i64>().unwrap());
                    assert_eq!(native.native.flags, fields[6].parse::<u32>().unwrap());
                    let pos: usize = fields[4].parse().unwrap();
                    let size: usize = fields[5].parse().unwrap();
                    assert_eq!(packet.source.sample.range, pos as u64..(pos + size) as u64);
                    assert_eq!(packet.source.payload.as_ref(), &bytes[pos..pos + size]);
                    assert!(player.current(event));
                }
                _ => panic!("unexpected native seek output"),
            }
        }
        assert!(wanted.next().is_none(), "short native seek packet sequence");
        let first_read = calls.lock().unwrap().iter().find_map(|call| match call {
            Call::Poll(offset) => Some(*offset),
            _ => None,
        });
        assert_eq!(first_read, Some(command.start_offset()));
        assert!(command.start_offset() > 0);
        drop(command);
        drop(events);
        drop(old);
        drop(player);
        drop(movie);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_range_seek_replays_the_encoded_edit_occurrences() {
    use triergon_isobmff::{
        Mov9CursorStatus, Mov9EncodedEditContextV2, Mov9EncodedEditJobV2, Mov9EncodedStreamJobV1,
        PinnedMovD32EncodedEditsV2, PinnedMovD32EncodedRegularV1,
    };
    let raw = include_bytes!("../../isobmff/tests/fixtures/encoded-edit-v2/original.mp4");
    let (mut opened, _) = open(raw, 97, 4096).unwrap();
    let cancel = off();
    let mut plans = Vec::new();
    for track in opened.movie.tracks() {
        let mut stream = Mov9EncodedStreamJobV1::new(
            track.header(),
            track.table(),
            &opened.input,
            opened.movie.major_brand(),
            Default::default(),
            PinnedMovD32EncodedRegularV1,
        )
        .unwrap();
        while stream
            .poll(&mut opened.input, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
            != Mov9CursorStatus::Ready
        {}
        let stream = stream.take(&mut opened.input, &cancel).unwrap();
        let mut edit = Mov9EncodedEditJobV2::new(
            Mov9EncodedEditContextV2 {
                table: track.table(),
                edits: track.edits().unwrap(),
                stream: &stream,
                media_base: track.header().time_base().unwrap(),
                movie_base: opened.movie.header().time_base().unwrap(),
            },
            &opened.input,
            Default::default(),
            PinnedMovD32EncodedEditsV2,
        )
        .unwrap();
        while edit
            .poll(&mut opened.input, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
            != Mov9CursorStatus::Ready
        {}
        plans.push(Some(edit.take(&mut opened.input, &cancel).unwrap()));
    }
    let mut index =
        Mp4IndexJob::new_native_encoded_range(&opened.movie, &plans, PlanLimits::default())
            .unwrap();
    while !index.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {}
    let plan = index.finish().unwrap();
    drop(plans);
    let engine = Engine::mp4(opened.input, plan, 1 << 20).unwrap();
    let mut player = Player::new_direct(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    player
        .set_clock_view(ClockChoice::ExplicitNativeProjection)
        .unwrap();
    let leading = [0xf0, 3, 0, 0, 0, 0, 0, 0, 0, 0];
    let trailing = [0, 0, 0, 0, 0xf0, 1, 0, 0, 0, 0];
    let linear = collect(&mut player, 4096).unwrap();
    let audio_trim: Vec<_> = linear
        .iter()
        .filter_map(|output| match output.event() {
            RawEvent::Mp4(Mp4Event::Packet(packet)) if packet.configuration.track.track_id == 2 => {
                packet.encoded_skip_samples
            }
            _ => None,
        })
        .collect();
    assert_eq!(audio_trim, [leading, trailing]);
    drop(linear);
    let command = {
        let mut job = player
            .prepare_mp4_native_seek(
                &opened.movie,
                Mp4NativeSeekRequest {
                    video: 1,
                    timestamp: 43_200,
                    limits: Default::default(),
                },
                Selection::All,
            )
            .unwrap();
        while !job.poll(&mut WorkBudget::new(4096), &cancel).unwrap() {}
        job.finish().unwrap()
    };
    assert_eq!(command.receipt().selected.timestamp, 29_328);
    assert_eq!(command.start_offset(), 38_074);
    assert!(
        player
            .apply_mp4_native_seek(&command, &mut WorkBudget::new(4096), &cancel)
            .unwrap()
    );
    let output = collect(&mut player, 4096).unwrap();
    let packets: Vec<_> = output
        .iter()
        .filter_map(|event| match event.event() {
            RawEvent::Mp4(Mp4Event::Packet(packet)) => Some(packet),
            _ => None,
        })
        .collect();
    // First two public packets from the independent range seek oracle.
    for (packet, (id, pts, dts, start, length)) in packets.iter().zip([
        (2, 87_048, 87_048, 38_074, 133),
        (1, 32_000, 29_328, 38_207, 3220),
    ]) {
        let native = &packet.native.as_ref().unwrap().native;
        assert_eq!(packet.configuration.track.track_id, id);
        assert_eq!((native.pts, native.dts), (Some(pts), Some(dts)));
        assert_eq!(packet.source.payload.as_ref(), &raw[start..start + length]);
    }
    assert!(packets.len() > 2);
    assert_eq!(
        packets
            .iter()
            .filter_map(|packet| packet.encoded_skip_samples)
            .collect::<Vec<_>>(),
        [trailing],
    );
}

#[test]
fn native_range_seek_refusals_leave_the_current_player_untouched() {
    let (mut player, movie, calls, _) =
        native_range_player(include_bytes!("fixtures/mp4/r-edits-repeat.mov"));
    let current = first_packet(&mut player);
    calls.lock().unwrap().clear();
    let request = Mp4NativeSeekRequest {
        video: 1,
        timestamp: 3500,
        limits: triergon_isobmff::Mov9NativeSeekLimits::default(),
    };
    let mut job = player
        .prepare_mp4_native_seek(&movie, request, Selection::All)
        .unwrap();
    assert!(matches!(
        job.poll(&mut WorkBudget::new(10_000), &off()),
        Err(Error::Core(C::Unsupported(
            "native seek overlapping index clock query"
        )))
    ));
    drop(job);
    assert!(player.current(&current));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn direct_read_exposes_pending_input_after_one_transport_poll() {
    let raw = include_bytes!("fixtures/mkv-feed/three-tracks.mkv");
    let b = pool();
    let (mut source, calls, _) = observed(&b, raw, 4096, false);
    source.pending = 3;
    let engine = Engine::matroska(
        &b,
        ID,
        SourceCapabilities {
            length: Some(raw.len() as u64),
            random_access: true,
        },
        triergon::Limits::default(),
        4096,
    )
    .unwrap();
    let mut player = Player::new(
        engine,
        SourceOwner::new(source),
        Selection::All,
        PlayerPolicy::default(),
    )
    .unwrap();
    let mut slot = None;
    let mut token = None;
    for count in 1..=3 {
        let Progress::NeedInput(request) = player
            .read_output(&mut slot, &mut WorkBudget::new(1 << 17), &off())
            .unwrap()
        else {
            panic!("transport has not completed this request")
        };
        if let Some(first) = &token {
            assert_eq!(&request.token, first);
        }
        token = Some(request.token);
        assert!(slot.is_none());
        assert_eq!(calls.lock().unwrap().len(), count);
    }
    assert!(matches!(
        player
            .read_output(&mut slot, &mut WorkBudget::new(1 << 17), &off())
            .unwrap(),
        Progress::Queued
    ));
    assert!(slot.as_ref().is_some_and(|output| player.current(output)));
}
