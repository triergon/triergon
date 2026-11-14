//! Program stream units (`dvd::program_stream`), the reference's DVD subtitle
//! parser and stream order, and its MP4 palette configuration
//! (`compat::dvd`). Expected values are the pinned FFmpeg build's output.
#[path = "dvd-support/mod.rs"]
mod support;

use support::{pack, padding, pes, pts_bytes, sector, subpicture};
use triergon_core::Budget;
use triergon_subtitles::compat::dvd::{SubpictureParser, SubpictureStreams, mp4_configuration};
use triergon_subtitles::dvd::program_stream::{Pes, Unit, Units};

#[test]
fn packs_and_pes_packets_are_read_in_order() {
    let mut mpeg1_pack = vec![0, 0, 1, 0xba, 0x21, 0, 1, 0, 1, 0x80, 0, 1];
    let mpeg1_pes = {
        let mut body = vec![0xff, 0xff, 0x40, 0x10];
        body.extend(pts_bytes(1234, 0x21));
        body.extend([7, 8, 9]);
        let mut out = vec![0, 0, 1, 0xc0];
        out.extend((body.len() as u16).to_be_bytes());
        out.extend(body);
        out
    };
    let file = [
        pack(),
        subpicture(2, &[1, 2, 3], Some(0x1_2345_6789)),
        pes(0xe0, &[4, 5], None),
        padding(10),
        mpeg1_pack.split_off(0),
        mpeg1_pes,
        vec![0, 0, 1, 0xb9],
    ]
    .concat();
    let units: Vec<_> = Units::new(&file).collect();
    assert_eq!(units.len(), 7);
    assert_eq!(units[0], Unit::Header);
    let Unit::Pes(sub) = &units[1] else { panic!() };
    assert_eq!(sub.pts, Some(0x1_2345_6789));
    assert_eq!(sub.subpicture(), Some((2, &[1, 2, 3][..])));
    assert_eq!(sub.span, 14..14 + 6 + 3 + 5 + 4);
    let Unit::Pes(video) = &units[2] else {
        panic!()
    };
    assert_eq!(
        (video.stream_id, video.pts, video.payload),
        (0xe0, None, &[4, 5][..])
    );
    assert_eq!(video.subpicture(), None);
    let Unit::Pes(pad) = &units[3] else { panic!() };
    assert_eq!(pad.payload, &[0xff; 4]);
    assert_eq!(units[4], Unit::Header);
    let Unit::Pes(audio) = &units[5] else {
        panic!()
    };
    assert_eq!((audio.pts, audio.payload), (Some(1234), &[7, 8, 9][..]));
    assert_eq!(units[6], Unit::End);
}

#[test]
fn a_unit_cut_by_the_window_ends_iteration_where_it_starts() {
    let file = [sector(&[subpicture(0, &[1; 10], None)]), pack()].concat();
    let mut units = Units::new(&file[..2048 + 6]);
    assert_eq!(units.by_ref().count(), 3);
    assert_eq!(units.position(), 2048);
    let mut units = Units::new(&file[30..2048 + 2]);
    assert!(matches!(
        units.next(),
        Some(Unit::Pes(Pes {
            stream_id: 0xbe,
            ..
        }))
    ));
    assert_eq!(units.next(), None);
    assert_eq!(units.position(), 2048 - 30);
}

#[test]
fn subpicture_streams_are_numbered_in_order_of_appearance() {
    let file = [
        sector(&[pes(0xe0, &[0; 8], None)]),
        sector(&[subpicture(3, &[1], None)]),
        sector(&[pes(0xbd, &[0x80, 1, 0, 1, 0x0b, 0x77], None)]),
        sector(&[subpicture(0, &[1], None)]),
        sector(&[subpicture(3, &[1], None)]),
    ]
    .concat();
    let mut streams = SubpictureStreams::default();
    for unit in Units::new(&file) {
        if let Unit::Pes(pes) = unit {
            streams.note(&pes);
        }
    }
    assert_eq!(streams.as_slice(), [3, 0]);
}

fn parse(packets: &[(&[u8], Option<u64>)]) -> Vec<(Option<u64>, Vec<u8>)> {
    let budget = Budget::new(1 << 20).unwrap();
    let mut parser = SubpictureParser::new(&budget);
    packets
        .iter()
        .filter_map(|&(data, pts)| {
            parser
                .push(data, pts)
                .unwrap()
                .map(|u| (u.pts, u.data.to_vec()))
        })
        .collect()
}

#[test]
fn the_parser_joins_packets_into_units() {
    let mut a = vec![0, 61];
    a.extend((2..61).map(|i| i as u8));
    let mut b = vec![0, 58];
    b.extend((2..58).map(|i| i as u8 ^ 0x55));
    // Joined with the first packet's time stamp, even an absent one.
    assert_eq!(
        parse(&[
            (&a[..30], Some(90000)),
            (&a[30..], Some(95000)),
            (&b, Some(180000))
        ]),
        [(Some(90000), a.clone()), (Some(180000), b.clone())]
    );
    assert_eq!(
        parse(&[(&a[..30], None), (&a[30..], Some(95000))]),
        [(None, a.clone())]
    );
    // An overfilled unit is discarded; the next packet starts a unit that keeps
    // the discarded one's time stamp.
    let ab = [a.clone(), b.clone()].concat();
    assert_eq!(
        parse(&[(&ab, Some(90000)), (&b, Some(180000)), (&a, Some(190000))]),
        [(Some(90000), b.clone()), (Some(190000), a.clone())]
    );
    // Too short for a size word: passed on as it is. Empty packets are ignored.
    assert_eq!(
        parse(&[(&[], Some(1)), (&a[..1], Some(90000)), (&b, Some(180000))]),
        [(Some(90000), vec![0]), (Some(180000), b.clone())]
    );
    // An unfinished unit never comes out.
    assert_eq!(parse(&[(&a[..30], Some(90000))]), []);
}

#[test]
fn the_mp4_colour_table_becomes_a_palette_line() {
    let specific = [
        0x00, 0x2a, 0x74, 0xe6, 0x00, 0x71, 0xce, 0x4e, 0x00, 0x1d, 0x80, 0x80, 0x00, 0xd9, 0x80,
        0x80, 0x00, 0x88, 0x2f, 0x40, 0x00, 0x6d, 0xcf, 0xbf, 0x00, 0xcd, 0x88, 0x17, 0x00, 0x51,
        0x58, 0x98, 0x00, 0x9a, 0x80, 0x80, 0x00, 0xb9, 0x80, 0x80, 0x00, 0x47, 0x60, 0x66, 0x00,
        0xaf, 0x61, 0x68, 0x00, 0x21, 0x7c, 0xba, 0x00, 0xad, 0x72, 0x8e, 0x00, 0xb5, 0x9a, 0x25,
        0x00, 0x45, 0xa7, 0x9f,
    ];
    let text = mp4_configuration(&specific).unwrap();
    assert_eq!(
        &text[..],
        &b"palette: 0a00eb, ec440b, 0e0e0e, e9e9e9, 09e60a, e912ea, e8fe07, 0b627b, a0a0a0, \
           c4c4c4, 0c630b, 87db88, 0c0088, 9fbcd2, e9ce07, 7b117b\n"[..]
    );
    assert_eq!(mp4_configuration(&specific[..63]), None);
}
