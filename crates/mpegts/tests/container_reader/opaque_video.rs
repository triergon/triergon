use super::{Drive, PID, Transport};
use triergon_core::Error;
use triergon_core::container::Event;
use triergon_mpegts::compat::annex_b::VideoEofPolicy;
use triergon_mpegts::{ReaderEvent, ReaderLimits, TraceEvent};

const LEADING_BYTES: Error = Error::Invalid("Annex B leading bytes");

fn report() -> ReaderLimits {
    ReaderLimits {
        video_eof_policy: VideoEofPolicy::ReportUnframed,
        ..ReaderLimits::default()
    }
}

#[test]
fn opaque_avc_and_hevc_report_one_original_run_at_eof() {
    for kind in [0x1b, 0x24] {
        let mut transport = Transport::new();
        transport.tables(&[(kind, PID, &[])], 0);
        transport.pes(PID, 0xe0, b"opaque\0", Some(90_000), None);
        transport.pes(PID, 0xe0, b"\0\x02tail", Some(93_000), None);

        let mut strict = Drive::new(&transport.bytes, ReaderLimits::default(), 23, 1);
        loop {
            match strict.next() {
                Err(error) => {
                    assert_eq!(error, LEADING_BYTES);
                    break;
                }
                Ok(Some(Event::Other(ReaderEvent::Source(_)))) => (),
                other => panic!("unexpected strict output: {other:?}"),
            }
        }

        let mut reader = Drive::new(&transport.bytes, report(), 23, 1);
        let mut video = None;
        let mut source_pes = 0;
        let mut finished = false;
        while let Some(event) = reader.next().unwrap() {
            match event {
                Event::Other(ReaderEvent::Source(source)) => match source.event() {
                    TraceEvent::Pes(_) => source_pes += 1,
                    TraceEvent::Finished => finished = true,
                    _ => (),
                },
                Event::Other(ReaderEvent::IncompleteVideo(observation)) => {
                    assert!(!finished);
                    assert_eq!(source_pes, 2);
                    assert!(video.replace(observation).is_none());
                }
                other => panic!("unexpected opaque output: {other:?}"),
            }
        }
        assert!(finished);
        reader.reader.reset().unwrap();
        drop(reader);
        let video = video.unwrap();
        assert_eq!(video.bytes().as_ref(), b"opaque\0\0\x02tail");
        assert_eq!(video.elementary_range(), 0..13);
        assert_eq!(video.track_info().stream().stream_type, kind);
        assert_eq!(video.sources().len(), 2);
        assert_eq!(video.sources()[0].elementary_range, 0..7);
        assert_eq!(video.sources()[1].elementary_range, 7..13);
        assert_eq!(video.clock().unwrap().pts.ticks90, Some(90_000));
        assert_eq!(video.clock().unwrap().dts.ticks90, None);
        for (origin, pts) in video.sources().iter().zip([90_000, 93_000]) {
            let TraceEvent::Pes(pes) = origin.pes.event() else {
                panic!("missing original PES");
            };
            assert_eq!(pes.header.pts_raw33, Some(pts));
            assert_eq!(pes.epochs, video.epochs());
            assert_eq!(origin.payload_range, 0..pes.payload_len);
        }
    }
}

#[test]
fn a_later_start_code_split_across_pes_refuses_the_opaque_run() {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    transport.pes(PID, 0xe0, b"opaque\0", None, None);
    transport.pes(PID, 0xe0, b"\0\x01\x65\x88", None, None);
    let mut reader = Drive::new(&transport.bytes, report(), 7, 1);
    loop {
        match reader.next() {
            Err(error) => {
                assert_eq!(error, LEADING_BYTES);
                break;
            }
            Ok(Some(Event::Other(ReaderEvent::Source(_)))) => (),
            other => panic!("unexpected opaque output: {other:?}"),
        }
    }
}

#[test]
fn opaque_run_extent_has_a_permanent_limit() {
    let mut transport = Transport::new();
    transport.tables(&[(0x1b, PID, &[])], 0);
    transport.pes(PID, 0xe0, b"abcd", None, None);
    transport.pes(PID, 0xe0, b"e", None, None);
    let mut limits = report();
    limits.video.maximum_access_unit_bytes = 4;
    let mut reader = Drive::new(&transport.bytes, limits, 23, 1);
    loop {
        match reader.next() {
            Err(error) => {
                assert_eq!(
                    error,
                    Error::Limit {
                        requested: 5,
                        maximum: 4
                    }
                );
                break;
            }
            Ok(Some(Event::Other(ReaderEvent::Source(_)))) => (),
            other => panic!("unexpected opaque output: {other:?}"),
        }
    }
}
