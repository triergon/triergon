//! Self-authored synthetic syncframes: framing, CRC policy, truncation/concealment and
//! typed errors. The frames are built bit by bit from the A/52:2018 syntax tables.

use triergon_codec_ac3::{
    AudioFrame, Compat, CrcPolicy, DecodeError, Decoder, DecoderOptions, Feature, NoiseMode,
    Speaker, StreamType, Syntax, access_unit_len, crc16, parse_header,
};

#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        for k in (0..n).rev() {
            self.bits.push((v >> k) & 1 != 0);
        }
    }
    fn bytes(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, &b) in self.bits.iter().enumerate() {
            if b {
                out[i / 8] |= 0x80 >> (i % 8);
            }
        }
        out
    }
}

/// crc1 that makes the CRC over frame[2..five8] zero (A/52 7.10.1), by GF(2) elimination.
fn solve_crc1(frame: &[u8], five8: usize) -> u16 {
    let n = five8 - 4;
    let mut msg = vec![0u8; 2 + n];
    msg[2..].copy_from_slice(&frame[4..five8]);
    let target = crc16(0, &msg);
    let cols: Vec<u16> = (0..16)
        .map(|i| {
            let mut m = vec![0u8; 2 + n];
            m[0] = ((1u16 << i) >> 8) as u8;
            m[1] = (1u16 << i) as u8;
            crc16(0, &m)
        })
        .collect();
    // Brute force is cheap enough for 16 unknowns via elimination on (lhs, rhs) rows.
    let mut rows: Vec<(u16, u16)> = (0..16)
        .map(|r| {
            (
                cols.iter()
                    .enumerate()
                    .fold(0, |a, (i, c)| a | (((c >> r) & 1) << i)),
                (target >> r) & 1,
            )
        })
        .collect();
    let mut pivots = Vec::new();
    let mut r = 0;
    for col in 0..16 {
        let Some(p) = (r..16).find(|&k| rows[k].0 >> col & 1 != 0) else {
            continue;
        };
        rows.swap(r, p);
        for k in 0..16 {
            if k != r && rows[k].0 >> col & 1 != 0 {
                rows[k].0 ^= rows[r].0;
                rows[k].1 ^= rows[r].1;
            }
        }
        pivots.push((col, r));
        r += 1;
    }
    pivots.iter().fold(
        0,
        |x, &(col, row)| if rows[row].1 != 0 { x | 1 << col } else { x },
    )
}

/// A silent 48 kHz mono AC-3 syncframe (frmsizecod 0 = 64 words) with valid CRCs.
/// `chbwcod` lets a test inject an invalid value (> 60).
fn mono_frame(chbwcod: u32) -> Vec<u8> {
    mono_frame_with_dither(chbwcod, false)
}

fn mono_frame_with_dither(chbwcod: u32, dither: bool) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(0x0b77, 16);
    w.put(0, 16); // crc1 placeholder
    w.put(0, 2); // fscod 48 kHz
    w.put(0, 6); // frmsizecod 0
    w.put(8, 5); // bsid
    w.put(0, 3); // bsmod
    w.put(1, 3); // acmod 1/0
    w.put(0, 1); // lfeon
    w.put(27, 5); // dialnorm
    w.put(0, 3); // compre, langcode, audprodie
    w.put(0, 2); // copyrightb, origbs
    w.put(0, 3); // timecod1e, timecod2e, addbsie
    for blk in 0..6 {
        w.put(0, 1); // blksw
        w.put(u32::from(dither), 1); // dithflag
        w.put(0, 1); // dynrnge
        if blk == 0 {
            w.put(1, 1); // cplstre
            w.put(0, 1); // cplinu
            w.put(3, 2); // chexpstr D45
            w.put(chbwcod, 6);
            w.put(10, 4); // absolute exponent
            for _ in 0..6 {
                w.put(62, 7); // three zero differentials
            }
            w.put(0, 2); // gainrng
            w.put(1, 1); // baie
            w.put(2, 2);
            w.put(1, 2);
            w.put(1, 2);
            w.put(2, 2);
            w.put(7, 3);
            w.put(1, 1); // snroffste
            w.put(0, 6); // csnroffst
            w.put(0, 4); // fsnroffst -> all-zero SNR offsets: no mantissas (7.2.2.1.1)
            w.put(4, 3); // fgaincod
        } else {
            w.put(0, 1); // cplstre
            w.put(0, 2); // chexpstr reuse
            w.put(0, 1); // baie
            w.put(0, 1); // snroffste
        }
        w.put(0, 1); // deltbaie
        w.put(0, 1); // skiple
    }
    let mut f = w.bytes(128);
    let five8 = ((64 >> 1) + (64 >> 3)) * 2;
    let c1 = solve_crc1(&f, five8);
    f[2..4].copy_from_slice(&c1.to_be_bytes());
    let c2 = crc16(0, &f[2..126]);
    f[126..].copy_from_slice(&c2.to_be_bytes());
    f
}

#[test]
fn synthetic_frame_header_and_crc() {
    let f = mono_frame(0);
    let h = parse_header(&f).unwrap();
    assert_eq!(
        (
            h.syntax,
            h.bsid,
            h.frame_bytes,
            h.sample_rate,
            h.acmod,
            h.lfeon
        ),
        (Syntax::Ac3, 8, 128, 48000, 1, false)
    );
    assert_eq!(h.stream_type, StreamType::Independent);
    assert_eq!(crc16(0, &f[2..80]), 0);
    assert_eq!(crc16(0, &f[2..]), 0);
    let two: Vec<u8> = [f.clone(), f.clone()].concat();
    assert_eq!(access_unit_len(&two).unwrap(), 128);
}

#[test]
fn decodes_silent_mono_frame() {
    let mut dec = Decoder::new();
    let mut out = AudioFrame::default();
    let info = dec.decode(&mono_frame(0), &mut out).unwrap();
    assert_eq!(info.samples, 1536);
    assert!(info.crc_ok);
    assert!(info.concealed.is_none());
    assert_eq!(info.dialnorm, 27);
    assert_eq!(out.layout.speakers(), &[Speaker::FrontCenter]);
    assert_eq!(triergon_codec_ac3::layout_name(&out.layout), "mono");
    assert_eq!(out.samples_per_channel, 1536);
    assert!(out.planes[0].iter().all(|&v| v == 0.0));
    assert!(info.downmix.is_none());
    // Appending a second frame.
    dec.decode(&mono_frame(0), &mut out).unwrap();
    assert_eq!(out.samples_per_channel, 3072);
    assert_eq!(out.planes[0].len(), 3072);
}

#[test]
fn compatible_reset_replays_the_first_dithered_frame() {
    let frame = mono_frame_with_dither(0, true);
    let mut decoder = Decoder::new();
    let mut output = AudioFrame::default();
    let info = decoder.decode(&frame, &mut output).unwrap();
    assert!(info.features.dither);
    assert!(info.concealed.is_none());
    let first = output.planes[0].clone();
    assert!(first.iter().any(|&sample| sample != 0.0));

    output.clear();
    decoder.decode(&frame, &mut output).unwrap();
    assert_ne!(output.planes[0], first);

    decoder.reset();
    output.clear();
    decoder.decode(&frame, &mut output).unwrap();
    assert_eq!(output.planes[0], first);
}

#[test]
fn noise_off_keeps_bap_zero_silent_with_either_compatibility_setting() {
    let frame = mono_frame_with_dither(0, true);
    for compat in [Compat::Specification, Compat::Reference] {
        let mut decoder = Decoder::with_options(DecoderOptions {
            compat,
            noise: NoiseMode::Off,
            ..Default::default()
        });
        let mut output = AudioFrame::default();
        let info = decoder.decode(&frame, &mut output).unwrap();
        assert!(info.features.dither);
        assert!(info.concealed.is_none());
        assert!(output.planes[0].iter().all(|&sample| sample == 0.0));
    }
}

#[test]
fn crc_mismatch_is_reported_or_rejected() {
    let mut f = mono_frame(0);
    f[127] ^= 0x01; // corrupt crc2
    let mut out = AudioFrame::default();
    let info = Decoder::new().decode(&f, &mut out).unwrap();
    assert!(
        !info.crc_ok,
        "default policy decodes and reports (FFmpeg-compatible)"
    );
    let mut strict = Decoder::with_options(DecoderOptions {
        crc: CrcPolicy::Reject,
        ..Default::default()
    });
    out.clear();
    assert_eq!(
        strict.decode(&f, &mut out).err(),
        Some(DecodeError::CrcMismatch {
            crc1_ok: true,
            crc2_ok: false
        })
    );
    let mut g = mono_frame(0);
    g[10] ^= 0x40; // inside the first 5/8: both CRCs fail
    assert_eq!(
        strict.decode(&g, &mut out).err(),
        Some(DecodeError::CrcMismatch {
            crc1_ok: false,
            crc2_ok: false
        })
    );
}

#[test]
fn truncated_ac3_frame_repeats_last_block() {
    let mut dec = Decoder::new();
    let mut out = AudioFrame::default();
    dec.decode(&mono_frame(0), &mut out).unwrap();
    out.clear();
    let f = mono_frame(0);
    let info = dec.decode(&f[..100], &mut out).unwrap();
    assert!(matches!(
        info.concealed,
        Some(DecodeError::Truncated {
            needed: 128,
            available: 100
        })
    ));
    assert_eq!(out.samples_per_channel, 1536);
}

#[test]
fn invalid_block_data_is_concealed() {
    let mut out = AudioFrame::default();
    let info = Decoder::new().decode(&mono_frame(61), &mut out).unwrap();
    assert_eq!(
        info.concealed,
        Some(DecodeError::InvalidData("chbwcod > 60"))
    );
    assert_eq!(out.samples_per_channel, 1536);
}

#[test]
fn typed_header_errors() {
    let mut dec = Decoder::new();
    let mut out = AudioFrame::default();
    assert_eq!(dec.decode(&[], &mut out).err(), Some(DecodeError::Empty));
    assert_eq!(
        dec.decode(&[0; 16], &mut out).err(),
        Some(DecodeError::NoSync)
    );
    let mut f = mono_frame(0);
    f[4] = 38; // frmsizecod 38 does not exist
    assert!(matches!(
        dec.decode(&f, &mut out).err(),
        Some(DecodeError::InvalidData(_))
    ));
    // bsid 9 (half-rate AC-3) decodes like FFmpeg 9.0.2 (tests/reduced_rate.rs).
    let mut f = mono_frame(0);
    f[5] = (9 << 3) | (f[5] & 7);
    assert_eq!(dec.decode(&f, &mut out).map(|i| i.sample_rate), Ok(24000));
    out.clear();
    let mut f = mono_frame(0);
    f[5] = (20 << 3) | (f[5] & 7);
    assert_eq!(
        dec.decode(&f, &mut out).err(),
        Some(DecodeError::Unsupported(Feature::BitstreamId(20)))
    );
}

/// E-AC-3 header: strmtyp, substreamid, frmsiz (words-1), fscod, numblkscod, acmod, lfeon, bsid 16.
fn eac3_header(strmtyp: u32, sid: u32, frmsiz: u32) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put(0x0b77, 16);
    w.put(strmtyp, 2);
    w.put(sid, 3);
    w.put(frmsiz, 11);
    w.put(0, 2);
    w.put(3, 2);
    w.put(2, 3);
    w.put(0, 1);
    w.put(16, 5);
    w.bytes(16)
}

#[test]
fn eac3_truncation_and_substream_errors() {
    let mut dec = Decoder::new();
    let mut out = AudioFrame::default();
    let h = eac3_header(0, 0, 255);
    let ph = parse_header(&h).unwrap();
    assert_eq!(
        (ph.syntax, ph.frame_bytes, ph.num_blocks, ph.acmod),
        (Syntax::Eac3, 512, 6, 2)
    );
    // A truncated E-AC-3 frame yields no output (FFmpeg 9.0.2 behaviour).
    assert_eq!(
        dec.decode(&h, &mut out).err(),
        Some(DecodeError::Truncated {
            needed: 512,
            available: 16
        })
    );
    assert_eq!(out.samples_per_channel, 0);
    assert_eq!(
        dec.decode(&eac3_header(1, 0, 255), &mut out).err(),
        Some(DecodeError::MissingIndependentSubstream)
    );
    assert_eq!(
        dec.decode(&eac3_header(0, 2, 255), &mut out).err(),
        Some(DecodeError::MissingIndependentSubstream)
    );
    assert_eq!(
        dec.decode(&eac3_header(3, 0, 255), &mut out).err(),
        Some(DecodeError::Unsupported(Feature::ReservedStreamType))
    );
}

#[test]
fn output_layout_mismatch_is_refused() {
    let mut dec = Decoder::new();
    let mut out = AudioFrame::default();
    dec.decode(&mono_frame(0), &mut out).unwrap();
    out.layout =
        triergon_codec_ac3::ChannelLayout::new([Speaker::FrontLeft, Speaker::FrontRight]).unwrap();
    assert_eq!(
        dec.decode(&mono_frame(0), &mut out).err(),
        Some(DecodeError::OutputLayoutMismatch)
    );
    dec.reset();
    out.clear();
    assert!(dec.decode(&mono_frame(0), &mut out).is_ok());
}
