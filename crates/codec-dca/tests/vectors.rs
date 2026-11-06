//! FATE vector tests. The vectors live outside git (`.research/m1d4/fate-suite/dts/`,
//! rights unreviewed); every test skips with a note when its file is absent. Core frames
//! are taken from the container (raw DTS / DTS-HD file chunks / MPEG-TS PES) by this
//! file's own parsing, extension substreams are dropped, and the decoded output is
//! checked against fingerprints of FFmpeg 9.0.2 decoding the same core-only bytes
//! (tests/data/vectors.fp, harness/codecs/dca/make_fingerprints.sh).

mod support;
use std::path::PathBuf;
use support::harness;

fn fate(rel: &str) -> Option<Vec<u8>> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research/m1d4/fate-suite/dts")
        .join(rel);
    match std::fs::read(&p) {
        Ok(b) => Some(b),
        Err(_) => {
            eprintln!("note: vector {} absent, skipped", p.display());
            None
        }
    }
}

/// STRMDATA payload of a DTS-HD file (8-byte chunk id, 8-byte big-endian size).
fn dtshd_stream(b: &[u8]) -> Vec<u8> {
    let mut p = 0;
    while p + 16 <= b.len() {
        let size = u64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()) as usize;
        let end = (p + 16 + size).min(b.len());
        if &b[p..p + 8] == b"STRMDATA" {
            return b[p + 16..end].to_vec();
        }
        p = end;
    }
    Vec::new()
}

/// Concatenated PES payloads of the MPEG-TS PID that carries DTS.
fn ts_dts(b: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;
    let mut pes: HashMap<u16, Vec<u8>> = HashMap::new();
    let mut started: HashMap<u16, bool> = HashMap::new();
    for pkt in b.as_chunks::<188>().0 {
        if pkt[0] != 0x47 {
            continue;
        }
        let pusi = pkt[1] & 0x40 != 0;
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        let afc = (pkt[3] >> 4) & 3;
        let mut off = 4;
        if afc & 2 != 0 {
            off += 1 + pkt[4] as usize;
        }
        if afc & 1 == 0 || off >= 188 {
            continue;
        }
        let mut payload = &pkt[off..];
        if pusi {
            if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                continue;
            }
            let hl = 9 + payload[8] as usize;
            if hl > payload.len() {
                continue;
            }
            payload = &payload[hl..];
            started.insert(pid, true);
        }
        if started.get(&pid).copied().unwrap_or(false) {
            pes.entry(pid).or_default().extend_from_slice(payload);
        }
    }
    pes.into_values()
        .find(|v| v.windows(4).any(|w| w == [0x7F, 0xFE, 0x80, 0x01]))
        .unwrap_or_default()
}

fn core_stream(rel: &str) -> Option<Vec<u8>> {
    let b = fate(rel)?;
    Some(if rel.ends_with(".dtshd") {
        dtshd_stream(&b)
    } else if rel.ends_with(".ts") {
        ts_dts(&b)
    } else {
        b
    })
}

fn run(name: &str, rel: &str) {
    let Some(bytes) = core_stream(rel) else {
        return;
    };
    let fps = harness::parse_fingerprints(include_str!("data/vectors.fp"));
    let fp = fps.iter().find(|f| f.name == name).expect("fingerprint");
    let frames = harness::split(&bytes);
    let d = harness::decode_all(&frames);
    assert!(d.errors.is_empty(), "{name}: {:?}", d.errors);
    if let Err(e) = harness::check(fp, &d) {
        panic!("{e}");
    }
}

macro_rules! vector {
    ($test:ident, $name:expr, $rel:expr) => {
        #[test]
        fn $test() {
            run($name, $rel);
        }
    };
}

vector!(dts_ts, "dts", "dts.ts");
vector!(dts_es, "dts_es", "dts_es.dts");
vector!(dtsx_7_1_4, "dtsx_7.1.4_24bit", "dtsx_7.1.4_24bit.dts");
vector!(
    master_audio_7_1,
    "master_audio_7.1_24bit",
    "master_audio_7.1_24bit.dts"
);
vector!(
    core_51_0,
    "core_51_24_48_768_0",
    "dcadec-suite/core_51_24_48_768_0.dtshd"
);
vector!(
    core_51_1,
    "core_51_24_48_768_1",
    "dcadec-suite/core_51_24_48_768_1.dtshd"
);
vector!(
    x96_51,
    "x96_51_24_96_1509",
    "dcadec-suite/x96_51_24_96_1509.dtshd"
);
vector!(
    x96_xch_61,
    "x96_xch_61_24_96_3840",
    "dcadec-suite/x96_xch_61_24_96_3840.dtshd"
);
vector!(
    x96_xxch_71,
    "x96_xxch_71_24_96_3840",
    "dcadec-suite/x96_xxch_71_24_96_3840.dtshd"
);
vector!(
    xbr_51,
    "xbr_51_24_48_3840",
    "dcadec-suite/xbr_51_24_48_3840.dtshd"
);
vector!(
    xbr_xch_61,
    "xbr_xch_61_24_48_3840",
    "dcadec-suite/xbr_xch_61_24_48_3840.dtshd"
);
vector!(
    xbr_xxch_71,
    "xbr_xxch_71_24_48_3840",
    "dcadec-suite/xbr_xxch_71_24_48_3840.dtshd"
);
vector!(
    xch_61,
    "xch_61_24_48_768",
    "dcadec-suite/xch_61_24_48_768.dtshd"
);
vector!(
    xll_51_16_192_0,
    "xll_51_16_192_768_0",
    "dcadec-suite/xll_51_16_192_768_0.dtshd"
);
vector!(
    xll_51_16_192_1,
    "xll_51_16_192_768_1",
    "dcadec-suite/xll_51_16_192_768_1.dtshd"
);
vector!(
    xll_51_24_48,
    "xll_51_24_48_768",
    "dcadec-suite/xll_51_24_48_768.dtshd"
);
vector!(
    xll_71_24_48_0,
    "xll_71_24_48_768_0",
    "dcadec-suite/xll_71_24_48_768_0.dtshd"
);
vector!(
    xll_71_24_48_1,
    "xll_71_24_48_768_1",
    "dcadec-suite/xll_71_24_48_768_1.dtshd"
);
vector!(
    xll_71_24_96,
    "xll_71_24_96_768",
    "dcadec-suite/xll_71_24_96_768.dtshd"
);
vector!(
    xll_x96_51,
    "xll_x96_51_24_96_1509",
    "dcadec-suite/xll_x96_51_24_96_1509.dtshd"
);
vector!(
    xll_xch_61,
    "xll_xch_61_24_48_768",
    "dcadec-suite/xll_xch_61_24_48_768.dtshd"
);
vector!(
    xxch_71,
    "xxch_71_24_48_2046",
    "dcadec-suite/xxch_71_24_48_2046.dtshd"
);

/// XLL-only stream: no core substream, nothing to decode (outside the core contract).
#[test]
fn xll_without_core_has_no_core_frames() {
    let Some(bytes) = core_stream("dcadec-suite/xll_51_24_48_none.dtshd") else {
        return;
    };
    assert!(harness::split(&bytes).is_empty());
}
