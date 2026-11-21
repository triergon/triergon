#![forbid(unsafe_code)]
//! Receiver acceptance across AVC sample widths 1-4 and HEVC array shapes at
//! several work quanta. Every scenario reaches the packet engine with published
//! outputs and a physical rejection; a short AVC update is a receiver rejection;
//! an unsupported HEVC decoder array is an initialization rejection; teardown
//! releases every pool charge. Each scenario's complete event stream (every step
//! with its remaining work and pool charge, every published or refused fact) is
//! pinned per quantum to a digest.
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, SourceId, SourceLocation, WorkBudget};
use triergon_framing::compat::nal::*;
use triergon_framing::{
    BuildStep,
    avc::{AvcConfigLimits, AvcConfigSource},
    hevc::{HvcConfigLimits, HvcConfigSource},
    nal::{
        Codec, NalPacket, NalPacketConfigWriter, NalPacketConfiguration, NalPacketFields,
        NalPacketLimits, NalPacketOutput, NalPacketSession,
    },
};

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
    /// Each event, and the pool charge after it for a step.
    events: Vec<(String, Option<usize>)>,
}
impl Run {
    fn push(&mut self, event: String) {
        self.events.push((event, None));
    }
    fn push_charged(&mut self, event: String, charge: usize) {
        self.events.push((event, Some(charge)));
    }
    fn count(&self, prefix: &str) -> usize {
        self.events
            .iter()
            .filter(|(e, _)| e.starts_with(prefix))
            .count()
    }
    /// FNV-1a over the events with their charges, one per line. Charges include
    /// storage headers, which depend on the pointer width.
    fn digest(&self) -> u64 {
        fnv(self.charged_lines())
    }
    fn charged_lines(&self) -> impl Iterator<Item = String> + '_ {
        self.events.iter().map(|(e, charge)| match charge {
            Some(charge) => format!("{e} {charge}"),
            None => e.clone(),
        })
    }
    /// FNV-1a over the events without the charges, the same on every target.
    fn portable_digest(&self) -> u64 {
        fnv(self.events.iter().map(|(e, _)| e.clone()))
    }
}
fn fnv(lines: impl Iterator<Item = String>) -> u64 {
    lines
        .flat_map(|e| e.into_bytes().into_iter().chain(*b"\n"))
        .fold(0xcbf2_9ce4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
        })
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
    run.push_charged(format!("{label} {kind} {}", work.remaining()), pool.used());
}

fn output(run: &mut Run, out: &NalPacketOutput) {
    run.push(format!(
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
        run.push(format!(
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
                    run.push(format!(
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
                    run.push(format!(
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
                            run.push(format!(
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
                run.push(format!("begin {e:?}"));
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
                    run.push(format!(
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
        run.push(format!("epoch {}", session.receiver_epoch()));
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
/// Complete event digests at QUANTA, including charged segment and descriptor storage.
#[rustfmt::skip]
const DIGESTS: &[(&str, [u64; 7])] = &[
    ("avc w1 ppsfalse", [17285422641446447198, 2230745833886204557, 1851689215705814961, 3600195679073255243, 5785690239773324103, 8985622630246532857, 17093610390918574579]),
    ("avc w1 ppstrue", [7292547070108656523, 6219428052987079028, 5426596406728002057, 4059796351807156027, 11248048960559478167, 17446618979745672059, 12945105085499480556]),
    ("avc w2 ppsfalse", [13320358982540235776, 704802303418140627, 6393635778999536101, 12808561517600387807, 15600147818262144622, 10720508017996173579, 14934037948031699461]),
    ("avc w2 ppstrue", [1651079812368923459, 7159902234480446250, 4176109395577293616, 1271188596686225159, 7536149940548402753, 7372677758568387901, 7058009841815645242]),
    ("avc w3", [2735698410748821240, 15891576266584841766, 7112799842658413573, 12826899343543782222, 155125124960427980, 13924994869714577066, 17239154548795879504]),
    ("avc w4 ppsfalse", [9511971198604021555, 14971294623749304775, 11648650837513965491, 15071855748491109263, 12588134226573659510, 8659294971060992280, 15183802368349871725]),
    ("avc w4 ppstrue", [4377003201874653796, 6409996617679025497, 5034666670769667812, 1773738127689045441, 7016015680472691390, 16173503166612842427, 8615067012612341599]),
    ("hevc duplicate w1", [9910832580902805424, 16693421327012712824, 14806096172208465617, 3907799959703786872, 14315106801699851244, 17380935361047004180, 5110946391222145513]),
    ("hevc duplicate w2", [7889000915536867037, 15317313688827186766, 1491104451998451482, 6840485378232770263, 16802101409200942319, 8298918724005046630, 5162746583755986696]),
    ("hevc duplicate w4", [18216424066115706969, 17818399654068929064, 11293517409646185751, 6507176767200657860, 11447282582501597548, 17911020415163518458, 10838682555584175695]),
    ("hevc empty-arrays w1", [15037957799422336682, 2871877818860412517, 15228284866398606783, 16782416160339602337, 2526546141509505362, 3371723338171011895, 15074062615366473322]),
    ("hevc empty-arrays w2", [8686426842094858392, 18333159237409484480, 4402575786173683043, 14596561654972059202, 9127713816652606216, 1209234932182677436, 2542167805084328437]),
    ("hevc empty-arrays w4", [11171841595408658893, 17252818562002243336, 7053256611627514659, 2065908070620540034, 13217012529370648955, 9766721017215115255, 10688944878037656461]),
    ("hevc plain w1", [6292022141916125438, 15101438785878153920, 3720024711352517481, 3153850372161614088, 3130469518382731156, 8451499472871459028, 17750192394114503736]),
    ("hevc plain w2", [2704274500857002276, 14137397594594404466, 11021245192143375994, 730918576033554300, 15827790947603256608, 4267081443161692293, 460897734625687883]),
    ("hevc plain w4", [12224180871905310000, 14815161814345758775, 1073711575734980618, 16354064068410710558, 6033342953478878295, 8439669247570285330, 786736307712287329]),
    ("hevc unsupported-array w1", [5476184788772367702, 17329045091883146261, 1585497751309825912, 8675395922970757620, 7671894141062228825, 9917847880055692583, 6547437202448059471]),
    ("hevc unsupported-array w2", [16134014497336672789, 6671215383318841174, 14780082398275901319, 550483467134043331, 8673181242587996250, 11832768706570633880, 9741475637790691360]),
    ("hevc unsupported-array w4", [3823235549170668027, 8215988849999512364, 5268001295051750937, 4347026750708315517, 3989390597320303800, 14295804104707713338, 6647512058677516930]),
];

/// The same runs without the pool charges, which are the same on every target.
#[rustfmt::skip]
const PORTABLE_DIGESTS: &[(&str, [u64; 7])] = &[
    ("avc w1 ppsfalse", [16482473703044354144, 2988823682605130670, 5786023407764052158, 15961910444181409628, 12518903802784237753, 3479998515002717514, 1054061539556039614]),
    ("avc w1 ppstrue", [17720783897794380135, 7337748734338931565, 12630152151861746082, 2841055154519300491, 5927051449560626686, 10148613368200490578, 16821081859354755702]),
    ("avc w2 ppsfalse", [3843477265536178866, 15184256661972586255, 6278445946435472870, 2446472929268731056, 5616176255053665715, 18193010230675587545, 1728979171689668198]),
    ("avc w2 ppstrue", [608217024012141315, 713658066555836443, 6612703360216815976, 15844476091979746547, 15177567544861438682, 4097317899360212410, 10442844577061575198]),
    ("avc w3", [6661991787235899415, 17539741892943926928, 9020879800894086890, 10186649022535435649, 17920231471902251044, 6121770616292289630, 8923117727738655443]),
    ("avc w4 ppsfalse", [12736028767780818506, 1427485424111642678, 13142305869876998336, 5823125766842033814, 1090307531995082567, 15201152448746575982, 3317017104609100604]),
    ("avc w4 ppstrue", [11227443814607072461, 15871474107291218334, 10166067730736528340, 3902574816388995611, 649025713924741222, 15247517016113099252, 5263830377423273802]),
    ("hevc duplicate w1", [16243320417335501758, 4186372945847276751, 1179514290971950982, 18140994449715506028, 18174560594134842466, 16096497267669251917, 2388132538558519264]),
    ("hevc duplicate w2", [4450349318808195949, 2339467378489442689, 2278783764424512103, 8813492516808463343, 12829383141671085361, 17608657330778461568, 2130480469907032562]),
    ("hevc duplicate w4", [9076335816396479147, 5147131813081883797, 13791531953995301531, 4202831857517607927, 10456095505411367345, 14406440517943573830, 14187961509594422459]),
    ("hevc empty-arrays w1", [8450212440552226539, 1103830480759064857, 12645859570567745139, 15467494472855889425, 17878940215734712039, 9803731230194155046, 7187267039098296336]),
    ("hevc empty-arrays w2", [9217732355416913946, 15039716209771327237, 9634174244281615610, 12008039199315165966, 12470100314871950722, 12036274939774970355, 5071838794639876149]),
    ("hevc empty-arrays w4", [16979690879069390576, 16777912774930279963, 6856967340097334828, 7120272360716313806, 9561943461439102358, 4572125011179992572, 8434003607260246530]),
    ("hevc plain w1", [9779519966609083676, 10918348941643432182, 2147977342916875914, 18370692667226177098, 6096705807252605812, 3579943433515494325, 15664339976204092431]),
    ("hevc plain w2", [14212147313313368139, 3741041856034122416, 11430466071928683293, 10955589960382294241, 6670332169620791553, 14569459135136849334, 8222394346438571167]),
    ("hevc plain w4", [4474151209360114561, 7121422310294561860, 5211183938209617315, 12045941434118391893, 3027091505662353745, 10911754708294328726, 4851191852671263004]),
    ("hevc unsupported-array w1", [5178945453032719339, 14155065510984607485, 10164932175155839421, 719628632202674839, 13629870051068623747, 14448108059109648404, 6653247413666289545]),
    ("hevc unsupported-array w2", [15380539554901452508, 17844604862602052766, 13854471526773284702, 11301971913605592392, 3308038433195786420, 7470407299031275491, 12583679146846322506]),
    ("hevc unsupported-array w4", [2828530323047394374, 36690609537497972, 14116979246125070132, 4120846298602538026, 3045530713844000990, 16222870935366611357, 7274594148499682408]),
];

/// Check each scenario's facts, work and charged storage at every quantum.
fn check_receives(codec: Codec, record: &[u8], packets: &[Packet], label: &str) -> Vec<String> {
    let find = |table: &[(&str, [u64; 7])]| {
        let Some(&(_, digests)) = table.iter().find(|(l, _)| *l == label) else {
            panic!("no digests for {label}");
        };
        digests
    };
    let (expected, portable) = (find(DIGESTS), find(PORTABLE_DIGESTS));
    let mut charge_failures = Vec::new();
    for ((q, expected), portable) in QUANTA.into_iter().zip(expected).zip(portable) {
        let run = receive(codec, record, packets, q);
        // Every scenario reaches the packet engine or the decoder-array refusal.
        assert!(
            run.count("init-rejection Some") == 1
                || (run.count("out ") >= 8 && run.count("rejection Some") >= 1),
            "{label} quantum {q}"
        );
        if let Ok(directory) = std::env::var("TRIERGON_PACKET_EVENTS") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("{label}-{q}.trace")),
                run.charged_lines().collect::<Vec<_>>().join("\n") + "\n",
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{label}-{q}.digest")),
                run.digest().to_string(),
            )
            .unwrap();
        }
        assert_eq!(run.portable_digest(), portable, "{label} quantum {q}");
        // The charges were recorded on a 64-bit target.
        if cfg!(target_pointer_width = "64") && run.digest() != expected {
            // Collect every trace before failing, so storage changes can be
            // reviewed against the complete event set before updating pins.
            charge_failures.push(format!(
                "{label} quantum {q}: charge digest {} != {expected}",
                run.digest()
            ));
        }
    }
    charge_failures
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
    let mut charge_failures = Vec::new();
    for width in [1u8, 2, 4] {
        for extra_pps in [false, true] {
            let record = avc_record(width, extra_pps);
            let packets = avc_packets(usize::from(width), &record);
            charge_failures.extend(check_receives(
                Codec::Avc,
                &record,
                &packets,
                &format!("avc w{width} pps{extra_pps}"),
            ));
        }
    }
    // Observed three-byte width keeps its source refusal on every output.
    let record = avc_record(3, true);
    charge_failures.extend(check_receives(
        Codec::Avc,
        &record,
        &avc_packets(3, &record),
        "avc w3",
    ));
    assert!(charge_failures.is_empty(), "{charge_failures:#?}");
}

#[test]
fn hevc_receiver_publishes_and_rejects_for_every_array_shape_and_quantum() {
    let mut charge_failures = Vec::new();
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
            charge_failures.extend(check_receives(
                Codec::Hevc,
                &record,
                &packets,
                &format!("hevc {name} w{width}"),
            ));
        }
    }
    assert!(charge_failures.is_empty(), "{charge_failures:#?}");
}
