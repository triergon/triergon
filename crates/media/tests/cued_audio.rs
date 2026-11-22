//! RFC 9559 CueTrack, CueClusterPosition and Block timestamp example.
#[path = "support/ebml.rs"]
mod ebml;

use std::{
    cell::RefCell,
    io::{Cursor, Read, Seek, SeekFrom},
    rc::Rc,
    sync::atomic::AtomicBool,
};
use triergon_matroska::{Limits, typed::BlockingReader};

struct Logged {
    input: Cursor<Vec<u8>>,
    seeks: Rc<RefCell<Vec<u64>>>,
}
impl Read for Logged {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}
impl Seek for Logged {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let offset = self.input.seek(pos)?;
        self.seeks.borrow_mut().push(offset);
        Ok(offset)
    }
}

#[test]
fn canonical_audio_cue_lands_and_replays_source_packets() {
    let (bytes, cluster, relative_cluster) = ebml::cued_aac();
    assert_eq!(
        &bytes[cluster as usize..cluster as usize + 4],
        &[0x1f, 0x43, 0xb6, 0x75]
    );
    // RFC 9559 defines CueTrack as the TrackNumber.
    let cue_track = ebml::uint(0xf7, 7);
    assert_eq!(
        bytes
            .windows(cue_track.len())
            .filter(|w| *w == cue_track)
            .count(),
        2
    );
    let cue_position = ebml::uint(0xf1, relative_cluster);
    assert_eq!(
        bytes
            .windows(cue_position.len())
            .filter(|w| *w == cue_position)
            .count(),
        1
    );
    let seeks = Rc::new(RefCell::new(Vec::new()));
    let source = Logged {
        input: Cursor::new(bytes),
        seeks: seeks.clone(),
    };
    let mut reader = BlockingReader::open_seekable(source, Limits::default()).unwrap();
    assert_eq!(reader.tracks()[0].number, 7);
    let cancel = AtomicBool::new(false);
    let landing = reader.seek_cued_aac(0, 1_010_000_000, &cancel).unwrap();
    assert!(landing.used_index);
    assert_eq!(landing.actual_ns, 1_000_000_000);
    assert!(seeks.borrow().contains(&cluster));
    let first = reader.next_packet(&cancel).unwrap().unwrap();
    let second = reader.next_packet(&cancel).unwrap().unwrap();
    assert_eq!(
        (first.track, first.pts_ns, first.data.as_ref()),
        (0, Some(1_000_000_000), &b"second"[..])
    );
    assert_eq!(
        (second.track, second.pts_ns, second.data.as_ref()),
        (0, Some(1_020_000_000), &b"third"[..])
    );
    assert_eq!(
        (
            first.dts_ns,
            first.duration_ns,
            second.dts_ns,
            second.duration_ns
        ),
        (None, None, None, None)
    );
    assert!(reader.next_packet(&cancel).unwrap().is_none());
}

#[test]
fn fractional_target_before_cue_lands_at_previous_source_cluster() {
    let (bytes, second_cluster, _) = ebml::cued_aac();
    let first_cluster = bytes
        .windows(4)
        .position(|w| w == [0x1f, 0x43, 0xb6, 0x75])
        .unwrap() as u64;
    let seeks = Rc::new(RefCell::new(Vec::new()));
    let source = Logged {
        input: Cursor::new(bytes),
        seeks: seeks.clone(),
    };
    let mut reader = BlockingReader::open_seekable(source, Limits::default()).unwrap();
    let cancel = AtomicBool::new(false);
    let landing = reader.seek_cued_aac(0, 999_600_000, &cancel).unwrap();
    assert!(landing.used_index);
    assert_eq!(landing.actual_ns, 0);
    assert!(first_cluster < second_cluster);
    assert!(seeks.borrow().contains(&first_cluster));
    for (pts, payload) in [
        (0, &b"first"[..]),
        (1_000_000_000, &b"second"[..]),
        (1_020_000_000, &b"third"[..]),
    ] {
        let packet = reader.next_packet(&cancel).unwrap().unwrap();
        assert_eq!(
            (
                packet.track,
                packet.pts_ns,
                packet.dts_ns,
                packet.duration_ns,
                packet.data.as_ref()
            ),
            (0, Some(pts), None, None, payload)
        );
    }
    assert!(reader.next_packet(&cancel).unwrap().is_none());
}

#[test]
fn audio_seek_refuses_missing_cue_and_other_configuration() {
    let cancel = AtomicBool::new(false);
    let (bytes, _, _) = ebml::cued_aac();
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(bytes.clone()), Limits::default()).unwrap();
    assert!(reader.seek_cued_aac(0, -1, &cancel).is_err());
    let mut altered = bytes;
    let at = altered.windows(2).position(|w| w == [0x12, 0x10]).unwrap();
    altered[at + 1] = 0x08;
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(altered), Limits::default()).unwrap();
    assert!(reader.seek_cued_aac(0, 1_010_000_000, &cancel).is_err());
}

#[test]
fn ac3_seek_refuses_unsupported_track_and_missing_preceding_cue() {
    let bytes = ebml::cued_ac3_coded();
    let cancel = AtomicBool::new(false);
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(bytes.clone()), Limits::default()).unwrap();
    assert!(reader.seek_cued_ac3(0, -1, &cancel).is_err());
    let mut altered = bytes;
    let rate = 48_000_f64.to_be_bytes();
    let at = altered.windows(rate.len()).position(|w| w == rate).unwrap();
    altered[at..at + rate.len()].copy_from_slice(&44_100_f64.to_be_bytes());
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(altered), Limits::default()).unwrap();
    assert!(reader.seek_cued_ac3(0, 130_000_000, &cancel).is_err());
}

#[test]
fn eac3_seek_accepts_profile_and_refuses_outside_it() {
    let bytes = ebml::cued_eac3_coded();
    let cancel = AtomicBool::new(false);
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(bytes.clone()), Limits::default()).unwrap();
    let landing = reader.seek_cued_eac3(0, 130_000_000, &cancel).unwrap();
    assert!(landing.used_index);
    assert_eq!(landing.actual_ns, 128_000_000);
    assert_eq!(
        reader.next_packet(&cancel).unwrap().unwrap().pts_ns,
        Some(128_000_000)
    );
    let mut reader = BlockingReader::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
    assert!(reader.seek_cued_eac3(0, -1, &cancel).is_err());
    let mut reader =
        BlockingReader::open_seekable(Cursor::new(ebml::cued_eac3_opaque()), Limits::default())
            .unwrap();
    assert!(reader.seek_cued_eac3(0, 130_000_000, &cancel).is_err());
}
