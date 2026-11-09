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
