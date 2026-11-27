use crate::support::sequential::*;
use crate::support::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use triergon_core::{Budget, Bytes, Error as C, InputCursor, SourceCapabilities, Time, WorkBudget};
use triergon_playback::compat::pes::startup;
use triergon_playback::{ByteSource, Source, SourceReply};
use triergon_playback::{SourceOwner, probe::SelectedInput, *};

struct StartupSource {
    inner: ByteSource,
    allow: Arc<AtomicBool>,
    len: usize,
    reads: Arc<AtomicUsize>,
}

impl Source for StartupSource {
    fn poll(
        &mut self,
        r: &triergon_core::InputRequest,
        w: &mut WorkBudget,
        c: &AtomicBool,
    ) -> Result<SourceReply> {
        if w.remaining() == 0 {
            return Ok(SourceReply::Pending);
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        if r.offset == self.len as u64 && !self.allow.load(Ordering::Relaxed) {
            return Ok(SourceReply::Pending);
        }
        self.inner.poll(r, w, c)
    }
    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
    fn abort(&mut self) {
        self.inner.abort();
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

fn native_setup(
    quantum: usize,
    known: bool,
) -> (
    startup::Startup<StartupSource>,
    Budget,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
) {
    let raw = include_bytes!("../fixtures/ts/m2ts192-opaque-prefix.m2ts");
    let b = pool();
    let allow = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(AtomicUsize::new(0));
    let source = StartupSource {
        inner: ByteSource::new(ID, Bytes::copy_from_slice(&b, raw).unwrap(), quantum).unwrap(),
        allow: allow.clone(),
        len: raw.len(),
        reads: reads.clone(),
    };
    let input = InputCursor::new(
        &b,
        ID,
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: false,
        },
        16 << 10,
    )
    .unwrap();
    (
        startup::Startup::new(SelectedInput {
            input,
            source: SourceOwner::new(source),
            format: Format::Ts(triergon_mpegts::PacketFormat::M2ts192),
        })
        .unwrap(),
        b,
        allow,
        reads,
    )
}

fn prepared_native() -> (startup::BoundInput<StartupSource>, Budget) {
    let (mut setup, b, _, _) = native_setup(16 << 10, true);
    for _ in 0..10_000 {
        if setup.advance(&mut WorkBudget::new(1), &off()).unwrap() == startup::StartupStep::Ready {
            return (setup.finish().unwrap(), b);
        }
    }
    panic!("finite PES startup stalled")
}

#[test]
fn native_m2ts_context_uses_real_complete_window_and_positive_eof_reply() {
    let (prepared, b) = prepared_native();
    assert_eq!(prepared.context().inspected_bytes(), 576);
    let mut p = prepared
        .into_player(
            1,
            triergon_mpegts::ParseLimits::default(),
            triergon_mpegts::clock::SourceTimePolicy::Unanchored,
            256,
            triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    p.set_clock_view(ClockChoice::Demux).unwrap();
    let out = collect(&mut p, 1).unwrap();
    let native: Vec<_> = out
        .iter()
        .filter_map(|o| {
            if let Event::DemuxPes { packet, clock } = o.event() {
                Some((packet, clock))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].0.position, 388);
    assert_eq!(native[0].0.source_origin.offset, 384);
    assert_eq!(native[0].0.flags, 0);
    assert_eq!(native[0].0.duration90_raw, 0);
    assert_eq!(native[0].1.pts90, Some(90_000));
    assert!(out.iter().any(|o| matches!(
        o.event(),
        Event::Ts(TsEvent {
            source: triergon_mpegts::stream::TraceEvent::Pes(_),
            ..
        })
    )));
    drop(out);
    drop(p);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn native_startup_pending_eof_is_never_completed_from_declared_length() {
    let (mut s, _, allow, reads) = native_setup(16 << 10, true);
    allow.store(false, Ordering::Relaxed);
    assert_eq!(
        s.advance(&mut WorkBudget::new(1), &off()).unwrap(),
        startup::StartupStep::NeedEof
    );
    for _ in 0..10 {
        assert_eq!(
            s.advance(&mut WorkBudget::new(1), &off()).unwrap(),
            startup::StartupStep::NeedEof
        );
    }
    assert_eq!(reads.load(Ordering::Relaxed), 11);
    allow.store(true, Ordering::Relaxed);
    for _ in 0..10_000 {
        if s.advance(&mut WorkBudget::new(1), &off()).unwrap() == startup::StartupStep::Ready {
            assert_eq!(s.finish().unwrap().context().inspected_bytes(), 576);
            return;
        }
    }
    panic!("startup failed to finish after actual EOF")
}

#[test]
fn native_startup_short_read_is_refusal_and_not_quantum_as_eof() {
    for known in [false, true] {
        let (mut s, b, _, _) = native_setup(7, known);
        let mut refused = false;
        for _ in 0..100 {
            match s.advance(&mut WorkBudget::new(1), &off()) {
                Err(C::Unsupported(_)) => {
                    refused = true;
                    break;
                }
                Ok(startup::StartupStep::Ready) => panic!("partial window admitted"),
                Ok(_) => {}
                Err(e) => panic!("wrong {e:?}"),
            }
        }
        assert!(refused);
        assert!(s.finish().is_err());
        assert_eq!(b.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_startup_work_zero_and_cancel_do_not_perform_source_reads() {
    let (mut s, b, _, reads) = native_setup(16 << 10, true);
    let before = b.used();
    for _ in 0..10 {
        assert_eq!(
            s.advance(&mut WorkBudget::new(0), &off()).unwrap(),
            startup::StartupStep::Yield
        );
    }
    assert_eq!(reads.load(Ordering::Relaxed), 0);
    assert_eq!(b.used(), before);
    assert!(matches!(
        s.advance(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(C::Cancelled)
    ));
    drop(s);
    assert_eq!(b.used(), Budget::control_storage_bytes());
}

#[test]
fn native_m2ts_seek_requires_new_context_and_keeps_old_output_scope() {
    let (prepared, _) = prepared_native();
    let mut p = prepared
        .into_player(
            1,
            triergon_mpegts::ParseLimits::default(),
            triergon_mpegts::clock::SourceTimePolicy::RelativeEpochZero,
            256,
            triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
            Selection::All,
            PlayerPolicy::default(),
        )
        .unwrap();
    let old = loop {
        p.advance(&mut WorkBudget::new(1), &off()).unwrap();
        if let Some(o) = p.pop()
            && o.event().track().is_some()
        {
            break o;
        }
    };
    assert!(matches!(
        p.seek_and_select(
            TrackKey::Ts {
                program: 1,
                pid: 256
            },
            Time::new(0, 1).unwrap(),
            Selection::All,
            &mut WorkBudget::new(1),
            &off()
        ),
        Err(C::Unsupported(_))
    ));
    assert!(p.current(&old));
}

struct SequentialStartup {
    source: ByteSource,
    next: Arc<AtomicUsize>,
}

impl Source for SequentialStartup {
    fn poll(
        &mut self,
        request: &triergon_core::InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<SourceReply> {
        let at = self.next.load(Ordering::Relaxed);
        assert_eq!(
            request.offset, at as u64,
            "startup bytes must never be read twice"
        );
        let reply = self.source.poll(request, work, cancel)?;
        if let SourceReply::Data(window) = &reply {
            self.next
                .store(at + window.bytes().len(), Ordering::Relaxed);
        }
        Ok(reply)
    }
    fn invalidate(&mut self) {
        panic!("M2TS prefix must preserve the source generation");
    }
    fn abort(&mut self) {
        self.source.abort();
    }
    fn close(&mut self) {
        self.source.close();
    }
}

fn long_m2ts() -> Vec<u8> {
    let mut data = include_bytes!("../fixtures/ts/m2ts192-opaque-prefix.m2ts").to_vec();
    assert_eq!(data.len(), 3 * 192);
    let mut null = [0xff; 192];
    null[..8].copy_from_slice(&[0, 0, 0, 0, 0x47, 0x1f, 0xff, 0x10]);
    let mut late: [u8; 192] = data[384..].try_into().unwrap();
    late[7] = (late[7] & 0xf0) | ((late[7] + 1) & 15);
    for _ in 3..95 {
        data.extend_from_slice(&null);
    }
    data.extend_from_slice(&late);
    data
}

fn prefix_setup(
    raw: &[u8],
    known: bool,
    quantum: usize,
) -> (
    startup::Startup<SequentialStartup>,
    Budget,
    Arc<AtomicUsize>,
) {
    let budget = pool();
    let next = Arc::new(AtomicUsize::new(0));
    let source = SequentialStartup {
        source: ByteSource::new(ID, Bytes::copy_from_slice(&budget, raw).unwrap(), quantum)
            .unwrap(),
        next: next.clone(),
    };
    let input = InputCursor::new(
        &budget,
        ID,
        SourceCapabilities {
            length: known.then_some(raw.len() as u64),
            random_access: false,
        },
        16 << 10,
    )
    .unwrap();
    let startup = startup::Startup::new_m2ts_prefix(SelectedInput {
        input,
        source: SourceOwner::new(source),
        format: Format::Ts(triergon_mpegts::PacketFormat::M2ts192),
    })
    .unwrap();
    (startup, budget, next)
}

fn open_prefix(mut startup: startup::Startup<SequentialStartup>) -> Player<SequentialStartup> {
    for _ in 0..30_000 {
        if startup.advance(&mut WorkBudget::new(31), &off()).unwrap() == startup::StartupStep::Ready
        {
            let prepared = startup.finish().unwrap();
            assert!(prepared.context().is_prefix());
            assert_eq!(prepared.context().inspected_bytes(), 8192);
            let mut player = prepared
                .into_player(
                    1,
                    triergon_mpegts::ParseLimits::default(),
                    triergon_mpegts::clock::SourceTimePolicy::Unanchored,
                    256,
                    triergon_mpegts::compat::pes::RegisteredCarriage::Avc,
                    Selection::All,
                    PlayerPolicy::default(),
                )
                .unwrap();
            player.set_clock_view(ClockChoice::Demux).unwrap();
            return player;
        }
    }
    panic!("prefix startup stalled");
}

#[test]
fn native_m2ts_prefix_preserves_the_same_sequential_source_through_late_pes() {
    let raw = long_m2ts();
    for (known, quantum) in [(true, 16384), (false, 13)] {
        let (startup, budget, next) = prefix_setup(&raw, known, quantum);
        let mut player = open_prefix(startup);
        assert_eq!(next.load(Ordering::Relaxed), 8192);
        let output = collect(&mut player, 1).unwrap();
        let positions: Vec<_> = output
            .iter()
            .filter_map(|item| match item.event() {
                Event::DemuxPes { packet, .. } => {
                    Some((packet.position, packet.source_origin.offset))
                }
                Event::PesRefusal { error, .. } => {
                    panic!("unexpected PES refusal {error:?}")
                }
                _ => None,
            })
            .collect();
        assert_eq!(positions, [(384, 384), (95 * 192, 95 * 192)]);
        assert_eq!(next.load(Ordering::Relaxed), raw.len());
        drop(output);
        drop(player);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_m2ts_prefix_pressure_keeps_the_completed_reply() {
    let raw = long_m2ts();
    let (mut startup, budget, next) = prefix_setup(&raw, false, 13);
    let lease = budget.reserve(budget.available() - 32).unwrap();
    assert!(matches!(
        startup.advance(&mut WorkBudget::new(31), &off()),
        Err(C::Backpressure { .. })
    ));
    assert_eq!(next.load(Ordering::Relaxed), 13);
    drop(lease);
    assert_eq!(
        startup.advance(&mut WorkBudget::new(31), &off()).unwrap(),
        startup::StartupStep::Yield
    );
    assert_eq!(next.load(Ordering::Relaxed), 13);
    let mut player = open_prefix(startup);
    assert_eq!(next.load(Ordering::Relaxed), 8192);
    assert!(collect(&mut player, 1).is_ok());
    drop(player);
    assert_eq!(budget.used(), Budget::control_storage_bytes());
}

#[test]
fn native_m2ts_prefix_requires_the_actual_complete_unambiguous_window() {
    let mut ambiguous = long_m2ts();
    for i in 1..=7 {
        ambiguous[188 * i] = 0x47;
    }
    for (raw, message) in [
        (&ambiguous[..], "M2TS prefix grid is ambiguous"),
        (
            &ambiguous[..8000],
            "source ended before the 8192-byte M2TS prefix",
        ),
    ] {
        let (mut startup, budget, _) = prefix_setup(raw, false, 1024);
        let failure = loop {
            match startup.advance(&mut WorkBudget::new(1024), &off()) {
                Ok(startup::StartupStep::Yield) => {}
                Err(error) => break error,
                _ => panic!("unproven prefix became ready"),
            }
        };
        assert!(matches!(failure, C::Unsupported(error) if error == message));
        drop(startup);
        assert_eq!(budget.used(), Budget::control_storage_bytes());
    }
}

#[test]
fn native_m2ts_prefix_grid_loss_preserves_source_and_refuses_native_tail() {
    let mut raw = long_m2ts();
    raw[50 * 192 + 4] = 0x46;
    let (startup, _, _) = prefix_setup(&raw, false, 512);
    let mut player = open_prefix(startup);
    let output = collect(&mut player, 1).unwrap();
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(o.event(), Event::DemuxPes { .. }))
            .count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(
                o.event(),
                Event::PesRefusal {
                    error: C::Unsupported("M2TS carrier grid lost during PES playback"),
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .filter(|o| matches!(
                o.event(),
                Event::Ts(TsEvent {
                    source: triergon_mpegts::stream::TraceEvent::Pes(_),
                    ..
                })
            ))
            .count(),
        2
    );
}
