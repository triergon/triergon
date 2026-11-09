//! The demuxer's PES packets for any stream type ([`PesCutter`]), its clock
//! ([`DemuxClock::place`]) and packet format detection.
use triergon_core::{Budget, Bytes};
use triergon_mpegts::compat::pes::{CutPes, DemuxBoundary, DemuxClock, PesCutter};
use triergon_mpegts::{PacketFormat, ParseLimits, TransportPacket, parse_transport};

const PID: u16 = 0x100;
const PCR_PID: u16 = 0x1ff;

/// One transport packet: `payload` on `pid`, padded with adaptation stuffing,
/// optionally carrying a PCR base.
fn packet(pid: u16, start: bool, payload: &[u8], pcr: Option<u64>) -> Vec<u8> {
    let mut adaptation = Vec::new();
    if let Some(base) = pcr {
        adaptation.extend_from_slice(&[
            0x10,
            (base >> 25) as u8,
            (base >> 17) as u8,
            (base >> 9) as u8,
            (base >> 1) as u8,
            ((base & 1) << 7) as u8 | 0x7e,
            0,
        ]);
    }
    let room = 184 - payload.len();
    let control = match (room, payload.is_empty()) {
        (0, _) => 0x10,
        (_, true) => 0x20,
        _ => 0x30,
    };
    let mut out = vec![
        0x47,
        u8::from(start) << 6 | (pid >> 8) as u8,
        pid as u8,
        control,
    ];
    if room > 0 {
        if adaptation.is_empty() && room > 1 {
            adaptation.push(0);
        }
        adaptation.resize(room - 1, 0xff);
        out.push((room - 1) as u8);
        out.extend_from_slice(&adaptation);
    }
    out.extend_from_slice(payload);
    assert_eq!(out.len(), 188);
    out
}

fn timestamp(prefix: u8, value: u64) -> [u8; 5] {
    [
        prefix << 4 | ((value >> 29) & 0x0e) as u8 | 1,
        (value >> 22) as u8,
        ((value >> 14) & 0xfe) as u8 | 1,
        (value >> 7) as u8,
        ((value << 1) & 0xfe) as u8 | 1,
    ]
}

/// A PES packet with stream ID 0xbd; `declared` false leaves its length 0.
fn pes(pts: Option<u64>, dts: Option<u64>, data: &[u8], declared: bool) -> Vec<u8> {
    let mut header = vec![0x80, 0, 0];
    if let Some(pts) = pts {
        header[1] = 0x80;
        header.extend(timestamp(if dts.is_some() { 3 } else { 2 }, pts));
    }
    if let Some(dts) = dts {
        header[1] = 0xc0;
        header.extend(timestamp(1, dts));
    }
    header[2] = (header.len() - 3) as u8;
    let length = if declared {
        header.len() + data.len()
    } else {
        0
    };
    let mut out = vec![0, 0, 1, 0xbd, (length >> 8) as u8, length as u8];
    out.extend(header);
    out.extend_from_slice(data);
    out
}

/// The PES packet split into transport payloads of at most `size` bytes.
fn carried(pes: &[u8], size: usize) -> Vec<Vec<u8>> {
    pes.chunks(size)
        .enumerate()
        .map(|(n, chunk)| packet(PID, n == 0, chunk, None))
        .collect()
}

/// Every packet the cutter emits for the transport packets `ts`.
fn cut(ts: &[Vec<u8>]) -> Vec<CutPes> {
    let pool = Budget::new(1 << 24).unwrap();
    let mut cutter = PesCutter::new(PID, pool.clone(), ParseLimits::default()).unwrap();
    cutter.set_pcr_pid(Some(PCR_PID));
    let mut out = Vec::new();
    for (index, raw) in ts.iter().enumerate() {
        let packet: TransportPacket = parse_transport(
            Bytes::copy_from_slice(&pool, raw).unwrap(),
            PacketFormat::Ts188,
            188 * index as u64,
        )
        .unwrap();
        cutter.push(&packet).unwrap();
        out.extend(std::iter::from_fn(|| cutter.pop()));
    }
    cutter.finish().unwrap();
    out.extend(std::iter::from_fn(|| cutter.pop()));
    out
}

/// A packet's PTS, DTS, boundary and data.
type Summary = (Option<u64>, Option<u64>, DemuxBoundary, Vec<u8>);

fn summary(packets: &[CutPes]) -> Vec<Summary> {
    packets
        .iter()
        .map(|p| {
            (
                p.pts_raw33,
                p.dts_raw33,
                p.boundary,
                p.data.as_ref().to_vec(),
            )
        })
        .collect()
}

#[test]
fn declared_packets_end_at_their_length_across_transport_packets() {
    let data: Vec<u8> = (0..=255).cycle().take(500).collect();
    let mut ts = carried(&pes(Some(9000), Some(6000), &data, true), 184);
    ts.extend(carried(&pes(Some(12_000), None, b"next", true), 184));
    let packets = cut(&ts);
    assert_eq!(
        summary(&packets),
        [
            (Some(9000), Some(6000), DemuxBoundary::FiniteLength, data),
            (
                Some(12_000),
                None,
                DemuxBoundary::FiniteLength,
                b"next".to_vec()
            ),
        ]
    );
    assert!(packets.iter().all(|p| p.stream_id == 0xbd));
}

#[test]
fn headers_split_across_transport_packets_are_joined() {
    for size in [1, 4, 7, 10, 13] {
        let packets = cut(&carried(
            &pes(Some(0x1_2345_6789), None, b"data", true),
            size,
        ));
        assert_eq!(
            summary(&packets),
            [(
                Some(0x1_2345_6789),
                None,
                DemuxBoundary::FiniteLength,
                b"data".to_vec()
            )],
            "{size}-byte payloads"
        );
    }
}

#[test]
fn unbounded_and_short_packets_end_at_the_next_start_or_the_end() {
    let mut ts = carried(&pes(Some(1), None, b"unbounded", false), 184);
    let mut short = pes(Some(2), None, b"declared longer", true);
    short.truncate(short.len() - 6);
    ts.extend(carried(&short, 184));
    ts.extend(carried(&pes(Some(3), None, b"last", false), 184));
    assert_eq!(
        summary(&cut(&ts)),
        [
            (
                Some(1),
                None,
                DemuxBoundary::NextPesStart,
                b"unbounded".to_vec()
            ),
            (
                Some(2),
                None,
                DemuxBoundary::NextPesStart,
                b"declared ".to_vec()
            ),
            (Some(3), None, DemuxBoundary::LogicalEof, b"last".to_vec()),
        ]
    );
}

#[test]
fn bytes_after_the_length_empty_packets_and_bad_starts_give_nothing() {
    let mut finite = pes(None, None, b"kept", true);
    finite.extend_from_slice(b"dropped");
    let mut ts = carried(&finite, 184);
    ts.extend(carried(&pes(Some(5), None, b"", true), 184));
    ts.push(packet(PID, true, &[0, 0, 2, 0xbd, 0, 3, 0x80, 0, 0], None));
    ts.push(packet(PID, false, b"continuation of nothing", None));
    assert_eq!(
        summary(&cut(&ts)),
        [(None, None, DemuxBoundary::FiniteLength, b"kept".to_vec())]
    );
}

#[test]
fn padding_streams_carry_data_without_an_optional_header() {
    let ts = [packet(
        PID,
        true,
        &[0, 0, 1, 0xbe, 0, 4, 0xff, 0xff, 0xff, 0xff],
        None,
    )];
    let packets = cut(&ts);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].stream_id, 0xbe);
    assert_eq!(packets[0].data.as_ref(), [0xff; 4]);
}

#[test]
fn the_program_clock_bounds_teletext_times() {
    let pes = |pts| packet(PID, true, &pes(Some(pts), None, b"page", true), None);
    let ts = [
        pes(100_000),
        packet(PCR_PID, false, &[], Some(90_000)),
        pes(80_000),
        pes(95_000),
        pes(200_000),
        packet(0x1fe, false, &[], Some(500_000)),
        pes(300_000),
    ];
    let bounded: Vec<_> = cut(&ts)
        .iter()
        .map(|p| (p.pcr_base, p.pcr_bounded_pts()))
        .collect();
    assert_eq!(
        bounded,
        [
            (None, None),
            (Some(90_000), Some(90_000)),
            (Some(90_000), Some(95_000)),
            (Some(90_000), Some(102_654)),
            (Some(90_000), Some(102_654)),
        ]
    );
}

#[test]
fn the_clock_places_times_from_the_first_and_falls_back_to_the_pts() {
    const PERIOD: i128 = 1 << 33;
    let mut clock = DemuxClock::new(1).unwrap();
    assert_eq!(clock.place(None, None).unwrap(), (None, None));
    let near_end = (1 << 33) - 90_000;
    assert_eq!(
        clock.place(Some(near_end), None).unwrap(),
        (
            Some(i128::from(near_end) - PERIOD),
            Some(i128::from(near_end) - PERIOD)
        )
    );
    assert_eq!(
        clock.place(Some(90_000), Some(45_000)).unwrap(),
        (Some(90_000), Some(45_000))
    );
    assert!(clock.place(Some(1 << 33), None).is_err());
}

#[test]
fn formats_are_detected_by_repeated_sync_bytes() {
    let mut one = [0; 188];
    one[0] = 0x47;
    let ts = one.repeat(3);
    assert_eq!(PacketFormat::detect(&ts), Some(PacketFormat::Ts188));
    let m2ts: Vec<u8> = [&[0; 4][..], &ts[..188]].concat().repeat(3);
    assert_eq!(PacketFormat::detect(&m2ts), Some(PacketFormat::M2ts192));
    assert_eq!(PacketFormat::detect(&[0x47]), Some(PacketFormat::Ts188));
    assert_eq!(PacketFormat::detect(b"PG\0\0"), None);
    assert_eq!(PacketFormat::detect(&[]), None);
}
