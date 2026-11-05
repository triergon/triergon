#![forbid(unsafe_code)]
//! The view-configuration receiver is the same packet/initialization state engine
//! as the eager observed compact receiver. The legacy path is the oracle: every
//! step's result and remaining work, every published fact and every refusal must
//! match, while the view never holds more pool charge at any step.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, SourceLocation, WorkBudget};
use triergon_framing::*;

static NO: AtomicBool = AtomicBool::new(false);
const LIMITS: NalPacketLimits = NalPacketLimits {
    maximum_packet_bytes: 1 << 16,
    maximum_output_bytes: 1 << 20,
    maximum_nals: 64,
    maximum_segments: 128,
};

#[derive(Default, Debug, PartialEq)]
struct Run {
    /// Literal step kinds, remaining work and every published or refused fact.
    events: Vec<String>,
    /// Pool charge after each step, compared as view <= legacy.
    used: Vec<usize>,
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

fn step_fact<T>(
    run: &mut Run,
    pool: &Budget,
    label: &str,
    work: &WorkBudget,
    r: &Result<BuildStep<T>, triergon_core::Error>,
) {
    let kind = match r {
        Ok(BuildStep::Yield) => "yield".to_string(),
        Ok(BuildStep::Ready(_)) => "ready".to_string(),
        Ok(BuildStep::Complete) => "complete".to_string(),
        Err(e) => format!("error {e:?}"),
    };
    run.events
        .push(format!("{label} {kind} {}", work.remaining()));
    run.used.push(pool.used());
}

fn output_facts(run: &mut Run, out: &NalPacketCompactOutput) {
    run.events.push(format!(
        "out {:?} cfg {:?} id {:?} crefuse {:?} prefuse {:?} epoch {} fwd {} segs {:?}",
        out.bytes().as_ref(),
        out.configuration_bytes().as_ref(),
        out.configuration_source_identity(),
        out.configuration_source_refusal(),
        out.packet_source_refusal(),
        out.receiver_epoch(),
        out.forward_new_extradata(),
        out.segments(),
    ));
    for i in 0..out.segments().len() {
        run.events
            .push(format!("seg {i} {:?}", out.segment_bytes(i)));
    }
}

macro_rules! receiver {
    ($name:ident, $avc_parser:ident, $hvc_parser:ident, $config:ident, $session:path, $writer:path, $packet:ident) => {
        fn $name(codec: Codec, record: &[u8], packets: &[Packet], q: u64) -> Run {
            let pool = Budget::new(1 << 20).unwrap();
            let base = pool.used();
            let mut run = Run::default();
            let raw = Bytes::copy_from_slice(&pool, record).unwrap();
            let config = match codec {
                Codec::Avc => {
                    let mut p = $avc_parser::new(&pool, avc_source(20), raw, avc_limits()).unwrap();
                    loop {
                        let mut w = WorkBudget::new(q);
                        let r = p.step(&mut w, &NO);
                        step_fact(&mut run, &pool, "avc", &w, &r);
                        match r {
                            Ok(BuildStep::Ready(v)) => break $config::ObservedAvc(v),
                            Ok(BuildStep::Yield) => (),
                            _ => return run,
                        }
                    }
                }
                Codec::Hevc => {
                    let mut p = $hvc_parser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
                    loop {
                        let mut w = WorkBudget::new(q);
                        let r = p.step(&mut w, &NO);
                        step_fact(&mut run, &pool, "hevc", &w, &r);
                        match r {
                            Ok(BuildStep::Ready(v)) => break $config::ObservedHevc(v),
                            Ok(BuildStep::Yield) => (),
                            _ => return run,
                        }
                    }
                }
            };
            let mut session = $session(&pool, config, LIMITS).unwrap();
            {
                let mut writer = $writer(&pool, session.configuration(), 1 << 20).unwrap();
                loop {
                    let mut w = WorkBudget::new(q);
                    let r = writer.step(&mut w, &NO);
                    step_fact(&mut run, &pool, "init", &w, &r);
                    match r {
                        Ok(BuildStep::Ready(v)) => {
                            run.events.push(format!(
                                "init {:?} src {:?} refuse {:?}",
                                v.bytes().as_ref(),
                                v.source_bytes().as_ref(),
                                v.strict_source_refusal()
                            ));
                            break;
                        }
                        Ok(BuildStep::Yield) => (),
                        _ => {
                            let r = writer.take_receiver_rejection();
                            run.events.push(format!(
                                "init-rejection {:?}",
                                r.map(|r| (r.source_bytes().to_vec(), r.declared_array_type()))
                            ));
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
                        let mut p = $avc_parser::new_update(
                            &pool,
                            avc_source(900),
                            e.clone(),
                            avc_limits(),
                        )
                        .unwrap();
                        let mut parsed = None;
                        loop {
                            let mut w = WorkBudget::new(q);
                            let r = p.step(&mut w, &NO);
                            step_fact(&mut run, &pool, "update", &w, &r);
                            match r {
                                Ok(BuildStep::Ready(v)) => {
                                    parsed = Some(v);
                                    break;
                                }
                                Ok(BuildStep::Yield) => (),
                                _ => {
                                    let r = p.take_update_rejection();
                                    run.events.push(format!(
                                        "update-rejection {:?}",
                                        r.map(|r| (
                                            r.native_receive_result(),
                                            r.source_bytes().to_vec()
                                        ))
                                    ));
                                    break;
                                }
                            }
                        }
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
                let mut job = match session.$packet(source, update) {
                    Ok(job) => job,
                    Err(e) => {
                        run.events.push(format!("begin {e:?}"));
                        continue;
                    }
                };
                loop {
                    let mut w = WorkBudget::new(q);
                    let r = job.step(&mut w, &NO);
                    step_fact(&mut run, &pool, "packet", &w, &r);
                    match r {
                        Ok(BuildStep::Ready(v)) => {
                            output_facts(&mut run, &v);
                            kept.push(v);
                            break;
                        }
                        Ok(BuildStep::Yield) => (),
                        Ok(BuildStep::Complete) => break,
                        Err(_) => {
                            let r = job.take_receiver_rejection();
                            run.events.push(format!(
                                "rejection {:?}",
                                r.map(|r| (
                                    r.kind(),
                                    r.source_error(),
                                    r.configuration_bytes().to_vec(),
                                    r.configuration_source_identity(),
                                    r.configuration_source_refusal(),
                                    r.packet_source_refusal(),
                                    r.receiver_epoch(),
                                ))
                            ));
                            break;
                        }
                    }
                }
                run.events
                    .push(format!("epoch {}", session.receiver_epoch()));
            }
            drop(session);
            for out in &kept {
                output_facts(&mut run, out);
            }
            run.used.push(pool.used());
            drop(kept);
            assert_eq!(pool.used(), base, "teardown releases every charge");
            run
        }
    };
}
receiver!(
    legacy,
    NalObservedAvcConfigParser,
    NalObservedHevcConfigParser,
    NalPacketConfiguration,
    NalPacketSession::new,
    NalPacketConfigWriter::new,
    packet_observed_compact
);
receiver!(
    view,
    NalObservedAvcConfigViewParser,
    NalObservedHevcConfigViewParser,
    NalPacketViewConfiguration,
    NalPacketSession::new_view,
    NalPacketConfigWriter::new_view,
    packet_observed_view
);

fn assert_same(codec: Codec, record: &[u8], packets: &[Packet], label: &str) {
    for q in [1, 2, 3, 5, 7, 16, 4096] {
        let old = legacy(codec, record, packets, q);
        let new = view(codec, record, packets, q);
        assert_eq!(old.events, new.events, "{label} quantum {q}");
        // Every scenario must reach the packet engine, so the comparison covers
        // published outputs rather than only an early configuration refusal.
        let outputs = old.events.iter().filter(|e| e.starts_with("out ")).count();
        let rejections = old
            .events
            .iter()
            .filter(|e| e.starts_with("rejection "))
            .count();
        if label.contains("unsupported") {
            assert!(
                old.events
                    .iter()
                    .any(|e| e.starts_with("init-rejection Some")),
                "{label}"
            );
        } else {
            assert!(
                outputs >= 4 && rejections >= 1,
                "{label} q{q}: {outputs} outputs, {rejections} rejections: {:#?}",
                old.events
            );
        }
        assert_eq!(old.used.len(), new.used.len(), "{label} quantum {q}");
        for (step, (a, b)) in old.used.iter().zip(&new.used).enumerate() {
            assert!(
                b <= a,
                "{label} quantum {q} step {step}: view {b} > legacy {a}"
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
fn avc_view_receiver_matches_eager_receiver_step_for_step() {
    for width in [1u8, 2, 4] {
        for extra_pps in [false, true] {
            let record = avc_record(width, extra_pps);
            let packets = avc_packets(usize::from(width), &record);
            assert_same(
                Codec::Avc,
                &record,
                &packets,
                &format!("avc w{width} pps{extra_pps}"),
            );
        }
    }
    // Observed three-byte width keeps its source refusal on every output.
    let record = avc_record(3, true);
    assert_same(Codec::Avc, &record, &avc_packets(3, &record), "avc w3");
}

#[test]
fn hevc_view_receiver_matches_eager_receiver_step_for_step() {
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
            assert_same(
                Codec::Hevc,
                &record,
                &packets,
                &format!("hevc {name} w{width}"),
            );
        }
    }
}
