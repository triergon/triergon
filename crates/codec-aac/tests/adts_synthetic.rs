//! Self-contained ADTS tests with synthetic (self-authored) bitstreams.
use triergon_codec::Budget;
use triergon_codec_aac::{AudioFrame, DecodeError, Decoder, Feature, Limits, Speaker};

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
        while !self.bits.len().is_multiple_of(8) {
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
    let payload_len: usize = blocks.iter().map(Vec::len).sum();
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

#[test]
fn a_budget_too_small_for_the_channel_state_is_a_typed_error() {
    let mut d = Decoder::with_limits(Limits {
        live_storage_bytes: 16 * 1024,
    })
    .unwrap();
    let mut f = AudioFrame::default();
    let frame = adts(1, 4, 1, &[block(false)], false);
    for _ in 0..2 {
        assert!(matches!(
            d.decode(&frame, &mut f),
            Err(DecodeError::Budget(_))
        ));
        assert_eq!(f.samples_per_channel, 0);
    }
}

#[test]
fn a_shared_budget_returns_to_idle_when_the_decoder_drops() {
    let budget = Budget::new(1 << 20).unwrap();
    let idle = budget.used();
    let mut d = Decoder::new_in(&budget);
    let mut f = AudioFrame::default();
    d.decode(&adts(1, 4, 1, &[block(false)], false), &mut f)
        .unwrap();
    assert!(budget.used() >= idle + 2 * 2048 * size_of::<f32>() + 1024 * size_of::<f32>());
    drop(d);
    assert_eq!(budget.used(), idle);
}

#[test]
fn decoding_resumes_after_a_refusal() {
    let budget = Budget::new(8 << 20).unwrap();
    let mut d = Decoder::new_in(&budget);
    let mut f = AudioFrame::default();
    let one = adts(1, 4, 1, &[block(false)], false);
    let three = adts(1, 4, 1, &[block(false), block(false), block(false)], false);
    // the first frame, then a frame that needs a longer output
    for au in [&one, &three] {
        let pressure = budget.reserve(budget.available()).unwrap();
        assert!(matches!(d.decode(au, &mut f), Err(DecodeError::Budget(_))));
        assert_eq!(f.samples_per_channel, 0);
        drop(pressure);
        d.decode(au, &mut f).unwrap();
        assert!(f.planes[0].iter().all(|&s| s == 0.0));
    }
    assert_eq!(f.samples_per_channel, 3 * 1024);
    // The worst case was charged up front, so with the pool exhausted the same access
    // units decode without claiming anything more.
    let pressure = budget.reserve(budget.available()).unwrap();
    for au in [&one, &three] {
        f.clear();
        d.decode(au, &mut f).unwrap();
    }
    drop(pressure);
}

/// A raw data block of a PCE declaring one front element, then that element, silent.
fn pce_block(cpe: bool) -> Vec<u8> {
    let mut w = Bw::new();
    w.put(5, 3); // ID_PCE
    w.put(0, 4); // element_instance_tag
    w.put(1, 2); // object_type: LC
    w.put(4, 4); // sampling_frequency_index
    w.put(1, 4); // front elements
    w.put(0, 4 + 4 + 2 + 3 + 4); // side, back, LFE, data and coupling elements
    w.put(0, 3); // no mono or stereo mixdown, no matrix mixdown
    w.put(cpe as u32, 1);
    w.put(0, 4); // its tag
    w.align();
    w.put(0, 8); // comment_field_bytes
    silent_block(&mut w, cpe);
    w.bytes()
}

#[test]
fn a_layout_switch_inside_an_access_unit_claims_nothing_more() {
    let budget = Budget::new(8 << 20).unwrap();
    let mut d = Decoder::new_in(&budget);
    let mut f = AudioFrame::default();
    let mono = adts(1, 4, 0, &[pce_block(false), pce_block(false)], false);
    d.decode(&mono, &mut f).unwrap();
    assert_eq!(f.planes.len(), 1);
    // Each access unit charges the largest output of its raw data blocks before it is
    // decoded, so a second block that switches to stereo needs nothing more from the pool.
    let switch = adts(1, 4, 0, &[pce_block(false), pce_block(true)], false);
    let pressure = budget.reserve(budget.available()).unwrap();
    f.clear();
    d.decode(&switch, &mut f).unwrap();
    assert_eq!(
        f.layout.speakers(),
        [Speaker::FrontLeft, Speaker::FrontRight]
    );
    // The switch restarts the frame, so it holds the stereo block only.
    assert_eq!(f.samples_per_channel, 1024);
    drop(pressure);
}
