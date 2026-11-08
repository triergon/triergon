//! The reader over authored streams: pages, packets, mappings, timing, seeking, limits.
use std::sync::atomic::AtomicBool;
use triergon_core::container::{
    CodecId, ContainerReader, Event, MediaKind, PacketInfo, SeekMethod, SeekPolicy, SeekRequest,
    TrackInfo, Trim,
};
use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceId, Step, Time,
    WorkBudget,
};
use triergon_framing::{VorbisHeaders, flac_stream_info};
use triergon_ogg::compat::clock::{ReferenceTiming, packet_timing};
use triergon_ogg::page::{BEGIN_OF_STREAM, CONTINUED, END_OF_STREAM, write_page};
use triergon_ogg::{Limits, Packet, Reader, ReaderEvent};

/// Lays out packets on pages the way an Ogg muxer does.
struct Muxer {
    serial: u32,
    sequence: u32,
    /// Lacing values and body of the page being filled.
    lacing: Vec<u8>,
    body: Vec<u8>,
    continued: bool,
    granule: i64,
    first: bool,
}

impl Muxer {
    fn new(serial: u32) -> Muxer {
        Muxer {
            serial,
            sequence: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            continued: false,
            granule: -1,
            first: true,
        }
    }

    /// Add a packet ending at `granule`, flushing full pages to `out`.
    fn packet(&mut self, packet: &[u8], granule: i64, out: &mut Vec<u8>) {
        let mut rest = packet;
        loop {
            if self.lacing.len() == 255 {
                self.flush(false, out);
                self.continued = true;
            }
            let take = rest.len().min(255);
            self.lacing.push(take as u8);
            self.body.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if take < 255 {
                break;
            }
            if rest.is_empty() {
                // A multiple of 255 ends with a zero lacing value.
                if self.lacing.len() == 255 {
                    self.flush(false, out);
                    self.continued = true;
                }
                self.lacing.push(0);
                break;
            }
        }
        self.granule = granule;
    }

    fn flush(&mut self, end: bool, out: &mut Vec<u8>) {
        let complete = self.lacing.last().is_some_and(|&v| v < 255);
        let mut flags = 0;
        if self.first {
            flags |= BEGIN_OF_STREAM;
        }
        if self.continued {
            flags |= CONTINUED;
        }
        if end {
            flags |= END_OF_STREAM;
        }
        let granule = if complete { self.granule } else { -1 };
        write_page(
            flags,
            granule,
            self.serial,
            self.sequence,
            &self.lacing,
            &self.body,
            out,
        );
        self.sequence += 1;
        self.lacing.clear();
        self.body.clear();
        self.first = false;
        self.continued = false;
    }
}

fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = b"OpusHead\x01\x02".to_vec();
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&[0, 0, 0]);
    head
}

/// A 20 ms CELT packet numbered `n`.
fn opus_packet(n: usize, length: usize) -> Vec<u8> {
    let mut packet = vec![0xfc];
    packet.extend((0..length.saturating_sub(1)).map(|i| (n + i) as u8));
    packet
}

/// An Opus stream of `count` packets, `per_page` to a page, whose last page trims `trim`.
fn opus_stream(serial: u32, count: usize, per_page: usize, trim: i64, out: &mut Vec<u8>) {
    let mut muxer = Muxer::new(serial);
    muxer.packet(&opus_head(312), 0, out);
    muxer.flush(false, out);
    muxer.packet(b"OpusTags\0\0\0\0\0\0\0\0", 0, out);
    muxer.flush(false, out);
    for n in 0..count {
        let last = n + 1 == count;
        let granule = 960 * (n as i64 + 1) - if last { trim } else { 0 };
        muxer.packet(&opus_packet(n, 20 + n % 7), granule, out);
        if (n + 1) % per_page == 0 || last {
            muxer.flush(last, out);
        }
    }
}

/// Drive a reader over `file` with windows of `window` bytes and steps of `work` units.
fn read_with(
    file: &[u8],
    window: usize,
    work: u64,
    random_access: bool,
    limits: Limits,
) -> (Reader, Vec<Event<Packet, ReaderEvent>>, Option<Error>) {
    let pool = Budget::new(64 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: random_access.then_some(file.len() as u64),
        random_access,
    };
    let mut reader = Reader::new(&pool, SourceId(1), capabilities, limits, window).unwrap();
    let mut events = Vec::new();
    let error = drive(&mut reader, &pool, file, work, &mut events, None).err();
    (reader, events, error)
}

/// Run until the end, feeding `file`; stop early after `stop` events.
fn drive(
    reader: &mut Reader,
    pool: &Budget,
    file: &[u8],
    work: u64,
    events: &mut Vec<Event<Packet, ReaderEvent>>,
    stop: Option<usize>,
) -> Result<(), Error> {
    let cancel = AtomicBool::new(false);
    let mut steps = 0u64;
    loop {
        steps += 1;
        assert!(steps < 50_000_000, "the reader does not progress");
        match reader.step(&mut WorkBudget::new(work), &cancel)? {
            Step::Ready(event) => {
                events.push(event);
                if stop.is_some_and(|stop| events.len() >= stop) {
                    return Ok(());
                }
            }
            Step::NeedInput(request) => supply(reader, pool, file, &request)?,
            Step::Yield => {}
            Step::Backpressure(_) => panic!("unexpected backpressure"),
            Step::End(_) => return Ok(()),
        }
    }
}

fn supply(
    reader: &mut Reader,
    pool: &Budget,
    file: &[u8],
    request: &InputRequest,
) -> Result<(), Error> {
    let start = usize::try_from(request.offset).unwrap().min(file.len());
    let end = (start + request.maximum).min(file.len());
    if start == end {
        return reader.finish(request.source, request.token.clone());
    }
    let bytes = Bytes::copy_from_slice(pool, &file[start..end])?;
    reader.provide(&InputWindow::new(
        request.source,
        request.token.clone(),
        request.offset,
        bytes,
    )?)
}

fn packets(events: &[Event<Packet, ReaderEvent>]) -> Vec<&Packet> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Packet(packet) => Some(packet),
            _ => None,
        })
        .collect()
}

type Summary = (
    u64,
    Vec<u8>,
    Option<i64>,
    Option<i64>,
    Trim,
    ReferenceTiming,
);

fn summary(events: &[Event<Packet, ReaderEvent>]) -> Vec<Summary> {
    packets(events)
        .into_iter()
        .map(|p| {
            (
                p.track().0,
                p.payload().to_vec(),
                p.pts(),
                p.duration(),
                p.trim(),
                packet_timing(p),
            )
        })
        .collect()
}

#[test]
fn opus_packets_are_timed_from_granules_with_pre_skip_and_end_trim() {
    let mut file = Vec::new();
    opus_stream(5, 23, 10, 100, &mut file);
    let (reader, events, error) = read_with(&file, 1 << 16, 1 << 20, true, Limits::default());
    assert_eq!(error, None);
    let track = &reader.tracks()[0];
    assert_eq!(
        (track.codec(), track.kind(), track.codec_tag()),
        (CodecId::Opus, MediaKind::Audio, &b"OpusHead"[..])
    );
    assert_eq!(track.configuration(), opus_head(312));
    assert_eq!(track.comments(), Some(&b"OpusTags\0\0\0\0\0\0\0\0"[..]));
    assert_eq!(
        track.time_base(),
        triergon_core::TimeBase::new(1, 48_000).unwrap()
    );
    assert_eq!(track.end(), Some(960 * 23 - 100 - 312));
    assert!(matches!(events[0], Event::Other(ReaderEvent::Track(0))));
    let packets = packets(&events);
    assert_eq!(packets.len(), 23);
    for (n, packet) in packets.iter().enumerate() {
        assert_eq!(packet.pts(), Some(960 * n as i64 - 312), "packet {n}");
        assert_eq!(packet.dts(), packet.pts());
        assert_eq!(packet.duration(), Some(960));
        assert_eq!(packet.payload(), opus_packet(n, 20 + n % 7));
    }
    assert_eq!(
        packets[0].trim(),
        Trim {
            start: Some(312),
            end: None
        }
    );
    assert_eq!(
        packets[22].trim(),
        Trim {
            start: None,
            end: Some(100)
        }
    );
    assert_eq!(packet_timing(packets[0]).skip_samples, 312);
    assert_eq!(packet_timing(packets[22]).discard_padding, 100);
    assert_eq!(packet_timing(packets[22]).duration, Some(860));
}

#[test]
fn any_window_size_and_work_budget_reads_the_same_packets() {
    let mut file = Vec::new();
    opus_stream(5, 40, 7, 0, &mut file);
    let (_, expected, _) = read_with(&file, 1 << 16, 1 << 20, false, Limits::default());
    let expected = summary(&expected);
    for (window, work) in [
        (1, 1),
        (1, 1 << 20),
        (2, 3),
        (11, 5),
        (4096, 100),
        (300, 1 << 20),
    ] {
        let (_, events, error) = read_with(&file, window, work, false, Limits::default());
        assert_eq!(error, None, "window {window} work {work}");
        assert_eq!(summary(&events), expected, "window {window} work {work}");
    }
}

#[test]
fn truncation_at_every_byte_ends_cleanly_with_a_prefix_of_the_packets() {
    let mut file = Vec::new();
    opus_stream(5, 6, 2, 30, &mut file);
    let (_, full, _) = read_with(&file, 1 << 16, 1 << 20, false, Limits::default());
    let full = summary(&full);
    for length in 0..file.len() {
        let (_, events, error) = read_with(&file[..length], 64, 1 << 20, false, Limits::default());
        assert_eq!(error, None, "length {length}");
        let got = summary(&events);
        assert!(got.len() <= full.len());
        // Packets of complete pages are unchanged, except the trims and times of the
        // last page, which a truncation removes.
        for (a, b) in got.iter().zip(&full) {
            assert_eq!(a.1, b.1, "length {length}");
        }
    }
}

#[test]
fn corrupt_bytes_never_panic() {
    let mut file = Vec::new();
    opus_stream(5, 5, 2, 30, &mut file);
    for at in 0..file.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut bad = file.clone();
            bad[at] ^= flip;
            // Rewrite the CRC sometimes so the damage reaches the parsers.
            let _ = read_with(&bad, 13, 1 << 20, false, Limits::default());
        }
    }
}

#[test]
fn long_empty_and_255_multiple_packets_survive_the_lacing() {
    let mut file = Vec::new();
    let mut muxer = Muxer::new(9);
    muxer.packet(b"\x00unknown mapping head", 0, &mut file);
    muxer.flush(false, &mut file);
    let sizes = [0, 255, 510, 70_000, 1, 0, 255 * 255, 254, 65_025 + 255];
    let payloads: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(n, &size)| (0..size).map(|i| (i * 7 + n) as u8).collect())
        .collect();
    for (n, payload) in payloads.iter().enumerate() {
        muxer.packet(payload, n as i64 + 1, &mut file);
        if n % 3 == 2 {
            muxer.flush(false, &mut file);
        }
    }
    muxer.flush(true, &mut file);
    for window in [1 << 16, 1000, 1] {
        let (reader, events, error) = read_with(&file, window, 1 << 20, false, Limits::default());
        assert_eq!(error, None);
        let track = &reader.tracks()[0];
        assert_eq!(track.codec(), CodecId::Other(b"\x00unknown"));
        assert_eq!(track.configuration(), b"\x00unknown mapping head");
        let got: Vec<Vec<u8>> = packets(&events)
            .iter()
            .map(|p| p.payload().to_vec())
            .collect();
        let lengths = |v: &[Vec<u8>]| v.iter().map(Vec::len).collect::<Vec<_>>();
        assert_eq!(lengths(&got), lengths(&payloads), "window {window}");
        assert!(got == payloads, "window {window}");
        assert!(
            packets(&events)
                .iter()
                .all(|p| p.pts().is_none() && p.duration().is_none())
        );
    }
}

#[test]
fn a_lost_page_drops_its_packets_and_the_next_page_keeps_its_time() {
    let mut file = Vec::new();
    opus_stream(5, 30, 5, 0, &mut file);
    // Pages: head, tags, then six pages of five packets. Drop the fourth audio page.
    let offsets = page_offsets(&file);
    let cut = file[offsets[5]..offsets[6]].to_vec();
    let mut lost = file[..offsets[5]].to_vec();
    lost.extend_from_slice(&file[offsets[6]..]);
    let (_, events, error) = read_with(&lost, 1 << 16, 1 << 20, false, Limits::default());
    assert_eq!(error, None);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Other(ReaderEvent::Lost { track: 0, .. })))
    );
    let kept = packets(&events);
    assert_eq!(kept.len(), 25);
    assert_eq!(kept[15].pts(), Some(960 * 20 - 312));
    // A bad CRC skips the page the same way, reporting the skipped bytes.
    let mut bad = file.clone();
    bad[offsets[5] + 40] ^= 0x55;
    let (_, events, error) = read_with(&bad, 1 << 16, 1 << 20, false, Limits::default());
    assert_eq!(error, None);
    assert!(events.iter().any(|e| matches!(
        e,
        Event::Other(ReaderEvent::Resync { offset, bytes }) if *offset == offsets[5] as u64 && *bytes == cut.len() as u64
    )));
    assert_eq!(summary(&events).len(), 25);
    // Garbage between pages is skipped and reported.
    let mut garbage = file[..offsets[5]].to_vec();
    garbage.extend_from_slice(b"OggXOgg junk OggS");
    garbage.extend_from_slice(&file[offsets[5]..]);
    let (_, events, error) = read_with(&garbage, 7, 1 << 20, false, Limits::default());
    assert_eq!(error, None);
    assert_eq!(packets(&events).len(), 30);
}

fn page_offsets(file: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut at = 0;
    while at < file.len() {
        offsets.push(at);
        at += triergon_ogg::PageHeader::parse(&file[at..])
            .unwrap()
            .page_len();
    }
    offsets.push(at);
    offsets
}

#[test]
fn multiplexed_streams_keep_their_own_packets_and_times() {
    let mut a = Vec::new();
    opus_stream(1, 12, 3, 0, &mut a);
    let mut b = Vec::new();
    opus_stream(2, 9, 2, 0, &mut b);
    let (pa, pb) = (page_offsets(&a), page_offsets(&b));
    // Both BOS pages first, then pages alternate.
    let mut file = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i + 1 < pa.len() || j + 1 < pb.len() {
        if i + 1 < pa.len() && (i <= j || j + 1 >= pb.len()) {
            file.extend_from_slice(&a[pa[i]..pa[i + 1]]);
            i += 1;
        } else {
            file.extend_from_slice(&b[pb[j]..pb[j + 1]]);
            j += 1;
        }
    }
    let (reader, events, error) = read_with(&file, 333, 1 << 20, true, Limits::default());
    assert_eq!(error, None);
    assert_eq!(reader.track_count(), 2);
    for (index, count) in [(0usize, 12usize), (1, 9)] {
        let track: Vec<_> = packets(&events)
            .into_iter()
            .filter(|p| p.track_index() == index)
            .collect();
        assert_eq!(track.len(), count);
        for (n, packet) in track.iter().enumerate() {
            assert_eq!(packet.pts(), Some(960 * n as i64 - 312));
        }
    }
}

#[test]
fn chained_links_follow_each_other() {
    let mut file = Vec::new();
    opus_stream(1, 10, 4, 200, &mut file);
    let second = file.len() as u64;
    opus_stream(1, 6, 4, 0, &mut file);
    let (reader, events, error) = read_with(&file, 1 << 16, 1 << 20, true, Limits::default());
    assert_eq!(error, None);
    assert_eq!(reader.track_count(), 2);
    let tracks = reader.tracks();
    assert_eq!((tracks[0].link(), tracks[1].link()), (0, 1));
    assert_ne!(tracks[0].id(), tracks[1].id());
    assert!(events.iter().any(
        |e| matches!(e, Event::Other(ReaderEvent::Link { index: 1, offset }) if *offset == second)
    ));
    // The first link presents 9600 - 200 - 312 samples; the second starts there.
    let end = 9600 - 200 - 312;
    assert_eq!(tracks[1].offset(), end);
    let link: Vec<_> = packets(&events)
        .into_iter()
        .filter(|p| p.track_index() == 1)
        .collect();
    assert_eq!(link[0].pts(), Some(end - 312));
    assert_eq!(link[0].trim().start, Some(312));
    // FFmpeg continues at the previous link's last packet end, ignoring pre-skip.
    assert_eq!(packet_timing(link[0]).pts, Some(9600 - 312 - 200));
}

/// A Vorbis setup header with two modes, short (0) and long (1).
fn vorbis_setup() -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |value: u32, width: usize| bits.extend((0..width).map(|k| value >> k & 1 != 0));
    put(0x1234, 16);
    put(1, 6);
    for long in [0, 1] {
        put(long, 1);
        put(0, 32);
        put(long, 8);
    }
    put(1, 1);
    let mut packet = b"\x05vorbis".to_vec();
    packet.extend(bits.chunks(8).map(|byte| {
        byte.iter()
            .enumerate()
            .fold(0u8, |v, (k, &b)| v | u8::from(b) << k)
    }));
    packet
}

fn vorbis_identification() -> Vec<u8> {
    let mut id = b"\x01vorbis\0\0\0\0\x02\x44\xac\0\0".to_vec();
    id.extend_from_slice(&[0; 12]);
    id.extend_from_slice(&[0xb8, 1]); // block sizes 256 and 2048
    id
}

#[test]
fn vorbis_durations_follow_the_block_sizes() {
    // Blocks: S L L S S L; mode bit after the type bit, then the long block's flags.
    let blocks = [false, true, true, false, false, true];
    let audio: Vec<Vec<u8>> = blocks
        .iter()
        .enumerate()
        .map(|(n, &long)| {
            let previous_long = n > 0 && blocks[n - 1];
            let next_long = blocks.get(n + 1).copied().unwrap_or(false);
            let first =
                u8::from(long) << 1 | u8::from(previous_long) << 2 | u8::from(next_long) << 3;
            vec![first, n as u8, 0xaa]
        })
        .collect();
    // Samples: 0, 64+512, 1024, 512+64, 128, 64+512.
    let exact = [0, 576, 1024, 576, 128, 576];
    let total: i64 = exact.iter().sum();
    let mut file = Vec::new();
    let mut muxer = Muxer::new(3);
    muxer.packet(&vorbis_identification(), 0, &mut file);
    muxer.flush(false, &mut file);
    muxer.packet(b"\x03vorbis comment", 0, &mut file);
    muxer.packet(&vorbis_setup(), 0, &mut file);
    muxer.flush(false, &mut file);
    let mut end = 0;
    for (n, packet) in audio.iter().enumerate() {
        end += exact[n];
        muxer.packet(packet, end, &mut file);
    }
    muxer.flush(false, &mut file);
    let (reader, events, error) = read_with(&file, 1 << 16, 1 << 20, true, Limits::default());
    assert_eq!(error, None);
    let track = &reader.tracks()[0];
    assert_eq!(track.codec(), CodecId::Vorbis);
    let headers = VorbisHeaders::from_laced(track.configuration()).unwrap();
    assert_eq!(headers.identification, vorbis_identification());
    assert_eq!(headers.setup, vorbis_setup());
    assert_eq!(track.comments(), Some(&b"\x03vorbis comment"[..]));
    let packets = packets(&events);
    let durations: Vec<_> = packets.iter().map(|p| p.duration().unwrap()).collect();
    assert_eq!(durations, exact);
    assert_eq!(packets.last().unwrap().end(), Some(total));
    assert_eq!(packets[0].pts(), Some(0));
    // FFmpeg's clock: the first packet states 128, the short block after a long one 128.
    let reference: Vec<_> = packets
        .iter()
        .map(|p| packet_timing(p).duration.unwrap())
        .collect();
    assert_eq!(reference, [128, 576, 1024, 128, 128, 576]);
    assert_eq!(packet_timing(packets[0]).pts, Some(-128));
}

/// A FLAC frame header for `block` samples at frame number `n`.
fn flac_frame(n: u8, block: u16) -> Vec<u8> {
    let mut frame = vec![0xff, 0xf8, 0x79, 0x18, n];
    frame.extend_from_slice(&(block - 1).to_be_bytes());
    frame.push(triergon_bitstream::crc::crc8(0, &frame));
    frame.extend_from_slice(&[0x11; 9]);
    frame
}

#[test]
fn ogg_flac_rebuilds_the_native_stream_header() {
    let mut info = [0u8; 34];
    info[..4].copy_from_slice(&[0x10, 0, 0x10, 0]);
    let packed: u64 = (44100 << 44) | (1 << 41) | (15 << 36) | 0x2710;
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    let mut first = b"\x7fFLAC\x01\x00\x00\x01fLaC\x00\x00\x00\x22".to_vec();
    first.extend_from_slice(&info);
    let comment = b"\x84\x00\x00\x08\x04\0\0\0abcd".to_vec();
    let mut file = Vec::new();
    let mut muxer = Muxer::new(4);
    muxer.packet(&first, 0, &mut file);
    muxer.flush(false, &mut file);
    muxer.packet(&comment, 0, &mut file);
    muxer.flush(false, &mut file);
    let blocks = [4096u16, 4096, 1808];
    let mut end = 0;
    for (n, &block) in blocks.iter().enumerate() {
        end += i64::from(block);
        muxer.packet(&flac_frame(n as u8, block), end, &mut file);
    }
    muxer.flush(true, &mut file);
    let (reader, events, error) = read_with(&file, 1 << 16, 1 << 20, true, Limits::default());
    assert_eq!(error, None);
    let track = &reader.tracks()[0];
    assert_eq!(
        (track.codec(), track.codec_tag()),
        (CodecId::Flac, &b"\x7fFLAC"[..])
    );
    let configuration = track.configuration();
    assert!(configuration.starts_with(b"fLaC\x00\x00\x00\x22"));
    assert!(configuration.ends_with(&comment));
    assert_eq!(flac_stream_info(configuration).unwrap().sample_rate, 44100);
    assert_eq!(track.comments(), Some(&comment[4..]));
    let delivered = packets(&events);
    let times: Vec<_> = delivered
        .iter()
        .map(|p| (p.pts().unwrap(), p.duration().unwrap()))
        .collect();
    assert_eq!(times, [(0, 4096), (4096, 4096), (8192, 1808)]);
    assert!(delivered.iter().all(|p| p.trim() == Trim::default()));
}

#[test]
fn seeks_land_before_the_target_and_resume_the_linear_packets() {
    let mut file = Vec::new();
    opus_stream(5, 400, 6, 0, &mut file);
    let (_, linear, _) = read_with(&file, 1 << 16, 1 << 20, true, Limits::default());
    let linear = summary(&linear);
    for random_access in [true, false] {
        for (target, policy) in [
            (0, SeekPolicy::PlaybackPreroll),
            (48_000 * 3 + 17, SeekPolicy::PlaybackPreroll),
            (48_000 * 3 + 17, SeekPolicy::PreviousRandomAccess),
            (48_000 * 7, SeekPolicy::PreviousRandomAccess),
            (1, SeekPolicy::PreviousRandomAccess),
            (48_000 * 100, SeekPolicy::PlaybackPreroll),
        ] {
            let pool = Budget::new(64 << 20).unwrap();
            let capabilities = SourceCapabilities {
                length: random_access.then_some(file.len() as u64),
                random_access,
            };
            let mut reader =
                Reader::new(&pool, SourceId(1), capabilities, Limits::default(), 4096).unwrap();
            let mut events = Vec::new();
            drive(&mut reader, &pool, &file, 1 << 20, &mut events, Some(30)).unwrap();
            let track = reader.tracks()[0].id();
            let target_time = Time::new(target, 48_000).unwrap();
            let request = reader
                .request_seek(SeekRequest {
                    track,
                    target: target_time,
                    policy,
                })
                .unwrap();
            supply(&mut reader, &pool, &file, &request).unwrap();
            let mut after = Vec::new();
            drive(&mut reader, &pool, &file, 1 << 20, &mut after, None).unwrap();
            let Some(Event::Seek(receipt)) = after.iter().find(|e| matches!(e, Event::Seek(_)))
            else {
                panic!("no receipt");
            };
            let method = if random_access {
                SeekMethod::BoundedScan
            } else {
                SeekMethod::Restart
            };
            assert_eq!(receipt.method, method);
            let packets = summary(&after);
            if target >= 48_000 * 100 {
                assert!(packets.is_empty());
                continue;
            }
            let first = &packets[0];
            let start = linear
                .iter()
                .position(|p| p == first)
                .expect("a linear packet");
            assert_eq!(&linear[start..], &packets[..], "target {target} {policy:?}");
            let preroll = if policy == SeekPolicy::PlaybackPreroll {
                3840
            } else {
                0
            };
            let threshold = target as i64 - preroll;
            let pts = first.2.unwrap();
            assert!(
                pts <= threshold.max(-312),
                "target {target}: lands at {pts}"
            );
            assert!(
                pts + 960 > threshold,
                "target {target}: lands early at {pts}"
            );
            assert_eq!(receipt.landing, Time::new(i128::from(pts), 48_000).unwrap());
            let discard = if policy == SeekPolicy::PlaybackPreroll {
                target_time.max(receipt.landing)
            } else {
                receipt.landing
            };
            assert_eq!(receipt.discard_until, discard);
            if random_access {
                assert!(
                    receipt.scanned_bytes < file.len() as u64 / 2,
                    "scanned {}",
                    receipt.scanned_bytes
                );
            }
        }
    }
}

#[test]
fn a_vorbis_seek_lands_on_the_block_before_the_target() {
    let mut file = Vec::new();
    let mut muxer = Muxer::new(3);
    muxer.packet(&vorbis_identification(), 0, &mut file);
    muxer.flush(false, &mut file);
    muxer.packet(b"\x03vorbis comment", 0, &mut file);
    muxer.packet(&vorbis_setup(), 0, &mut file);
    muxer.flush(false, &mut file);
    // Long blocks throughout: each completes 1024 samples, the first none.
    let mut end = 0;
    for n in 0..2000 {
        let first = 0b1110;
        end += if n == 0 { 0 } else { 1024 };
        muxer.packet(&[first, n as u8, (n >> 8) as u8], end, &mut file);
        if n % 20 == 19 {
            muxer.flush(false, &mut file);
        }
    }
    muxer.flush(true, &mut file);
    let mut blocking =
        triergon_ogg::BlockingReader::open(std::io::Cursor::new(file), Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let track = blocking.tracks()[0].id();
    let target = Time::new(1_000_000, 44_100).unwrap();
    let receipt = blocking
        .seek(
            SeekRequest {
                track,
                target,
                policy: SeekPolicy::PlaybackPreroll,
            },
            &cancel,
        )
        .unwrap();
    let first = blocking.next_packet(&cancel).unwrap().unwrap();
    let second = blocking.next_packet(&cancel).unwrap().unwrap();
    // The target sample 1_000_000 is in the packet starting at 999_424; the one before
    // it starts 1024 earlier.
    assert_eq!(second.pts(), Some(999_424));
    assert_eq!(first.pts(), Some(998_400));
    assert_eq!(receipt.landing, Time::new(998_400, 44_100).unwrap());
    assert_eq!(receipt.discard_until, target);
    assert_eq!(receipt.method, SeekMethod::BoundedScan);
}

#[test]
fn budgets_and_limits_refuse_without_panicking() {
    let mut file = Vec::new();
    opus_stream(5, 10, 4, 0, &mut file);
    // Packets inside one page are views of it; one gathered across pages is limited.
    let mut long = Vec::new();
    let mut muxer = Muxer::new(9);
    muxer.packet(b"head", 0, &mut long);
    muxer.flush(false, &mut long);
    muxer.packet(&vec![7; 70_000], 1, &mut long);
    muxer.flush(true, &mut long);
    let limits = Limits {
        packet_bytes: 66_000,
        ..Limits::default()
    };
    let (_, _, error) = read_with(&long, 1 << 16, 1 << 20, false, limits);
    assert!(
        matches!(
            error,
            Some(Error::Limit {
                maximum: 66_000,
                ..
            })
        ),
        "{error:?}"
    );
    let limits = Limits {
        header_bytes: 10,
        ..Limits::default()
    };
    let (reader, events, error) = read_with(&file, 1 << 16, 1 << 20, false, limits);
    assert_eq!(error, None);
    assert!(matches!(
        reader.tracks()[0].header_error(),
        Some(Error::Limit { .. })
    ));
    assert!(packets(&events).iter().all(|p| p.pts().is_none()));
    let limits = Limits {
        sync_scan_bytes: 100,
        ..Limits::default()
    };
    let mut garbage = vec![0x42; 500];
    garbage.extend_from_slice(&file);
    let (_, _, error) = read_with(&garbage, 1 << 16, 1 << 20, false, limits);
    assert_eq!(error, Some(Error::Policy("Ogg capture pattern scan")));

    // A pool smaller than the packets held reports backpressure, then continues once
    // they are released.
    let mut file = Vec::new();
    opus_stream(5, 200, 4, 0, &mut file);
    let pool = Budget::new(Budget::control_storage_bytes() + 4_000).unwrap();
    let capabilities = SourceCapabilities {
        length: None,
        random_access: false,
    };
    let mut reader = Reader::new(&pool, SourceId(1), capabilities, Limits::default(), 512).unwrap();
    let cancel = AtomicBool::new(false);
    let mut held = Vec::new();
    let mut pressured = false;
    let mut count = 0;
    loop {
        match reader.step(&mut WorkBudget::new(1 << 20), &cancel).unwrap() {
            Step::Ready(Event::Packet(packet)) => {
                count += 1;
                held.push(packet);
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => match supply(&mut reader, &pool, &file, &request) {
                Ok(()) => {}
                Err(Error::Backpressure { .. }) => {
                    // The caller's window copy is what failed: release and ask again.
                    assert!(!held.is_empty(), "the reader alone exceeds the pool");
                    pressured = true;
                    held.clear();
                }
                Err(error) => panic!("{error}"),
            },
            Step::Backpressure(_) => {
                pressured = true;
                held.clear();
            }
            Step::End(_) => break,
        }
    }
    assert!(pressured);
    assert_eq!(count, 200);
}
