//! Bit-exactness against FFmpeg reference output for the HEVC conformance vectors in the
//! (gitignored) FATE suite download. Skips cleanly when the vectors are absent.
//!
//! By default a feature-covering subset runs; set HEVC_CONFORMANCE_FULL=1 for every vector.
//! Expected values are MD5 sums of `ffmpeg -f rawvideo -pix_fmt yuv420p|yuv420p10le` output
//! (tests/ffmpeg_ref_md5.txt). Out-of-set vectors must be refused with `Unsupported`.

use std::path::PathBuf;

use triergon_codec_hevc::{AccessUnitSplitter, DecodeError, Decoder, Row};

const QUICK: &[&str] = &[
    "AMP_B_Samsung_6.bit",
    "AMVP_A_MTK_4.bit",
    "CAINIT_E_SHARP_3.bit",
    "DBLK_B_SONY_3.bit",
    "DSLICE_A_HHI_5.bit",
    "ENTP_A_Qualcomm_1.bit",
    "IPRED_B_Nokia_3.bit",
    "LTRPSPS_A_Qualcomm_1.bit",
    "MERGE_F_MTK_4.bit",
    "NoOutPrior_A_Qualcomm_1.bit",
    "PICSIZE_C_Bossen_1.bit",
    "PMERGE_B_TI_3.bit",
    "POC_A_Bossen_3.bit",
    "RAP_B_Bossen_1.bit",
    "RPS_E_qualcomm_5.bit",
    "SAO_D_Samsung_5.bit",
    "SLIST_B_Sony_8.bit",
    "TILES_B_Cisco_1.bit",
    "TSKIP_A_MS_3.bit",
    "WPP_B_ericsson_MAIN10_2.bit",
    "WP_MAIN10_B_Toshiba_3.bit",
    "ipcm_E_NEC_2.bit",
    "PS_A_VIDYO_3.bit",
];

/// Vectors outside the selected Main/Main10/MSP feature set (refused with Unsupported).
const OUT_OF_SET: &[&str] = &[
    "ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi_1.bit",
    "IPCM_A_RExt_NEC.bit",
    "IPCM_B_RExt_NEC.bit",
    "Main_422_10_A_RExt_Sony_1.bin",
    "Main_422_10_B_RExt_Sony_1.bin",
    "PERSIST_RPARAM_A_RExt_Sony_1.bit",
    "PERSIST_RPARAM_A_RExt_Sony_3.bit",
    "QMATRIX_A_RExt_Sony_1.bit",
    "SAO_A_RExt_MediaTek_1.bit",
    "WPP_HIGH_TP_444_8BIT_RExt_Apple_2.bit",
];

fn vector_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/m1d4/fate-suite/hevc-conformance")
}

fn decode_all(data: &[u8]) -> (Vec<u8>, Vec<DecodeError>) {
    let mut dec = Decoder::new();
    let mut out = Vec::new();
    let mut errs = Vec::new();
    for au in AccessUnitSplitter::new(data) {
        if let Err(e) = dec.send(au, None) {
            errs.push(e);
        }
        while let Some(p) = dec.receive() {
            for pl in &p.planes {
                pl.write_raw(&mut out);
            }
        }
    }
    dec.flush();
    while let Some(p) = dec.receive() {
        for pl in &p.planes {
            pl.write_raw(&mut out);
        }
    }
    (out, errs)
}

#[test]
fn conformance_vectors_match_ffmpeg() {
    let dir = vector_dir();
    if !dir.is_dir() {
        eprintln!(
            "note: {} absent; skipping HEVC conformance test",
            dir.display()
        );
        return;
    }
    let full = std::env::var("HEVC_CONFORMANCE_FULL").is_ok();
    let table = include_str!("ffmpeg_ref_md5.txt");
    let mut failures = Vec::new();
    let mut ran = 0;
    for line in table.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        let (name, md5) = (f[0], f[4]);
        if !full && !QUICK.contains(&name) && !OUT_OF_SET.contains(&name) {
            continue;
        }
        let path = dir.join(name);
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        ran += 1;
        let (out, errs) = decode_all(&data);
        if OUT_OF_SET.contains(&name) {
            if !errs
                .iter()
                .any(|e| matches!(e, DecodeError::Unsupported(_)))
            {
                failures.push(format!("{name}: expected Unsupported refusal"));
            }
            continue;
        }
        let got = md5::hex(&out);
        if got != md5 || !errs.is_empty() {
            failures.push(format!(
                "{name}: md5 {got} != {md5} errors {:?}",
                errs.first()
            ));
        }
    }
    eprintln!("ran {ran} vectors");
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}

/// Minimal MD5 (RFC 1321) for test comparison only.
mod md5 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    pub fn hex(data: &[u8]) -> String {
        let k: Vec<u32> = (0..64)
            .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
            .collect();
        let (mut a0, mut b0, mut c0, mut d0) =
            (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
        let mut msg = data.to_vec();
        let bitlen = (data.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bitlen.to_le_bytes());
        for chunk in msg.chunks(64) {
            let m: Vec<u32> = (0..16)
                .map(|i| u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap()))
                .collect();
            let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
            for i in 0..64 {
                let (f, g) = match i / 16 {
                    0 => ((b & c) | (!b & d), i),
                    1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let f2 = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(f2.rotate_left(S[i]));
            }
            a0 = a0.wrapping_add(a);
            b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c);
            d0 = d0.wrapping_add(d);
        }
        let mut s = String::new();
        for v in [a0, b0, c0, d0] {
            for byte in v.to_le_bytes() {
                s.push_str(&format!("{byte:02x}"));
            }
        }
        s
    }

    #[test]
    fn known() {
        assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }
}

#[test]
fn plane_raw_matches_rows() {
    let path = vector_dir().join("CONFWIN_A_Sony_1.bit");
    let Ok(data) = std::fs::read(&path) else {
        eprintln!("note: {} absent; skipping", path.display());
        return;
    };
    let mut dec = Decoder::new();
    let mut checked = 0;
    for au in AccessUnitSplitter::new(&data) {
        dec.send(au, None).unwrap();
        while let Some(p) = dec.receive() {
            for pl in &p.planes {
                let (all, stride, off) = pl.raw();
                for y in 0..pl.height {
                    match (pl.row(y), &all) {
                        (Row::U8(r), Row::U8(a)) => {
                            assert_eq!(r, &a[off + y * stride..off + y * stride + pl.width])
                        }
                        (Row::U16(r), Row::U16(a)) => {
                            assert_eq!(r, &a[off + y * stride..off + y * stride + pl.width])
                        }
                        _ => panic!("sample type mismatch"),
                    }
                }
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
}
