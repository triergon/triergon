//! Legacy QuickTime metadata uses the same data-reference validation as other tracks.
mod common;

use common::{child, roots};
use std::{io::Cursor, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Error, InputCursor, Result, SourceId, WorkBudget,
    blocking::{Feed, seekable_capabilities},
};
use triergon_isobmff::open::{Movie, MovieOpen, OpenLimits, OpenStep};

fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut atom = u32::try_from(8 + payload.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    atom.extend_from_slice(kind);
    atom.extend_from_slice(payload);
    atom
}

fn movie(data_handler: Option<&[u8]>, dependency_flags: u32, local: bool) -> Vec<u8> {
    // The authored fixture puts all media before moov, so inserting metadata
    // cannot move any of its sample offsets.
    let seed = include_bytes!("fixtures/r-end-co64.mov");
    let pool = Budget::new(1 << 20).unwrap();
    let boxes = roots(&pool, seed).unwrap();
    let [_, media, movie] = boxes.as_slice() else {
        panic!("expected one file type, media and movie box");
    };
    let track = child(movie, b"trak").unwrap();
    let mdia = child(&track, b"mdia").unwrap();
    let minf = child(&mdia, b"minf").unwrap();
    let stbl = child(&minf, b"stbl").unwrap();
    let stsz = child(&stbl, b"stsz").unwrap();
    let count = u32::from_be_bytes(stsz.payload()[8..12].try_into().unwrap());
    let mut dependency = dependency_flags.to_be_bytes().to_vec();
    dependency.resize(4 + usize::try_from(count).unwrap(), 0);
    let mut table = stbl.payload().to_vec();
    table.extend(atom(b"sdtp", &dependency));

    let flags = u32::from(local).to_be_bytes();
    let data_entry = atom(b"url ", &flags);
    let mut references = vec![0, 0, 0, 0, 0, 0, 0, 1];
    references.extend(data_entry);

    let mut media_info = atom(b"vmhd", child(&minf, b"vmhd").unwrap().payload());
    media_info.extend(atom(b"dinf", &atom(b"dref", &references)));
    if let Some(handler) = data_handler {
        media_info.extend(atom(b"hdlr", handler));
    }
    media_info.extend(atom(b"stbl", &table));

    let mut information = atom(b"mdhd", child(&mdia, b"mdhd").unwrap().payload());
    let mut handler = child(&mdia, b"hdlr").unwrap().payload().to_vec();
    handler[4..8].copy_from_slice(b"mhlr");
    information.extend(atom(b"hdlr", &handler));
    information.extend(atom(b"minf", &media_info));

    let mut track_body = atom(b"tkhd", child(&track, b"tkhd").unwrap().payload());
    track_body.extend(atom(b"mdia", &information));
    let mut movie_body = atom(b"mvhd", child(movie, b"mvhd").unwrap().payload());
    movie_body.extend(atom(b"trak", &track_body));
    let mut container = atom(b"ftyp", b"isom\0\0\0\0isom");
    container.extend(atom(b"mdat", media.payload()));
    container.extend(atom(b"moov", &movie_body));
    container
}

fn open(bytes: &[u8], recover: bool) -> Result<Movie> {
    let pool = Budget::new(1 << 20)?;
    let mut source = Cursor::new(bytes);
    let caps = seekable_capabilities(&mut source)?;
    let mut feed = Feed::new(source, &pool)?;
    let mut input = InputCursor::new(&pool, SourceId(1), caps, 31)?;
    let mut opener = if recover {
        MovieOpen::new_recovery(&input, OpenLimits::default())?
    } else {
        MovieOpen::new(&input, OpenLimits::default())?
    };
    let cancel = AtomicBool::new(false);
    loop {
        match opener.advance(&mut input, &mut WorkBudget::new(7), &cancel)? {
            OpenStep::NeedInput(request) => feed.supply(&mut input, request)?,
            OpenStep::Ready => return opener.finish(&input),
            OpenStep::Root(_) | OpenStep::Yield => {}
        }
    }
}

fn handler() -> Vec<u8> {
    let mut data = b"\0\0\0\0dhlrcios".to_vec();
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(b"\x04test");
    data
}

#[test]
fn recovery_admits_the_legacy_handler_for_local_media_only() {
    let handler = handler();
    let fixture = movie(Some(&handler), 0, true);
    assert_eq!(
        open(&fixture, false).unwrap_err(),
        Error::Invalid("unexpected track handler box")
    );
    assert_eq!(open(&fixture, true).unwrap().tracks().len(), 1);
    assert_eq!(
        open(&movie(Some(&handler), 0, false), true).unwrap_err(),
        Error::Unsupported("external source data reference")
    );
    assert_eq!(
        open(&movie(Some(&handler[..handler.len() - 1]), 0, true), true).unwrap_err(),
        Error::Invalid("legacy QuickTime data handler")
    );
}

#[test]
fn recovery_accepts_only_the_observed_dependency_flag() {
    assert!(open(&movie(None, 0, true), false).is_ok());
    assert!(open(&movie(None, 1, true), false).is_err());
    let recovered = open(&movie(None, 1, true), true).unwrap();
    let strict = open(&movie(None, 0, true), false).unwrap();
    assert_eq!(
        recovered.tracks()[0].table().sample_count(),
        strict.tracks()[0].table().sample_count()
    );
    assert!(open(&movie(None, 2, true), true).is_err());
    assert!(open(&movie(None, 0x0100_0001, true), true).is_err());
}
