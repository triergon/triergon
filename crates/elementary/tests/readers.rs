//! Every reader over authored streams, for any window size, work and length knowledge.
mod common;
use common::*;
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::container::{
    CodecId, ContainerReader, SeekMethod, SeekPolicy, SeekRequest, TrackInfo,
};
use triergon_core::{Budget, Error, Step, Time, WorkBudget};
use triergon_elementary::{
    Ac3, Adts, BlockingReader, Flac, FlacBlockKind, Format, Limits, MpegAudio, Reader, SeekBasis,
    TagKind,
};

/// The same packets whatever the window size and step work, for a known and for an
/// unknown source length (which may differ: FFmpeg withholds an MPEG audio end trim it
/// cannot check). Returns the known-length reading.
fn invariant<F: Format>(file: &[u8]) -> Reading {
    let known_length = read_file::<F>(file, 1 << 30, usize::MAX, true).unwrap();
    let unknown_length = read_file::<F>(file, 1 << 30, usize::MAX, false).unwrap();
    for (work, most, known) in [
        (1 << 30, 1, true),
        (64, 3, false),
        (5000, 7, true),
        (1 << 30, 4096, false),
        (100, 65536, true),
    ] {
        let reading = read_file::<F>(file, work, most, known).unwrap();
        let reference = if known {
            &known_length
        } else {
            &unknown_length
        };
        let first = reading
            .packets
            .iter()
            .zip(&reference.packets)
            .position(|(a, b)| a != b);
        assert_eq!(
            (
                reading.packets.len(),
                first.map(|i| (&reading.packets[i], &reference.packets[i]))
            ),
            (reference.packets.len(), None),
            "work {work}, window {most}, known {known}"
        );
        assert_eq!(reading.opened, 1);
    }
    known_length
}

fn mp3_stream(frames: u32) -> Vec<u8> {
    (0..frames).flat_map(|i| mp3_frame(i, i % 3 == 1)).collect()
}

#[test]
fn mpeg_audio_frames_follow_their_headers_after_tags() {
    let mut file = id3v2(30);
    file.extend(mp3_stream(20));
    file.extend(id3v1());
    let reading = invariant::<MpegAudio>(&file);
    assert_eq!(reading.packets.len(), 20);
    assert_eq!(reading.packets[0].offset, 40);
    for (i, p) in reading.packets.iter().enumerate() {
        assert_eq!((p.pts, p.duration), (i as i64 * 1152, 1152));
        assert!(p.complete && p.start_trim.is_none());
    }
    let end = reading
        .packets
        .last()
        .map(|p| p.offset as usize + p.len)
        .unwrap();
    assert_eq!(end, file.len() - 128, "ID3v1 is not audio");

    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<MpegAudio>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    let kinds: Vec<_> = reader.tags().iter().map(|t| (t.kind, t.len)).collect();
    assert_eq!(kinds, [(TagKind::Id3v2, 40), (TagKind::Id3v1, 128)]);
    let track = reader.stream().unwrap();
    assert_eq!(
        (track.codec(), track.codec_tag()),
        (CodecId::MpegAudio, b"mp3".as_slice())
    );
    assert_eq!((track.sample_rate(), track.channels()), (44100, Some(2)));
    assert!(track.configuration().is_empty());
}

#[test]
fn mpeg_audio_junk_joins_the_next_packet_and_a_short_tail_is_incomplete() {
    let frames: Vec<Vec<u8>> = (0..12).map(|i| mp3_frame(i, false)).collect();
    let mut file = noise(1, 700); // leading junk is dropped
    for (i, frame) in frames.iter().enumerate() {
        if i == 6 {
            file.extend(noise(2, 333).into_iter().map(|b| b & 0x7f));
        }
        file.extend(frame);
    }
    file.extend(&mp3_frame(99, false)[..100]);
    let reading = invariant::<MpegAudio>(&file);
    let lens: Vec<_> = reading.packets.iter().map(|p| p.len).collect();
    let mut expected = vec![417; 13];
    expected[6] += 333;
    expected[12] = 100;
    assert_eq!(lens, expected);
    assert!(!reading.packets[12].complete);
    assert_eq!(reading.packets[12].pts, 12 * 1152);
}

#[test]
fn a_lame_info_frame_is_not_audio_and_trims_like_ffmpeg() {
    let frames = 10;
    let audio = mp3_stream(frames);
    let mut file = mp3_info(frames, (audio.len() + 417) as u32, 576, 1260);
    file.extend(&audio);
    let reading = invariant::<MpegAudio>(&file);
    assert_eq!(reading.packets.len(), 10);
    assert_eq!(reading.packets[0].offset, 417);
    assert_eq!(reading.packets[0].start_trim, Some(576 + 529));
    assert_eq!(reading.packets[9].end_trim, Some(1260 - 529));
    assert!(
        reading.packets[1..9]
            .iter()
            .all(|p| p.end_trim.is_none() && p.start_trim.is_none())
    );

    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<MpegAudio>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    assert_eq!(
        reader.stream().unwrap().duration(),
        Some(10 * 1152 - 576 - 1260)
    );
    let info = reader.format().info_frame().unwrap();
    assert!(info.consistent);

    // A source of unknown length, such as a pipe, keeps the start trim only.
    let piped = read_file::<MpegAudio>(&file, 1 << 30, 4096, false).unwrap();
    assert_eq!(piped.packets[0].start_trim, Some(1105));
    assert!(piped.packets.iter().all(|p| p.end_trim.is_none()));

    // So does a source far longer than the frame states.
    let mut longer = file.clone();
    longer.extend(noise(5, audio.len()).into_iter().map(|b| b & 0x7f));
    let reading = read_file::<MpegAudio>(&longer, 1 << 30, 4096, true).unwrap();
    assert_eq!(reading.packets[0].start_trim, Some(1105));
    assert!(reading.packets.iter().all(|p| p.end_trim.is_none()));
}

#[test]
fn flac_frames_end_at_the_next_header_their_crc_confirms() {
    let mut file = id3v2(10);
    let head = flac_head(10 * 4096, &[(0, 0)]);
    file.extend(&head);
    let start = file.len();
    let frames: Vec<Vec<u8>> = (0..10)
        .map(|i| flac_frame(i, 300 + 37 * i as usize))
        .collect();
    for frame in &frames {
        file.extend(frame);
    }
    let reading = invariant::<Flac>(&file);
    let mut offset = start as u64;
    for (i, (p, frame)) in reading.packets.iter().zip(&frames).enumerate() {
        assert_eq!((p.offset, p.len), (offset, frame.len()));
        assert_eq!((p.pts, p.duration), (i as i64 * 4096, 4096));
        offset += frame.len() as u64;
    }
    assert_eq!(reading.packets.len(), 10);

    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<Flac>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    let track = reader.stream().unwrap();
    assert_eq!(track.codec(), CodecId::Flac);
    let config = track.configuration();
    assert_eq!(&config[..8], b"fLaC\x80\x00\x00\x22");
    assert_eq!(&config[8..], &head[8..42], "STREAMINFO as FFmpeg keeps it");
    assert_eq!(track.duration(), Some(10 * 4096));
    let kinds: Vec<_> = reader
        .format()
        .metadata()
        .iter()
        .map(|m| (m.kind, m.body.is_some()))
        .collect();
    assert_eq!(
        kinds,
        [
            (FlacBlockKind::StreamInfo, true),
            (FlacBlockKind::SeekTable, true),
            (FlacBlockKind::Padding, false)
        ]
    );
    assert_eq!(reader.format().seek_points().len(), 1);
}

#[test]
fn a_damaged_flac_frame_ends_at_the_header_that_numbers_on() {
    let frames: Vec<Vec<u8>> = (0..6).map(|i| flac_frame(i, 500)).collect();
    let mut file = flac_head(6 * 4096, &[]);
    for (i, frame) in frames.iter().enumerate() {
        let mut frame = frame.clone();
        if i == 2 {
            frame[200] ^= 0x10;
        }
        file.extend(frame);
    }
    let reading = invariant::<Flac>(&file);
    let lens: Vec<_> = reading.packets.iter().map(|p| p.len).collect();
    assert_eq!(lens, vec![frames[0].len(); 6]);
}

#[test]
fn adts_frames_drop_junk_and_keep_a_truncated_tail() {
    let mut file = id3v2(4);
    for i in 0..8 {
        if i == 4 {
            file.extend([0x12; 50]);
        }
        file.extend(adts_frame(i, 200 + i as usize));
    }
    file.extend(&adts_frame(9, 300)[..120]);
    let reading = invariant::<Adts>(&file);
    let lens: Vec<_> = reading.packets.iter().map(|p| p.len).collect();
    assert_eq!(lens, [200, 201, 202, 203, 204, 205, 206, 207, 120]);
    assert_eq!(reading.packets[4].offset, 14 + 200 + 201 + 202 + 203 + 50);
    assert_eq!(reading.packets[8].pts, 8 * 1024);
}

#[test]
fn ac3_junk_joins_the_packet_before_it() {
    let mut file = Vec::new();
    for i in 0..6 {
        file.extend(ac3_frame(i));
        if i == 2 {
            file.extend([0x55; 77]);
        }
    }
    let reading = invariant::<Ac3>(&file);
    let lens: Vec<_> = reading.packets.iter().map(|p| p.len).collect();
    assert_eq!(lens, [512, 512, 512 + 77, 512, 512, 512]);
    assert_eq!(reading.packets[5].pts, 5 * 1536);

    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<Ac3>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    let track = reader.stream().unwrap();
    assert_eq!(
        (track.codec(), track.codec_tag()),
        (CodecId::Ac3, b"ac-3".as_slice())
    );
    assert_eq!(track.sample_rate(), 48000);
}

fn seek<F: Format>(
    file: &[u8],
    seconds: (i128, u128),
    policy: SeekPolicy,
) -> (Reading, triergon_elementary::SeekDetail) {
    let pool = Budget::new(64 << 20).unwrap();
    let mut reader = open::<F>(&pool, file, true);
    let cancel = AtomicBool::new(false);
    while reader.stream().is_none() {
        match reader.step(&mut WorkBudget::new(1 << 20), &cancel).unwrap() {
            Step::NeedInput(request) => supply(&mut reader, &pool, file, &request, 4096),
            Step::End(_) => panic!("no stream"),
            _ => {}
        }
    }
    let request = reader
        .request_seek(SeekRequest {
            track: triergon_elementary::TRACK,
            target: Time::new(seconds.0, seconds.1).unwrap(),
            policy,
        })
        .unwrap();
    supply(&mut reader, &pool, file, &request, 4096);
    let reading = read(&mut reader, &pool, file, 1 << 20, 4096).unwrap();
    (reading, reader.last_seek().unwrap())
}

#[test]
fn a_flac_seek_lands_on_the_frame_holding_the_target() {
    let frames: Vec<Vec<u8>> = (0..400)
        .map(|i| flac_frame(i, 900 + (i as usize * 7) % 300))
        .collect();
    let offsets: Vec<u64> = frames
        .iter()
        .scan(0u64, |at, f| {
            let here = *at;
            *at += f.len() as u64;
            Some(here)
        })
        .collect();
    for table in [
        vec![],
        vec![
            (0, 0),
            (100 * 4096, offsets[100]),
            (300 * 4096, offsets[300]),
        ],
    ] {
        let mut file = flac_head(400 * 4096, &table);
        file.extend(frames.concat());
        // 10 s at 44.1 kHz is sample 441000, inside frame 107.
        let (reading, detail) = seek::<Flac>(&file, (10, 1), SeekPolicy::PreviousRandomAccess);
        assert_eq!(reading.receipts.len(), 1);
        assert_eq!(reading.packets[0].pts, 107 * 4096);
        assert_eq!(
            reading.receipts[0].landing,
            Time::new(107 * 4096, 44100).unwrap()
        );
        assert_eq!(detail.basis, SeekBasis::Exact);
        let method = if table.is_empty() {
            SeekMethod::BoundedScan
        } else {
            SeekMethod::Index
        };
        assert_eq!(detail.receipt.method, method);
        assert_eq!(reading.packets.len(), 400 - 107);
    }
}

#[test]
fn an_mpeg_audio_seek_says_what_its_landing_rests_on() {
    let audio = mp3_stream(200);
    let (reading, detail) = seek::<MpegAudio>(&audio, (2, 1), SeekPolicy::PlaybackPreroll);
    assert_eq!(detail.basis, SeekBasis::Bitrate);
    let first = &reading.packets[0];
    assert_eq!(first.pts % 1152, 0);
    // Aimed three frames early (preroll and one frame of estimate error).
    assert!(
        (88200 - 4 * 1152..=88200).contains(&first.pts),
        "{}",
        first.pts
    );
    assert_eq!(detail.receipt.discard_until, Time::new(2, 1).unwrap());

    let mut file = mp3_info(200, (audio.len() + 417) as u32, 576, 1260);
    file.extend(&audio);
    let (reading, detail) = seek::<MpegAudio>(&file, (2, 1), SeekPolicy::PreviousRandomAccess);
    assert_eq!(detail.basis, SeekBasis::Toc);
    assert!((reading.packets[0].pts - 88200).abs() <= 2 * 1152);
}

#[test]
fn an_adts_seek_counts_frames_from_the_start() {
    let file: Vec<u8> = (0..100)
        .flat_map(|i| adts_frame(i, 150 + i as usize % 40))
        .collect();
    let (reading, detail) = seek::<Adts>(&file, (1, 1), SeekPolicy::PlaybackPreroll);
    // Sample 44100 lies in frame 43; one frame of preroll comes before it.
    assert_eq!(reading.packets[0].pts, 42 * 1024);
    assert_eq!(detail.receipt.method, SeekMethod::Restart);
    assert_eq!(detail.basis, SeekBasis::Exact);
}

#[test]
fn malformed_input_is_an_error_or_a_short_stream_never_a_panic() {
    let mut cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        noise(3, 5000),
        vec![0xff; 3000],
        b"fLaC".to_vec(),
    ];
    let flac: Vec<u8> = [
        flac_head(0, &[(0, 0)]),
        flac_frame(0, 300),
        flac_frame(1, 300),
    ]
    .concat();
    let mp3 = mp3_stream(5);
    for cut in (0..flac.len()).step_by(37) {
        cases.push(flac[..cut].to_vec());
    }
    for cut in (0..mp3.len()).step_by(41) {
        cases.push(mp3[..cut].to_vec());
    }
    for (i, case) in cases.iter().enumerate() {
        let mut flipped = case.clone();
        if let Some(byte) = flipped.get_mut(i % case.len().max(1)) {
            *byte ^= 0x5a;
        }
        for file in [case, &flipped] {
            let _ = read_file::<Flac>(file, 97, 13, i % 2 == 0);
            let _ = read_file::<MpegAudio>(file, 97, 13, i % 2 == 0);
            let _ = read_file::<Adts>(file, 97, 13, i % 2 == 0);
            let _ = read_file::<Ac3>(file, 97, 13, i % 2 == 0);
        }
    }
}

#[test]
fn a_source_without_frames_is_an_error() {
    let error = read_file::<MpegAudio>(&noise(9, 2000), 1 << 20, 4096, true).unwrap_err();
    assert_eq!(error, Error::Invalid("no frame in the source"));
}

#[test]
fn a_small_pool_is_a_typed_error() {
    let file: Vec<u8> = [
        flac_head(0, &[]),
        flac_frame(0, 20000),
        flac_frame(1, 20000),
    ]
    .concat();
    let pool = Budget::new(8 << 10).unwrap();
    let mut reader = open::<Flac>(&pool, &file, false);
    let error = read(&mut reader, &pool, &file, 1 << 20, 1024).unwrap_err();
    assert!(matches!(error, Error::Limit { .. }), "{error:?}");
}

#[test]
fn cancellation_is_sticky_until_reset() {
    let file = mp3_stream(4);
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<MpegAudio>(&pool, &file, true);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(1 << 20), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    cancel.store(false, Ordering::Relaxed);
    assert_eq!(
        reader
            .step(&mut WorkBudget::new(1 << 20), &cancel)
            .unwrap_err(),
        Error::Cancelled
    );
    reader.reset().unwrap();
    assert_eq!(
        read(&mut reader, &pool, &file, 1 << 20, 4096)
            .unwrap()
            .packets
            .len(),
        4
    );
}

#[test]
fn the_blocking_reader_reads_and_seeks() {
    let file: Vec<u8> = (0..60).flat_map(ac3_frame).collect();
    let mut reader =
        BlockingReader::<Ac3, _>::open(std::io::Cursor::new(file), Limits::default()).unwrap();
    let mut count = 0;
    while reader.next_packet().unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 60);
    let receipt = reader
        .seek(SeekRequest {
            track: triergon_elementary::TRACK,
            target: Time::new(1, 2).unwrap(),
            policy: SeekPolicy::PreviousRandomAccess,
        })
        .unwrap();
    // Sample 24000 is in frame 15 of 1536 samples.
    assert_eq!(receipt.landing, Time::new(15 * 1536, 48000).unwrap());
    let next = reader.next_packet().unwrap().unwrap();
    assert_eq!(
        triergon_core::container::PacketInfo::pts(&next),
        Some(15 * 1536)
    );
}

#[test]
fn readers_are_concrete_container_readers() {
    fn tracks<R: ContainerReader>(reader: &R) -> usize {
        reader.track_count()
    }
    let pool = Budget::new(1 << 20).unwrap();
    let reader = Reader::<Adts>::new(
        &pool,
        triergon_core::SourceId(1),
        capabilities(&[], false),
        Limits::default(),
    )
    .unwrap();
    assert_eq!(tracks(&reader), 0);
}

#[test]
fn an_eac3_access_unit_holds_its_dependent_substream() {
    let file: Vec<u8> = (0..5)
        .flat_map(|i| [eac3_frame(2 * i, false), eac3_frame(2 * i + 1, true)].concat())
        .collect();
    let reading = invariant::<Ac3>(&file);
    let units: Vec<_> = reading
        .packets
        .iter()
        .map(|p| (p.offset, p.len, p.pts))
        .collect();
    let expected: Vec<_> = (0..5).map(|i| (i * 512, 512, i as i64 * 1536)).collect();
    assert_eq!(units, expected);
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = open::<Ac3>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    let track = reader.stream().unwrap();
    assert_eq!(
        (track.codec(), track.codec_tag()),
        (CodecId::Eac3, b"ec-3".as_slice())
    );
}

#[test]
fn flac_without_its_marker_starts_at_the_first_confirmed_frame() {
    let frames: Vec<Vec<u8>> = (0..5).map(|i| flac_frame(40 + i, 400)).collect();
    let mut file = id3v2(20);
    file.extend(noise(4, 900));
    let start = file.len() as u64;
    file.extend(frames.concat());
    let reading = invariant::<Flac>(&file);
    assert_eq!(reading.packets.len(), 5);
    assert_eq!(
        reading.packets[0].offset, start,
        "the bytes before it are not a packet"
    );
    assert_eq!(reading.packets[0].pts, 40 * 4096);
    let pool = Budget::new(16 << 20).unwrap();
    let mut reader = open::<Flac>(&pool, &file, true);
    read(&mut reader, &pool, &file, 1 << 20, 4096).unwrap();
    let track = reader.stream().unwrap();
    assert_eq!((track.sample_rate(), track.channels()), (44100, Some(1)));
    assert!(track.configuration().is_empty());
    assert!(reader.format().stream_info().is_none());
}
