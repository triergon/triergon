//! Session-level behaviour of the capture pipeline (contract `session`).

use triergon_capture::pixel::{self, Orientation};
use triergon_capture::{CaptureError, Session, SessionConfig, Sink, mpeg1};

fn frame(w: usize, h: usize, seed: u32) -> Vec<u8> {
    (0..w * h * 4)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) >> 24) as u8)
        .collect()
}

fn packets(ts: &[u8]) -> Vec<&[u8]> {
    assert_eq!(ts.len() % 188, 0);
    ts.chunks(188).collect()
}

fn pid(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[1] & 0x1F, p[2]])
}

/// First PES payload bytes of the video PID in one frame's output.
fn pes_payload_start(ts: &[u8]) -> Vec<u8> {
    let p = packets(ts)
        .into_iter()
        .find(|p| pid(p) == 0x100 && p[1] & 0x40 != 0)
        .unwrap();
    let start = if p[3] & 0x20 != 0 {
        5 + p[4] as usize
    } else {
        4
    };
    p[start..].to_vec()
}

fn pts_dts(ts: &[u8]) -> (i64, i64) {
    let h = pes_payload_start(ts);
    let read = |b: &[u8]| -> i64 {
        (((b[0] as i64 >> 1) & 7) << 30)
            | ((b[1] as i64) << 22)
            | ((b[2] as i64 >> 1) << 15)
            | ((b[3] as i64) << 7)
            | (b[4] as i64 >> 1)
    };
    (read(&h[9..14]), read(&h[14..19]))
}

#[test]
fn session_starts_with_tables_and_a_sequence_header_and_timing_advances() {
    let cfg = SessionConfig::streem(64, 48, 2_500_000);
    let mut s = Session::open(&cfg).unwrap();
    let mut outputs = Vec::new();
    for k in 0..32 {
        let mut out = Vec::new();
        let report = s
            .encode(&frame(64, 48, k), Orientation::TopDown, &mut out)
            .unwrap();
        assert_eq!(report.ts_bytes, out.len());
        assert_eq!(report.picture.key, k % 30 == 0);
        outputs.push(out);
    }
    let first = packets(&outputs[0]);
    assert_eq!(
        [pid(first[0]), pid(first[1]), pid(first[2])],
        [0x11, 0x00, 0x1000]
    );
    // The ES of the first picture opens with a sequence header (00 00 01 B3).
    assert_eq!(&pes_payload_start(&outputs[0])[19..23], &[0, 0, 1, 0xB3]);
    assert_ne!(&pes_payload_start(&outputs[1])[19..23], &[0, 0, 1, 0xB3]);
    assert_eq!(&pes_payload_start(&outputs[30])[19..23], &[0, 0, 1, 0xB3]);
    for (k, out) in outputs.iter().enumerate() {
        assert_eq!(pts_dts(out), (3000 * (k as i64 + 1), 3000 * k as i64));
    }
    // Continuity counters of the video PID run on across frames.
    let mut cc = None;
    for out in &outputs {
        for p in packets(out).into_iter().filter(|p| pid(p) == 0x100) {
            if let Some(prev) = cc {
                assert_eq!(p[3] & 0x0F, (prev + 1) & 0x0F);
            }
            cc = Some(p[3] & 0x0F);
        }
    }
}

#[test]
fn keepalive_reencodes_with_fresh_timing() {
    // Streem's idle keepalive re-encodes the last picture; the stream must keep
    // advancing rather than repeat bytes.
    let mut s = Session::open(&SessionConfig::streem(48, 32, 1_000_000)).unwrap();
    let picture = frame(48, 32, 7);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    s.encode(&picture, Orientation::TopDown, &mut a).unwrap();
    s.encode(&picture, Orientation::TopDown, &mut b).unwrap();
    assert_ne!(a, b);
    assert_eq!(pts_dts(&b), (6000, 3000));
}

struct Failing(usize);

impl Sink for Failing {
    fn send(&mut self, _: &[u8]) -> bool {
        if self.0 == 0 {
            return false;
        }
        self.0 -= 1;
        true
    }
}

#[test]
fn sink_failure_poisons_the_session() {
    let mut s = Session::open(&SessionConfig::streem(32, 32, 1_000_000)).unwrap();
    let mut sink = Failing(1);
    let picture = frame(32, 32, 1);
    assert!(s.encode(&picture, Orientation::TopDown, &mut sink).is_ok());
    assert_eq!(
        s.encode(&picture, Orientation::TopDown, &mut sink)
            .unwrap_err(),
        CaptureError::Sink
    );
    assert_eq!(
        s.encode(&picture, Orientation::TopDown, &mut Vec::new())
            .unwrap_err(),
        CaptureError::Closed
    );
}

#[test]
fn bottom_up_input_equals_flipped_top_down_input() {
    let (w, h) = (48usize, 32usize);
    let top = frame(w, h, 3);
    let mut bottom = Vec::with_capacity(top.len());
    for row in top.chunks(w * 4).rev() {
        bottom.extend_from_slice(row);
    }
    let cfg = SessionConfig::streem(w as u32, h as u32, 2_500_000);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    Session::open(&cfg)
        .unwrap()
        .encode(&top, Orientation::TopDown, &mut a)
        .unwrap();
    Session::open(&cfg)
        .unwrap()
        .encode(&bottom, Orientation::BottomUp, &mut b)
        .unwrap();
    assert_eq!(a, b);
}

#[test]
fn both_streem_geometries_and_rate_extremes_open() {
    for (w, h) in [(960, 540), (480, 270)] {
        for rate in [100_000, 2_500_000, 25_500_000] {
            let s = Session::open(&SessionConfig::streem(w, h, rate)).unwrap();
            assert_eq!(s.geometry().width(), w as usize);
        }
    }
}

#[test]
fn invalid_requests_are_explicit_errors() {
    assert!(matches!(
        Session::open(&SessionConfig::streem(63, 48, 1_000_000)),
        Err(CaptureError::Pixel(pixel::Error::Geometry { .. }))
    ));
    assert!(matches!(
        Session::open(&SessionConfig::streem(64, 48, 0)),
        Err(CaptureError::Encoder(mpeg1::Error::BitRate(_)))
    ));
    let mut s = Session::open(&SessionConfig::streem(64, 48, 1_000_000)).unwrap();
    assert!(matches!(
        s.encode(&[0; 10], Orientation::TopDown, &mut Vec::new()),
        Err(CaptureError::Pixel(pixel::Error::InputLength { .. }))
    ));
}
#[test]
fn session_and_direct_encoder_share_gop_validation() {
    let mut c = SessionConfig::streem(16, 16, 1_000_000);
    c.gop = 0;
    assert!(matches!(
        Session::open(&c),
        Err(CaptureError::Encoder(mpeg1::Error::Gop(0)))
    ));
}
#[test]
fn invalid_session_mux_configuration_is_an_error() {
    let mut c = SessionConfig::streem(16, 16, 1_000_000);
    c.mux.video_pid = c.mux.pmt_pid;
    assert!(matches!(Session::open(&c), Err(CaptureError::Mux(_))));
}
