//! PMT codec classification (H.222.0 Table 2-34, EN 300 468 descriptors), the
//! demuxer's reading of PMT entries and its PAT/PMT acceptance.
use std::sync::atomic::AtomicBool;
use triergon_core::container::CodecId;
use triergon_core::{Budget, Bytes, SourceId, WorkBudget};
use triergon_mpegts::compat::stream::{self, Program, ProgramReader};
use triergon_mpegts::psi::{
    DescriptorNamespace, DvbNamespaceEvidence, ElementaryStream, PmtReader, PsiCrc32,
    SectionAssembler, TableStep,
};
use triergon_mpegts::{AssemblyStep, PacketFormat, ParseLimits, parse_transport};

const DVB: DescriptorNamespace =
    DescriptorNamespace::Dvb(DvbNamespaceEvidence::CallerDeclaredTransportProfile);
/// Two subtitling services: "eng" type 0x10 pages 1/2, "deu" type 0x20 pages 0x0102/0x0304.
const SUBTITLING: [u8; 18] = [
    0x59, 16, b'e', b'n', b'g', 0x10, 0, 1, 0, 2, b'd', b'e', b'u', 0x20, 1, 2, 3, 4,
];
/// Two teletext pages: subtitles (type 2) magazine 8 page 0x88, and an initial page.
const TELETEXT: [u8; 12] = [
    0x56, 10, b'e', b'n', b'g', 0x10, 0x88, b'e', b'n', b'g', 0x09, 0x00,
];

/// A PSI section with its CRC; `reserved` is the value of every reserved bit.
fn section(table: u8, extension: u16, body: &[u8], reserved: bool) -> Vec<u8> {
    let bits = if reserved { 0xff } else { 0x00 };
    let length = body.len() + 9;
    let mut data = vec![
        table,
        0x80 | (bits & 0x30) | (length >> 8) as u8,
        length as u8,
        (extension >> 8) as u8,
        extension as u8,
        (bits & 0xc0) | 1,
        0,
        0,
    ];
    data.extend_from_slice(body);
    let crc = PsiCrc32::of(&data);
    data.extend_from_slice(&crc.to_be_bytes());
    data
}

fn pmt_body(pcr: u16, streams: &[(u8, u16, &[u8])], reserved: bool) -> Vec<u8> {
    let bits = if reserved { 0xe0 } else { 0x00 };
    let info = if reserved { 0xf0 } else { 0x00 };
    let mut body = vec![bits | (pcr >> 8) as u8, pcr as u8, info, 0];
    for (kind, pid, descriptors) in streams {
        body.extend_from_slice(&[
            *kind,
            bits | (pid >> 8) as u8,
            *pid as u8,
            info | (descriptors.len() >> 8) as u8,
            descriptors.len() as u8,
        ]);
        body.extend_from_slice(descriptors);
    }
    body
}

/// The streams of a PMT read by the standard reader under `namespace`.
fn standard(streams: &[(u8, u16, &[u8])], namespace: DescriptorNamespace) -> Vec<ElementaryStream> {
    let pool = Budget::new(1 << 20).unwrap();
    let mut payload = vec![0];
    payload.extend(section(2, 1, &pmt_body(0x100, streams, true), true));
    let mut assembler =
        SectionAssembler::new(0x1000, pool.clone(), ParseLimits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    assembler
        .push(
            Bytes::copy_from_slice(&pool, &payload).unwrap(),
            true,
            SourceId(1),
            0,
        )
        .unwrap();
    let AssemblyStep::Output(section) = assembler
        .poll(&mut WorkBudget::new(u64::MAX), &cancel)
        .unwrap()
    else {
        panic!("no section");
    };
    let mut reader =
        PmtReader::new_with_namespace(&section, &pool, ParseLimits::default(), namespace).unwrap();
    loop {
        match reader
            .poll(&mut WorkBudget::new(u64::MAX), &cancel)
            .unwrap()
        {
            TableStep::Output(map) => return map.streams.as_slice().to_vec(),
            TableStep::Yield => {}
            TableStep::Done => panic!("no PMT"),
        }
    }
}

#[test]
fn standard_codecs_follow_stream_types_and_dvb_descriptors_only_under_dvb() {
    let streams: [(u8, u16, &[u8]); 8] = [
        (0x01, 0x101, &[]),
        (0x02, 0x102, &[]),
        (0x0f, 0x103, &[]),
        (0x11, 0x104, &[]),
        (0x1b, 0x105, &[]),
        (0x24, 0x106, &[]),
        (0x06, 0x107, &SUBTITLING),
        (0x90, 0x108, &[]),
    ];
    let codecs = |namespace| {
        standard(&streams, namespace)
            .iter()
            .map(ElementaryStream::codec)
            .collect::<Vec<_>>()
    };
    let named = [
        Some(CodecId::Mpeg2Video),
        Some(CodecId::Mpeg2Video),
        Some(CodecId::Aac),
        Some(CodecId::AacLatm),
        Some(CodecId::H264),
        Some(CodecId::Hevc),
    ];
    let unspecified = codecs(DescriptorNamespace::Unspecified);
    assert_eq!(unspecified[..6], named);
    assert_eq!(unspecified[6..], [None, None]);
    let dvb = codecs(DVB);
    assert_eq!(dvb[..6], named);
    assert_eq!(dvb[6..], [Some(CodecId::DvbSubtitle), None]);
}

#[test]
fn the_first_codec_descriptor_names_private_data() {
    let teletext_first = [&TELETEXT[..], &SUBTITLING[..]].concat();
    let ac3 = [0x6a, 1, 0];
    let eac3 = [0x52, 1, 7, 0x7a, 1, 0];
    let streams: [(u8, u16, &[u8]); 5] = [
        (0x06, 0x101, &teletext_first),
        (0x06, 0x102, &ac3),
        (0x06, 0x103, &eac3),
        (0x06, 0x104, &[0x52, 1, 7]),
        (0x06, 0x105, &[0x59, 9, 0]), // malformed: the loop ends
    ];
    let codecs: Vec<_> = demuxed(&streams)
        .iter()
        .map(ElementaryStream::dvb_descriptor_codec)
        .collect();
    assert_eq!(
        codecs,
        [
            Some(CodecId::DvbTeletext),
            Some(CodecId::Ac3),
            Some(CodecId::Eac3),
            None,
            None
        ]
    );
}

/// Transport packets carrying `payload` on `pid`, padded with adaptation stuffing.
fn packets(pid: u16, payload: &[u8], counter: &mut u8) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, chunk) in payload.chunks(184).enumerate() {
        let start = if index == 0 { 0x40 } else { 0 };
        out.extend_from_slice(&[0x47, start | (pid >> 8) as u8, pid as u8]);
        let stuffing = 184 - chunk.len();
        if stuffing == 0 {
            out.push(0x10 | *counter);
        } else {
            out.push(0x30 | *counter);
            out.push((stuffing - 1) as u8);
            if stuffing > 1 {
                out.push(0);
                out.extend(std::iter::repeat_n(0xff, stuffing - 2));
            }
        }
        out.extend_from_slice(chunk);
        *counter = (*counter + 1) & 15;
    }
    out
}

fn psi(pid: u16, section: &[u8], counter: &mut u8) -> Vec<u8> {
    packets(pid, &[&[0][..], section].concat(), counter)
}

/// Every PMT the reader reports for the transport stream `ts`.
fn programs(ts: &[u8]) -> Vec<Program> {
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = ProgramReader::new(pool.clone(), ParseLimits::default()).unwrap();
    let mut out = Vec::new();
    for (index, raw) in ts.chunks_exact(188).enumerate() {
        let packet = parse_transport(
            Bytes::copy_from_slice(&pool, raw).unwrap(),
            PacketFormat::Ts188,
            188 * index as u64,
        )
        .unwrap();
        reader.push(&packet).unwrap();
        while let Some(program) = reader.poll().unwrap() {
            out.push(program);
        }
    }
    out
}

/// The streams of a PMT read by the demuxer's reader.
fn demuxed(streams: &[(u8, u16, &[u8])]) -> Vec<ElementaryStream> {
    let mut counter = (0, 0);
    let pat = section(0, 1, &[0, 1, 0xe0, 0x20], true);
    let pmt = section(2, 1, &pmt_body(0x100, streams, true), true);
    let mut ts = psi(0, &pat, &mut counter.0);
    ts.extend(psi(0x20, &pmt, &mut counter.1));
    let found = programs(&ts);
    assert_eq!(found.len(), 1);
    found[0].streams.as_slice().to_vec()
}

#[test]
fn tables_are_read_whatever_their_reserved_bits_and_reported_once() {
    let (mut pat_counter, mut pmt_counter) = (0, 0);
    let pat = section(0, 1, &[0, 0, 0, 0x10, 0, 7, 0x00, 0x20], false);
    let pmt = section(
        2,
        7,
        &pmt_body(
            0x1fff,
            &[(0x90, 0x101, &[]), (0x06, 0x102, &SUBTITLING)],
            false,
        ),
        false,
    );
    let mut ts = Vec::new();
    for _ in 0..3 {
        ts.extend(psi(0, &pat, &mut pat_counter));
        // The same packet again, as a duplicate.
        ts.extend_from_within(ts.len() - 188..);
        ts.extend(psi(0x20, &pmt, &mut pmt_counter));
    }
    let found = programs(&ts);
    assert_eq!(found.len(), 1);
    let program = &found[0];
    assert_eq!(program.pmt_pid, 0x20);
    assert_eq!((program.number, program.pcr_pid), (7, None));
    let codecs: Vec<_> = program
        .streams
        .as_slice()
        .iter()
        .map(stream::codec)
        .collect();
    assert_eq!(codecs, [Some(CodecId::Pgs), Some(CodecId::DvbSubtitle)]);
}

#[test]
fn cooperative_discovery_resumes_after_zero_work_and_cancellation() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut reader = ProgramReader::new(pool.clone(), ParseLimits::default()).unwrap();
    let mut counter = (0, 0);
    let mut ts = psi(0, &section(0, 1, &[0, 1, 0xe0, 0x20], true), &mut counter.0);
    ts.extend(psi(
        0x20,
        &section(2, 1, &pmt_body(0x100, &[(0x1b, 0x100, &[])], true), true),
        &mut counter.1,
    ));
    let cancel = AtomicBool::new(false);
    let mut outputs = 0;
    for raw in ts.chunks_exact(188) {
        let packet = parse_transport(
            Bytes::copy_from_slice(&pool, raw).unwrap(),
            PacketFormat::Ts188,
            0,
        )
        .unwrap();
        reader.push(&packet).unwrap();
        assert!(matches!(
            reader
                .poll_with_work(&mut WorkBudget::new(0), &cancel)
                .unwrap(),
            AssemblyStep::Yield
        ));
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            reader.poll_with_work(&mut WorkBudget::new(1), &cancel),
            Err(triergon_core::Error::Cancelled)
        ));
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        let mut drained = false;
        for _ in 0..1024 {
            match reader
                .poll_with_work(&mut WorkBudget::new(1), &cancel)
                .unwrap()
            {
                AssemblyStep::Output(program) => {
                    outputs += 1;
                    assert_eq!(
                        (program.number, program.pmt_pid, program.pcr_pid),
                        (1, 0x20, Some(0x100))
                    );
                    assert_eq!(program.streams.as_slice()[0].pid, 0x100);
                }
                AssemblyStep::Yield => {}
                AssemblyStep::NeedPayload => {
                    drained = true;
                    break;
                }
                AssemblyStep::Finished => panic!("unexpected end"),
            }
        }
        assert!(drained);
    }
    assert_eq!(outputs, 1);
}

#[test]
fn damaged_and_pending_sections_are_skipped_and_long_ones_assembled() {
    let mut counter = (0, 0);
    let pat = section(0, 1, &[0, 1, 0xe0, 0x20], true);
    let mut bad = section(2, 1, &pmt_body(0x100, &[(0x1b, 0x100, &[])], true), true);
    let last = bad.len() - 1;
    bad[last] ^= 1;
    let mut pending = section(2, 1, &pmt_body(0x100, &[(0x02, 0x100, &[])], true), true);
    pending[5] &= !1;
    let crc = PsiCrc32::of(&pending[..pending.len() - 4]);
    let at = pending.len() - 4;
    pending[at..].copy_from_slice(&crc.to_be_bytes());
    // 40 registration descriptors make the section span two packets.
    let registrations: Vec<u8> = (0..40)
        .flat_map(|_| [5, 4, b'H', b'D', b'M', b'V'])
        .collect();
    let long = section(
        2,
        1,
        &pmt_body(0x100, &[(0x24, 0x100, &registrations)], true),
        true,
    );
    let mut ts = psi(0, &pat, &mut counter.0);
    for table in [&bad, &pending, &long] {
        ts.extend(psi(0x20, table, &mut counter.1));
    }
    let found = programs(&ts);
    assert_eq!(found.len(), 1);
    let streams = found[0].streams.as_slice();
    assert_eq!(streams.len(), 1);
    assert_eq!(stream::codec(&streams[0]), Some(CodecId::Hevc));
    assert_eq!(streams[0].descriptor_bytes.len(), registrations.len());
}

#[test]
fn demuxer_codecs_add_private_types_and_read_dvb_tags_in_any_namespace() {
    let streams: [(u8, u16, &[u8]); 6] = [
        (0x81, 0x101, &[]),
        (0x87, 0x102, &[]),
        (0x90, 0x103, &[]),
        (0x06, 0x104, &SUBTITLING),
        (0x06, 0x105, &TELETEXT),
        (0x1b, 0x106, &[]),
    ];
    let codecs: Vec<_> = demuxed(&streams).iter().map(stream::codec).collect();
    assert_eq!(
        codecs,
        [
            Some(CodecId::Ac3),
            Some(CodecId::Eac3),
            Some(CodecId::Pgs),
            Some(CodecId::DvbSubtitle),
            Some(CodecId::DvbTeletext),
            Some(CodecId::H264),
        ]
    );
}

#[test]
fn configuration_packs_services_and_pages() {
    let streams: [(u8, u16, &[u8]); 3] = [
        (0x06, 0x101, &SUBTITLING),
        (0x06, 0x102, &TELETEXT),
        (0x1b, 0x103, &[]),
    ];
    let found = demuxed(&streams);
    let configuration = |n: usize| stream::configuration(&found[n]).collect::<Vec<_>>();
    assert_eq!(configuration(0), [0, 1, 0, 2, 0x10, 1, 2, 3, 4, 0x20]);
    assert_eq!(configuration(1), [0x10, 0x88, 0x09, 0x00]);
    assert!(configuration(2).is_empty());
}

#[test]
fn video_is_counted_by_stream_type() {
    let video = [
        0x01, 0x02, 0x10, 0x1b, 0x20, 0x24, 0x33, 0x42, 0xd1, 0xd2, 0xea,
    ];
    let other = [
        0x03, 0x04, 0x06, 0x0f, 0x11, 0x15, 0x1c, 0x81, 0x86, 0x87, 0x90,
    ];
    let streams: Vec<(u8, u16, &[u8])> = other
        .iter()
        .chain(&video)
        .enumerate()
        .map(|(n, &kind)| (kind, 0x100 + n as u16, &[][..]))
        .collect();
    let counted: Vec<u8> = demuxed(&streams)
        .iter()
        .filter(|es| stream::is_video(es))
        .map(|es| es.stream_type)
        .collect();
    assert_eq!(counted, video);
}

/// Cumulative authored PMTs: every prefix is independently observable by ffprobe.
fn identity_cascade(maps: &[Vec<(u8, u16, &'static [u8])>]) -> Vec<u8> {
    let mut counter = (0, 0);
    let mut ts = psi(0, &section(0, 1, &[0, 1, 0xe0, 0x20], true), &mut counter.0);
    let mut pes_counters = std::collections::BTreeMap::new();
    for (version, streams) in maps.iter().enumerate() {
        let mut map = section(2, 1, &pmt_body(0x100, streams, true), true);
        map[5] = 0xc1 | ((version as u8 & 31) << 1);
        let end = map.len() - 4;
        let crc = PsiCrc32::of(&map[..end]);
        map[end..].copy_from_slice(&crc.to_be_bytes());
        ts.extend(psi(0x20, &map, &mut counter.1));
        for (ordinal, (_, pid, _)) in streams.iter().enumerate() {
            let marker = [
                0,
                0,
                1,
                0xbd,
                0,
                7,
                0x80,
                0,
                0,
                version as u8,
                ordinal as u8,
                0xfe,
                0xed,
            ];
            ts.extend(packets(
                *pid,
                &marker,
                pes_counters.entry(*pid).or_insert(0),
            ));
        }
    }
    // Force ordinary transport probing even for a one-map prefix.
    for _ in 0..10 {
        ts.extend_from_slice(&[0x47, 0x1f, 0xff, 0x10]);
        ts.extend(std::iter::repeat_n(0xff, 184));
    }
    ts
}

type IdentityMaps = Vec<Vec<(u8, u16, &'static [u8])>>;

fn identity_cases() -> Vec<(&'static str, IdentityMaps)> {
    vec![
        (
            "replace",
            vec![
                vec![(2, 256, &[][..]), (0x81, 257, &[])],
                vec![(2, 512, &[]), (0x81, 513, &[])],
                vec![(2, 768, &[]), (0x81, 769, &[])],
            ],
        ),
        (
            "reorder",
            vec![
                vec![(0x81, 256, &[]), (0x81, 257, &[])],
                vec![(0x81, 513, &[]), (0x81, 512, &[])],
                vec![(0x81, 768, &[]), (0x81, 769, &[])],
            ],
        ),
        (
            "type",
            vec![
                vec![(2, 256, &[])],
                vec![(0x81, 512, &[])],
                vec![(2, 768, &[])],
            ],
        ),
        (
            "same-pid-type",
            vec![
                vec![(2, 256, &[])],
                vec![(0x81, 256, &[])],
                vec![(2, 512, &[])],
            ],
        ),
        (
            "remove",
            vec![
                vec![(2, 256, &[]), (0x81, 257, &[])],
                vec![(2, 512, &[])],
                vec![(2, 768, &[]), (0x81, 769, &[])],
            ],
        ),
        (
            "survive",
            vec![
                vec![(2, 256, &[]), (0x81, 257, &[])],
                vec![(0x81, 257, &[]), (2, 512, &[])],
                vec![(0x81, 768, &[]), (2, 769, &[])],
            ],
        ),
        (
            "insert",
            vec![vec![(2, 256, &[])], vec![(0x81, 512, &[]), (2, 513, &[])]],
        ),
        (
            "component",
            vec![
                vec![(0x81, 256, &[0x52, 1, 1]), (0x81, 257, &[0x52, 1, 2])],
                vec![(0x81, 512, &[0x52, 1, 2]), (0x81, 513, &[0x52, 1, 1])],
            ],
        ),
        (
            "language",
            vec![
                vec![
                    (0x81, 256, &[10, 4, b'e', b'n', b'g', 0]),
                    (0x81, 257, &[10, 4, b's', b'p', b'a', 0]),
                ],
                vec![
                    (0x81, 512, &[10, 4, b's', b'p', b'a', 0]),
                    (0x81, 513, &[10, 4, b'e', b'n', b'g', 0]),
                ],
            ],
        ),
    ]
}

#[path = "../../../xtask/src/sha256.rs"]
mod sha256;

#[test]
#[ignore = "black-box probe: requires TRIERGON_PMT_FFPROBE pointing to pinned ffprobe"]
fn identity_black_box_cascades() {
    let probe = std::env::var_os("TRIERGON_PMT_FFPROBE").expect("pinned ffprobe path");
    assert_eq!(
        sha256::file_hex(std::path::Path::new(&probe)).unwrap(),
        "d9d0840413169e2ec50eb1a2e3c9bebf3aa5a15ab97217ac987ca9a901b33896"
    );
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.research/pmt-identity");
    std::fs::create_dir_all(&root).unwrap();
    let mut summary = String::new();
    for (name, maps) in identity_cases() {
        for end in 1..=maps.len() {
            let input = root.join(format!("{name}-{end}.ts"));
            let ts = identity_cascade(&maps[..end]);
            std::fs::write(&input, &ts).unwrap();
            summary.push_str(&format!("{name}-{end} input {}\n", sha256::hex(&ts)));
            for merge in ["0", "1"] {
                let output = std::process::Command::new(&probe)
                    .args([
                        "-v",
                        "error",
                        "-bitexact",
                        "-f",
                        "mpegts",
                        "-fflags",
                        "+noparse+nofillin",
                        "-merge_pmt_versions",
                        merge,
                        "-show_entries",
                        "stream=index,id,codec_tag:packet=stream_index,pos,size",
                        "-show_packets",
                        "-of",
                        "compact",
                        "-i",
                    ])
                    .arg(&input)
                    .output()
                    .unwrap();
                std::fs::write(
                    root.join(format!("{name}-{end}-{merge}.stdout")),
                    &output.stdout,
                )
                .unwrap();
                std::fs::write(
                    root.join(format!("{name}-{end}-{merge}.stderr")),
                    &output.stderr,
                )
                .unwrap();
                assert!(output.status.success());
                summary.push_str(&format!(
                    "merge={merge} stdout {} stderr {}\n",
                    sha256::hex(&output.stdout),
                    sha256::hex(&output.stderr)
                ));
                for line in std::str::from_utf8(&output.stdout).unwrap().lines() {
                    let field = |key: &str| {
                        line.split('|')
                            .find_map(|part| part.strip_prefix(key))
                            .unwrap()
                    };
                    if line.starts_with("packet|") {
                        let offset: usize = field("pos=").parse().unwrap();
                        let pid = (u16::from(ts[offset + 1] & 31) << 8) | u16::from(ts[offset + 2]);
                        summary.push_str(&format!(
                            "packet pid={pid} index={}\n",
                            field("stream_index=")
                        ));
                    } else if line.starts_with("stream|") {
                        summary.push_str(&format!(
                            "stream index={} id={}\n",
                            field("index="),
                            field("id=")
                        ));
                    }
                }
            }
        }
    }
    std::fs::write(root.join("observed.txt"), &summary).unwrap();
    assert_eq!(summary, include_str!("data/pmt-identity-cascades.txt"));
}

#[test]
fn positional_identities_match_supported_black_box_packet_assignments() {
    use stream::{PositionalStreamIds, StreamIdentity};
    let pinned = include_str!("data/pmt-identity-cascades.txt");
    for (name, maps) in identity_cases() {
        if matches!(name, "remove" | "survive" | "insert" | "component") {
            continue; // These probes establish explicit refusal boundaries below.
        }
        for end in 1..=maps.len() {
            let pool = Budget::new(1 << 20).unwrap();
            let mut identities = PositionalStreamIds::new(pool, ParseLimits::default()).unwrap();
            let mut packets = String::new();
            let found = programs(&identity_cascade(&maps[..end]));
            assert_eq!(found.len(), end);
            for program in &found {
                let assigned = identities.apply(program).unwrap();
                for (index, identity) in assigned.as_slice().iter().enumerate() {
                    assert_eq!(
                        *identity,
                        StreamIdentity {
                            id: maps[0][index].1,
                            pid: program.streams.as_slice()[index].pid
                        }
                    );
                    packets.push_str(&format!("packet pid={} index={index}\n", identity.pid));
                }
            }
            let section = pinned
                .split(&format!("{name}-{end} input "))
                .nth(1)
                .unwrap();
            let merged = section.split("merge=1 stdout ").nth(1).unwrap();
            let expected = merged
                .lines()
                .skip(1)
                .take_while(|line| line.starts_with("packet "))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            assert_eq!(packets, expected, "{name}-{end}");
        }
    }
}

#[test]
fn positional_identity_refusals_preserve_the_last_mapping() {
    use stream::PositionalStreamIds;
    for (name, maps) in identity_cases() {
        if !matches!(name, "remove" | "survive" | "insert" | "component") {
            continue;
        }
        let found = programs(&identity_cascade(&maps));
        let pool = Budget::new(1 << 20).unwrap();
        let mut identities = PositionalStreamIds::new(pool, ParseLimits::default()).unwrap();
        if name == "component" {
            assert!(matches!(
                identities.apply(&found[0]),
                Err(triergon_core::Error::Unsupported(_))
            ));
        } else {
            let first = identities.apply(&found[0]).unwrap();
            assert!(matches!(
                identities.apply(&found[1]),
                Err(triergon_core::Error::Unsupported(_))
            ));
            assert_eq!(
                identities.apply(&found[0]).unwrap().as_slice(),
                first.as_slice()
            );
        }
    }
    let mut found = programs(&identity_cascade(&[vec![(2, 256, &[]), (2, 257, &[])]]));
    let mut program = found.pop().unwrap();
    let pool = Budget::new(4096).unwrap();
    let mut identities = PositionalStreamIds::new(pool.clone(), ParseLimits::default()).unwrap();
    let first = identities.apply(&program).unwrap();
    program.number += 1;
    assert!(identities.apply(&program).is_err());
    program.number -= 1;
    program.pmt_pid += 1;
    assert!(identities.apply(&program).is_err());
    program.pmt_pid -= 1;
    program.streams.as_mut_slice()[1].pid = 256;
    assert!(matches!(
        identities.apply(&program),
        Err(triergon_core::Error::Invalid(_))
    ));
    program.streams.as_mut_slice()[1].pid = 257;
    let held = pool.reserve(pool.available()).unwrap();
    assert!(identities.apply(&program).is_err());
    drop(held);
    assert_eq!(
        identities.apply(&program).unwrap().as_slice(),
        first.as_slice()
    );
    let mut limited = PositionalStreamIds::new(
        pool,
        ParseLimits {
            streams: 1,
            ..ParseLimits::default()
        },
    )
    .unwrap();
    assert!(limited.apply(&program).is_err());
}

#[test]
fn positional_descriptor_limits_and_malformed_loops_are_refused() {
    use stream::PositionalStreamIds;
    let pool = Budget::new(1 << 20).unwrap();
    for (descriptors, limits) in [
        (&[10][..], ParseLimits::default()),
        (
            &[10, 0, 10, 0][..],
            ParseLimits {
                descriptors: 1,
                ..ParseLimits::default()
            },
        ),
        (
            &[10, 0, 10, 0][..],
            ParseLimits {
                section_bytes: 3,
                ..ParseLimits::default()
            },
        ),
    ] {
        let mut found = programs(&identity_cascade(&[vec![(2, 256, &[])]]));
        let mut program = found.pop().unwrap();
        program.streams.as_mut_slice()[0].descriptor_bytes =
            Bytes::copy_from_slice(&pool, descriptors).unwrap();
        let mut identities = PositionalStreamIds::new(pool.clone(), limits).unwrap();
        assert!(identities.apply(&program).is_err());
        program.streams.as_mut_slice()[0].descriptor_bytes =
            Bytes::copy_from_slice(&pool, &[]).unwrap();
        assert_eq!(identities.apply(&program).unwrap().as_slice()[0].id, 256);
    }
}
