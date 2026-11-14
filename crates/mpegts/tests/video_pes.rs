//! Unbounded PES packets (`PES_packet_length` 0) of video stream types.

use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Bytes, InputCursor, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_mpegts::psi::{DescriptorNamespace, PsiCrc32};
use triergon_mpegts::{ParseLimits, TraceEvent, TransportStream};

/// A PES packet's PTS and payload.
type Pes = (Option<u64>, Vec<u8>);

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
                    let end = ts.len().min(offset + request.maximum);
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
                pes.push((p.header.pts_raw33, p.payload().flatten().copied().collect()));
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
    for stream_type in [0x01, 0x02, 0x1b, 0x24] {
        let (pes, faults) = demux(&muxed(stream_type));
        assert_eq!(faults, 0, "stream type {stream_type:#04x}");
        let times: Vec<_> = pes.iter().map(|(pts, _)| *pts).collect();
        assert_eq!(times, [Some(3003), Some(6006), Some(9009)]);
        assert_eq!(pes[1].1, [0, 0, 1, 0, 1, 0x0f, 0xff, 0xf8]);
    }
}

#[test]
fn other_streams_still_need_a_length() {
    let (pes, faults) = demux(&muxed(0x06));
    assert!(pes.is_empty());
    assert!(faults > 0);
}
