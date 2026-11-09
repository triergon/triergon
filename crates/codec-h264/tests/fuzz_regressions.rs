//! Corruptions of JVT conformance vectors that `cargo xtask codecs fuzz --lane h264` found,
//! rebuilt from their FATE seeds (under the gitignored
//! `.research/fate-samples/h264-conformance/`) by the recorded edits, so no FATE bytes are
//! kept here. Each decodes to typed errors in a build with overflow checks. Skips cleanly
//! when the vectors are absent.

use std::path::PathBuf;
use triergon_codec_h264::{DecodeError, Decoder};
use triergon_framing::AnnexBUnits;
use triergon_harness_protocol::md5::hex_digest;

enum Edit {
    /// Remove `remove` bytes at `at`, then insert `bytes` there.
    Insert {
        at: usize,
        remove: usize,
        bytes: &'static [u8],
    },
    /// Remove `remove` bytes at `at`, then insert a copy of `len` bytes from `from`.
    Copy {
        at: usize,
        remove: usize,
        from: usize,
        len: usize,
    },
}

struct Case {
    seed: &'static str,
    seed_md5: &'static str,
    input_md5: &'static str,
    edits: &'static [Edit],
}

const CASES: &[Case] = &[
    Case {
        // dequantisation overflow: seed 284, case-000577
        seed: "FRext/HPCVNL_BRCM_A.264",
        seed_md5: "2de76547280e95f2ec6223996270cff7",
        input_md5: "0a803d311b4c221553e2b1c8e812e0d3",
        edits: &[
            Edit::Insert {
                at: 148490,
                remove: 1687940,
                bytes: &[],
            },
            Edit::Insert {
                at: 11892,
                remove: 4,
                bytes: &[0x00, 0x00, 0x00, 0x00],
            },
        ],
    },
    Case {
        // dequantisation overflow: seed 284, case-000839
        seed: "SL1_SVA_B.264",
        seed_md5: "28931097c74d123d63587030f60a6166",
        input_md5: "f60fe7cc2d0f8d3552c0566b9b396a16",
        edits: &[
            Edit::Insert {
                at: 1331,
                remove: 10042,
                bytes: &[],
            },
            Edit::Copy {
                at: 1223,
                remove: 0,
                from: 1249,
                len: 53,
            },
            Edit::Insert {
                at: 66,
                remove: 4,
                bytes: &[0x00, 0x00, 0x00, 0x00],
            },
            Edit::Insert {
                at: 892,
                remove: 346,
                bytes: &[],
            },
        ],
    },
    Case {
        // dequantisation overflow: seed 285, case-000404
        seed: "CVWP5_TOSHIBA_E.264",
        seed_md5: "15bc158205b829c621012ffea06913c9",
        input_md5: "b5b0751a1dae8a03447c50dbf9949391",
        edits: &[
            Edit::Copy {
                at: 1133124,
                remove: 0,
                from: 706854,
                len: 3633,
            },
            Edit::Insert {
                at: 269976,
                remove: 307,
                bytes: &[],
            },
            Edit::Insert {
                at: 1028255,
                remove: 4,
                bytes: &[0x00, 0x00, 0x00, 0x00],
            },
            Edit::Copy {
                at: 847951,
                remove: 0,
                from: 192161,
                len: 1222,
            },
        ],
    },
    Case {
        // mvd out of range: seed 280, case-000877
        seed: "Sharp_MP_Field_1_B.jvt",
        seed_md5: "44941da36b87c95ba0799d26abb70a70",
        input_md5: "1971709f32aba0c9fdcf3bba90788c01",
        edits: &[
            Edit::Copy {
                at: 51395,
                remove: 0,
                from: 49603,
                len: 3846,
            },
            Edit::Insert {
                at: 53321,
                remove: 8,
                bytes: &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            },
            Edit::Insert {
                at: 194639,
                remove: 1,
                bytes: &[0x52],
            },
            Edit::Copy {
                at: 125146,
                remove: 0,
                from: 135617,
                len: 911,
            },
        ],
    },
    Case {
        // mvd out of range: seed 283, case-000761
        seed: "MR2_MW_A.264",
        seed_md5: "7c54a8f21b7b230401d2c0adfbee7ca7",
        input_md5: "3bdb314039e87ec6033dd79ef2f924b8",
        edits: &[
            Edit::Insert {
                at: 262520,
                remove: 8,
                bytes: &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            },
            Edit::Insert {
                at: 108062,
                remove: 1,
                bytes: &[0x1e],
            },
            Edit::Insert {
                at: 158633,
                remove: 0,
                bytes: &[
                    0xca, 0x65, 0xc1, 0xe5, 0xdb, 0x95, 0x1b, 0x46, 0x79, 0xad, 0xc5,
                ],
            },
        ],
    },
];

fn rebuild(seed: &[u8], edits: &[Edit]) -> Vec<u8> {
    let mut data = seed.to_vec();
    for edit in edits {
        let (at, remove, bytes) = match *edit {
            Edit::Insert { at, remove, bytes } => (at, remove, bytes.to_vec()),
            Edit::Copy {
                at,
                remove,
                from,
                len,
            } => (at, remove, data[from..from + len].to_vec()),
        };
        data.splice(at..at + remove, bytes);
    }
    data
}

#[test]
fn fuzz_findings_decode_to_typed_errors() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/fate-samples/h264-conformance");
    for case in CASES {
        let Ok(seed) = std::fs::read(dir.join(case.seed)) else {
            eprintln!("skipped: no {} in {}", case.seed, dir.display());
            continue;
        };
        assert_eq!(hex_digest(&seed), case.seed_md5, "{}", case.seed);
        let input = rebuild(&seed, case.edits);
        assert_eq!(hex_digest(&input), case.input_md5, "{}", case.seed);
        let mut decoder = Decoder::new();
        let mut errors: Vec<DecodeError> = Vec::new();
        for nal in AnnexBUnits::new(&input) {
            errors.extend(decoder.send_nal(nal, None).err());
            while decoder.receive().is_some() {}
        }
        decoder.flush();
        while decoder.receive().is_some() {}
        assert!(
            !errors.is_empty(),
            "{}: decoded without an error",
            case.seed
        );
    }
}
