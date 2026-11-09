//! The DVD receiver (`compat::dvd::Decoder`) on authored units. Expected values
//! are the pinned FFmpeg build's `dvdsub` output for the same units.
#[path = "dvd-support/mod.rs"]
mod support;

use support::{Cmd, SHOW, field, glyphs, raw_unit, unit};
use triergon_core::{Budget, BudgetVec, Error, WorkBudget};
use triergon_subtitles::compat::dvd::{Decoder, Limits, Settings, Status, Subtitle, ifo_palette};

const PALETTE: [u32; 16] = [
    0x000000, 0xf0f0f0, 0xcccccc, 0x999999, 0x3333fa, 0x1111bb, 0xfa3333, 0xbb1111, 0x33fa33,
    0x11bb11, 0xfafa33, 0xbbbb11, 0xfa33fa, 0xbb11bb, 0x33fafa, 0x11bbbb,
];

fn settings() -> Settings {
    Settings {
        palette: Some(PALETTE),
        ..Settings::default()
    }
}

/// What a subtitle shows, owned.
#[derive(Debug, PartialEq, Eq)]
struct Shown {
    start: u32,
    end: u32,
    forced: bool,
    rect: (u16, u16, u16, u16),
    palette: Vec<u32>,
    rows: Vec<Vec<u8>>,
}

impl From<Subtitle<'_>> for Shown {
    fn from(s: Subtitle<'_>) -> Self {
        let b = s.bitmap;
        let rows = (0..usize::from(b.height))
            .map(|y| b.pixels[y * b.stride..][..usize::from(b.width)].to_vec())
            .collect();
        Self {
            start: s.start_ms,
            end: s.end_ms,
            forced: s.forced,
            rect: (b.x, b.y, b.width, b.height),
            palette: b.palette.to_vec(),
            rows,
        }
    }
}

/// Decode each packet; one entry per packet.
fn decode_all(settings: Settings, packets: &[&[u8]]) -> Vec<Option<Shown>> {
    let budget = Budget::new(1 << 26).unwrap();
    let mut decoder = Decoder::new(&budget, settings, Limits::default());
    packets
        .iter()
        .map(
            |p| match decoder.decode(p, &mut WorkBudget::new(u64::MAX)).unwrap() {
                Status::Subtitle(s) => Some(s.into()),
                Status::Nothing => None,
                Status::Yield => panic!("unbounded work yielded"),
            },
        )
        .collect()
}

fn decode(settings: Settings, packet: &[u8]) -> Option<Shown> {
    decode_all(settings, &[packet]).pop().unwrap()
}

fn shows(packet: &[u8]) -> Option<Shown> {
    decode(settings(), packet)
}

fn crop(rows: &[Vec<u8>], x: usize, y: usize, w: usize, h: usize) -> Vec<Vec<u8>> {
    rows[y..y + h]
        .iter()
        .map(|r| r[x..x + w].to_vec())
        .collect()
}

#[test]
fn a_unit_presents_its_bitmap_colours_and_window() {
    let image = glyphs(16, 6, 1, 0);
    let shown = shows(&unit(&image, 100, 200, &[(0, &SHOW), (100, &[Cmd::Stop])])).unwrap();
    assert_eq!(shown.start, 0);
    assert_eq!(shown.end, 1137);
    assert!(!shown.forced);
    assert_eq!(
        shown.palette,
        [0x00000000, 0xfff0f0f0, 0xffcccccc, 0xff999999]
    );
    assert_eq!(shown.rect, (100, 200, 16, 6));
    assert_eq!(shown.rows, image);
}

#[test]
fn the_window_is_the_last_start_and_stop_in_traversal_order() {
    let image = glyphs(16, 6, 1, 0);
    let window = |sequences: &[(u16, &[Cmd])]| {
        shows(&unit(&image, 0, 0, sequences)).map(|s| (s.start, s.end))
    };
    assert_eq!(
        window(&[(50, &SHOW), (150, &[Cmd::Stop])]),
        Some((568, 1706))
    );
    assert_eq!(window(&[(0, &SHOW)]), Some((0, 0)));
    assert_eq!(
        window(&[(100, &SHOW), (50, &[Cmd::Stop])]),
        Some((1137, 568))
    );
    assert_eq!(
        window(&[
            (0, &SHOW),
            (100, &[Cmd::Stop]),
            (200, &[Cmd::Start]),
            (300, &[Cmd::Stop])
        ]),
        Some((2275, 3413))
    );
    assert_eq!(
        window(&[(0xffff, &SHOW), (0xffff, &[Cmd::Stop])]),
        Some((745642, 745642))
    );
}

#[test]
fn a_subtitle_without_an_end_ends_with_its_packet() {
    let image = glyphs(16, 6, 1, 0);
    let budget = Budget::new(1 << 20).unwrap();
    let mut decoder = Decoder::new(&budget, settings(), Limits::default());
    let packet = unit(&image, 0, 0, &[(0, &SHOW)]);
    let Status::Subtitle(s) = decoder
        .decode(&packet, &mut WorkBudget::new(u64::MAX))
        .unwrap()
    else {
        panic!("no subtitle");
    };
    assert_eq!(s.ending_with_packet(2500).end_ms, 2500);
    let packet = unit(&image, 0, 0, &[(0, &SHOW), (90, &[Cmd::Stop])]);
    let Status::Subtitle(s) = decoder
        .decode(&packet, &mut WorkBudget::new(u64::MAX))
        .unwrap()
    else {
        panic!("no subtitle");
    };
    assert_eq!(s.ending_with_packet(4321).end_ms, 1024);
}

#[test]
fn transparent_borders_are_removed_unless_the_unit_is_a_menu() {
    let image = glyphs(16, 6, 1, 1);
    let shown = shows(&unit(&image, 10, 10, &[(0, &SHOW), (100, &[Cmd::Stop])])).unwrap();
    assert_eq!(shown.rect.0, 11);
    assert_eq!(shown.rect.1, 11);
    let (w, h) = (usize::from(shown.rect.2), usize::from(shown.rect.3));
    assert_eq!(shown.rows, crop(&image, 1, 1, w, h));

    let menu = [
        Cmd::Palette([3, 2, 1, 0]),
        Cmd::Alpha([15, 15, 15, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Forced,
    ];
    let shown = shows(&unit(&image, 10, 10, &[(0, &menu), (100, &[Cmd::Stop])])).unwrap();
    assert!(shown.forced);
    assert_eq!(shown.rect, (10, 10, 16, 6));
    assert_eq!(shown.rows, image);

    // A forced start after the bitmap keeps the area but not the flag.
    let shown = shows(&unit(
        &image,
        10,
        10,
        &[(0, &SHOW), (50, &[Cmd::Forced]), (100, &[Cmd::Stop])],
    ))
    .unwrap();
    assert!(!shown.forced);
    assert_eq!(shown.rect, (10, 10, 16, 6));

    // A menu is shown even when every colour is transparent.
    let clear = [Cmd::Alpha([0; 4]), Cmd::Area, Cmd::Offsets, Cmd::Forced];
    let shown = shows(&unit(&image, 10, 10, &[(0, &clear)])).unwrap();
    assert_eq!(shown.palette, [0; 4]);
}

#[test]
fn a_fully_transparent_bitmap_presents_nothing() {
    let image = glyphs(16, 6, 1, 1);
    let clear = [Cmd::Alpha([0; 4]), Cmd::Area, Cmd::Offsets, Cmd::Start];
    assert_eq!(shows(&unit(&image, 0, 0, &[(0, &clear)])), None);
    // Without a contrast command every colour is transparent.
    let bare = [
        Cmd::Palette([3, 2, 1, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Start,
    ];
    assert_eq!(shows(&unit(&image, 0, 0, &[(0, &bare)])), None);
}

#[test]
fn a_sequence_with_area_and_offsets_redraws_with_the_current_colours() {
    let image = glyphs(16, 6, 1, 0);
    let redraw = [Cmd::Palette([5, 6, 7, 8]), Cmd::Area, Cmd::Offsets];
    let shown = shows(&unit(&image, 0, 0, &[(0, &SHOW), (50, &redraw)])).unwrap();
    assert_eq!(
        shown.palette,
        [0x0033fa33, 0xffbb1111, 0xfffa3333, 0xff1111bb]
    );
    // A palette change without both area and offsets leaves the bitmap as drawn.
    for change in [
        &[Cmd::Palette([5, 6, 7, 8]), Cmd::Area][..],
        &[Cmd::Palette([5, 6, 7, 8]), Cmd::Offsets][..],
    ] {
        let shown = shows(&unit(&image, 0, 0, &[(0, &SHOW), (50, change)])).unwrap();
        assert_eq!(shown.palette[1], 0xfff0f0f0);
    }
    // Area and offsets must share a sequence.
    let split = [
        Cmd::Palette([3, 2, 1, 0]),
        Cmd::Alpha([15, 15, 15, 0]),
        Cmd::Area,
    ];
    assert_eq!(
        shows(&unit(&image, 0, 0, &[(0, &split), (10, &[Cmd::Offsets])])),
        None
    );
    // A later area moves the redrawn bitmap; a reversed or one-row area is skipped.
    let shown = shows(&unit(
        &image,
        0,
        0,
        &[
            (0, &SHOW),
            (50, &[Cmd::AreaAt(20, 35, 30, 35), Cmd::Offsets]),
        ],
    ))
    .unwrap();
    assert_eq!(shown.rect.0, 20);
    for skipped in [Cmd::AreaAt(35, 20, 30, 35), Cmd::AreaAt(0, 15, 30, 30)] {
        let shown = shows(&unit(
            &image,
            0,
            0,
            &[(0, &SHOW), (50, &[skipped, Cmd::Offsets])],
        ))
        .unwrap();
        assert_eq!(shown.rect, (0, 0, 16, 6));
    }
    // A failed redraw drops the unit even after a good one.
    let narrow = [Cmd::AreaAt(0, 5, 0, 5), Cmd::Offsets];
    assert_eq!(
        shows(&unit(&image, 0, 0, &[(0, &SHOW), (50, &narrow)])),
        None
    );
}

#[test]
fn an_unknown_command_ends_its_sequence_and_truncated_operands_drop_the_unit() {
    let image = glyphs(16, 6, 1, 0);
    let colcon: &[Cmd] = &[
        Cmd::Alpha([15, 15, 15, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Raw(&[7, 0, 6, 0x0f, 0xff, 0xff, 0xff]),
        Cmd::Palette([3, 2, 1, 0]),
    ];
    let shown = shows(&unit(&image, 0, 0, &[(0, colcon), (100, &[Cmd::Stop])])).unwrap();
    assert_eq!(shown.palette, [0, 0xff000000, 0xff000000, 0xff000000]);
    assert_eq!(shown.end, 1137);

    let mut truncated = unit(&image, 0, 0, &[(0, &SHOW), (100, &[Cmd::Stop])]);
    let end = truncated.len() - 1;
    truncated[end] = 3; // a palette command without operands replaces 0xff
    assert_eq!(shows(&truncated), None);
}

#[test]
fn links_are_followed_forward_only() {
    let image = glyphs(16, 6, 1, 0);
    let mut packet = unit(
        &image,
        0,
        0,
        &[(0, &SHOW), (100, &[Cmd::Stop]), (200, &[Cmd::Stop])],
    );
    let control = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let second = control + 4 + 3 + 3 + 7 + 5 + 1 + 1;
    let third = second + 6;
    let window = |packet: &[u8]| shows(packet).map(|s| s.end);
    assert_eq!(window(&packet), Some(2275));
    packet[control + 2..control + 4].copy_from_slice(&(third as u16).to_be_bytes());
    assert_eq!(window(&packet), Some(2275));
    packet[control + 2..control + 4].copy_from_slice(&(control as u16).to_be_bytes());
    assert_eq!(window(&packet), Some(0));
    packet[control + 2..control + 4].copy_from_slice(&(second as u16).to_be_bytes());
    packet[second + 2..second + 4].copy_from_slice(&(control as u16).to_be_bytes());
    assert_eq!(window(&packet), Some(1137));
}

#[test]
fn a_control_offset_of_zero_runs_no_sequence() {
    let image = glyphs(16, 6, 1, 1);
    let a = unit(&image, 10, 10, &[(0, &SHOW), (100, &[Cmd::Stop])]);
    let bare = unit(
        &image,
        30,
        30,
        &[
            (0, &[Cmd::Area, Cmd::Offsets, Cmd::Start]),
            (100, &[Cmd::Stop]),
        ],
    );
    // Colour commands right after the header, where a sequence at offset 0
    // would read them.
    let narrow = [0, 15, 0, 0, 3, 0x76, 0x54, 4, 0xf8, 0x40, 0xff, 0, 0, 0, 0];
    let wide = [
        0, 0, 0, 0, 0, 21, 0, 0, 0, 0, 3, 0x12, 0x34, 4, 0xff, 0x80, 0xff, 0, 0, 0, 0,
    ];
    let out = decode_all(settings(), &[&a, &narrow, &bare, &wide, &bare]);
    let palette = &out[0].as_ref().unwrap().palette;
    assert_eq!((&out[1], &out[3]), (&None, &None));
    assert_eq!(&out[2].as_ref().unwrap().palette, palette);
    assert_eq!(&out[4].as_ref().unwrap().palette, palette);
}

#[test]
fn pixels_read_to_the_end_of_the_unit_and_runs_stay_on_their_line() {
    let full = field(&[&[(16, 1)], &[(16, 1)], &[(16, 1)]]);
    let body = |top: u16, bottom: u16| {
        let mut b = vec![3, 0x32, 0x10, 4, 0xff, 0xf0, 5, 0, 0, 0x0f, 0, 0, 5];
        b.extend([6]);
        b.extend(top.to_be_bytes());
        b.extend(bottom.to_be_bytes());
        b.extend([1, 0xff]);
        b
    };
    let rows = |packet: &[u8]| shows(packet).map(|s| s.rows);
    // Both fields from one offset.
    assert_eq!(
        rows(&raw_unit(&full, [(0, body(4, 4))])),
        Some(vec![vec![1; 16]; 6])
    );
    // A field that runs out reads zero: the transparent rows are cropped away.
    let two = field(&[&[(16, 1)], &[(16, 1)]]);
    assert_eq!(
        rows(&raw_unit(&two, [(0, body(4, 4))])),
        Some(vec![vec![1; 16]; 4])
    );
    // Fill-to-end codes, and runs past the line end.
    let fill = field(&[&[(4, 2), (0, 1)], &[(0, 3)], &[(0, 3)]]);
    let shown = rows(&raw_unit(
        &[fill.clone(), full.clone()].concat(),
        [(0, body(4, 4 + fill.len() as u16))],
    ))
    .unwrap();
    assert_eq!(shown[0], [vec![2; 4], vec![1; 12]].concat());
    assert_eq!(shown[2], vec![3; 16]);
    for bad in [
        field(&[&[(17, 1)]]),
        field(&[&[(10, 1), (10, 2)]]),
        field(&[&[(10, 1)], &[(16, 1)]]),
    ] {
        let data = [bad.clone(), full.clone()].concat();
        assert_eq!(
            rows(&raw_unit(&data, [(0, body(4, 4 + bad.len() as u16))])),
            None
        );
    }
    // A field offset within four bytes of the end drops the unit; past the
    // control table the field reads the zeros that follow it.
    let length = raw_unit(&full, [(0, body(4, 4))]).len() as u16 + 8;
    for (offset, visible) in [
        (length - 8, true),
        (length - 4, true),
        (length - 3, false),
        (0x7000, false),
    ] {
        let mut packet = raw_unit(&full, [(0, body(4, offset))]);
        packet.extend([0; 8]);
        assert_eq!(rows(&packet).is_some(), visible, "offset {offset}");
    }
}

#[test]
fn units_are_reassembled_as_the_reference_does() {
    let a = unit(
        &glyphs(16, 6, 1, 1),
        10,
        10,
        &[(0, &SHOW), (100, &[Cmd::Stop])],
    );
    let b = unit(
        &glyphs(20, 4, 2, 0),
        50,
        60,
        &[(0, &SHOW), (100, &[Cmd::Stop])],
    );
    let at = |out: &[Option<Shown>]| -> Vec<usize> {
        out.iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|_| i))
            .collect()
    };
    let x_of = |out: &[Option<Shown>], i: usize| out[i].as_ref().unwrap().rect.0;
    // Completion, with an empty packet in between.
    let out = decode_all(settings(), &[&a[..30], &[], &a[30..], &b]);
    assert_eq!(at(&out), [2, 3]);
    // Fewer than ten bytes never start a unit (and the rest then starts one).
    assert_eq!(at(&decode_all(settings(), &[&a[..9], &b])), [1]);
    assert_eq!(at(&decode_all(settings(), &[&a[..9], &a[9..], &b])), []);
    // An incomplete start whose control offset lies beyond its size is dropped.
    let mut bad = a[..10].to_vec();
    bad[2..4].copy_from_slice(&0xffff_u16.to_be_bytes());
    let out = decode_all(settings(), &[&bad, &a]);
    assert_eq!(at(&out), [1]);
    // A pending unit swallows whatever follows until it completes.
    let out = decode_all(settings(), &[&a[..30], &b, &b]);
    assert_eq!(at(&out), [2]);
    assert_eq!(x_of(&out, 2), 50);
    // Bytes after a unit are discarded.
    let out = decode_all(settings(), &[&[&a[..], &b[..]].concat(), &b]);
    assert_eq!((x_of(&out, 0), x_of(&out, 1)), (11, 50));
    // A unit is complete once its first control header has arrived.
    let mut big = a.clone();
    big[..2].copy_from_slice(&(a.len() as u16 + 20).to_be_bytes());
    let control = usize::from(u16::from_be_bytes([a[2], a[3]]));
    assert_eq!(at(&decode_all(settings(), &[&big])), [0]);
    assert_eq!(at(&decode_all(settings(), &[&big[..control + 4], &b])), [1]);
    assert_eq!(
        at(&decode_all(settings(), &[&big[..control + 3], &a, &b])),
        [2]
    );
}

#[test]
fn the_automatic_palette_ranks_opaque_selectors() {
    let image = glyphs(16, 6, 1, 0);
    let automatic = |selectors: [u8; 4], alpha: [u8; 4]| {
        let cmds = [
            Cmd::Palette(selectors),
            Cmd::Alpha(alpha),
            Cmd::Area,
            Cmd::Offsets,
            Cmd::Start,
        ];
        decode(Settings::default(), &unit(&image, 0, 0, &[(0, &cmds)])).map(|s| s.palette)
    };
    assert_eq!(
        automatic([3, 2, 1, 0], [15, 8, 15, 15]),
        Some(vec![0xff000000, 0xff545454, 0x88a9a9a9, 0xfffefefe])
    );
    assert_eq!(
        automatic([3, 2, 3, 1], [15, 8, 15, 0]),
        Some(vec![0x00000000, 0xff000000, 0x88fefefe, 0xff000000])
    );
}

#[test]
fn forced_only_drops_ordinary_subtitles() {
    let image = glyphs(16, 6, 1, 0);
    let only = Settings {
        forced_only: true,
        ..settings()
    };
    assert_eq!(
        decode(only.clone(), &unit(&image, 0, 0, &[(0, &SHOW)])),
        None
    );
    let forced = [
        Cmd::Palette([3, 2, 1, 0]),
        Cmd::Alpha([15; 4]),
        Cmd::Forced,
        Cmd::Area,
        Cmd::Offsets,
    ];
    assert!(
        decode(only, &unit(&image, 0, 0, &[(0, &forced)]))
            .unwrap()
            .forced
    );
}

#[test]
fn configuration_and_options_select_the_palette() {
    let read = |text: &[u8]| Settings::from_configuration(text).unwrap();
    let text = b"size: 720x480\npalette: 111111, 222222\nPalette: 333333\n  palette: 444444\n";
    assert_eq!(read(text).palette.unwrap()[..3], [0x111111, 0x222222, 0]);
    let last = read(b"palette: 1, 2, 3, 4\r\npalette: 5\n");
    assert_eq!(last.palette.unwrap()[..2], [5, 0]);
    assert_eq!(read(b"size: 720x480\n").palette, None);
    assert_eq!(read(b"palette:").palette, Some([0; 16]));
    // Lines end at CR as well as LF, and the text at its first NUL.
    assert_eq!(read(b"x\rpalette: 7").palette.unwrap()[0], 7);
    assert_eq!(read(b"x\0\npalette: 7").palette, None);
    // A list continues on the next line; runs of commas and spaces separate
    // entries, and an entry without digits is black.
    let tolerant = read(b"palette: 1,,2\n3, ,4, -1, +5, 0x6, 0xg\n")
        .palette
        .unwrap();
    assert_eq!(tolerant[..9], [1, 2, 3, 4, 0xffffff, 5, 6, 0, 0]);

    let mut options = Settings::default();
    options
        .apply_palette_option(b"112233,445566,778899,aabbcc,0xddeeff,AbCdEf, 102030 ,1122334455");
    assert_eq!(
        options.palette.unwrap()[..9],
        [
            0x112233, 0x445566, 0x778899, 0xaabbcc, 0xddeeff, 0xabcdef, 0x102030, 0x334455, 0
        ]
    );
    options.apply_palette_option(b"zz,445566");
    assert_eq!(options.palette, Some([0; 16]));

    options.apply_ifo(None);
    assert_eq!(options.palette, None);
}

/// The canvas of the `size:` line, read as the reference's `sscanf("%dx%d")`
/// and refused where the reference cannot open its decoder.
#[test]
fn the_size_line_sets_the_canvas() {
    let canvas = |text: &[u8]| Settings::from_configuration(text).map(|s| s.canvas);
    assert_eq!(canvas(b"size: 720x480\n"), Ok(Some([720, 480])));
    assert_eq!(canvas(b"size: 720x576\r\n"), Ok(Some([720, 576])));
    assert_eq!(
        canvas(b"size:1920x1080\npalette: 1\n"),
        Ok(Some([1920, 1080]))
    );
    assert_eq!(canvas(b"# no size\npalette: 1\n"), Ok(None));
    assert_eq!(canvas(b""), Ok(None));
    // The last size wins; the numbers may follow on the next line.
    assert_eq!(
        canvas(b"size: 720x480\nsize: 1280x720\n"),
        Ok(Some([1280, 720]))
    );
    assert_eq!(canvas(b"size:\n 720x480\n"), Ok(Some([720, 480])));
    assert_eq!(canvas(b"size: +720x480junk\n"), Ok(Some([720, 480])));
    // Without two numbers the line is ignored.
    for malformed in [
        &b"size: 720 x 480\n"[..],
        b"size: 720*480\n",
        b"size: x480\n",
        b"size: 720x\n",
        b"size: wide\n",
        b"Size: 720x480\n",
        b" size: 720x480\n",
    ] {
        assert_eq!(canvas(malformed), Ok(None), "{malformed:?}");
    }
    // Sizes the reference refuses to open a decoder with.
    for refused in [
        &b"size: 0x480\n"[..],
        b"size: 720x0\n",
        b"size: -720x480\n",
        b"size: 268435456x1\n",
        b"size: 16384x16384\n",
    ] {
        assert_eq!(
            canvas(refused),
            Err(Error::Invalid("DVD configuration size")),
            "{refused:?}"
        );
    }
    // The largest square the reference accepts, and one past it.
    assert_eq!(canvas(b"size: 16255x16255"), Ok(Some([16255, 16255])));
    assert!(canvas(b"size: 16256x16256").is_err());
    // A number keeps its low 32 bits, as the reference stores it in an `int`.
    assert_eq!(canvas(b"size: 4294968016x480"), Ok(Some([720, 480])));
}

#[test]
fn ifo_colour_tables_convert_with_nearest_rounding() {
    let mut ifo = vec![0; 4096];
    ifo[..12].copy_from_slice(b"DVDVIDEO-VTS");
    ifo[0xcc..0xd0].copy_from_slice(&1_u32.to_be_bytes());
    ifo[0x800..0x802].copy_from_slice(&1_u16.to_be_bytes());
    ifo[0x80c..0x810].copy_from_slice(&16_u32.to_be_bytes());
    let entries = [(16 + 12, 128 + 3, 200 - 5), (200, 114, 128), (144, 125, 5)];
    for (i, (y, cr, cb)) in entries.into_iter().enumerate() {
        let at = 0x810 + 0xa4 + 4 * i;
        ifo[at..at + 4].copy_from_slice(&[0, y, cr, cb]);
    }
    let palette = ifo_palette(&ifo).unwrap();
    assert_eq!(palette[1..3], [0xc0e2d6, 0x90c800]);
    assert_eq!(palette[3], 0x008800);
    assert_eq!(ifo_palette(&ifo[..0x810 + 0xa4 + 20]), None);
    ifo[11] = b'G';
    assert_eq!(ifo_palette(&ifo), None);
}

#[test]
fn work_is_charged_and_an_exhausted_budget_consumes_nothing() {
    let a = unit(
        &glyphs(16, 6, 1, 1),
        10,
        10,
        &[(0, &SHOW), (100, &[Cmd::Stop])],
    );
    let budget = Budget::new(1 << 20).unwrap();
    let mut decoder = Decoder::new(&budget, settings(), Limits::default());
    assert_eq!(
        decoder.decode(&a[..30], &mut WorkBudget::new(0)).unwrap(),
        Status::Nothing
    );
    // The unit's bytes, its two sequences' commands (`SHOW` and `0xff`, then
    // `Stop` and `0xff`) and its pixels.
    let need = (a.len() + 20 + 2 + 16 * 6) as u64;
    assert_eq!(
        decoder
            .decode(&a[30..], &mut WorkBudget::new(need - 1))
            .unwrap(),
        Status::Yield
    );
    // The yielded unit waits for `resume`; another packet is refused.
    assert!(matches!(
        decoder.decode(&a[30..], &mut WorkBudget::new(u64::MAX)),
        Err(Error::Invalid(_))
    ));
    // The work of the slice that yielded counts towards the rest.
    let mut work = WorkBudget::new(need);
    assert!(matches!(
        decoder.resume(&mut work).unwrap(),
        Status::Subtitle(_)
    ));
    assert_eq!(work.spent(), 1);
    // Any quantum makes progress, and the slices pay the unit's work once.
    let mut attempts = 0;
    let mut status = decoder.decode(&a, &mut WorkBudget::new(7)).unwrap();
    while status == Status::Yield {
        attempts += 1;
        status = decoder.resume(&mut WorkBudget::new(7)).unwrap();
    }
    assert!(matches!(status, Status::Subtitle(_)));
    assert_eq!(attempts, need.div_ceil(7) - 1);
    // An abandoned unit leaves the decoder as it was.
    assert_eq!(
        decoder.decode(&a, &mut WorkBudget::new(7)).unwrap(),
        Status::Yield
    );
    decoder.abandon();
    assert!(matches!(
        decoder.resume(&mut WorkBudget::new(7)),
        Err(Error::Invalid(_))
    ));
    // A decode can be undone and repeated, colours included.
    let first = decoder.decode(&a, &mut WorkBudget::new(u64::MAX)).unwrap();
    assert!(matches!(first, Status::Subtitle(_)));
    decoder.rewind().unwrap();
    assert!(matches!(
        decoder.decode(&a, &mut WorkBudget::new(u64::MAX)).unwrap(),
        Status::Subtitle(_)
    ));

    let limits = Limits {
        max_pixels: 16 * 6 - 1,
        ..Limits::default()
    };
    let mut small = Decoder::new(&budget, settings(), limits);
    assert!(matches!(
        small.decode(&a, &mut WorkBudget::new(u64::MAX)),
        Err(Error::Limit {
            requested: 96,
            maximum: 95
        })
    ));
}

#[test]
fn colour_selectors_and_contrast_carry_over_between_units() {
    let image = glyphs(16, 6, 1, 0);
    let first = [
        Cmd::Palette([7, 6, 5, 4]),
        Cmd::Alpha([15, 15, 15, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Start,
    ];
    let bare = [Cmd::Area, Cmd::Offsets, Cmd::Start];
    let out = decode_all(
        settings(),
        &[
            &unit(&image, 0, 0, &[(0, &first)]),
            &unit(&image, 0, 0, &[(0, &bare)]),
        ],
    );
    let palettes: Vec<_> = out
        .iter()
        .map(|s| s.as_ref().unwrap().palette.clone())
        .collect();
    assert_eq!(
        palettes[0],
        [0x003333fa, 0xff1111bb, 0xfffa3333, 0xffbb1111]
    );
    assert_eq!(palettes[1], palettes[0]);
    // Changes made by a unit that presents nothing carry over too.
    let narrow = [
        Cmd::Palette([3, 2, 1, 0]),
        Cmd::Alpha([15, 8, 1, 0]),
        Cmd::AreaAt(0, 5, 0, 5),
        Cmd::Offsets,
    ];
    let out = decode_all(
        settings(),
        &[
            &unit(&image, 0, 0, &[(0, &narrow)]),
            &unit(&image, 0, 0, &[(0, &bare)]),
        ],
    );
    assert_eq!(out[0], None);
    assert_eq!(
        out[1].as_ref().unwrap().palette,
        [0x00000000, 0x11f0f0f0, 0x88cccccc, 0xff999999]
    );
}

/// An extended unit: a zero word, 32-bit size and control offset, then 8-bit
/// fields and sequences with 32-bit links.
fn extended(rows: &[Vec<u8>], commands: &[u8]) -> Vec<u8> {
    let mut top = support::Bits::default();
    let mut bottom = support::Bits::default();
    for (y, row) in rows.iter().enumerate() {
        let w = if y % 2 == 0 { &mut top } else { &mut bottom };
        for &colour in row {
            w.put(0b01, 2); // a single pixel with an 8-bit colour
            w.put(u32::from(colour), 8);
        }
        w.align();
    }
    let (top, bottom) = (top.bytes, bottom.bytes);
    let control = 10 + top.len() + bottom.len();
    let area = [0x85, 1, 0, 31, 1, 0, 15 + rows.len() as u8];
    let mut out = vec![0, 0];
    let mut body = commands.to_vec();
    body.extend(area);
    body.push(0x86);
    body.extend(10_u32.to_be_bytes());
    body.extend((10 + top.len() as u32).to_be_bytes());
    body.extend([1, 0xff]);
    out.extend(((control + 6 + body.len()) as u32).to_be_bytes());
    out.extend((control as u32).to_be_bytes());
    out.extend(top);
    out.extend(bottom);
    out.extend(0_u16.to_be_bytes());
    out.extend((control as u32).to_be_bytes());
    out.extend(body);
    out
}

#[test]
fn extended_units_present_eight_bit_pixels_with_their_own_colours() {
    let rows: Vec<Vec<u8>> = (0..2)
        .map(|y| (0..16).map(|x| (x * 16 + y) as u8).collect())
        .collect();
    let mut palette = vec![0x83];
    for i in 0..256 {
        palette.extend([16 + (i % 220) as u8, 128, 128]);
    }
    let mut contrast = vec![0x84, 255];
    contrast.extend([0; 255]);
    let commands = [palette.clone(), contrast.clone()].concat();
    let shown = shows(&extended(&rows, &commands)).unwrap();
    assert_eq!(shown.palette.len(), 256);
    assert_eq!(shown.palette[0], 0x00000000);
    assert_eq!(shown.palette[1], 0xff010101);
    // Pixel value 0 is transparent and cropped away.
    assert_eq!(shown.rect, (16, 16, 16, 2));
    assert_eq!((shown.rows[0][0], shown.rows[1][0]), (0, 1));
    // The colours must be in the unit itself; the contrast carries over.
    let out = decode_all(
        settings(),
        &[
            &extended(&rows, &commands),
            &extended(&rows, &contrast),
            &extended(&rows, &palette),
        ],
    );
    assert!(out[0].is_some() && out[1].is_none());
    assert_eq!(out[2].as_ref().unwrap().palette[1], 0xff010101);
}

/// A pool of `size` bytes, `held` of them taken by something else.
#[derive(Clone, Copy)]
struct Pool {
    size: usize,
    held: usize,
}

const ROOMY: Pool = Pool {
    size: 1 << 20,
    held: 0,
};

/// Decode `before`, then `refused`, which must fail, then `after`; and the
/// same without `refused`, each with and without a rewind just before
/// `after`. What `after` presents must agree every time. Returns the refusal
/// and what `after` presents without one.
fn refusal_changes_nothing(
    pool: Pool,
    limits: Limits,
    before: &[&[u8]],
    refused: &[u8],
    after: &[&[u8]],
) -> (Error, Vec<Option<Shown>>) {
    let run = |send_refused: bool, rewind: bool| {
        let budget = Budget::new(pool.size).unwrap();
        let _held = BudgetVec::try_filled(&budget, 0_u8, pool.held).unwrap();
        let mut decoder = Decoder::new(&budget, settings(), limits);
        let mut decode = |packet: &[u8]| {
            decoder
                .decode(packet, &mut WorkBudget::new(u64::MAX))
                .map(|status| match status {
                    Status::Subtitle(s) => Some(Shown::from(s)),
                    Status::Nothing => None,
                    Status::Yield => panic!("unbounded work yielded"),
                })
        };
        for packet in before {
            decode(packet).unwrap();
        }
        let refusal = send_refused.then(|| decode(refused).unwrap_err());
        drop(decode);
        if rewind {
            decoder.rewind().unwrap();
        }
        let shown: Vec<_> = after
            .iter()
            .map(
                |packet| match decoder.decode(packet, &mut WorkBudget::new(u64::MAX)) {
                    Ok(Status::Subtitle(s)) => Some(Shown::from(s)),
                    Ok(Status::Nothing) => None,
                    other => panic!("{other:?}"),
                },
            )
            .collect();
        (refusal, shown)
    };
    let (_, presented) = run(false, false);
    let mut refusal = None;
    for rewind in [false, true] {
        let (_, expected) = run(false, rewind);
        let (error, shown) = run(true, rewind);
        assert_eq!(shown, expected, "rewind: {rewind}");
        refusal = error;
    }
    (refusal.unwrap(), presented)
}

/// A unit that sets these colours, then draws `area` from the image at the
/// origin.
fn recoloured(rows: &[Vec<u8>], selectors: [u8; 4], area: Cmd) -> Vec<u8> {
    unit(
        rows,
        0,
        0,
        &[
            (0, &[Cmd::Palette(selectors), Cmd::Alpha([15, 15, 15, 15])]),
            (50, &[area, Cmd::Offsets, Cmd::Start]),
        ],
    )
}

#[test]
fn a_unit_over_its_size_limit_changes_nothing() {
    let a = unit(
        &glyphs(16, 6, 1, 1),
        10,
        10,
        &[(0, &SHOW), (100, &[Cmd::Stop])],
    );
    let b = unit(&glyphs(20, 4, 2, 0), 50, 60, &[(0, &SHOW)]);
    let limits = Limits {
        max_unit: 4096,
        ..Limits::default()
    };
    let huge = vec![0x10; 4097];
    // While a unit is buffered: it still completes with its own bytes.
    let (refusal, presented) =
        refusal_changes_nothing(ROOMY, limits, &[&a[..30]], &huge[1..], &[&a[30..], &b]);
    assert_eq!(
        refusal,
        Error::Limit {
            requested: 30 + 4096,
            maximum: 4096
        }
    );
    assert!(presented.iter().all(Option::is_some));
    // After a unit completed: a rewind still gives it back.
    let (refusal, _) =
        refusal_changes_nothing(ROOMY, limits, &[&a[..30], &a[30..]], &huge, &[&a[30..]]);
    assert_eq!(
        refusal,
        Error::Limit {
            requested: 4097,
            maximum: 4096
        }
    );
}

#[test]
fn an_area_over_its_pixel_limit_keeps_the_colours_and_the_buffered_unit() {
    let image = glyphs(16, 6, 1, 1);
    let a = unit(&image, 10, 10, &[(0, &SHOW)]);
    let bare = unit(&image, 0, 0, &[(0, &[Cmd::Area, Cmd::Offsets, Cmd::Start])]);
    let limits = Limits {
        max_pixels: 16 * 6,
        ..Limits::default()
    };
    let over = Error::Limit {
        requested: 100 * 100,
        maximum: 16 * 6,
    };
    let wide = Cmd::AreaAt(0, 99, 0, 99);
    // The refused unit's colours, set before its area, do not carry over.
    let (refusal, presented) = refusal_changes_nothing(
        ROOMY,
        limits,
        &[&a],
        &recoloured(&image, [7, 6, 5, 4], wide),
        &[&bare],
    );
    assert_eq!(refusal, over);
    assert_eq!(
        presented[0].as_ref().unwrap().palette,
        shows(&a).unwrap().palette
    );
    // A refused continuation leaves the start it continued buffered.
    let bad = recoloured(&image, [7, 6, 5, 4], wide);
    let good = recoloured(&image, [3, 2, 1, 0], Cmd::Area);
    assert_eq!(bad[..30], good[..30]);
    let (refusal, presented) = refusal_changes_nothing(
        ROOMY,
        limits,
        &[&bad[..30]],
        &bad[30..],
        &[&good[30..], &bare],
    );
    assert_eq!(refusal, over);
    assert_eq!(presented[0], shows(&good));
    assert_eq!(
        presented[1].as_ref().unwrap().palette,
        presented[0].as_ref().unwrap().palette
    );
}

#[test]
fn memory_the_budget_refuses_changes_nothing() {
    let image = glyphs(16, 6, 1, 1);
    let a = unit(&image, 10, 10, &[(0, &SHOW)]);
    let bare = unit(&image, 0, 0, &[(0, &[Cmd::Area, Cmd::Offsets, Cmd::Start])]);
    // Its plane needs (100 + 16) * 100 bytes.
    let wide = recoloured(&image, [7, 6, 5, 4], Cmd::AreaAt(0, 99, 0, 99));
    let small = Pool {
        size: 8192,
        held: 0,
    };
    let crowded = Pool {
        size: 1 << 16,
        held: (1 << 16) - 8192,
    };
    for pool in [small, crowded] {
        let (refusal, presented) =
            refusal_changes_nothing(pool, Limits::default(), &[&a], &wide, &[&bare]);
        assert!(
            matches!(
                refusal,
                Error::Limit {
                    requested: 11600,
                    ..
                } | Error::Backpressure {
                    requested: 11600,
                    ..
                }
            ),
            "{refusal:?}"
        );
        assert!(presented[0].is_some());
        // The buffer a continuation needs.
        let huge = vec![0x10; 9000];
        let (refusal, presented) =
            refusal_changes_nothing(pool, Limits::default(), &[&a[..30]], &huge, &[&a[30..]]);
        assert!(
            matches!(refusal, Error::Limit { .. } | Error::Backpressure { .. }),
            "{refusal:?}"
        );
        assert_eq!(presented[0], shows(&a));
    }
}

/// Decode each packet in slices of `quantum`, resuming until it is done; one
/// entry per packet.
fn decode_sliced(packets: &[&[u8]], quantum: u64) -> Vec<Option<Shown>> {
    let budget = Budget::new(1 << 26).unwrap();
    let mut decoder = Decoder::new(&budget, settings(), Limits::default());
    packets
        .iter()
        .map(|packet| {
            let mut status = decoder
                .decode(packet, &mut WorkBudget::new(quantum))
                .unwrap();
            while status == Status::Yield {
                status = decoder.resume(&mut WorkBudget::new(quantum)).unwrap();
            }
            match status {
                Status::Subtitle(s) => Some(s.into()),
                Status::Nothing => None,
                Status::Yield => unreachable!(),
            }
        })
        .collect()
}

/// Units that draw 2-bit and 8-bit pixels, set selectors, contrast and
/// `0x83` colours, redraw, and carry their colours on, present the same in
/// slices of any size, down to one unit of work, as in one call each.
#[test]
fn slices_present_what_one_call_does() {
    let image = glyphs(16, 6, 1, 0);
    let first = [
        Cmd::Palette([7, 6, 5, 4]),
        Cmd::Alpha([15, 8, 1, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Start,
    ];
    let bare = [Cmd::Area, Cmd::Offsets, Cmd::Start];
    let redraw = [Cmd::Palette([5, 6, 7, 8]), Cmd::Area, Cmd::Offsets];
    let rows: Vec<Vec<u8>> = (0..2)
        .map(|y| (0..16).map(|x| (x * 16 + y) as u8).collect())
        .collect();
    let mut palette = vec![0x83];
    for i in 0..256 {
        palette.extend([16 + (i % 220) as u8, 128, 128]);
    }
    let mut contrast = vec![0x84, 255];
    contrast.extend([0; 255]);
    let packets = [
        unit(&image, 0, 0, &[(0, &first)]),
        unit(&image, 0, 0, &[(0, &bare)]),
        unit(&image, 0, 0, &[(0, &SHOW), (50, &redraw)]),
        extended(&rows, &[palette.clone(), contrast.clone()].concat()),
        extended(&rows, &contrast),
        extended(&rows, &palette),
    ];
    let packets: Vec<&[u8]> = packets.iter().map(Vec::as_slice).collect();
    let whole = decode_all(settings(), &packets);
    assert_eq!(whole.iter().flatten().count(), 5);
    for quantum in [1, 3, 64, 1000] {
        assert_eq!(decode_sliced(&packets, quantum), whole, "quantum {quantum}");
    }
}

/// A yielded unit that came whole in its packet is copied; a copy the pool
/// refuses fails the decode and leaves the receiver as it was, and a copy is
/// freed once its unit completes or is abandoned.
#[test]
fn the_copy_of_a_yielded_unit_is_refused_whole_and_freed() {
    let image = glyphs(16, 6, 1, 0);
    let first = [
        Cmd::Palette([7, 6, 5, 4]),
        Cmd::Alpha([15, 8, 1, 0]),
        Cmd::Area,
        Cmd::Offsets,
        Cmd::Start,
    ];
    let a = unit(&image, 0, 0, &[(0, &first)]);
    let b = unit(&image, 0, 0, &[(0, &[Cmd::Area, Cmd::Offsets, Cmd::Start])]);
    let expected = decode_all(settings(), &[&a, &b]);
    let budget = Budget::new(1 << 20).unwrap();
    let mut decoder = Decoder::new(&budget, settings(), Limits::default());
    // With no work the unit yields before it is read, and its copy finds no
    // room.
    let taken = budget.reserve(budget.available()).unwrap();
    assert!(matches!(
        decoder.decode(&a, &mut WorkBudget::new(0)),
        Err(Error::Backpressure { .. })
    ));
    assert!(matches!(
        decoder.resume(&mut WorkBudget::new(1)),
        Err(Error::Invalid(_))
    ));
    drop(taken);
    let shown: Vec<_> = [&a, &b]
        .iter()
        .map(
            |packet| match decoder.decode(packet, &mut WorkBudget::new(u64::MAX)) {
                Ok(Status::Subtitle(s)) => Some(Shown::from(s)),
                other => panic!("{other:?}"),
            },
        )
        .collect();
    assert_eq!(shown, expected);
    // Sliced or not, the decoder ends holding the same; abandoning frees the
    // copy too, and a suspended unit cannot be rewound.
    let held = |quantum: u64| {
        let budget = Budget::new(1 << 20).unwrap();
        let mut decoder = Decoder::new(&budget, settings(), Limits::default());
        let mut status = decoder.decode(&a, &mut WorkBudget::new(quantum)).unwrap();
        while status == Status::Yield {
            status = decoder.resume(&mut WorkBudget::new(quantum)).unwrap();
        }
        budget.used()
    };
    assert_eq!(held(1), held(u64::MAX));
    let before = budget.used();
    assert_eq!(
        decoder.decode(&a, &mut WorkBudget::new(1)).unwrap(),
        Status::Yield
    );
    assert_eq!(budget.used() - before, a.len());
    assert!(matches!(decoder.rewind(), Err(Error::Invalid(_))));
    decoder.abandon();
    assert_eq!(budget.used(), before);
}
