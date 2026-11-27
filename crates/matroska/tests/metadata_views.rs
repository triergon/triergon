//! Source metadata fields exposed through the public views.
#[path = "support/ebml.rs"]
mod ebml;

use std::io::Cursor;

use ebml::{cluster, element as e, mkv, simple, track, uint as u};
use triergon_matroska::{BlockingReader, Limits};

#[test]
fn chapter_edition_and_tag_flags_use_their_schema_defaults() {
    for flag in [None, Some(0), Some(1)] {
        let field = |id| flag.map_or_else(Vec::new, |value| u(id, value));
        let chapter = e(0xb6, &[u(0x73c4, 11), u(0x91, 0), field(0x98)].concat());
        let chapters = e(0x1043a770, &e(0x45b9, &[field(0x45db), chapter].concat()));
        let tag = e(
            0x67c8,
            &[e(0x45a3, b"TITLE"), e(0x4487, b"Name"), field(0x4484)].concat(),
        );
        let tags = e(0x1254c367, &e(0x7373, &tag));
        let bytes = mkv(&[
            e(0x1654ae6b, &track(1, 1, &[])),
            chapters,
            tags,
            cluster(0, simple(1, 0, 0x80, b"frame")),
        ]
        .concat());
        let reader = BlockingReader::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
        let documents = reader.metadata_documents();
        let edition = documents
            .iter()
            .find_map(|doc| doc.editions().next())
            .unwrap();
        assert_eq!(edition.is_default().unwrap(), flag == Some(1));
        assert_eq!(
            edition.chapters().next().unwrap().hidden().unwrap(),
            flag == Some(1)
        );
        let tag = documents.iter().find_map(|doc| doc.tags().next()).unwrap();
        assert_eq!(
            tag.values().next().unwrap().is_default().unwrap(),
            flag != Some(0)
        );
    }
}

#[test]
fn writing_app_and_attachment_description_preserve_presence() {
    for description in [None, Some(&b"description"[..]), Some(&b""[..])] {
        let field = |id| description.map_or_else(Vec::new, |text| e(id, text));
        let info = e(0x1549a966, &[e(0x4d80, b"muxer"), field(0x5741)].concat());
        let attached = e(
            0x61a7,
            &[
                u(0x46ae, 12),
                e(0x466e, b"a.bin"),
                e(0x4660, b"application/octet-stream"),
                e(0x465c, b"bytes"),
                field(0x467e),
            ]
            .concat(),
        );
        let bytes = mkv(&[
            info,
            e(0x1654ae6b, &track(1, 1, &[])),
            e(0x1941a469, &attached),
            cluster(0, simple(1, 0, 0x80, b"frame")),
        ]
        .concat());
        let reader = BlockingReader::open_seekable(Cursor::new(bytes), Limits::default()).unwrap();
        let docs = reader.metadata_documents();
        let info = docs.iter().find_map(|doc| doc.info()).unwrap();
        let attached = docs
            .iter()
            .find_map(|doc| doc.attachments().next())
            .unwrap();
        let expected = description.map(|bytes| std::str::from_utf8(bytes).unwrap());
        assert_eq!(info.writing_app().unwrap(), expected);
        assert_eq!(attached.description().unwrap(), expected);
    }
}
