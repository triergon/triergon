//! Regression against the existing API: cloning a library track must share its
//! immutable allocations, including source bytes and non-static strings.
use std::io::Cursor;
use triergon::{Limits, typed::BlockingReader as Demuxer};

fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out: Vec<_> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|b| *b == 0)
        .collect();
    let width = (1..=8)
        .find(|w| (bytes.len() as u64) < (1_u64 << (7 * w)) - 1)
        .unwrap();
    out.extend(((1_u64 << (7 * width)) | bytes.len() as u64).to_be_bytes()[8 - width..].iter());
    out.extend(bytes);
    out
}
fn fixture() -> Vec<u8> {
    let track = [
        e(0xd7, &[1]),
        e(0x73c5, &[1]),
        e(0x83, &[1]),
        e(0x86, b"V_AUTHORED/OPAQUE"),
        e(0x63a2, b"private"),
        e(0x536e, b"authored name"),
        e(0x22b59c, b"fra"),
        e(0x22b59d, b"fr-CA"),
    ]
    .concat();
    [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[e(0x1654ae6b, &e(0xae, &track)), e(0x1f43b675, &[])].concat(),
        ),
    ]
    .concat()
}
fn tracks() -> (triergon::matroska::Track, triergon::matroska::Track) {
    let reader = Demuxer::open(Cursor::new(fixture()), Limits::default()).unwrap();
    let a = reader.tracks()[0].clone();
    let b = a.clone();
    drop(reader);
    (a, b)
}

#[test]
fn codec_private_clones_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert_eq!(&a.codec_private[..], b"private");
    assert_eq!(
        a.codec_private.as_ptr(),
        b.codec_private.as_ptr(),
        "retained Track::clone must not allocate an uncharged CodecPrivate copy"
    );
}

#[test]
fn source_metadata_clones_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert!(!a.source_metadata.is_empty());
    assert_eq!(
        a.source_metadata.as_ptr(),
        b.source_metadata.as_ptr(),
        "retained Track::clone must share the complete immutable source payload"
    );
}

#[test]
fn non_static_track_strings_share_storage_after_reader_destruction() {
    let (a, b) = tracks();
    assert_eq!(
        a.name.as_ptr(),
        b.name.as_ptr(),
        "track name was deep-cloned"
    );
    assert_eq!(
        a.codec.as_ptr(),
        b.codec.as_ptr(),
        "unknown codec ID was deep-cloned"
    );
    assert_eq!(
        a.language.as_ptr(),
        b.language.as_ptr(),
        "source language was deep-cloned"
    );
    assert_eq!(
        a.language_bcp47.as_ref().unwrap().as_ptr(),
        b.language_bcp47.as_ref().unwrap().as_ptr(),
        "BCP47 value was deep-cloned"
    );
}

#[test]
fn encoded_private_and_owned_text_are_checked_views_of_the_complete_source_entry() {
    let (track, _) = tracks();
    assert!(
        track
            .codec_private
            .shares_storage_with(&track.source_metadata)
    );
    let start = track.source_metadata.as_ptr() as usize;
    let end = start + track.source_metadata.len();
    for bytes in [
        track.codec_private.as_ref(),
        track.codec.as_bytes(),
        track.name.as_bytes(),
        track.language.as_bytes(),
        track.language_bcp47.as_ref().unwrap().as_bytes(),
    ] {
        let at = bytes.as_ptr() as usize;
        assert!(at >= start && at + bytes.len() <= end);
    }
    assert_eq!(track.codec_private.as_ref(), b"private");
    assert_eq!(track.name.as_str(), "authored name");
}

#[test]
fn sibling_source_views_keep_one_charged_tracks_owner_after_teardown() {
    use triergon_core::Budget;
    let entry = |number| {
        [
            e(0xd7, &[number]),
            e(0x73c5, &[number]),
            e(0x83, &[1]),
            e(0x86, b"V_AUTHORED/OPAQUE"),
            e(0x63a2, b"private"),
        ]
        .concat()
    };
    let first = entry(1);
    let second = entry(2);
    let raw = [e(0xae, &first), e(0xae, &second)].concat();
    let bytes = [
        e(0x1a45dfa3, &e(0x4282, b"matroska")),
        e(
            0x18538067,
            &[e(0x1654ae6b, &raw), e(0x1f43b675, &[])].concat(),
        ),
    ]
    .concat();
    let pool = Budget::new(64 * 1024).unwrap();
    let baseline = pool.used();
    let reader = Demuxer::open_in(Cursor::new(bytes), Limits::default(), &pool).unwrap();
    let a = reader.tracks()[0].clone();
    let b = reader.tracks()[1].clone();
    assert_eq!(a.source_metadata.as_ref(), first.as_slice());
    assert_eq!(b.source_metadata.as_ref(), second.as_slice());
    assert!(a.source_metadata.shares_storage_with(&b.source_metadata));
    assert!(a.codec_private.shares_storage_with(&b.codec_private));
    let retained = a.codec_private.clone();
    assert!(retained.retained_capacity() >= raw.len());
    drop((reader, a, b));
    assert!(pool.used() > baseline);
    assert_eq!(retained.as_ref(), b"private");
    drop(retained);
    assert_eq!(pool.used(), baseline);
}
