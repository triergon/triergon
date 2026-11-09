//! The PGS segment parsers: packet and SUP framing, palettes, object
//! fragments, compositions and windows.
use triergon_subtitles::pgs::{
    Composition, Envelope, ObjectFragment, Palette, SegmentIter, Windows,
};

fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut output = vec![kind];
    output.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    output.extend_from_slice(body);
    output
}

#[test]
fn packet_and_sup_headers_preserve_raw_clocks_unknown_types_and_borrowed_bytes() {
    let p = packet(0x7f, &[1, 2, 3]);
    let segment = SegmentIter::new(&p, Envelope::Packet)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(segment.kind, 0x7f);
    assert_eq!(segment.payload, [1, 2, 3]);
    assert_eq!(segment.clock, None);
    assert!(std::ptr::eq(segment.raw.as_ptr(), p.as_ptr()));
    let mut sup = b"PG".to_vec();
    sup.extend_from_slice(&u32::MAX.to_be_bytes());
    sup.extend_from_slice(&42u32.to_be_bytes());
    sup.extend_from_slice(&p);
    let segment = SegmentIter::new(&sup, Envelope::Sup)
        .next()
        .unwrap()
        .unwrap();
    let clock = segment.clock.unwrap();
    assert_eq!((clock.pts, clock.dts), (u32::MAX, 42));
    assert_eq!(segment.payload, [1, 2, 3]);
    for end in 1..sup.len() {
        let mut iter = SegmentIter::new(&sup[..end], Envelope::Sup);
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }
}

#[test]
fn palette_keeps_entry_order_partial_updates_and_exact_y_cr_cb_alpha() {
    let bytes = packet(0x14, &[3, 9, 7, 16, 128, 129, 255, 7, 17, 130, 131, 0]);
    let palette = Palette::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!((palette.id, palette.version), (3, 9));
    let entries = palette.entries().collect::<Vec<_>>();
    assert_eq!(
        (
            entries[0].id,
            entries[0].y,
            entries[0].cr,
            entries[0].cb,
            entries[0].alpha
        ),
        (7, 16, 128, 129, 255)
    );
    assert_eq!((entries[1].id, entries[1].alpha), (7, 0));
    let malformed = packet(0x14, &[3, 9, 7]);
    assert!(
        Palette::parse(
            SegmentIter::new(&malformed, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap()
        )
        .is_err()
    );
}

#[test]
fn object_fragment_lengths_dimensions_and_sequence_flags_stay_distinct() {
    let bytes = packet(0x15, &[0x12, 0x34, 7, 0xc0, 0, 0, 6, 0, 2, 0, 1, 1, 2]);
    let fragment = ObjectFragment::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (fragment.id, fragment.version, fragment.last),
        (0x1234, 7, true)
    );
    let start = fragment.first.unwrap();
    assert_eq!((start.data_length, start.width, start.height), (6, 2, 1));
    assert_eq!(fragment.data, [1, 2]);
    let bytes = packet(0x15, &[0x12, 0x34, 7, 0x40, 0, 0]);
    let fragment = ObjectFragment::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(fragment.first.is_none());
    assert!(fragment.last);
    assert_eq!(fragment.data, [0, 0]);
}

#[test]
fn cropping_and_forced_flags_are_independent_and_composition_records_are_exact() {
    for flags in [0u8, 0x40, 0x80, 0xc0] {
        let mut body = vec![
            7, 128, 4, 56, 0x10, 0x12, 0x34, 0x80, 0x80, 3, 1, 0, 7, 2, flags, 0, 9, 0, 10,
        ];
        if flags & 0x80 != 0 {
            body.extend_from_slice(&[0, 1, 0, 2, 0, 3, 0, 4]);
        }
        let bytes = packet(0x16, &body);
        let composition = Composition::parse(
            SegmentIter::new(&bytes, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (composition.width, composition.height, composition.number),
            (1920, 1080, 0x1234)
        );
        let object = composition.objects().next().unwrap().unwrap();
        assert_eq!(
            (object.id, object.window, object.x, object.y),
            (7, 2, 9, 10)
        );
        assert_eq!(object.forced(), flags & 0x40 != 0);
        assert_eq!(object.crop.is_some(), flags & 0x80 != 0);
        if let Some(crop) = object.crop {
            assert_eq!((crop.x, crop.y, crop.width, crop.height), (1, 2, 3, 4));
        }
        body.push(0);
        let bytes = packet(0x16, &body);
        assert!(
            Composition::parse(
                SegmentIter::new(&bytes, Envelope::Packet)
                    .next()
                    .unwrap()
                    .unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn window_count_and_geometry_cannot_consume_trailing_data() {
    let bytes = packet(0x17, &[1, 7, 0, 8, 0, 9, 0, 10, 0, 11]);
    let windows = Windows::parse(
        SegmentIter::new(&bytes, Envelope::Packet)
            .next()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let window = windows.iter().next().unwrap();
    assert_eq!(
        (window.id, window.x, window.y, window.width, window.height),
        (7, 8, 9, 10, 11)
    );
    let bytes = packet(0x17, &[0, 1]);
    assert!(
        Windows::parse(
            SegmentIter::new(&bytes, Envelope::Packet)
                .next()
                .unwrap()
                .unwrap()
        )
        .is_err()
    );
}
