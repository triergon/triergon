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
    ("avc w1 ppsfalse", [4418330204671502470, 7449949531837460278, 8695124573122500607, 9481729284554181735, 10207602519990863475, 8033649898058253744, 8069778484836380580]),
    ("avc w1 ppstrue", [16225289171070223536, 3387119235510163025, 2340803335905531758, 11912235583603506078, 8053532090879454367, 11010003472950896996, 8561368428880665395]),
    ("avc w2 ppsfalse", [14276396201330122432, 6345117587877258811, 16876951407020233327, 11011646661194813705, 29555688408091297, 10360575670166905740, 8374666913085672456]),
    ("avc w2 ppstrue", [8960917237146222406, 18301477655369777309, 2210426438252357277, 4014023020201081507, 1326886371383368845, 5591003610460553371, 1717704015767059845]),
    ("avc w3", [6406153395097990936, 2589343449689901326, 643118395463042564, 11568626163572956698, 10436280829509291845, 9991003618778519281, 9966043970347345847]),
    ("avc w4 ppsfalse", [14347049478569958527, 10104417666273329012, 8635319231213782285, 10967282728551764925, 5655282093577565414, 8636451161062875313, 16210409831458388993]),
    ("avc w4 ppstrue", [4348378652438792245, 10942866992095354395, 15816903003615152444, 1796376683000612500, 13211867039605314523, 10830720833384334746, 17213905062885628993]),
    ("hevc duplicate w1", [11727728702027494025, 7414427304660315616, 1817785007770809974, 12571648485973509954, 3153474400013350282, 17128020658943242985, 17618075856014778381]),
    ("hevc duplicate w2", [16816806109133509990, 11769251492282179958, 7342241523633630801, 4636733120617690188, 2126011529403792511, 4873915915949937624, 4015945133816519762]),
    ("hevc duplicate w4", [18400079418597804629, 18147915471423402467, 1996305224940476796, 15572850879505436498, 17563051928803886474, 16242965851958269467, 2673450737920712147]),
    ("hevc empty-arrays w1", [14462187495883043233, 11790353043237025258, 6201632495744888822, 3409716912808089351, 9576027947134224536, 13999316447947301990, 12174180436113173246]),
    ("hevc empty-arrays w2", [8882430916472964279, 2308313333460562115, 14871784045200525355, 138933959563676263, 9998976378867400962, 12797459644369840253, 3597747990813164361]),
    ("hevc empty-arrays w4", [10823684903402338566, 12477925237855661820, 14965016194135242005, 9186840991212185442, 9244232414203035970, 14555238688340035910, 15576334647452414745]),
    ("hevc plain w1", [14717486767691305505, 6267779509476894908, 15704514481951212445, 7073033268481659301, 7143374617807399877, 9461715629107789780, 3028940437476654199]),
    ("hevc plain w2", [17012709019478116814, 9506353605008364642, 821999777467623435, 17703179914819765698, 18421154609185230903, 736381268770068964, 3655579807457586806]),
    ("hevc plain w4", [14777600694543154760, 13459842046557531231, 8428461350230522981, 10538453646005746435, 5339645600650651273, 3603155295181131188, 9126338616412648048]),
    ("hevc unsupported-array w1", [9760657034952486696, 1909685243231031197, 6530699571153027736, 2672250236220841708, 7444361582556524331, 1565554288769659725, 9838977261231973581]),
    ("hevc unsupported-array w2", [9980674468244188663, 12510024032811284670, 4615778744638086439, 17921271603648022459, 17645955684425257500, 17281371535035820590, 7108050433788582830]),
    ("hevc unsupported-array w4", [9139310300119093833, 134922366974068244, 18047749306727472057, 12161628579397686997, 4717624351277987974, 17443595228734678340, 7270274127487440580]),
];

/// The same runs without the pool charges, which are the same on every target.
#[rustfmt::skip]
const PORTABLE_DIGESTS: &[(&str, [u64; 7])] = &[
    ("avc w1 ppsfalse", [2256347623992834736, 4265673472245162728, 15179184187654749746, 14183514230321850004, 10733758782645297225, 10385725381043726226, 7899794078024733482]),
    ("avc w1 ppstrue", [673931533190296135, 11569350596395407627, 17042810944419291250, 17053433291019092563, 14600224433648516894, 17137894179538144598, 3901527180633955406]),
    ("avc w2 ppsfalse", [1333730246525783658, 16481891829050580865, 15458473768405367110, 12835335555160797256, 2181590641699057207, 10770550681163530829, 16589189909287955982]),
    ("avc w2 ppstrue", [2672104647541187067, 990982567357414665, 13336318473568418580, 11749976466042846923, 5890670006291799502, 9635557289109933814, 470434865908464858]),
    ("avc w3", [14794570997764277023, 3942294613383599726, 12890687309931322494, 16349812548964505545, 10552020776402667272, 1472709340843449154, 4644804317486818587]),
    ("avc w4 ppsfalse", [2771369101774375842, 7422381978175873312, 8086263135789596404, 5071467325665359230, 12840526548302979639, 15000201055667970146, 14138028527097126004]),
    ("avc w4 ppstrue", [12957772225580837573, 17922710448879971380, 5478737309477967284, 11041241092048836835, 733269565343069318, 18041960493422402684, 11575726872333999118]),
    ("hevc duplicate w1", [3807593546944538082, 9341538096988435899, 2986500487649268218, 1606637359708142752, 16960322981971877170, 8022824650958261625, 9589664023962805108]),
    ("hevc duplicate w2", [9059684322830037237, 12714935978012710789, 2979174008314602419, 12941609899127453151, 12313172972877992617, 9999246069050355548, 10394502468612717250]),
    ("hevc duplicate w4", [16023726762582414611, 17735381764492022889, 13856624053045341443, 707584774695000347, 17654158894063860901, 6927230372977447786, 5260356241055734295]),
    ("hevc empty-arrays w1", [17628941545287898271, 4088224800533671157, 13008012973909832347, 5724959958633547481, 408887850132911475, 2819163014179527510, 16691644663871955096]),
    ("hevc empty-arrays w2", [15551017428459297090, 10455839012469780273, 11965778491528756246, 17317721042073365910, 4886024492691062822, 12163574026891353463, 10861673632531298777]),
    ("hevc empty-arrays w4", [6533590736436913496, 17161162342851925719, 9740457345307462296, 1542060123559435814, 4234586079152330638, 51559739484233620, 18348119467985174682]),
    ("hevc plain w1", [2906661291560124328, 2740011613605216506, 5141227049611859962, 9299217723769507158, 10106706095059315164, 9103600998391546905, 7987420119865469659]),
    ("hevc plain w2", [10209181214667542147, 9312304363666431564, 17204778475029579189, 11778581045963711521, 8516161193010253717, 12753858519870983018, 13931392850846800403]),
    ("hevc plain w4", [11477059864794006601, 9954995569936385152, 12194593930012743111, 16095011437634768657, 16340096133160850501, 11024078131276237554, 15079941682612678308]),
    ("hevc unsupported-array w1", [15961570404249900217, 11820551920314727283, 6347649427229525621, 1850938885543510223, 9777632856047298019, 5701286876594803038, 17709725954489604001]),
    ("hevc unsupported-array w2", [6907556066812535354, 11422264675295448804, 15145571379425607734, 5044977320886142112, 16755333616125670932, 2011747524977357757, 14854009784908230114]),
    ("hevc unsupported-array w4", [4444520668675455896, 13959612938313425998, 14786629297830374284, 1951013741772967682, 10683691208253335742, 16285626213819425795, 4351122034030628480]),
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
