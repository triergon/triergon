//! FFmpeg's teletext decoder (`compat::teletext`) on authored PES payloads,
//! against the reference's observed subtitles.

mod teletext_common;

use teletext_common::{Flags, NORMAL, SUBTITLE, enhancement, header, payload, row, triplet};
use triergon_core::{Budget, WorkBudget};
use triergon_subtitles::compat::teletext::{
    Decoded, Decoder, Format, Options, PageSelection, Refusal,
};

/// A decode call's index, subtitle time and result: `Ok(Some(text))` for a
/// subtitle with text, `Ok(None)` for an empty one, `Err` for a refusal.
type Call = (usize, Option<i64>, Result<Option<String>, Refusal>);

/// Every decode call with a result; drains after the last packet as the
/// reference does.
fn decode(options: Options, packets: &[Vec<u8>]) -> Vec<Call> {
    let pool = Budget::new(16 << 20).expect("pool");
    let mut decoder = Decoder::new(&pool, options).expect("decoder");
    let mut out = Vec::new();
    for index in 0..packets.len() + 64 {
        let (data, pts) = packets.get(index).map_or((&[][..], None), |p| {
            (p.as_slice(), Some(1_000_000 * (index as i64 + 1)))
        });
        let mut work = WorkBudget::new(Decoder::MAX_PACKET_WORK);
        match decoder.decode(data, pts, &mut work).expect("decode") {
            Decoded::Subtitle(s) => out.push((index, s.pts, Ok(s.ass.map(str::to_owned)))),
            Decoded::Refused(refusal) => out.push((index, None, Err(refusal))),
            Decoded::Nothing if index >= packets.len() => break,
            Decoded::Nothing => {}
        }
    }
    out
}

fn options(format: Format) -> Options {
    Options {
        format,
        ..Options::default()
    }
}

fn subtitle_pages() -> Vec<Vec<u8>> {
    vec![
        payload(&[
            header(8, 0x88, SUBTITLE),
            row(8, 20, b"      \x0d\x0b\x0bGood evening.\x0a\x0a"),
            row(8, 22, b"        \x0d\x03\x0b\x0bsecond\x0a\x0a"),
            header(8, 0xFF, NORMAL),
        ]),
        payload(&[header(8, 0x88, SUBTITLE), header(8, 0xFF, NORMAL)]),
    ]
}

#[test]
fn subtitle_page_as_text() {
    let out = decode(options(Format::Text), &subtitle_pages());
    assert_eq!(
        out,
        [
            (
                0,
                Some(1_000_000),
                Ok(Some(
                    "0,0,Default,,0,0,0,,Good evening.\\Nsecond".to_owned()
                ))
            ),
            (1, Some(2_000_000), Ok(None)),
        ]
    );
}

#[test]
fn subtitle_page_as_ass() {
    let out = decode(options(Format::Ass), &subtitle_pages());
    let expected = "0,0,Subtitle,,0,0,0,,{\\an2}Good\\hevening. \\N\\h{\\c&H00FFFF&}\\h\\hsecond\\h\\h\\h\\h \\N";
    assert_eq!(out[0], (0, Some(1_000_000), Ok(Some(expected.to_owned()))));
    assert_eq!(out[1], (1, Some(2_000_000), Ok(None)));
}

#[test]
fn normal_page_as_text_and_teletext_style() {
    let packets = vec![payload(&[
        header(1, 0x00, NORMAL),
        row(1, 2, b"\x01Red \x02Green \x1d\x04blue on green\x1c"),
        row(1, 4, b"\x0dDouble"),
        row(1, 8, b"\x12\x1e!\"#\x07text"),
        header(1, 0xFF, NORMAL),
    ])];
    let text = decode(options(Format::Text), &packets);
    assert_eq!(
        text[0].2,
        Ok(Some(
            "0,0,Default,,0,0,0,,Red  Green   blue on green\\NDouble\\N\u{ee20}\u{ee21}\u{ee22}\u{ee23}\u{ee23}text"
                .to_owned()
        ))
    );
    let ass = decode(options(Format::Ass), &packets);
    let Ok(Some(event)) = &ass[0].2 else {
        panic!("no ASS event: {ass:?}");
    };
    let rows: Vec<&str> = event
        .trim_start_matches("0,0,Teletext,,0,0,0,,")
        .split("\\N")
        .collect();
    assert_eq!(rows.len(), 25);
    assert_eq!(
        rows[1],
        "\\h{\\c&H0000FF&}Red\\h\\h{\\c&H00FF00&}Green\\h{\\3c&H00FF00&}\\h\\h{\\c&HFF0000&}blue\\hon\\hgreen{\\3c&H000000&}\\h\\h\\h\\h\\h\\h\\h\\h\\h\\h\\h\\h\\h{\\c&HFFFFFF&}\\h "
    );
    assert_eq!(
        rows[4],
        "\\h".repeat(41) + " ",
        "the row under double height is blank"
    );
}

#[test]
fn a_full_queue_refuses_the_packet_and_loses_pages() {
    let mut packets = Vec::new();
    for i in 0..27u8 {
        let page = (((i + 10) / 10) << 4) | ((i + 10) % 10);
        packets.extend([
            header(1, page, NORMAL),
            row(1, 2, format!("page {i}").as_bytes()),
        ]);
    }
    packets.push(header(1, 0xFF, NORMAL));
    let out = decode(options(Format::Text), &[payload(&packets[..55])]);
    assert_eq!(out[0], (0, None, Err(Refusal::QueueFull)));
    assert_eq!(out.len(), 26, "the refusal and the 25 queued pages");
    assert_eq!(
        out[25].2,
        Ok(Some("24,0,Default,,0,0,0,,page 24".to_owned()))
    );
}

#[test]
fn payloads_that_are_not_whole_data_fields_are_refused() {
    let mut short = payload(&[header(8, 0x88, SUBTITLE)]);
    short.truncate(1 + 46);
    let mut bad_length = payload(&[header(8, 0x88, SUBTITLE)]);
    bad_length[2] = 0x2B;
    let out = decode(options(Format::Text), &[short, bad_length, vec![0x10]]);
    assert_eq!(
        out.iter().map(|o| o.2.clone()).collect::<Vec<_>>(),
        [
            Err(Refusal::InvalidData),
            Err(Refusal::InvalidData),
            Err(Refusal::InvalidData)
        ]
    );
}

#[test]
fn page_selection_matches_three_digit_numbers() {
    let packets = vec![payload(&[
        header(8, 0x88, SUBTITLE),
        row(8, 20, b"\x0b\x0b888"),
        header(1, 0x00, NORMAL),
        row(1, 20, b"100"),
        header(1, 0xFF, NORMAL),
        header(8, 0x89, NORMAL),
        row(8, 20, b"889"),
        header(8, 0xFF, NORMAL),
    ])];
    let texts = |pages: &str| {
        let options = Options {
            pages: PageSelection::parse(pages),
            ..options(Format::Text)
        };
        decode(options, &packets)
            .into_iter()
            .filter_map(|o| o.2.ok().flatten())
            .map(|t| t.rsplit(",,").next().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(texts("*"), ["100", "888", "889"]);
    assert_eq!(texts("1-1000"), ["100"]);
    assert_eq!(texts("889 888"), ["888", "889"]);
    assert_eq!(texts("subtitle"), ["888"]);
    assert_eq!(texts("*888"), ["888"]);
}

#[test]
fn uncomposable_marks_cut_the_text() {
    let packets = vec![payload(&[
        header(1, 0x00, NORMAL),
        enhancement(1, &[triplet(40 + 20, 0x04, 2), triplet(3, 0x12, b'q')]),
        row(1, 2, b"before"),
        row(1, 20, b"xxxx cut"),
        row(1, 22, b"after"),
        header(1, 0xFF, NORMAL),
    ])];
    let out = decode(options(Format::Text), &packets);
    assert_eq!(out[0].2, Ok(Some("0,0,Default,,0,0,0,,before".to_owned())));
}

/// The texts of the subtitles with text.
fn texts(packets: &[Vec<u8>]) -> Vec<String> {
    decode(options(Format::Text), packets)
        .into_iter()
        .filter_map(|call| call.2.ok().flatten())
        .map(|text| text.rsplit(",,").next().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_header_without_erase_uses_its_copy_even_if_the_page_restarts() {
    let page = |subcode: u16, erase: bool| {
        header(
            8,
            0x89,
            Flags {
                erase,
                subcode,
                ..NORMAL
            },
        )
    };
    let end = header(8, 0xFF, NORMAL);
    let packets = [
        payload(&[page(0x3F7F, true), row(8, 5, b"AAA"), end]),
        payload(&[page(0x0001, true), row(8, 6, b"BBB"), end]),
        // Subcode 0000 uses the 3F7F copy, which a page of subcode 3F7F,
        // building on the newest copy of any subcode, then builds on.
        payload(&[page(0x0000, false), page(0x3F7F, false), end]),
    ];
    assert_eq!(texts(&packets), ["AAA", "BBB", "AAA"]);
}

#[test]
fn a_subtitle_page_drops_its_copy_after_a_header_that_dropped_it() {
    let subtitle = |erase: bool| header(8, 0x89, Flags { erase, ..SUBTITLE });
    let end = header(8, 0xFF, NORMAL);
    let first = payload(&[header(8, 0x89, NORMAL), row(8, 3, b"row three"), end]);
    // Headers that drop the copy chain the drop on; one that keeps it does not.
    let dropped = payload(&[subtitle(true), subtitle(false), subtitle(false), end]);
    let kept = payload(&[
        subtitle(true),
        header(
            8,
            0x89,
            Flags {
                erase: false,
                ..NORMAL
            },
        ),
        subtitle(false),
        end,
    ]);
    assert_eq!(texts(&[first.clone(), dropped]), ["row three"]);
    assert_eq!(texts(&[first, kept]), ["row three", "row three"]);
}
