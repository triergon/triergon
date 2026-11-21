//! Unbounded PES packets (`PES_packet_length` 0) of video stream types.

use std::sync::atomic::AtomicBool;
use triergon_core::container::{CodecId, MediaKind, TrackInfo};
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_mpegts::psi::{DescriptorNamespace, PsiCrc32};
use triergon_mpegts::{ParseLimits, TraceEvent, TransportStream};

/// A PES packet's PTS, DTS and payload.
type Pes = (Option<u64>, Option<u64>, Vec<u8>);

/// The PES packets of PID 0x100, and the count of diagnostics on it.
fn demux(ts: &[u8]) -> (Vec<Pes>, usize) {
    let pool = Budget::new(1 << 24).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(ts.len() as u64),
        random_access: false,
    };
    let mut input = InputCursor::new(&pool, SourceId(1), capabilities, 64 * 1024).unwrap();
    let mut stream = TransportStream::new_single_program_segment(
        pool.clone(),
        ParseLimits::default(),
        DescriptorNamespace::Unspecified,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let (mut pes, mut faults) = (Vec::new(), 0);
    loop {
        match stream
            .poll(&mut input, &mut WorkBudget::new(1 << 20), &cancel)
            .unwrap()
        {
            Step::NeedInput(request) => {
                let offset = usize::try_from(request.offset).unwrap();
                if offset < ts.len() {
                    let end = ts.len().min(offset + request.maximum.min(7));
                    let window = Bytes::copy_from_slice(&pool, &ts[offset..end]).unwrap();
                    input
                        .provide_owned(
                            InputWindow::new(request.source, request.token, request.offset, window)
                                .unwrap(),
                        )
                        .unwrap();
                } else {
                    input.finish(request.source, request.token).unwrap();
                }
            }
            Step::Ready(TraceEvent::Pes(p)) if p.pid == 0x100 => {
                pes.push((
                    p.header.pts_raw33,
                    p.header.dts_raw33,
                    p.payload().flatten().copied().collect(),
                ));
            }
            Step::Ready(TraceEvent::ProgramMap { map, .. }) => {
                for entry in map.streams.iter() {
                    if entry.stream_type == 0x33 {
                        assert_eq!(entry.codec(), Some(CodecId::Vvc));
                        assert_eq!(entry.kind(), MediaKind::Video);
                        assert_eq!(entry.codec_tag(), &[0x33]);
                        assert!(entry.configuration().is_empty());
                    }
                }
            }
            Step::Ready(TraceEvent::Diagnostic(fault)) if fault.pid == Some(0x100) => faults += 1,
            Step::Ready(TraceEvent::Finished) => break,
            Step::End(_) => stream.end_of_source().unwrap(),
            Step::Ready(_) | Step::Yield => {}
        }
    }
    (pes, faults)
}

/// Three pictures in unbounded PES packets, the PMT naming `stream_type`.
fn muxed(stream_type: u8) -> Vec<u8> {
    let config = MuxConfig::default();
    let mut mux = Muxer::new(&config).unwrap();
    let mut ts = Vec::new();
    for n in 0..3u8 {
        let picture = [0, 0, 1, 0, n, 0x0f, 0xff, 0xf8];
        mux.write_picture(
            &picture,
            3003 * i64::from(n + 1),
            3003 * i64::from(n),
            n == 0,
            &mut ts,
        );
    }
    for packet in ts.chunks_exact_mut(188) {
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        if pid == config.pmt_pid {
            // Pointer field, then the section: its first stream entry follows
            // the 12-byte header (no program descriptors).
            let section = &mut packet[5..];
            let length = 3 + ((usize::from(section[1] & 0x0f) << 8) | usize::from(section[2]));
            section[12] = stream_type;
            let crc = PsiCrc32::of(&section[..length - 4]);
            section[length - 4..length].copy_from_slice(&crc.to_be_bytes());
        }
    }
    ts
}

#[test]
fn mpeg_video_streams_carry_unbounded_pes() {
    for stream_type in [0x01, 0x02, 0x1b, 0x24, 0x33] {
        let (pes, faults) = demux(&muxed(stream_type));
        assert_eq!(faults, 0, "stream type {stream_type:#04x}");
        let times: Vec<_> = pes.iter().map(|(pts, _, _)| *pts).collect();
        assert_eq!(times, [Some(3003), Some(6006), Some(9009)]);
        assert_eq!(pes[1].2, [0, 0, 1, 0, 1, 0x0f, 0xff, 0xf8]);
    }
}

#[test]
fn finite_vvc_pes_keeps_the_same_source_boundaries() {
    let original = muxed(0x33);
    let mut finite = original.clone();
    for carrier in finite.chunks_exact_mut(188) {
        let pid = (u16::from(carrier[1] & 31) << 8) | u16::from(carrier[2]);
        if pid == 0x100 {
            let start = 5 + usize::from(carrier[4]);
            let length = u16::try_from(188 - start - 6).unwrap();
            carrier[start + 4..start + 6].copy_from_slice(&length.to_be_bytes());
        }
    }
    assert_eq!(demux(&finite), demux(&original));
}

#[test]
fn other_streams_still_need_a_length() {
    for stream_type in [0x06, 0x34, 0x36] {
        let (pes, faults) = demux(&muxed(stream_type));
        assert!(pes.is_empty());
        assert!(faults > 0);
    }
}

#[test]
fn required_vvc_inputs_preserve_every_source_payload_and_clock() {
    let expected: Vec<Pes> = (0..3u8)
        .map(|n| {
            (
                Some(3003 * u64::from(n + 1)),
                Some(3003 * u64::from(n)),
                vec![0, 0, 1, 0, n, 0x0f, 0xff, 0xf8],
            )
        })
        .collect();
    for input in [
        include_bytes!("../../../harness/fixtures/mpegts/vvc-finite.ts").as_slice(),
        include_bytes!("../../../harness/fixtures/mpegts/vvc-unbounded.ts").as_slice(),
    ] {
        assert_eq!(demux(input), (expected.clone(), 0));
    }
}

/// A no-parser demux control: synthetic bodies establish carriage, not decoding.
/// Set TRIERGON_FFMPEG and TRIERGON_FFPROBE to public 9.0.2 binaries.
#[test]
#[ignore = "requires public FFmpeg 9.0.2 binaries"]
fn vvc_pes_matches_public_ffmpeg_without_codec_parsing() {
    use std::process::Command;
    let ffmpeg = std::env::var("TRIERGON_FFMPEG").unwrap();
    let ffprobe = std::env::var("TRIERGON_FFPROBE").unwrap();
    for binary in [&ffmpeg, &ffprobe] {
        let version = Command::new(binary).arg("-version").output().unwrap();
        assert!(version.status.success());
        assert!(
            String::from_utf8(version.stdout)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .contains("version 9.0.2 ")
        );
    }
    let path = std::env::temp_dir().join(format!("triergon-vvc-pes-{}.ts", std::process::id()));
    let ts = muxed(0x33);
    std::fs::write(&path, &ts).unwrap();
    let metadata = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-fflags",
            "+noparse+nofillin",
            "-f",
            "mpegts",
            "-show_data",
            "-show_entries",
            "stream=codec_name,codec_type:packet=pts,dts,size,data",
            "-of",
            "compact",
        ])
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(
        metadata.status.success(),
        "{}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let (packets, faults) = demux(&ts);
    assert_eq!(faults, 0);
    let text = String::from_utf8(metadata.stdout).unwrap();
    for (index, (_, _, bytes)) in packets.iter().enumerate() {
        assert_eq!(bytes.len(), 8);
        let hex = bytes
            .chunks_exact(2)
            .map(|pair| format!("{:02x}{:02x}", pair[0], pair[1]))
            .collect::<Vec<_>>()
            .join(" ");
        let line = text
            .lines()
            .filter(|line| line.starts_with("packet|"))
            .nth(index)
            .unwrap();
        assert!(
            line.contains(&format!("data=\\n00000000: {hex} ")),
            "{line}"
        );
    }
    assert!(text.contains("codec_name=vvc|codec_type=video"), "{text}");
    for (pts, dts) in [(3003, 0), (6006, 3003), (9009, 6006)] {
        assert!(
            text.contains(&format!("packet|pts={pts}|dts={dts}|size=8")),
            "{text}"
        );
    }
    assert_eq!(
        packets
            .iter()
            .map(|(pts, dts, _)| (*pts, *dts))
            .collect::<Vec<_>>(),
        [
            (Some(3003), Some(0)),
            (Some(6006), Some(3003)),
            (Some(9009), Some(6006))
        ]
    );
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("packet|"))
            .count(),
        3
    );
}
