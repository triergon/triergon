//! Page assembly and presentation (EN 300 706).

#[allow(unreachable_pub)]
#[path = "../../tests/teletext_common/mod.rs"]
mod teletext_common;

use super::charset::CharacterSet;
use super::display::{Charsets, Colour, Display, Glyph, Part, Style};
use super::page::{Assembler, Page, Reception};
use super::presentation::{Designations, Presentation, Remapping, Rgb};
use teletext_common::{
    Flags, NORMAL, SUBTITLE, designated_enhancement, enhancement, header, row, triplet,
};

/// The pages each packet completes.
fn assemble(packets: &[[u8; 42]]) -> Vec<Page> {
    let mut assembler = Assembler::new();
    let mut pages = Vec::new();
    for packet in packets {
        assembler
            .push(packet, |reception| {
                if let Reception::Page { page, .. } = reception {
                    pages.push(page.clone());
                }
                Ok(())
            })
            .expect("push");
    }
    pages
}

fn text(page: &Page, row: u8) -> String {
    page.row(row)
        .map(|codes| String::from_utf8_lossy(codes).trim_end().to_owned())
        .unwrap_or_default()
}

#[test]
fn pages_end_on_the_next_header_of_their_magazine() {
    let pages = assemble(&[
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"eight"),
        header(1, 0x00, NORMAL),
        row(1, 2, b"one"),
        header(8, 0x89, NORMAL),
        header(1, 0xFF, NORMAL),
    ]);
    let numbers: Vec<u16> = pages.iter().map(|p| p.header.page.hex()).collect();
    assert_eq!(numbers, [0x888, 0x100]);
    assert_eq!(text(&pages[0], 20), "eight");
}

#[test]
fn serial_pages_end_on_any_header_and_repeats_restart() {
    let serial = Flags {
        serial: true,
        ..SUBTITLE
    };
    let pages = assemble(&[
        header(8, 0x88, serial),
        row(8, 20, b"lost"),
        header(8, 0x88, serial),
        row(8, 21, b"kept"),
        header(1, 0x00, serial),
        row(8, 22, b"after its end"),
        header(1, 0xFF, serial),
    ]);
    assert_eq!(pages.len(), 2);
    assert_eq!(
        (text(&pages[0], 20), text(&pages[0], 21)),
        (String::new(), "kept".to_owned())
    );
    assert_eq!(text(&pages[0], 22), "");
}

#[test]
fn pages_without_erase_merge_onto_their_last_copy() {
    let keep = Flags {
        erase: false,
        ..SUBTITLE
    };
    let pages = assemble(&[
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"first"),
        row(8, 22, b"second"),
        header(8, 0xFF, NORMAL),
        header(8, 0x88, keep),
        row(8, 22, b"replaced"),
        header(8, 0xFF, NORMAL),
    ]);
    assert_eq!(
        text(&pages[1], 20),
        "",
        "a transmission holds only its own packets"
    );
    let mut merged = pages[1].clone();
    merged.complete_from(&pages[0]);
    assert_eq!(
        (text(&merged, 20), text(&merged, 22)),
        ("first".to_owned(), "replaced".to_owned())
    );
}

#[test]
fn damaged_headers_discard_the_page_and_parity_errors_the_row() {
    let mut damaged = header(8, 0x89, SUBTITLE);
    damaged[3] ^= 0x03;
    let mut bad_row = row(8, 21, b"parity");
    bad_row[5] ^= 0x80;
    let pages = assemble(&[
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"discarded"),
        damaged,
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"kept"),
        bad_row,
        header(8, 0xFF, NORMAL),
    ]);
    assert_eq!(pages.len(), 1);
    assert_eq!(
        (text(&pages[0], 20), text(&pages[0], 21)),
        ("kept".to_owned(), String::new())
    );
}

fn present(packets: &[[u8; 42]]) -> Display {
    let page = assemble(packets).pop().expect("page");
    let english = CharacterSet::from_designation(0).expect("English");
    let style = Style {
        charsets: Charsets {
            primary: english,
            secondary: english,
        },
        remapping: Remapping::default(),
        enhanced: true,
    };
    Display::present(&page, style, b"        ")
}

#[test]
fn boxes_need_two_codes_and_double_height_covers_the_next_row() {
    let display = present(&[
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"a\x0bb\x0b\x0bc\x0ad\x0a\x0ae"),
        row(8, 22, b"\x0d\x01\x0b\x0bDH"),
        row(8, 23, b"hidden"),
        header(8, 0xFF, NORMAL),
    ]);
    let boxed: Vec<bool> = display.rows[20][..10].iter().map(|c| c.boxed).collect();
    // A box starts and ends at the second of its codes.
    assert_eq!(
        boxed,
        [
            false, false, false, false, true, true, true, true, true, false
        ]
    );
    assert!(display.is_visible(20, 5) && !display.is_visible(20, 0));
    let dh = display.rows[22][4];
    assert_eq!(
        (dh.glyph, dh.foreground, dh.size.height),
        (Glyph::Char('D'), Colour(1), Part::First)
    );
    assert_eq!(display.rows[23][4].size.height, Part::Second);
    assert_eq!(display.rows[23][0].glyph, Glyph::Char(' '));
}

#[test]
fn held_mosaics_keep_their_form() {
    let display = present(&[
        header(1, 0x00, NORMAL),
        row(1, 2, b"\x11\x1e\x1a!\x19\x12"),
        header(1, 0xFF, NORMAL),
    ]);
    let glyphs: Vec<Glyph> = display.rows[2][1..6].iter().map(|c| c.glyph).collect();
    let mosaic = |code, separated| Glyph::Mosaic { code, separated };
    assert_eq!(
        glyphs,
        [
            mosaic(0x20, false),
            mosaic(0x20, false),
            mosaic(0x21, true),
            mosaic(0x21, true),
            mosaic(0x21, true)
        ]
    );
}

#[test]
fn enhancements_compose_and_move_only_forward() {
    let display = present(&[
        header(8, 0x88, SUBTITLE),
        enhancement(
            8,
            &[
                triplet(40 + 20, 0x04, 10),
                triplet(4, 0x12, b'e'),
                triplet(12, 0x0F, 0x69),
                triplet(11, 0x12, b'q'),
            ],
        ),
        row(8, 20, b"\x0b\x0b"),
        header(8, 0xFF, NORMAL),
    ]);
    assert_eq!(display.rows[20][10].glyph, Glyph::Char('é'));
    assert_eq!(display.rows[20][4].glyph, Glyph::Char(' '));
    assert_eq!(display.rows[20][12].glyph, Glyph::Uncomposable);
}

#[test]
fn rows_23_and_24_ignore_double_height() {
    let display = present(&[
        header(1, 0x00, NORMAL),
        row(1, 23, b"\x0dab"),
        row(1, 24, b"cd"),
        header(1, 0xFF, NORMAL),
    ]);
    assert_eq!(display.rows[23][1].size.height, Part::Single);
    assert_eq!(display.rows[24][0].glyph, Glyph::Char('c'));
}

/// The 13 triplets of a format 1 packet from its fields, least significant
/// bit first.
fn format_1(fields: &[(u32, usize)]) -> [[u8; 3]; 13] {
    let mut bits = Vec::new();
    for &(value, width) in fields {
        bits.extend((0..width).map(|i| value >> i & 1));
    }
    bits.resize(13 * 18, 0);
    let mut triplets = [[0; 3]; 13];
    for (triplet_bytes, chunk) in triplets.iter_mut().zip(bits.chunks(18)) {
        let value = chunk.iter().rev().fold(0, |v, bit| v << 1 | bit);
        *triplet_bytes = triplet(
            (value & 0x3F) as u8,
            (value >> 6 & 0x1F) as u8,
            (value >> 11) as u8,
        );
    }
    triplets
}

#[test]
fn format_1_packets_carry_designations_colours_and_remapping() {
    let mut fields = vec![(0, 4), (0, 3), (0x08, 7), (0x20, 7), (0, 7)];
    fields.extend((0..16).map(|i| (0x456 + i, 12)));
    fields.extend([(0, 5), (0, 5), (0, 1), (5, 3)]);
    let triplets = format_1(&fields);
    let presentation = Presentation::parse(&triplets).expect("format 1");
    assert_eq!(
        presentation.designations,
        Designations {
            default: 0x08,
            second: 0x20
        }
    );
    assert_eq!(
        presentation.colours[1],
        Rgb {
            red: 7,
            green: 5,
            blue: 4
        }
    );
    let remapping = presentation.remapping;
    assert_eq!(
        (remapping.foreground(1), remapping.background(1)),
        (Colour(17), Colour(9))
    );
    fields[0] = (1, 4);
    assert_eq!(
        Presentation::parse(&format_1(&fields)),
        None,
        "page function 1"
    );
    let mut damaged = triplets;
    damaged[7][0] ^= 0x03;
    assert_eq!(Presentation::parse(&damaged), None);
}

#[test]
fn enhancement_attributes_reach_the_level_1_code_that_sets_them() {
    let display = present(&[
        header(1, 0x00, NORMAL),
        enhancement(
            1,
            &[
                triplet(40 + 2, 0x04, 0),
                triplet(1, 0x00, 4),
                triplet(1, 0x03, 2),
                triplet(7, 0x0C, 0x10),
                triplet(40 + 3, 0x04, 0),
                triplet(1, 0x00, 4),
            ],
        ),
        row(1, 2, b"ab\x01cd\x1def"),
        header(1, 0xFF, NORMAL),
    ]);
    let colours = |row: usize| -> Vec<(u8, u8)> {
        display.rows[row][..9]
            .iter()
            .map(|c| (c.foreground.0, c.background.0))
            .collect()
    };
    // Foreground through the colour code, background up to the next
    // background code; invert from column 7.
    assert_eq!(
        colours(2),
        [
            (7, 0),
            (4, 2),
            (4, 2),
            (1, 2),
            (1, 2),
            (1, 1),
            (1, 1),
            (1, 1),
            (1, 1)
        ]
    );
    assert!(display.rows[2][7].inverted && !display.rows[2][6].inverted);
    assert_eq!(display.rows[2][7].shown_colours(), (Colour(1), Colour(1)));
    // Moving down a row ends the colours, not invert.
    assert_eq!(display.rows[3][1].foreground, Colour(4));
    assert!(display.rows[3][0].inverted);
}

/// The characters of display row `row` from column `from`, spaces trimmed.
fn chars(display: &Display, row: usize, from: usize) -> String {
    let text: String = display.rows[row][from..]
        .iter()
        .map(|cell| match cell.glyph {
            Glyph::Char(c) => c,
            _ => '?',
        })
        .collect();
    text.trim_end().to_owned()
}

#[test]
fn enhancement_packets_out_of_order_end_their_reception() {
    let page = |designations: &[u8]| {
        let mut packets = vec![header(1, 0x00, NORMAL)];
        packets.extend(
            designations
                .iter()
                .map(|&d| designated_enhancement(1, d, &[])),
        );
        packets.push(header(1, 0xFF, NORMAL));
        assemble(&packets).pop().expect("page")
    };
    assert!(page(&[0, 1, 2]).has_enhancement(2));
    // A repeat, a gap or a packet before X/26/0 ends the reception.
    assert!(!page(&[0, 0, 1]).has_enhancement(1));
    assert!(!page(&[0, 2, 1]).has_enhancement(1));
    assert!(!page(&[1, 0]).has_enhancement(0));
}

#[test]
fn local_objects_draw_from_the_active_position_and_origin_modifier() {
    let display = present(&[
        header(1, 0x00, NORMAL),
        enhancement(
            1,
            &[
                triplet(40 + 2, 0x04, 2),
                // Origin modifier: one row down, three columns right.
                triplet(40 + 1, 0x10, 3),
                // Active object at X/26/1 triplet 0.
                triplet(40, 0x11, 0x10),
                triplet(5, 0x09, b'Z'),
                // A definition ends the page's own triplets.
                triplet(40, 0x15, 0),
                triplet(7, 0x09, b'Y'),
            ],
        ),
        designated_enhancement(
            1,
            1,
            &[
                triplet(0, 0x09, b'A'),
                triplet(2, 0x00, 1),
                // Row 1 of the object, column 1.
                triplet(40 + 1, 0x04, 1),
                triplet(1, 0x09, b'B'),
                triplet(40, 0x16, 0),
                triplet(3, 0x09, b'C'),
            ],
        ),
        row(1, 2, b"row two text"),
        header(1, 0xFF, NORMAL),
    ]);
    assert!(display.enhanced);
    assert_eq!(chars(&display, 2, 0), "row tZo text");
    assert_eq!(chars(&display, 3, 5), "A");
    assert_eq!(chars(&display, 4, 6), "B");
    // An active object's colour reaches to the row's end.
    let foreground: Vec<u8> = display.rows[3][6..]
        .iter()
        .map(|c| c.foreground.0)
        .collect();
    assert_eq!(foreground[..2], [7, 1]);
    assert!(foreground[1..].iter().all(|&colour| colour == 1));
}

#[test]
fn adaptive_and_passive_objects_reach_less_than_active_ones() {
    let object = |mode: u8| {
        present(&[
            header(1, 0x00, NORMAL),
            enhancement(
                1,
                &[
                    triplet(40 + 2, 0x04, 0),
                    triplet(40, mode, 0x10),
                    triplet(40, 0x15, 0),
                ],
            ),
            designated_enhancement(1, 1, &[triplet(1, 0x00, 1), triplet(6, 0x09, b'Q')]),
            row(1, 2, b"\x04abc\x03defghijkl"),
            header(1, 0xFF, NORMAL),
        ])
    };
    let foreground = |display: &Display| -> Vec<u8> {
        display.rows[2][..9]
            .iter()
            .map(|c| c.foreground.0)
            .collect()
    };
    // Active: up to the Level 1 colour code, which it reaches.
    assert_eq!(foreground(&object(0x11)), [7, 1, 1, 1, 1, 3, 3, 3, 3]);
    // Adaptive: past the colour code, through the active position.
    assert_eq!(foreground(&object(0x12)), [7, 1, 1, 1, 1, 1, 1, 3, 3]);
    // Passive: only the character it places, white on black where unset.
    assert_eq!(foreground(&object(0x13)), [7, 4, 4, 4, 4, 3, 1, 3, 3]);
}

#[test]
fn object_pages_and_drcs_present_the_page_at_level_1() {
    let page = |page_triplets: &[[u8; 3]], object: &[[u8; 3]]| {
        let mut triplets = vec![triplet(40 + 2, 0x04, 0), triplet(3, 0x09, b'Z')];
        triplets.extend_from_slice(page_triplets);
        present(&[
            header(1, 0x00, NORMAL),
            enhancement(1, &triplets),
            designated_enhancement(1, 1, object),
            header(1, 0xFF, NORMAL),
        ])
    };
    let invoke = |address: u8, mode: u8| [triplet(address, mode, 0x10), triplet(40, 0x15, 0)];
    // An object of a POP or GPOP page (address 48 and up).
    assert!(!page(&invoke(48, 0x11), &[]).enhanced);
    assert!(!page(&invoke(63, 0x13), &[]).enhanced);
    // A local object that is not there draws nothing.
    assert!(page(&[triplet(40, 0x11, 0x20)], &[]).enhanced);
    // In an active object, an adaptive one of an object page; the same type
    // is ignored.
    assert!(!page(&invoke(40, 0x11), &[triplet(63, 0x12, 0)]).enhanced);
    let ignored = page(
        &invoke(40, 0x11),
        &[triplet(63, 0x11, 0), triplet(1, 0x09, b'Q')],
    );
    assert!(ignored.enhanced);
    assert_eq!(chars(&ignored, 2, 3), "ZQ");
    // DRCS characters and row mode 0x0B, but not after a definition.
    assert!(!page(&[triplet(5, 0x0D, 3)], &[]).enhanced);
    assert!(!page(&[triplet(40 + 5, 0x0B, 0)], &[]).enhanced);
    assert!(page(&[triplet(40, 0x15, 0), triplet(40 + 5, 0x0B, 0)], &[]).enhanced);
}

#[test]
fn colour_8_is_the_full_row_colour_outside_boxes() {
    let colours = |triplets: &[[u8; 3]]| -> Vec<u8> {
        let display = present(&[
            header(1, 0x00, NORMAL),
            enhancement(1, triplets),
            header(1, 0xFF, NORMAL),
        ]);
        (1..4)
            .map(|row| display.rows[row][3].foreground.0)
            .collect()
    };
    let foreground_8 = triplet(3, 0x00, 8);
    // Full row colour 19 for row 1.
    assert_eq!(
        colours(&[triplet(40 + 1, 0x01, 19), foreground_8]),
        [19, 7, 7]
    );
    // A colour for this row only ends when the active position moves down,
    // before the row it leaves takes the attributes.
    assert_eq!(
        colours(&[
            triplet(40 + 1, 0x01, 19),
            foreground_8,
            triplet(40 + 2, 0x04, 0)
        ]),
        [0, 7, 7]
    );
    // One for this row and those below lasts.
    let down = [
        triplet(40 + 1, 0x01, 0x60 | 0x14),
        triplet(40 + 3, 0x04, 0),
        foreground_8,
    ];
    assert_eq!(colours(&down), [7, 7, 20]);
}
