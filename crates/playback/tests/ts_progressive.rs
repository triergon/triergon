use std::{cell::Cell, rc::Rc, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceFailure,
    SourceId, Time, WorkBudget,
};
use triergon_mpegts::psi::PsiCrc32;
use triergon_playback::{
    Error, Source, SourceReply,
    ts_progressive::{
        TsProgressiveLimits, TsProgressiveReader, TsProgressiveStep, TsProgressiveUnit,
    },
};

const BASELINE: &[u8] = include_bytes!("fixtures/hls/baseline-stereo.ts");
// Audio and video have produced complete units by the end of the first audio PES.
const BASELINE_PREFIX: usize = 39 * 188;
const ID: SourceId = SourceId(31);

#[derive(Clone, Copy)]
enum Completion {
    End,
    Cut,
}

#[derive(Default)]
struct BodyState {
    available: Cell<usize>,
    read: Cell<usize>,
    yield_before_reply: Cell<bool>,
    completion: Cell<Option<Completion>>,
    aborts: Cell<usize>,
    closes: Cell<usize>,
}

struct Body<'a> {
    state: Rc<BodyState>,
    bytes: &'a [u8],
    quantum: usize,
    pool: Budget,
}

impl Source for Body<'_> {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        _: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        assert_eq!(request.source, ID);
        let offset = request.offset as usize;
        assert_eq!(offset, self.state.read.get());
        if self.state.yield_before_reply.replace(false) {
            work.take(work.remaining());
            return Ok(SourceReply::Pending);
        }
        let remaining = self.state.available.get() - offset;
        if remaining == 0 {
            let source = request.source;
            let token = request.token.clone();
            return Ok(match self.state.completion.get() {
                Some(Completion::End) if work.try_charge(1) => SourceReply::End { source, token },
                Some(Completion::Cut) if work.try_charge(1) => SourceReply::Failed {
                    source,
                    token,
                    failure: SourceFailure::Transport,
                },
                _ => SourceReply::Pending,
            });
        }
        if !work.try_charge(1) {
            return Ok(SourceReply::Pending);
        }
        let end = offset + remaining.min(self.quantum).min(request.maximum);
        assert!(end > offset);
        let bytes = Bytes::copy_from_slice(&self.pool, &self.bytes[offset..end])?;
        let window =
            InputWindow::new(request.source, request.token.clone(), request.offset, bytes)?;
        self.state.read.set(end);
        Ok(SourceReply::Data(window))
    }

    fn invalidate(&mut self) {
        panic!("progressive feed must not reposition the body");
    }

    fn abort(&mut self) {
        self.state.aborts.set(self.state.aborts.get() + 1);
    }

    fn close(&mut self) {
        assert_eq!(self.state.aborts.get(), 1);
        self.state.closes.set(self.state.closes.get() + 1);
    }
}

fn reader<'a>(
    pool: &Budget,
    bytes: &'a [u8],
    length: Option<u64>,
    quantum: usize,
) -> (TsProgressiveReader<Body<'a>>, Rc<BodyState>) {
    let state = Rc::new(BodyState::default());
    let input = InputCursor::new(
        pool,
        ID,
        SourceCapabilities {
            length,
            random_access: false,
        },
        quantum,
    )
    .unwrap();
    let source = Body {
        state: state.clone(),
        bytes,
        quantum,
        pool: pool.clone(),
    };
    (
        TsProgressiveReader::new(input, source, TsProgressiveLimits::default()).unwrap(),
        state,
    )
}

fn next(
    reader: &mut TsProgressiveReader<Body<'_>>,
    quantum: u64,
) -> triergon_playback::Result<TsProgressiveStep> {
    let cancel = AtomicBool::new(false);
    for _ in 0..200_000 {
        match reader.advance(&mut WorkBudget::new(quantum), &cancel)? {
            TsProgressiveStep::Yield => {}
            step => return Ok(step),
        }
    }
    panic!("progressive reader stalled");
}

fn closed(pool: &Budget, state: &BodyState) {
    assert_eq!(state.aborts.get(), 1);
    assert_eq!(state.closes.get(), 1);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn pending_source_that_spends_its_work_yields_before_waiting_for_input() {
    let pool = Budget::new(64 * 1024).unwrap();
    let (mut reader, source) = reader(&pool, BASELINE, None, 4096);
    source.yield_before_reply.set(true);
    let cancel = AtomicBool::new(false);
    let mut work = WorkBudget::new(32);
    assert!(matches!(
        reader.advance(&mut work, &cancel).unwrap(),
        TsProgressiveStep::Yield
    ));
    assert_eq!(work.remaining(), 0);
    let TsProgressiveStep::NeedInput(request) =
        reader.advance(&mut WorkBudget::new(32), &cancel).unwrap()
    else {
        panic!("expected the outstanding input request");
    };
    assert_eq!(request.source, ID);
    assert_eq!(request.offset, 0);
    drop(reader);
    closed(&pool, &source);
}

#[test]
fn early_units_and_eof_match_retained_payload_order_and_wire_clocks() {
    let baseline_rows = include_str!("fixtures/hls/baseline-stereo.tsv");
    let baseline_order = include_str!("fixtures/hls/baseline-stereo.order");
    // Repeat one AAC continuation on PID 257 with continuity counter 1.
    let carrier = &BASELINE[26 * 188..27 * 188];
    assert_eq!(carrier[..4], [0x47, 0x01, 0x01, 0x11]);
    let duplicate = [&BASELINE[..27 * 188], carrier, &BASELINE[27 * 188..]].concat();
    for (raw, rows, order, prefix, rate, channels, has_video_dts) in [
        (
            BASELINE,
            baseline_rows,
            baseline_order,
            BASELINE_PREFIX,
            44_100_u32,
            2,
            false,
        ),
        (
            duplicate.as_slice(),
            baseline_rows,
            baseline_order,
            BASELINE_PREFIX + 188,
            44_100,
            2,
            false,
        ),
        (
            include_bytes!("fixtures/hls/bframes-multislice.ts").as_slice(),
            include_str!("fixtures/hls/bframes-multislice.tsv"),
            include_str!("fixtures/hls/bframes-multislice.order"),
            55 * 188,
            48_000,
            1,
            true,
        ),
    ] {
        for (read, work, length) in [(1, 1, None), (4096, 4096, Some(raw.len() as u64))] {
            let pool = Budget::new(64 * 1024).unwrap();
            let (mut reader, source) = reader(&pool, raw, length, read);
            source.available.set(prefix);
            let mut order = order.trim_end().bytes();
            let mut video = rows.lines().filter(|line| line.starts_with('v')).peekable();
            let mut audio = rows.lines().filter(|line| line.starts_with('a')).peekable();
            let mut next_audio = None;
            let mut before_end = [0, 0];
            let mut pauses = 0;
            loop {
                match next(&mut reader, work).unwrap() {
                    TsProgressiveStep::Unit(TsProgressiveUnit::Video(unit)) => {
                        assert_eq!(order.next(), Some(b'v'));
                        let mut row = video.next().unwrap().split_ascii_whitespace().skip(1);
                        let pts = row.next().unwrap().parse::<i64>().unwrap();
                        let dts = row.next().unwrap().parse::<i64>().unwrap();
                        let clock = unit.clock.unwrap();
                        assert_eq!(clock.pts90, Some(pts));
                        assert_eq!(clock.dts90, Some(dts));
                        assert_eq!(clock.source_pts_raw33, Some(pts as u64));
                        assert_eq!(clock.source_dts_raw33, has_video_dts.then_some(dts as u64));
                        assert_eq!(unit.idr, row.next() == Some("1"));
                        assert_hex(unit.source_bytes().as_ref(), row.next().unwrap());
                        if source.completion.get().is_none() {
                            before_end[0] += 1;
                        }
                    }
                    TsProgressiveStep::Unit(TsProgressiveUnit::Audio {
                        frame,
                        presentation,
                    }) => {
                        assert_eq!(order.next(), Some(b'a'));
                        let mut row = audio.next().unwrap().split_ascii_whitespace().skip(1);
                        let pts = wire_time(row.next().unwrap());
                        let dts = wire_time(row.next().unwrap());
                        assert_eq!((frame.pts, frame.dts), (pts, dts));
                        let duration = Time::new(1024, u128::from(rate)).unwrap();
                        assert_eq!(frame.duration, duration);
                        assert_eq!(frame.header.coded_frequency_hz(), Some(rate));
                        assert_eq!(frame.header.coded_channel_count(), Some(channels));
                        assert_eq!(Some(presentation), pts.or(dts).or(next_audio));
                        next_audio = Some(presentation.checked_add(duration).unwrap());
                        assert_hex(frame.bytes.as_ref(), row.nth(3).unwrap());
                        if source.completion.get().is_none() {
                            before_end[1] += 1;
                        }
                    }
                    TsProgressiveStep::NeedInput(request) => {
                        assert_eq!(source.closes.get(), 0);
                        // A small work budget can expose a request before the
                        // player's next poll of an already available reply.
                        if request.offset < source.available.get() as u64
                            || source.completion.get().is_some()
                        {
                            continue;
                        }
                        match pauses {
                            0 => {
                                assert_eq!(request.offset as usize, prefix);
                                assert!(before_end.iter().all(|&count| count > 0));
                                source.available.set(raw.len());
                            }
                            1 => {
                                assert_eq!(request.offset as usize, raw.len());
                                if length.is_some() {
                                    assert_eq!(request.maximum, 0);
                                }
                                assert!(video.peek().is_some(), "final video must wait for End");
                                source.completion.set(Some(Completion::End));
                            }
                            _ => panic!("unexpected request after EOF"),
                        }
                        pauses += 1;
                    }
                    TsProgressiveStep::End => break,
                    other => panic!("unexpected step {other:?}"),
                }
            }
            assert_eq!(pauses, 2);
            assert!(order.next().is_none() && video.next().is_none() && audio.next().is_none());
            assert!(matches!(
                next(&mut reader, work),
                Ok(TsProgressiveStep::End)
            ));
            drop(reader);
            closed(&pool, &source);
        }
    }
}

fn assert_hex(bytes: &[u8], expected: &str) {
    assert_eq!(bytes.len() * 2, expected.len());
    for (&byte, hex) in bytes.iter().zip(expected.as_bytes().as_chunks::<2>().0) {
        assert_eq!(
            byte,
            u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap()
        );
    }
}

fn wire_time(ticks: &str) -> Option<Time> {
    (ticks != "-").then(|| Time::new(ticks.parse().unwrap(), 90_000).unwrap())
}

#[test]
fn continuous_mux_exceeds_the_live_budget() {
    // Consecutive FFmpeg mux sections retain continuity counters and clocks.
    let raw = [
        include_bytes!("fixtures/hls/media-00.ts").as_slice(),
        include_bytes!("fixtures/hls/media-01.ts").as_slice(),
    ]
    .concat();
    let pool = Budget::new(24 * 1024).unwrap();
    assert!(raw.len() > pool.limit());
    let (mut reader, source) = reader(&pool, &raw, Some(raw.len() as u64), 188);
    source.available.set(raw.len());
    source.completion.set(Some(Completion::End));
    let mut counts = [0, 0];
    let mut payload_bytes = 0;
    loop {
        match next(&mut reader, 4096).unwrap() {
            TsProgressiveStep::Unit(TsProgressiveUnit::Video(unit)) => {
                counts[0] += 1;
                payload_bytes += unit.source_bytes().len();
            }
            TsProgressiveStep::Unit(TsProgressiveUnit::Audio { frame, .. }) => {
                counts[1] += 1;
                payload_bytes += frame.bytes.len();
            }
            TsProgressiveStep::NeedInput(request) => {
                assert_eq!(request.offset, source.read.get() as u64);
            }
            TsProgressiveStep::End => break,
            other => panic!("unexpected step {other:?}"),
        }
    }
    assert_eq!(counts, [120, 174]);
    assert!(
        payload_bytes > pool.peak(),
        "peak charged capacity {}",
        pool.peak()
    );
    closed(&pool, &source);
}

#[test]
fn a_pes_at_the_window_boundary_leaves_room_for_the_next_reply() {
    const CARRIER: usize = 188;
    const WINDOW: usize = 512 * CARRIER;
    let mut null = [0xff; CARRIER];
    null[..4].copy_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
    // The first video PES starts in the fixture's fourth carrier.
    // Make that carrier the last one in a full input window.
    let mut raw = null.repeat(508);
    raw.extend_from_slice(BASELINE);
    while raw.len() < WINDOW * 2 {
        raw.extend_from_slice(&null);
    }
    let pool = Budget::new(160 * 1024).unwrap();
    assert!(raw.len() > pool.limit());
    let (mut reader, source) = reader(&pool, &raw, None, WINDOW);
    source.available.set(raw.len());
    source.completion.set(Some(Completion::End));
    let mut counts = [0, 0];
    loop {
        match next(&mut reader, 65536).unwrap() {
            TsProgressiveStep::Unit(TsProgressiveUnit::Video(_)) => counts[0] += 1,
            TsProgressiveStep::Unit(TsProgressiveUnit::Audio { .. }) => counts[1] += 1,
            TsProgressiveStep::NeedInput(request) => {
                assert_eq!(request.offset, source.read.get() as u64);
            }
            TsProgressiveStep::End => break,
            other => panic!("expected to read both windows, got {other:?}"),
        }
    }
    assert_eq!(counts, [12, 24]);
    assert_eq!(source.read.get(), raw.len());
    closed(&pool, &source);
}

#[test]
fn transport_cut_short_length_and_partial_carrier_refuse_without_eof_flush() {
    for (prefix, length, completion, error) in [
        (BASELINE_PREFIX, None, Completion::Cut, Error::Transport),
        (
            BASELINE.len(),
            Some(BASELINE.len() as u64),
            Completion::Cut,
            Error::Transport,
        ),
        (
            BASELINE_PREFIX,
            Some(BASELINE.len() as u64),
            Completion::End,
            Error::Truncated,
        ),
        (BASELINE.len() - 1, None, Completion::End, Error::Truncated),
    ] {
        let pool = Budget::new(64 * 1024).unwrap();
        let (mut reader, source) = reader(&pool, BASELINE, length, 301);
        source.available.set(prefix);
        assert_eq!(drain_to_request(&mut reader).offset as usize, prefix);
        source.completion.set(Some(completion));
        let result = next(&mut reader, 4096);
        assert!(
            matches!(result, Err(actual) if actual == error),
            "{result:?}"
        );
        assert!(matches!(next(&mut reader, 4096), Err(Error::Closed)));
        closed(&pool, &source);
        drop(reader);
        closed(&pool, &source);
    }
}

#[test]
fn corrupt_selected_carriers_retire_the_feed() {
    for (index, mask, expected) in [
        (1, 0x80, "TS transport error indicator"),
        (3, 0x04, "TS continuity loss"),
    ] {
        let mut raw = BASELINE.to_vec();
        let carrier = &mut raw[26 * 188..27 * 188];
        assert_eq!(carrier[..4], [0x47, 0x01, 0x01, 0x11]);
        carrier[index] ^= mask;
        let pool = Budget::new(64 * 1024).unwrap();
        let (mut reader, source) = reader(&pool, &raw, None, 301);
        source.available.set(raw.len());
        source.completion.set(Some(Completion::End));
        loop {
            match next(&mut reader, 4096) {
                Ok(TsProgressiveStep::Unit(_) | TsProgressiveStep::NeedInput(_)) => {}
                Err(Error::Invalid(actual)) if actual == expected => break,
                other => panic!("expected {expected}, got {other:?}"),
            }
        }
        assert!(matches!(next(&mut reader, 4096), Err(Error::Closed)));
        closed(&pool, &source);
    }
}

fn drain_to_request(reader: &mut TsProgressiveReader<Body<'_>>) -> InputRequest {
    loop {
        match next(reader, 4096).unwrap() {
            TsProgressiveStep::NeedInput(request) => return request,
            TsProgressiveStep::Unit(_) => {}
            other => panic!("expected an open body, got {other:?}"),
        }
    }
}

#[test]
fn carrier_aligned_end_inside_a_finite_audio_pes_is_refused() {
    let pool = Budget::new(64 * 1024).unwrap();
    let (mut reader, source) = reader(&pool, BASELINE, None, 188);
    let prefix = BASELINE.len() - 188;
    source.available.set(prefix);
    assert_eq!(drain_to_request(&mut reader).offset as usize, prefix);
    source.completion.set(Some(Completion::End));
    loop {
        match next(&mut reader, 4096) {
            Ok(TsProgressiveStep::Unit(TsProgressiveUnit::Video(_))) => {}
            Err(Error::Truncated | Error::Invalid("HLS AAC corrupt or empty PES")) => break,
            other => panic!("expected refusal of truncated audio PES, got {other:?}"),
        }
    }
    closed(&pool, &source);
}

#[test]
fn cancellation_retires_the_body_and_old_completion_cannot_end_its_replacement() {
    let pool = Budget::new(64 * 1024).unwrap();
    let (mut first, source) = reader(&pool, BASELINE, None, 4096);
    source.available.set(BASELINE_PREFIX);
    let request = drain_to_request(&mut first);
    assert!(pool.used() > Budget::control_storage_bytes());
    let cancel = AtomicBool::new(true);
    assert!(matches!(
        first.advance(&mut WorkBudget::new(0), &cancel),
        Err(Error::Cancelled)
    ));
    closed(&pool, &source);
    drop(first);
    closed(&pool, &source);

    let (mut reopened, source) = reader(&pool, BASELINE, None, 4096);
    assert_eq!(drain_to_request(&mut reopened).offset, 0);
    let late = SourceReply::End {
        source: request.source,
        token: request.token,
    };
    assert!(matches!(
        reopened.provide(&late),
        Err(Error::StaleGeneration { .. })
    ));
    source.available.set(BASELINE.len());
    let pressure = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        next(&mut reopened, 4096),
        Ok(TsProgressiveStep::Backpressure { .. })
    ));
    assert_eq!(source.read.get(), 0);
    drop(pressure);
    assert!(matches!(
        next(&mut reopened, 4096),
        Ok(TsProgressiveStep::Unit(_))
    ));
    drop(reopened);
    closed(&pool, &source);
}

#[test]
fn pmt_requires_unique_avc_and_adts_even_if_payload_is_unchanged() {
    for stream_type in [0x11, 0x1b] {
        let mut raw = BASELINE.to_vec();
        edit_pmt(&mut raw, 2, |pmt| pmt[17] = stream_type);
        let pool = Budget::new(64 * 1024).unwrap();
        let (mut reader, source) = reader(&pool, &raw, None, 188);
        source.available.set(raw.len());
        let result = next(&mut reader, 4096);
        assert!(matches!(result, Err(Error::Unsupported(_))), "{result:?}");
        closed(&pool, &source);
    }
}

#[test]
fn active_pmt_change_retires_the_feed_after_early_delivery() {
    let mut raw = BASELINE.to_vec();
    // Change the version of the last PMT, after complete A/V has arrived.
    edit_pmt(&mut raw, 40, |pmt| pmt[5] ^= 2);
    let pool = Budget::new(64 * 1024).unwrap();
    let (mut reader, source) = reader(&pool, &raw, None, 4096);
    source.available.set(BASELINE_PREFIX);
    drain_to_request(&mut reader);
    source.available.set(raw.len());
    assert!(matches!(
        next(&mut reader, 4096),
        Err(Error::Unsupported("progressive TS changes active PMT"))
    ));
    closed(&pool, &source);
}

fn edit_pmt(raw: &mut [u8], carrier: usize, edit: impl FnOnce(&mut [u8])) {
    let start = carrier * 188 + 5;
    let pmt = &mut raw[start..start + 26];
    assert_eq!((pmt[0], pmt[12], pmt[17]), (2, 0x1b, 0x0f));
    edit(pmt);
    let crc = PsiCrc32::of(&pmt[..22]).to_be_bytes();
    pmt[22..].copy_from_slice(&crc);
}
