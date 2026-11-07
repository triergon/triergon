#![forbid(unsafe_code)]
//! Receiver acceptance across AVC sample widths 1-4 and HEVC array shapes at
//! several work quanta. Every scenario reaches the packet engine with published
//! outputs and a physical rejection; a short AVC update is a receiver rejection;
//! an unsupported HEVC decoder array is an initialization rejection; teardown
//! releases every pool charge.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, SourceLocation, WorkBudget};
use triergon_framing::compat::nal::*;
use triergon_framing::*;

static NO: AtomicBool = AtomicBool::new(false);
const LIMITS: NalPacketLimits = NalPacketLimits {
    maximum_packet_bytes: 1 << 16,
    maximum_output_bytes: 1 << 20,
    maximum_nals: 64,
    maximum_segments: 128,
};

#[derive(Default, Debug)]
struct Run {
    outputs: usize,
    rejections: usize,
    /// Reference receive result (with `reference-trace`) and raw update bytes.
    update_rejections: Vec<(Option<i32>, Vec<u8>)>,
    /// Raw record and declared array type.
    init_rejection: Option<(Vec<u8>, u8)>,
}

struct Packet {
    bytes: Vec<u8>,
    extra: Option<Vec<u8>>,
}

fn avc_limits() -> AvcConfigLimits {
    AvcConfigLimits {
        maximum_record_bytes: 65_536,
        maximum_nals: 65_536,
        maximum_nal_bytes: 65_535,
    }
}
fn avc_source(offset: u64) -> AvcConfigSource {
    AvcConfigSource {
        source: SourceId(1),
        generation: 1,
        offset,
    }
}
fn hvc_limits() -> HvcConfigLimits {
    HvcConfigLimits {
        maximum_record_bytes: 65_536,
        maximum_arrays: 255,
        maximum_nals: 65_536,
        maximum_nal_bytes: 65_535,
    }
}
fn hvc_source() -> HvcConfigSource {
    HvcConfigSource {
        source: SourceId(1),
        generation: 1,
        offset: 40,
    }
}

fn avc_record(width: u8, extra_pps: bool) -> Vec<u8> {
    // Baseline profile: no avcC extension block follows the parameter sets.
    let mut raw = vec![1, 66, 0, 40, 0xfc | (width - 1), 0xe2];
    raw.extend_from_slice(&[0, 4, 0x67, 0x64, 0x00, 0x28]);
    raw.extend_from_slice(&[0, 2, 0x67, 0x01]);
    raw.push(if extra_pps { 2 } else { 1 });
    raw.extend_from_slice(&[0, 3, 0x68, 0xee, 0x3c]);
    if extra_pps {
        raw.extend_from_slice(&[0, 2, 0x68, 0x01]);
    }
    raw
}
fn hvc_record(width: u8, groups: &[(u8, Vec<Vec<u8>>)]) -> Vec<u8> {
    let mut out = vec![
        1,
        0x01,
        0x60,
        0,
        0,
        0,
        0x90,
        0,
        0,
        0,
        0,
        0,
        93,
        0xf0,
        0,
        0xfc,
        0xfd,
        0xf8,
        0xf8,
        0,
        0,
        0x0c | (width - 1),
        groups.len() as u8,
    ];
    for (header, nals) in groups {
        out.push(*header);
        out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            out.extend_from_slice(&(n.len() as u16).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    out
}
fn hvc_nal(kind: u8, tag: u8) -> Vec<u8> {
    vec![kind << 1, 1, tag, 0x5a]
}
fn framed(width: usize, nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes()[4 - width..]);
        out.extend_from_slice(n);
    }
    out
}

fn receive(codec: Codec, record: &[u8], packets: &[Packet], q: u64) -> Run {
    let pool = Budget::new(1 << 20).unwrap();
    let base = pool.used();
    let mut run = Run::default();
    let raw = Bytes::copy_from_slice(&pool, record).unwrap();
    let config = match codec {
        Codec::Avc => {
            let mut p =
                ReceiverAvcConfigParser::new(&pool, avc_source(20), raw, avc_limits()).unwrap();
            loop {
                match p.step(&mut WorkBudget::new(q), &NO).unwrap() {
                    BuildStep::Ready(v) => break NalPacketConfiguration::ReceiverAvc(v),
                    BuildStep::Yield => (),
                    BuildStep::Complete => panic!("missing AVC configuration"),
                }
            }
        }
        Codec::Hevc => {
            let mut p =
                ReceiverHevcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
            loop {
                match p.step(&mut WorkBudget::new(q), &NO).unwrap() {
                    BuildStep::Ready(v) => break NalPacketConfiguration::ReceiverHevc(v),
                    BuildStep::Yield => (),
                    BuildStep::Complete => panic!("missing HEVC configuration"),
                }
            }
        }
    };
    let mut session = NalPacketSession::new(&pool, config, LIMITS).unwrap();
    {
        let mut writer =
            NalPacketConfigWriter::new(&pool, session.configuration(), 1 << 20).unwrap();
        loop {
            match writer.step(&mut WorkBudget::new(q), &NO) {
                Ok(BuildStep::Ready(_)) => break,
                Ok(BuildStep::Yield) => (),
                _ => {
                    run.init_rejection = writer
                        .take_receiver_rejection()
                        .map(|r| (r.source_bytes().to_vec(), r.declared_array_type()));
                    drop(writer);
                    drop(session);
                    assert_eq!(pool.used(), base, "teardown releases every charge");
                    return run;
                }
            }
        }
    }
    let mut kept = Vec::new();
    for (index, packet) in packets.iter().enumerate() {
        let extra = packet
            .extra
            .as_ref()
            .map(|e| Bytes::copy_from_slice(&pool, e).unwrap());
        let update = match (codec, &extra) {
            (Codec::Avc, Some(e)) if !e.is_empty() => {
                let mut p = ReceiverAvcConfigParser::new_update(
                    &pool,
                    avc_source(900),
                    e.clone(),
                    avc_limits(),
                )
                .unwrap();
                let parsed = loop {
                    match p.step(&mut WorkBudget::new(q), &NO) {
                        Ok(BuildStep::Ready(v)) => break Some(v),
                        Ok(BuildStep::Yield) => (),
                        _ => {
                            if let Some(r) = p.take_update_rejection() {
                                #[cfg(feature = "reference-trace")]
                                let native = Some(r.native_receive_result());
                                #[cfg(not(feature = "reference-trace"))]
                                let native = None;
                                run.update_rejections
                                    .push((native, r.source_bytes().to_vec()));
                            }
                            break None;
                        }
                    }
                };
                match parsed {
                    Some(v) => Some(v),
                    None => continue,
                }
            }
            _ => None,
        };
        let source = NalPacket {
            bytes: Bytes::copy_from_slice(&pool, &packet.bytes).unwrap(),
            location: SourceLocation {
                source: SourceId(1),
                offset: 1000 + index as u64,
            },
            generation: 1,
            fields: NalPacketFields {
                pts: index as i64,
                dts: index as i64,
                duration: 1,
                position: -1,
                flags: 1,
                stream_index: 0,
            },
            new_extradata: extra,
        };
        let Ok(mut job) = session.receiver_packet(source, update) else {
            continue;
        };
        loop {
            match job.step(&mut WorkBudget::new(q), &NO) {
                Ok(BuildStep::Ready(v)) => {
                    run.outputs += 1;
                    kept.push(v);
                    break;
                }
                Ok(BuildStep::Yield) => (),
                Ok(BuildStep::Complete) => break,
                Err(_) => {
                    if job.take_receiver_rejection().is_some() {
                        run.rejections += 1;
                    }
                    break;
                }
            }
        }
    }
    drop(session);
    drop(kept);
    assert_eq!(pool.used(), base, "teardown releases every charge");
    run
}

fn assert_receives(codec: Codec, record: &[u8], packets: &[Packet], label: &str) {
    for q in [1, 2, 3, 5, 7, 16, 4096] {
        let run = receive(codec, record, packets, q);
        if label.contains("unsupported") {
            assert_eq!(
                run.init_rejection,
                Some((record.to_vec(), 55)),
                "{label} quantum {q}"
            );
            continue;
        }
        // Every scenario must reach the packet engine, so the run covers
        // published outputs rather than only an early configuration refusal.
        assert!(
            run.outputs >= 4 && run.rejections >= 1,
            "{label} quantum {q}: {run:?}"
        );
        if codec == Codec::Avc {
            // The two-byte version-one update is shorter than a complete record.
            #[cfg(feature = "reference-trace")]
            let native = Some(-1_094_995_529);
            #[cfg(not(feature = "reference-trace"))]
            let native = None;
            assert_eq!(
                run.update_rejections,
                [(native, vec![1, 100])],
                "{label} quantum {q}"
            );
        }
    }
}

fn avc_packets(width: usize, record: &[u8]) -> Vec<Packet> {
    let idr: &[u8] = &[0x65, 0x88, 0x84, 0x21];
    let non: &[u8] = &[0x41, 0x9a, 0x02];
    let sps: &[u8] = &[0x67, 0x64, 0x00, 0x28];
    let pps: &[u8] = &[0x68, 0xee, 0x3c];
    let sei: &[u8] = &[0x06, 0x05, 0x01];
    vec![
        Packet {
            bytes: framed(width, &[sei, idr]),
            extra: None,
        },
        Packet {
            bytes: framed(width, &[non]),
            extra: None,
        },
        Packet {
            bytes: framed(width, &[pps, idr, non]),
            extra: Some(Vec::new()),
        },
        Packet {
            bytes: framed(width, &[sps, pps, idr]),
            extra: None,
        },
        Packet {
            bytes: framed(width, &[idr]),
            extra: Some(record.to_vec()),
        },
        Packet {
            bytes: vec![0, 0, 0, 9, 0x65][4 - width.min(4)..].to_vec(),
            extra: None,
        },
        Packet {
            bytes: framed(width, &[idr]),
            extra: Some(vec![1, 100]),
        },
        Packet {
            bytes: Vec::new(),
            extra: None,
        },
        Packet {
            bytes: framed(width, &[&[0x25, 0x01], non]),
            extra: None,
        },
    ]
}

#[test]
fn avc_receiver_publishes_and_rejects_at_every_width_and_quantum() {
    for width in [1u8, 2, 4] {
        for extra_pps in [false, true] {
            let record = avc_record(width, extra_pps);
            let packets = avc_packets(usize::from(width), &record);
            assert_receives(
                Codec::Avc,
                &record,
                &packets,
                &format!("avc w{width} pps{extra_pps}"),
            );
        }
    }
    // Observed three-byte width keeps its source refusal on every output.
    let record = avc_record(3, true);
    assert_receives(Codec::Avc, &record, &avc_packets(3, &record), "avc w3");
}

#[test]
fn hevc_receiver_publishes_and_rejects_for_every_array_shape_and_quantum() {
    let irap = hvc_nal(19, 1);
    let trail = hvc_nal(1, 2);
    let prefix_sei = hvc_nal(39, 3);
    let vps = hvc_nal(32, 4);
    for width in [1u8, 2, 4] {
        let w = usize::from(width);
        let packets = vec![
            Packet {
                bytes: framed(w, &[&prefix_sei, &irap]),
                extra: None,
            },
            Packet {
                bytes: framed(w, &[&trail]),
                extra: Some(vec![9, 9]),
            },
            Packet {
                bytes: framed(w, &[&vps, &irap, &trail]),
                extra: None,
            },
            Packet {
                bytes: vec![0, 0, 0, 9, 0x26][4 - w..].to_vec(),
                extra: None,
            },
            Packet {
                bytes: framed(w, &[&[]]),
                extra: None,
            },
            Packet {
                bytes: framed(w, &[&[0x80, 0x01]]),
                extra: None,
            },
        ];
        for (name, record) in [
            (
                "plain",
                hvc_record(
                    width,
                    &[
                        (32, vec![hvc_nal(32, 7)]),
                        (33, vec![hvc_nal(33, 8)]),
                        (34, vec![hvc_nal(34, 9)]),
                    ],
                ),
            ),
            (
                "duplicate",
                hvc_record(
                    width,
                    &[
                        (32 | 128, vec![hvc_nal(32, 7), hvc_nal(32, 7)]),
                        (33, vec![hvc_nal(33, 8)]),
                        (34, vec![hvc_nal(34, 9), hvc_nal(34, 1)]),
                        (39, vec![prefix_sei.clone()]),
                    ],
                ),
            ),
            (
                "empty-arrays",
                hvc_record(width, &[(32, vec![]), (33, vec![hvc_nal(33, 8)])]),
            ),
            (
                "unsupported-array",
                hvc_record(
                    width,
                    &[(32, vec![hvc_nal(32, 7)]), (55, vec![hvc_nal(55, 1)])],
                ),
            ),
        ] {
            assert_receives(
                Codec::Hevc,
                &record,
                &packets,
                &format!("hevc {name} w{width}"),
            );
        }
    }
}
