use std::{cell::Cell, rc::Rc, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, WorkBudget,
};
use triergon_playback::{
    ByteSource, Source, SourceReply,
    hls_segment::{
        HlsFeedUnit, HlsSegmentAudio, HlsSegmentLimits, HlsSegmentProgress, HlsSegmentReader,
        HlsSegmentTracks,
    },
};

const RAW: &[u8] = include_bytes!("fixtures/hls/bframes-multislice.ts");

#[derive(Default)]
struct Lifecycle {
    aborts: Cell<usize>,
    closes: Cell<usize>,
    starved: Cell<bool>,
}

struct ObservedSource {
    bytes: ByteSource,
    trace: Rc<Lifecycle>,
    fail_at: Option<u64>,
}

impl Source for ObservedSource {
    fn poll(
        &mut self,
        request: &InputRequest,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> triergon_core::Result<SourceReply> {
        if !self.trace.starved.get() && work.try_charge(1) {
            self.trace.starved.set(true);
            return Ok(SourceReply::Pending);
        }
        if self.fail_at.is_some_and(|offset| request.offset >= offset) && work.try_charge(1) {
            return Ok(SourceReply::Failed {
                source: request.source,
                token: request.token.clone(),
                failure: SourceFailure::Transport,
            });
        }
        self.bytes.poll(request, work, cancel)
    }

    fn invalidate(&mut self) {
        self.bytes.invalidate();
    }

    fn abort(&mut self) {
        self.trace.aborts.set(self.trace.aborts.get() + 1);
        self.bytes.abort();
    }

    fn close(&mut self) {
        self.trace.closes.set(self.trace.closes.get() + 1);
        self.bytes.close();
    }
}

fn input(
    pool: &Budget,
    id: SourceId,
    raw: &[u8],
    length: Option<u64>,
    fail_at: Option<u64>,
) -> (InputCursor, ObservedSource, Rc<Lifecycle>) {
    let trace = Rc::new(Lifecycle::default());
    let bytes = ByteSource::new(id, Bytes::copy_from_slice(pool, raw).unwrap(), 188).unwrap();
    let source = ObservedSource {
        bytes,
        trace: trace.clone(),
        fail_at,
    };
    let input = InputCursor::new(
        pool,
        id,
        SourceCapabilities {
            length,
            random_access: false,
        },
        4096,
    )
    .unwrap();
    (input, source, trace)
}

fn poll(
    reader: &mut HlsSegmentReader<ObservedSource>,
) -> triergon_playback::Result<HlsSegmentProgress> {
    reader.advance(&mut WorkBudget::new(4096), &AtomicBool::new(false))
}

fn request(reader: &mut HlsSegmentReader<ObservedSource>) -> InputRequest {
    for _ in 0..1000 {
        match poll(reader).unwrap() {
            HlsSegmentProgress::NeedInput(request) => return request,
            HlsSegmentProgress::Yield => {}
            other => panic!("expected source request, got {other:?}"),
        }
    }
    panic!("missing source request");
}

fn ready(reader: &mut HlsSegmentReader<ObservedSource>) -> triergon_playback::Result<()> {
    for _ in 0..30_000 {
        match poll(reader)? {
            HlsSegmentProgress::Ready => return Ok(()),
            HlsSegmentProgress::Yield | HlsSegmentProgress::NeedInput(_) => {}
            other => panic!("unexpected segment progress {other:?}"),
        }
    }
    panic!("segment stalled");
}

fn assert_closed(trace: &Lifecycle) {
    assert_eq!(trace.aborts.get(), 1);
    assert_eq!(trace.closes.get(), 1);
}

#[test]
fn transport_failure_discards_partial_feed_and_retry_rejects_the_old_reply() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let limits = HlsSegmentLimits::default();
    let length = Some(RAW.len() as u64);
    let cancel = AtomicBool::new(false);
    let (cursor, source, failed_trace) = input(&pool, SourceId(41), RAW, length, Some(5000));
    let mut first =
        HlsSegmentReader::new(cursor, source, HlsSegmentTracks::default(), limits).unwrap();
    let original = request(&mut first);
    let late = SourceReply::Data(
        InputWindow::new(
            original.source,
            original.token,
            original.offset,
            Bytes::copy_from_slice(&pool, &RAW[..1]).unwrap(),
        )
        .unwrap(),
    );
    assert!(first.take(&cancel).unwrap().is_none());
    assert!(matches!(ready(&mut first), Err(Error::Transport)));
    assert!(failed_trace.starved.get());
    assert_closed(&failed_trace);
    assert!(matches!(first.take(&cancel), Err(Error::Closed)));
    drop(first);
    assert_closed(&failed_trace);

    let (cursor, source, retry_trace) = input(&pool, SourceId(42), RAW, length, None);
    let mut retry =
        HlsSegmentReader::new(cursor, source, HlsSegmentTracks::default(), limits).unwrap();
    request(&mut retry);
    let stale = retry.provide(&late);
    assert!(
        matches!(stale, Err(Error::StaleGeneration { .. })),
        "{stale:?}"
    );
    drop(late);
    assert!(retry.take(&cancel).unwrap().is_none());
    ready(&mut retry).unwrap();
    assert_closed(&retry_trace);
    assert_eq!(
        retry.take(&cancel).unwrap().unwrap().access_units().len(),
        47
    );
    assert!(retry.take(&cancel).unwrap().is_none());
    drop(retry);
    assert_closed(&retry_trace);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn partial_entity_and_source_feed_limits_cannot_publish_a_segment() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let default = HlsSegmentLimits::default();
    for (data, length, limits, expected) in [
        (RAW, Some(RAW.len() as u64 + 188), default, Error::Truncated),
        (&RAW[..RAW.len() - 1], None, default, Error::Truncated),
        (
            RAW,
            None,
            HlsSegmentLimits {
                maximum_source_bytes: RAW.len() - 1,
                ..default
            },
            Error::Limit {
                requested: RAW.len(),
                maximum: RAW.len() - 1,
            },
        ),
        (
            RAW,
            None,
            HlsSegmentLimits {
                maximum_access_units: 4,
                ..default
            },
            Error::Limit {
                requested: 5,
                maximum: 4,
            },
        ),
        (
            RAW,
            None,
            HlsSegmentLimits {
                maximum_feed_bytes: 1000,
                ..default
            },
            Error::Limit {
                requested: 1916,
                maximum: 1000,
            },
        ),
    ] {
        let (cursor, source, trace) = input(&pool, SourceId(43), data, length, None);
        let mut reader =
            HlsSegmentReader::new(cursor, source, HlsSegmentTracks::default(), limits).unwrap();
        assert!(matches!(ready(&mut reader), Err(actual) if actual == expected));
        assert!(matches!(
            reader.take(&AtomicBool::new(false)),
            Err(Error::Closed)
        ));
        assert_closed(&trace);
    }
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn adjacent_segments_keep_distinct_clocks_and_the_previous_feed_owner() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let cancel = AtomicBool::new(false);
    let mut previous = None;
    for (id, raw, expected_count, expected_pts) in [
        (
            SourceId(51),
            include_bytes!("fixtures/hls/media-00.ts").as_slice(),
            145,
            132006,
        ),
        (
            SourceId(52),
            include_bytes!("fixtures/hls/media-01.ts").as_slice(),
            149,
            312186,
        ),
    ] {
        let (cursor, source, trace) = input(&pool, id, raw, None, None);
        let limits = HlsSegmentLimits {
            maximum_source_bytes: raw.len(),
            ..HlsSegmentLimits::default()
        };
        let mut reader =
            HlsSegmentReader::new(cursor, source, HlsSegmentTracks::default(), limits).unwrap();
        ready(&mut reader).unwrap();
        assert_closed(&trace);
        let output = reader.take(&cancel).unwrap().unwrap();
        assert_eq!(output.access_units().len(), expected_count);
        let Some(HlsFeedUnit::Video(first)) = output.access_units().next() else {
            panic!("missing first video access unit");
        };
        assert!(first.idr);
        assert_eq!(first.clock.unwrap().pts90, Some(expected_pts));
        if let Some(previous) = previous.replace(output) {
            assert_eq!(previous.access_units().len(), 145);
            let Some(HlsFeedUnit::Video(first)) = previous.access_units().next() else {
                panic!("missing retained video access unit");
            };
            assert_eq!(first.clock.unwrap().pts90, Some(132006));
        }
    }
    drop(previous);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn cancellation_and_pool_pressure_keep_segment_publication_atomic() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let (cursor, source, trace) = input(&pool, SourceId(44), RAW, None, None);
    let mut reader = HlsSegmentReader::new(
        cursor,
        source,
        HlsSegmentTracks::default(),
        HlsSegmentLimits::default(),
    )
    .unwrap();
    request(&mut reader);
    assert!(matches!(
        reader.advance(&mut WorkBudget::new(1), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_closed(&trace);
    drop(reader);
    assert_eq!(pool.used(), Budget::control_storage_bytes());

    let (cursor, source, trace) = input(&pool, SourceId(45), RAW, None, None);
    let mut reader = HlsSegmentReader::new(
        cursor,
        source,
        HlsSegmentTracks::default(),
        HlsSegmentLimits::default(),
    )
    .unwrap();
    ready(&mut reader).unwrap();
    assert_closed(&trace);
    let reserve = pool.reserve(pool.available()).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        reader.take(&cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(reserve);
    assert_eq!(
        reader.take(&cancel).unwrap().unwrap().access_units().len(),
        47
    );
    drop(reader);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn parser_only_output_uses_the_queue_clock_and_preserves_the_pes_facts() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    let (cursor, source, trace) = input(&pool, SourceId(55), RAW, None, None);
    let mut reader = HlsSegmentReader::new(
        cursor,
        source,
        HlsSegmentTracks::default(),
        HlsSegmentLimits::default(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    assert!(reader.take_parser_only(&cancel).unwrap().is_none());
    ready(&mut reader).unwrap();
    assert_closed(&trace);

    let reserve = pool.reserve(pool.available()).unwrap();
    assert!(matches!(
        reader.take_parser_only(&cancel),
        Err(Error::Backpressure { .. })
    ));
    drop(reserve);
    let output = reader.take_parser_only(&cancel).unwrap().unwrap();
    assert!(reader.take(&cancel).unwrap().is_none());
    let mut audio = output.access_units().filter_map(|unit| match unit {
        HlsFeedUnit::Audio(frame, clock) => Some((frame, clock)),
        HlsFeedUnit::Video(_) => None,
    });
    let (first, clock) = audio.next().unwrap();
    assert!(first.pts.is_some());
    assert_eq!(clock.pts_ns, 1_458_666_667);
    assert_eq!(clock.duration_ns, 21_333_333);
    let (second, clock) = audio.next().unwrap();
    assert!(second.pts.is_none());
    assert_eq!(clock.pts_ns, 1_480_000_000);
    assert_eq!(audio.count(), 29);
    assert_eq!(output.audio_tail_ns(), Some(2_098_666_662));
    drop(output);
    drop(reader);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn explicit_aac_pid_and_disabled_audio_apply_before_delivery() {
    let pool = Budget::new(8 * 1024 * 1024).unwrap();
    for (selection, expected) in [
        (HlsSegmentAudio::Pid(257), Some(31)),
        (HlsSegmentAudio::Pid(999), None),
        (HlsSegmentAudio::Disabled, Some(0)),
    ] {
        let (cursor, source, trace) = input(&pool, SourceId(46), RAW, None, None);
        let tracks = HlsSegmentTracks {
            video_pid: Some(256),
            audio: selection,
        };
        let mut reader =
            HlsSegmentReader::new(cursor, source, tracks, HlsSegmentLimits::default()).unwrap();
        if let Some(count) = expected {
            ready(&mut reader).unwrap();
            let output = reader.take(&AtomicBool::new(false)).unwrap().unwrap();
            assert_eq!(
                output
                    .access_units()
                    .filter(|unit| matches!(unit, HlsFeedUnit::Audio(_, _)))
                    .count(),
                count
            );
        } else {
            assert!(matches!(
                ready(&mut reader),
                Err(Error::Unsupported("HLS segment has no requested AAC PID"))
            ));
        }
        assert_closed(&trace);
    }
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
