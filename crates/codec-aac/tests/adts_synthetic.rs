//! Self-contained ADTS tests with synthetic (self-authored) bitstreams.
use triergon_codec_aac::{AudioFrame, DecodeError, Decoder, Feature, Speaker};

struct Bw {
    bits: Vec<u8>,
}

impl Bw {
    fn new() -> Bw {
        Bw { bits: Vec::new() }
    }
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(((v >> i) & 1) as u8);
        }
    }
    fn align(&mut self) {
        while self.bits.len() % 8 != 0 {
            self.bits.push(0);
        }
    }
    fn bytes(mut self) -> Vec<u8> {
        self.align();
        self.bits
            .chunks(8)
            .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
            .collect()
    }
}

/// Silent single channel element (max_sfb 0) followed by END.
fn silent_block(w: &mut Bw, cpe: bool) {
    if cpe {
        w.put(1, 3);
        w.put(0, 4);
        w.put(0, 1); // common_window 0
        for _ in 0..2 {
            silent_ics(w);
        }
    } else {
        w.put(0, 3);
        w.put(0, 4);
        silent_ics(w);
    }
    w.put(7, 3);
    w.align();
}

fn silent_ics(w: &mut Bw) {
    w.put(100, 8); // global_gain
    w.put(0, 1); // reserved
    w.put(0, 2); // ONLY_LONG
    w.put(0, 1); // sine
    w.put(0, 6); // max_sfb
    w.put(0, 1); // predictor_data_present
    w.put(0, 3); // pulse, tns, gain control
}

fn adts(profile: u32, sfi: u32, chcfg: u32, blocks: &[Vec<u8>], crc: bool) -> Vec<u8> {
    let nblk = blocks.len();
    let mut extra = 0;
    if crc {
        extra = 2 * (nblk - 1) + 2 + if nblk > 1 { 2 * nblk } else { 0 };
    }
    let payload_len: usize = blocks.iter().map(|b| b.len()).sum();
    let len = 7 + extra + payload_len;
    let mut h = Bw::new();
    h.put(0xfff, 12);
    h.put(0, 1);
    h.put(0, 2);
    h.put(!crc as u32, 1);
    h.put(profile, 2);
    h.put(sfi, 4);
    h.put(0, 1);
    h.put(chcfg, 3);
    h.put(0, 4);
    h.put(len as u32, 13);
    h.put(0x7ff, 11);
    h.put(nblk as u32 - 1, 2);
    let mut out = h.bytes();
    if crc {
        for _ in 1..nblk {
            out.extend_from_slice(&[0, 0]); // raw_data_block_position
        }
        out.extend_from_slice(&[0, 0]); // crc_check
    }
    for b in blocks {
        out.extend_from_slice(b);
        if crc && nblk > 1 {
            out.extend_from_slice(&[0, 0]);
        }
    }
    out
}

fn block(cpe: bool) -> Vec<u8> {
    let mut w = Bw::new();
    silent_block(&mut w, cpe);
    w.bytes()
}

#[test]
fn silent_mono_frame() {
    let mut d = Decoder::new();
    let mut f = AudioFrame::default();
    d.decode(&adts(1, 4, 1, &[block(false)], false), &mut f)
        .unwrap();
    assert_eq!(f.sample_rate, 44100);
    assert_eq!(f.samples_per_channel, 1024);
    assert_eq!(f.layout.speakers(), &[Speaker::FrontCenter]);
    assert!(f.planes[0].iter().all(|&s| s == 0.0));
}

#[test]
fn crc_protected_and_multiple_raw_data_blocks() {
    let mut d = Decoder::new();
    let mut f = AudioFrame::default();
    d.decode(&adts(1, 3, 2, &[block(true)], true), &mut f)
        .unwrap();
    assert_eq!(f.samples_per_channel, 1024);
    assert_eq!(
        f.layout.speakers(),
        &[Speaker::FrontLeft, Speaker::FrontRight]
    );
    // three raw_data_blocks in one ADTS frame, with and without CRC
    for crc in [false, true] {
        d.decode(
            &adts(1, 3, 2, &[block(true), block(true), block(true)], crc),
            &mut f,
        )
        .unwrap();
        assert_eq!(f.samples_per_channel, 3 * 1024);
        assert_eq!(f.planes.len(), 2);
    }
}

#[test]
fn typed_errors() {
    let mut d = Decoder::new();
    let mut f = AudioFrame::default();
    assert_eq!(
        d.decode(&[0xff, 0xf1, 0x50], &mut f),
        Err(DecodeError::Truncated)
    );
    assert!(matches!(
        d.decode(&[0x12; 16], &mut f),
        Err(DecodeError::InvalidData(_))
    ));
    // profile 2 = SSR
    assert_eq!(
        d.decode(&adts(2, 4, 1, &[block(false)], false), &mut f),
        Err(DecodeError::Unsupported(Feature::SsrGainControl))
    );
    // header claims more bytes than given
    let mut a = adts(1, 4, 1, &[block(false)], false);
    a.truncate(a.len() - 1);
    assert_eq!(d.decode(&a, &mut f), Err(DecodeError::Truncated));
    // element not in the channel configuration (CPE in a 3-channel configuration slot list is fine,
    // an LFE in configuration 2 is not)
    let mut w = Bw::new();
    w.put(3, 3);
    w.put(0, 4);
    silent_ics(&mut w);
    w.put(7, 3);
    assert!(matches!(
        d.decode(&adts(1, 4, 2, &[w.bytes()], false), &mut f),
        Err(DecodeError::InvalidData(_))
    ));
    // the decoder keeps working afterwards
    d.decode(&adts(1, 4, 1, &[block(false)], false), &mut f)
        .unwrap();
    assert_eq!(f.samples_per_channel, 1024);
}

#[test]
fn fixed_channel_configurations_map_like_ffmpeg() {
    use Speaker::*;
    let cases: &[(u32, &[bool], &[Speaker])] = &[
        (3, &[false, true], &[FrontLeft, FrontRight, FrontCenter]),
        (
            4,
            &[false, true, false],
            &[FrontLeft, FrontRight, FrontCenter, BackCenter],
        ),
        (
            5,
            &[false, true, true],
            &[FrontLeft, FrontRight, FrontCenter, SideLeft, SideRight],
        ),
        (
            7,
            &[false, true, true, true],
            &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
        ),
    ];
    for &(cfg, elems, want) in cases {
        let mut w = Bw::new();
        for &cpe in elems {
            if cpe {
                w.put(1, 3);
                w.put(0, 4);
                w.put(0, 1);
                silent_ics(&mut w);
                silent_ics(&mut w);
            } else {
                w.put(0, 3);
                w.put(0, 4);
                silent_ics(&mut w);
            }
        }
        if cfg == 7 {
            w.put(3, 3);
            w.put(0, 4);
            silent_ics(&mut w);
        }
        w.put(7, 3);
        let mut d = Decoder::new();
        let mut f = AudioFrame::default();
        d.decode(&adts(1, 4, cfg, &[w.bytes()], false), &mut f)
            .unwrap();
        assert_eq!(f.layout.speakers(), want, "chcfg {cfg}");
    }
}
