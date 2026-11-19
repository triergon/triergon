//! The blocking openers select the same reader as the slice API and preserve its input.
use std::io::{self, Cursor, ErrorKind, Read, Seek, SeekFrom};
use std::mem::discriminant;
use triergon::{
    AnyReader, Budget, ContainerReader, Error, Probe, SourceCapabilities, SourceId, open_blocking,
    open_sequential, probe,
};

/// Short reads with an interruption before each one. At the end the source
/// either reports EOF or requires its caller to wait for more input.
struct Fragmented<'a> {
    data: Cursor<&'a [u8]>,
    chunk: usize,
    interrupt: bool,
    end_error: Option<ErrorKind>,
    read_bytes: usize,
}

impl<'a> Fragmented<'a> {
    fn new(bytes: &'a [u8], chunk: usize, end_error: Option<ErrorKind>) -> Self {
        Self {
            data: Cursor::new(bytes),
            chunk,
            interrupt: true,
            end_error,
            read_bytes: 0,
        }
    }
}

impl Read for Fragmented<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.interrupt {
            self.interrupt = false;
            return Err(ErrorKind::Interrupted.into());
        }
        self.interrupt = true;
        let want = out.len().min(self.chunk);
        let count = self.data.read(&mut out[..want])?;
        if count == 0
            && let Some(kind) = self.end_error
        {
            return Err(kind.into());
        }
        self.read_bytes += count;
        Ok(count)
    }
}

impl Seek for Fragmented<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.data.seek(to)
    }
}

fn check_openers(bytes: &[u8], expected: Probe, consumed: usize, end_error: Option<ErrorKind>) {
    assert_eq!(probe(bytes), expected);
    for chunk in [1, 4096] {
        let pool = Budget::new(1 << 20).unwrap();
        for seekable in [false, true] {
            let caps = SourceCapabilities {
                length: seekable.then_some(bytes.len() as u64),
                random_access: seekable,
            };
            let direct = AnyReader::open(bytes, &pool, SourceId(1), caps).map(|r| discriminant(&r));
            let mut source = Fragmented::new(bytes, chunk, end_error);
            let opened = if seekable {
                source.data.set_position(2);
                open_blocking(&mut source, &pool).map(|r| {
                    assert_eq!(r.reader().capabilities(), caps);
                    let kind = discriminant(r.reader());
                    assert_eq!(r.into_inner().data.position(), 0);
                    kind
                })
            } else {
                open_sequential(&mut source, &pool).map(|r| {
                    assert_eq!(r.reader().capabilities(), caps);
                    let kind = discriminant(r.reader());
                    let mut replay = r.into_inner();
                    if end_error.is_some() {
                        assert_eq!(pool.used() - Budget::control_storage_bytes(), consumed);
                    }
                    let mut copy = vec![0; bytes.len()];
                    replay.read_exact(&mut copy).unwrap();
                    assert_eq!(copy, bytes);
                    kind
                })
            };
            assert_eq!(opened, direct);
            if seekable || opened.is_err() {
                assert_eq!(source.read_bytes, consumed);
            }
            assert_eq!(pool.used(), Budget::control_storage_bytes());
        }
    }
}

#[test]
fn conclusive_signatures_open_before_another_read_and_rewind_or_replay() {
    for (bytes, kind) in [
        (&b"\x1a\x45\xdf\xa3"[..], Probe::Matroska),
        (&b"OggS\0"[..], Probe::Ogg),
    ] {
        check_openers(bytes, kind, bytes.len(), Some(ErrorKind::WouldBlock));
    }
    for kind in [
        b"ftyp", b"styp", b"moov", b"moof", b"mdat", b"free", b"skip", b"sidx",
    ] {
        // The opening box takes precedence over a putative FLAC marker.
        let bytes = [b"fLaC".as_slice(), kind].concat();
        check_openers(&bytes, Probe::Isobmff, 8, Some(ErrorKind::WouldBlock));
    }
    assert_eq!(probe(b"\0\0\0\x10wide"), Probe::Unknown);
    let flac = if cfg!(feature = "elementary") {
        Probe::Flac
    } else {
        Probe::Unknown
    };
    check_openers(b"fLaC\0", flac, 5, Some(ErrorKind::WouldBlock));
}

#[test]
fn facade_prefers_ts188_even_if_the_m2ts_signature_also_matches() {
    let mut bytes = [0; 389];
    for at in [4, 196, 388] {
        bytes[at] = 0x47;
    }
    check_openers(&bytes, Probe::M2ts, 389, Some(ErrorKind::WouldBlock));
    for at in [0, 188, 376] {
        bytes[at] = 0x47;
    }
    check_openers(&bytes, Probe::Ts, 377, Some(ErrorKind::WouldBlock));
    check_openers(&bytes[..377], Probe::Ts, 377, Some(ErrorKind::WouldBlock));
}

#[test]
fn short_eof_and_transport_errors_remain_distinct() {
    let pool = Budget::new(1 << 20).unwrap();
    for bytes in [&b""[..], b"\x1a\x45\xdf", b"OggS", b"ID3"] {
        check_openers(bytes, Probe::Unknown, bytes.len(), None);
        for kind in [ErrorKind::WouldBlock, ErrorKind::UnexpectedEof] {
            let mut source = Fragmented::new(bytes, 1, Some(kind));
            assert_eq!(
                open_sequential(&mut source, &pool).err(),
                Some(Error::Io(kind))
            );
            assert_eq!(
                open_blocking(&mut source, &pool).err(),
                Some(Error::Io(kind))
            );
        }
    }
}

#[cfg(not(feature = "mpegts"))]
#[test]
fn disabled_transport_reader_is_refused_at_the_signature_without_replay_storage() {
    let pool = Budget::new(Budget::control_storage_bytes()).unwrap();
    let bytes = [0x47; 377];
    let source = Fragmented::new(&bytes, 1, Some(ErrorKind::WouldBlock));
    assert_eq!(
        open_sequential(source, &pool).err(),
        Some(Error::Unsupported("MPEG-TS reader feature disabled"))
    );
}

#[cfg(feature = "elementary")]
#[test]
fn tagged_elementary_frames_keep_the_slice_probes_confirmation_rules() {
    // ID3v2 with a five-byte body and a ten-byte footer.
    let mut tag = b"ID3\x04\0\x10\0\0\0\x05".to_vec();
    tag.resize(25, 0);
    for (header, frame_bytes, kind) in [
        (&b"\xff\xfb\x90\x64"[..], 417, Probe::MpegAudio),
        (&b"\xff\xf1\x50\x80\x01\x3f\xfc"[..], 9, Probe::Adts),
        (&b"\x0b\x77\x12\x34\x10\x40\x40\0"[..], 512, Probe::Ac3),
    ] {
        let mut frame = header.to_vec();
        frame.resize(frame_bytes, 0);
        let mut bytes = [tag.as_slice(), &frame].concat();
        check_openers(&bytes, Probe::Unknown, bytes.len(), None);
        bytes.extend_from_slice(header);
        check_openers(&bytes, kind, bytes.len(), None);
    }
    // The declared tag includes these apparent MPEG headers and continues
    // past EOF. Probing inside its body would pick an incorrect reader.
    let mut truncated = b"ID3\x04\0\0\0\0\x20\0".to_vec();
    let mut frame = b"\xff\xfb\x90\x64".to_vec();
    frame.resize(417, 0);
    truncated.extend(frame.repeat(3));
    check_openers(&truncated, Probe::Unknown, truncated.len(), None);

    tag.extend_from_slice(b"fLaC");
    check_openers(&tag, Probe::Flac, tag.len(), None);
}

#[cfg(feature = "elementary")]
#[test]
fn last_probe_byte_confirms_late_mpeg_audio_and_replay_releases_its_charge() {
    const LIMIT: usize = 64 * 1024;
    let mut frame = b"\xff\xfb\x90\x64".to_vec();
    frame.resize(417, 0);
    let mut bytes = vec![0; LIMIT - 2 * frame.len() - 4];
    bytes.extend(frame.repeat(3));
    assert_eq!(probe(&bytes[..LIMIT - 1]), Probe::Unknown);
    assert_eq!(probe(&bytes[..LIMIT]), Probe::MpegAudio);
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = Fragmented::new(&bytes, 1, Some(ErrorKind::WouldBlock));
    let mut replay = open_sequential(&mut input, &pool).unwrap().into_inner();
    let base = Budget::control_storage_bytes();
    assert_eq!(pool.used() - base, LIMIT);
    let mut copy = vec![0; bytes.len()];
    replay.read_exact(&mut copy[..LIMIT - 1]).unwrap();
    assert_eq!(pool.used() - base, LIMIT);
    replay.read_exact(&mut copy[LIMIT - 1..LIMIT]).unwrap();
    assert_eq!(pool.used(), base);
    replay.read_exact(&mut copy[LIMIT..]).unwrap();
    assert_eq!(copy, bytes);
    drop(replay);
    let mut seekable = Fragmented::new(&bytes, 7, Some(ErrorKind::WouldBlock));
    assert!(matches!(
        open_blocking(&mut seekable, &pool).unwrap().reader(),
        AnyReader::MpegAudio(_)
    ));
    assert_eq!(seekable.read_bytes, LIMIT);
    assert_eq!(pool.used(), base);

    // Moving the third header one byte outside the bound must prevent selection.
    bytes.insert(0, 0);
    assert_eq!(
        open_sequential(bytes.as_slice(), &pool).err(),
        Some(Error::Unsupported("source format not recognised"))
    );
    let small = Budget::new(base + 512).unwrap();
    assert!(matches!(
        open_blocking(Cursor::new(bytes), &small),
        Err(Error::Limit { .. })
    ));
    assert_eq!(small.used(), base);
}

#[cfg(feature = "elementary")]
#[test]
fn sequential_budget_pressure_leaves_the_pipe_at_the_signature() {
    let mut frames = vec![0x21; 65536];
    for at in [200, 617, 1034] {
        frames[at..at + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    }
    for (input, expected) in [
        (&b"fLaC\x00\x00\x00\x22"[..], Probe::Flac),
        (&frames, Probe::MpegAudio),
    ] {
        let pool = Budget::new(Budget::control_storage_bytes() + 65536).unwrap();
        let mut pipe = input;
        for available in [0, 32768] {
            let held = pool.reserve(pool.available() - available).unwrap();
            assert!(matches!(
                open_sequential(&mut pipe, &pool),
                Err(Error::Backpressure { .. })
            ));
            assert_eq!(pipe.len(), input.len());
            drop(held);
        }
        let opened = open_sequential(&mut pipe, &pool).unwrap();
        assert!(matches!(
            (opened.reader(), expected),
            (AnyReader::Flac(_), Probe::Flac) | (AnyReader::MpegAudio(_), Probe::MpegAudio)
        ));
        let mut bytes = Vec::new();
        opened.into_inner().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, input);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

#[cfg(feature = "elementary")]
#[test]
fn seekable_elementary_probe_fits_a_single_window_in_the_pool() {
    let base = Budget::control_storage_bytes();
    let pool = Budget::new(base + 65536).unwrap();
    let mut bytes = vec![0x21; 65536];
    for at in [200, 617, 1034] {
        bytes[at..at + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    }
    let mut file = Cursor::new(bytes);
    for _ in 0..2 {
        let opened = open_blocking(&mut file, &pool).unwrap();
        assert!(matches!(opened.reader(), AnyReader::MpegAudio(_)));
        assert_eq!(opened.into_inner().position(), 0);
        assert_eq!(pool.used(), base);
    }
    assert_eq!(pool.peak(), base + 65536);
}
