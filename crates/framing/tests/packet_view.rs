#![forbid(unsafe_code)]
//! Receiver acceptance across AVC sample widths 1-4 and HEVC array shapes at
//! several work quanta. Every scenario reaches the packet engine with published
//! outputs and a physical rejection; a short AVC update is a receiver rejection;
//! an unsupported HEVC decoder array is an initialization rejection; teardown
//! releases every pool charge. Each scenario's complete event stream (every step
//! with its remaining work and pool charge, every published or refused fact) is
//! pinned per quantum to a digest recorded before the forms were merged.
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

/// Every step result with its remaining work and pool charge, and every
/// published or refused fact, in order.
#[derive(Default)]
struct Run {
    events: Vec<String>,
}
impl Run {
    fn count(&self, prefix: &str) -> usize {
        self.events.iter().filter(|e| e.starts_with(prefix)).count()
    }
    /// FNV-1a over the events, one per line.
    fn digest(&self) -> u64 {
        self.events
            .iter()
            .flat_map(|e| e.bytes().chain(*b"\n"))
            .fold(0xcbf2_9ce4_8422_2325, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
            })
    }
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

fn step<T>(
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
    run.events.push(format!(
        "{label} {kind} {} {}",
        work.remaining(),
        pool.used()
    ));
}

fn output(run: &mut Run, out: &NalPacketOutput) {
    run.events.push(format!(
        "out {:?} cfg {:?} id {:?} crefuse {:?} prefuse {:?} epoch {} fwd {}",
        out.bytes().as_ref(),
        out.configuration_bytes().as_ref(),
        out.configuration_source_identity(),
        out.configuration_source_refusal(),
        out.packet_source_refusal(),
        out.receiver_epoch(),
        out.forward_new_extradata(),
    ));
    for (i, s) in out.segments().iter().enumerate() {
        run.events.push(format!(
            "seg {i} {:?} {} {} {:?}",
            s.origin(),
            s.prefix_bytes(),
            s.output_offset(),
            out.segment_bytes(i)
        ));
    }
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
                let mut w = WorkBudget::new(q);
                let r = p.step(&mut w, &NO);
                step(&mut run, &pool, "avc", &w, &r);
                match r {
                    Ok(BuildStep::Ready(v)) => break NalPacketConfiguration::ReceiverAvc(v),
                    Ok(BuildStep::Yield) => (),
                    _ => return run,
                }
            }
        }
        Codec::Hevc => {
            let mut p =
                ReceiverHevcConfigParser::new(&pool, hvc_source(), raw, hvc_limits()).unwrap();
            loop {
                let mut w = WorkBudget::new(q);
                let r = p.step(&mut w, &NO);
                step(&mut run, &pool, "hevc", &w, &r);
                match r {
                    Ok(BuildStep::Ready(v)) => break NalPacketConfiguration::ReceiverHevc(v),
                    Ok(BuildStep::Yield) => (),
                    _ => return run,
                }
            }
        }
    };
    let mut session = NalPacketSession::new(&pool, config, LIMITS).unwrap();
    {
        let mut writer =
            NalPacketConfigWriter::new(&pool, session.configuration(), 1 << 20).unwrap();
        loop {
            let mut w = WorkBudget::new(q);
            let r = writer.step(&mut w, &NO);
            step(&mut run, &pool, "init", &w, &r);
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
                    let mut w = WorkBudget::new(q);
                    let r = p.step(&mut w, &NO);
                    step(&mut run, &pool, "update", &w, &r);
                    match r {
                        Ok(BuildStep::Ready(v)) => break Some(v),
                        Ok(BuildStep::Yield) => (),
                        _ => {
                            let r = p.take_update_rejection();
                            #[cfg(feature = "reference-trace")]
                            if let Some(r) = &r {
                                assert_eq!(r.native_receive_result(), -1_094_995_529);
                            }
                            run.events.push(format!(
                                "update-rejection {:?}",
                                r.map(|r| (r.source_bytes().to_vec(), r.source_error()))
                            ));
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
        let mut job = match session.receiver_packet(source, update) {
            Ok(job) => job,
            Err(e) => {
                run.events.push(format!("begin {e:?}"));
                continue;
            }
        };
        loop {
            let mut w = WorkBudget::new(q);
            let r = job.step(&mut w, &NO);
            step(&mut run, &pool, "packet", &w, &r);
            match r {
                Ok(BuildStep::Ready(v)) => {
                    output(&mut run, &v);
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
    // Outputs keep every fact after the session closes.
    for out in &kept {
        output(&mut run, out);
    }
    drop(kept);
    assert_eq!(pool.used(), base, "teardown releases every charge");
    run
}

const QUANTA: [u64; 7] = [1, 2, 3, 5, 7, 16, 4096];
/// Recorded at QUANTA from the compact receiver on `main` at `0f154393`.
#[rustfmt::skip]
const DIGESTS: &[(&str, [u64; 7])] = &[
    ("avc w1 ppsfalse", [1947442219789186057, 1185511715728659596, 8307867964941644497, 16897372378937639698, 17866696144606131561, 5629152097004961167, 18136709142933314019]),
    ("avc w1 ppstrue", [13884842486497754742, 7760196371067881174, 8533287113242588717, 3299030819101980589, 28164783380591304, 15532338594060495250, 2993719951551840672]),
    ("avc w2 ppsfalse", [18239225528200379728, 13696431517684817629, 1177911623385127194, 14722527547198414937, 9852954414541737502, 8247547752360730507, 5065442223320622584]),
    ("avc w2 ppstrue", [3186796332870143657, 9787146178842508965, 6758172136960999258, 2013678162438990773, 7693912216902509816, 16660758004324448607, 875409473685783686]),
    ("avc w3", [16842999140837986091, 13141484540749877097, 13752474366022095176, 11986371681674494830, 1314298917957113964, 10689175764475304426, 12754415927343330207]),
    ("avc w4 ppsfalse", [6881063878196693147, 16788212410440608559, 8861556221903114139, 14865897147439908912, 14004740400971062279, 418115431769904358, 17699106426060566749]),
    ("avc w4 ppstrue", [13984787769768039516, 4966072882948194836, 12326267232434518053, 1703232038220652748, 131814776833187396, 3399827663089223451, 13214804148910948561]),
    ("hevc duplicate w1", [3124786859819776625, 1630484269661242907, 10996132396304132206, 3848208352051949208, 8615005597859952558, 9479307566767738734, 1429814736607856650]),
    ("hevc duplicate w2", [7620262315426779902, 9082895688902331519, 2414145230931230328, 14051324404230534766, 6556259138145128636, 12964702671778931216, 12653359729152742345]),
    ("hevc duplicate w4", [12166421534325376975, 14338866888887521327, 3786663991869377886, 1887436272715170888, 14218994063198536257, 10316036579912284558, 5144817311338313907]),
    ("hevc empty-arrays w1", [4472031776549822959, 389195086017800842, 512248974393640783, 5196724354034341913, 7642291549320038400, 5678538463194126218, 4270409721997365512]),
    ("hevc empty-arrays w2", [12093839843655496258, 14297394989122481366, 7641131992357581990, 5921002143566267030, 10139872864010920011, 15879448962327190946, 16856545013193957969]),
    ("hevc empty-arrays w4", [4544666759356073468, 16069129778441921885, 1445292836986433385, 9501364019316999730, 10720420832453371739, 5133658117174576628, 3768710688477938181]),
    ("hevc plain w1", [1151220572727913936, 17146245933546370549, 5503377946578327264, 2327234607465535447, 9713478295318565475, 3763506753170346512, 12884181674464957107]),
    ("hevc plain w2", [18347078724543440790, 10955919909891640212, 11516865353179713685, 11464422390447011651, 3832210858104039010, 11884959605875041875, 13851452055297469670]),
    ("hevc plain w4", [4918472576543061793, 11332669951336139187, 40232216270817173, 15544172345539372930, 733418252062103648, 14728754430991790955, 247588522761713046]),
    ("hevc unsupported-array w1", [13228462911118913844, 5816232290387083747, 12526470776149356078, 3858220857909355972, 439654780368376682, 16238039268892528066, 2153331595941929985]),
    ("hevc unsupported-array w2", [5103550455282199555, 12793933050465456660, 15257397603592746829, 1252985044042593747, 1305757942712506281, 4252329059126782593, 14139041805707675458]),
    ("hevc unsupported-array w4", [9276415840149683133, 6722290642593121470, 10083458655981451603, 6426923991653888973, 15108859086384853719, 16137964412663070607, 17233005384820849888]),
];

/// Each scenario's event stream at every quantum, as recorded from the receiver
/// before its configuration forms were merged.
fn assert_receives(codec: Codec, record: &[u8], packets: &[Packet], label: &str) {
    let Some(&(_, expected)) = DIGESTS.iter().find(|(l, _)| *l == label) else {
        panic!("no digests for {label}");
    };
    for (q, expected) in QUANTA.into_iter().zip(expected) {
        let run = receive(codec, record, packets, q);
        // Every scenario reaches the packet engine or the decoder-array refusal.
        assert!(
            run.count("init-rejection Some") == 1
                || (run.count("out ") >= 8 && run.count("rejection Some") >= 1),
            "{label} quantum {q}"
        );
        assert_eq!(run.digest(), expected, "{label} quantum {q}");
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
