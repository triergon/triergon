#![forbid(unsafe_code)]
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_subtitles::compat::document::Syntax;
use triergon_subtitles::reference_trace::carriage::{
    CallerClock, Discovery, DocumentInput, Limits, Operation, ProgressKind, Session, Source, Status,
};
use triergon_subtitles::text::{Encoding, Format, Input, Timing};

struct Case {
    id: &'static str,
    payload: &'static [u8],
    unicode: &'static [u8],
    file_name: &'static [u8],
    packet: bool,
    expected: &'static [u8],
    codec_id: i32,
    codec_name: &'static str,
    position_raw: i64,
    flags: i32,
    pts_raw: i64,
    dts_raw: i64,
    duration_raw: i64,
    packet_time_base: [i32; 2],
    stream_time_base: [i32; 2],
}
#[path = "text-carriage-fixtures/native40/cases.rs"]
mod cases;

fn run_case(index: usize) {
    let case = &cases::ALL[index];
    for quantum in [1, 7, 4096] {
        let pool = Budget::new(1 << 20).unwrap();
        let baseline = pool.used();
        let payload = Bytes::copy_from_slice(&pool, case.payload).unwrap();
        let input = if case.packet {
            Source::Sample(Input {
                format: Format::Utf8Packet,
                encoding: Encoding::Utf8,
                payload,
                codec_id: None,
                codec_private: None,
                charset_label: None,
                addition: None,
                timing: Timing::unknown(),
                source_id: SourceId(41),
                generation: 7,
                sequence: 53,
                configuration_epoch: 11,
                container_position: None,
            })
        } else {
            Source::Document(DocumentInput {
                syntax: Syntax::SubRip,
                payload,
                source_id: SourceId(41),
                generation: 7,
                sequence: 53,
                configuration_epoch: 11,
            })
        };
        let original = input.clone();
        let mut offered = Some(input);
        let mut session = Session::new(&pool, Limits::default());
        let operation = if case.packet {
            Operation::Sample(CallerClock {
                numerator: 1,
                denominator: 1000,
                pts: None,
                dts: None,
                duration: None,
            })
        } else {
            Operation::Document {
                discovery: Discovery::Header,
                file_name: Bytes::copy_from_slice(&pool, case.file_name).unwrap(),
            }
        };
        session.provide(&mut offered, operation).unwrap();
        session.finish();
        let cancel = AtomicBool::new(false);
        let mut result = None;
        for _ in 0..case.payload.len() * 12 + 4096 {
            match session
                .poll(&mut WorkBudget::new(quantum), &cancel)
                .unwrap()
            {
                Status::Yield => {}
                Status::Ready => {
                    assert!(result.is_none());
                    result = session.take_output();
                }
                Status::End => break,
                other => panic!("{} {other:?}", case.id),
            }
        }
        let result = result.expect("bounded complete source must publish");
        let facts = |source: &Source| match source {
            Source::Sample(input) => (
                input.payload.clone(),
                input.source_id,
                input.generation,
                input.sequence,
                input.configuration_epoch,
            ),
            Source::Document(document) => (
                document.payload.clone(),
                document.source_id,
                document.generation,
                document.sequence,
                document.configuration_epoch,
            ),
        };
        let (payload, source_id, generation, sequence, epoch) = facts(&result.input);
        assert_eq!(payload.as_ref(), case.payload);
        assert!(payload.shares_storage_with(&facts(&original).0));
        if let (Source::Sample(actual), Source::Sample(offered)) = (&result.input, &original) {
            assert_eq!(actual.timing, offered.timing);
        }
        assert_eq!(source_id, SourceId(41));
        assert_eq!(generation, 7);
        assert_eq!(sequence, 53);
        assert_eq!(epoch, 11);
        assert!(result.error_code.is_none());
        let stream = result.stream.as_ref().unwrap();
        assert_eq!(stream.index, 0);
        assert_eq!(stream.codec_type, 3);
        assert_eq!(stream.codec_id, case.codec_id);
        assert_eq!(stream.codec_name, case.codec_name);
        assert_eq!(stream.codec_tag, 0);
        assert_eq!(stream.time_base, case.stream_time_base);
        assert_eq!(stream.start_raw, i64::MIN);
        assert_eq!(stream.duration_raw, i64::MIN);
        assert_eq!(stream.disposition, 0);
        assert!(stream.extradata.is_none());
        assert!(stream.coded_side_data.is_empty());
        assert!(stream.metadata.is_empty());
        assert_eq!(result.packets.len(), 1);
        let packet = &result.packets.as_slice()[0];
        assert_eq!(packet.ordinal, 0);
        assert_eq!(packet.stream, 0);
        assert_eq!(packet.data.as_ref(), case.expected, "{}", case.id);
        assert_eq!(packet.position_raw, case.position_raw);
        assert_eq!(packet.flags, case.flags);
        assert!(!packet.buffer_is_null);
        assert_eq!(packet.pts_raw, case.pts_raw);
        assert_eq!(packet.dts_raw, case.dts_raw);
        assert_eq!(packet.duration_raw, case.duration_raw);
        assert_eq!(packet.packet_time_base, case.packet_time_base);
        assert_eq!(packet.stream_time_base, case.stream_time_base);
        assert!(packet.side_data.is_empty());
        if case.packet {
            assert!(result.format.is_none());
            assert!(result.progress.is_empty());
            assert_eq!(packet.data.as_ref(), case.unicode);
        } else {
            let format = result.format.unwrap();
            assert_eq!(format.name, "srt");
            assert_eq!(format.probe_size, 5_000_000);
            assert_eq!(format.max_analyze_duration, 0);
            let progress = result.progress.as_slice();
            assert_eq!(progress.len(), 4);
            assert_eq!(
                (progress[0].kind, progress[0].ordinal, progress[0].returned),
                (ProgressKind::FormatOpen, None, Some(0))
            );
            assert_eq!(
                (progress[1].kind, progress[1].ordinal, progress[1].returned),
                (ProgressKind::Discovery, None, None)
            );
            assert_eq!(
                (progress[2].kind, progress[2].ordinal, progress[2].returned),
                (ProgressKind::ReadFrame, Some(0), Some(0))
            );
            assert_eq!(
                (progress[3].kind, progress[3].ordinal, progress[3].returned),
                (ProgressKind::ReadFrame, Some(1), Some(-541478725))
            );
        }
        // Native byte behavior never changes the independent source oracle.
        assert!(std::str::from_utf8(case.unicode).is_ok());
        drop(result);
        drop(session);
        drop(original);
        drop(offered);
        assert_eq!(pool.used(), baseline);
    }
}
include!("text-carriage-fixtures/native40/names.rs");
