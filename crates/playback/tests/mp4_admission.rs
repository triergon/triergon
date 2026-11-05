use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, Error as CoreError, InputCursor, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_playback::{
    ByteSource, Error, Format, Result,
    io::{SelectedInput, SourceOwner},
    mp4_open::{MetadataReadScope, Movie, Mp4Open, OpenLimits, OpenProgress},
};

const ISO: &[u8] = include_bytes!("../../isobmff/tests/fixtures/encoded-edit-v2/original.mp4");
const QUICKTIME: &[u8] =
    include_bytes!("../../isobmff/tests/fixtures/encoded-metadata-v4/qt-v1-wave.mov");
const DATA_HANDLER: &[u8] =
    b"\0\0\0\x2chdlr\0\0\0\0dhlrurl \0\0\0\0\0\0\0\0\0\0\0\0\x0bDataHandler";

fn open(raw: &[u8], progressive: bool) -> Result<Movie> {
    let pool = Budget::new(1 << 22)?;
    let id = SourceId(772);
    let selected = SelectedInput {
        input: InputCursor::new(
            &pool,
            id,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: true,
            },
            4096,
        )?,
        source: SourceOwner::new(ByteSource::new(
            id,
            Bytes::copy_from_slice(&pool, raw)?,
            97,
        )?),
        format: Format::Mp4,
    };
    let mut opener = if progressive {
        Mp4Open::new_progressive(selected, OpenLimits::default())?
    } else {
        Mp4Open::new(selected, OpenLimits::default())?
    };
    let cancelled = AtomicBool::new(false);
    for _ in 0..100_000 {
        if matches!(
            opener.advance(&mut WorkBudget::new(17), &cancelled)?,
            OpenProgress::Ready
        ) {
            return Ok(opener.finish()?.movie);
        }
    }
    panic!("MP4 opener did not finish");
}

fn word(raw: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(raw[at..at + 4].try_into().unwrap())
}

fn atom(raw: &[u8], mut children: Range<usize>, kind: &[u8; 4]) -> Range<usize> {
    while children.start < children.end {
        let start = children.start;
        let end = start + word(raw, start) as usize;
        assert!((start + 8..=children.end).contains(&end));
        if &raw[start + 4..start + 8] == kind {
            return start..end;
        }
        children.start = end;
    }
    panic!("fixture is missing {kind:?}");
}

fn with_minf_atoms(fixture: &[u8], added: &[u8], after_dinf: bool) -> Vec<u8> {
    let moov = atom(fixture, 0..fixture.len(), b"moov");
    let trak = atom(fixture, moov.start + 8..moov.end, b"trak");
    let mdia = atom(fixture, trak.start + 8..trak.end, b"mdia");
    let minf = atom(fixture, mdia.start + 8..mdia.end, b"minf");
    let dinf = atom(fixture, minf.start + 8..minf.end, b"dinf");
    let at = if after_dinf { dinf.end } else { dinf.start };
    let extra = u32::try_from(added.len()).unwrap();
    let mut raw = fixture.to_vec();
    if moov.start < atom(fixture, 0..fixture.len(), b"mdat").start {
        // The front QuickTime fixture has one track and one chunk.
        let stbl = atom(fixture, minf.start + 8..minf.end, b"stbl");
        let stco = atom(fixture, stbl.start + 8..stbl.end, b"stco");
        assert_eq!(word(fixture, stco.start + 12), 1);
        let offset = stco.start + 16;
        raw[offset..offset + 4].copy_from_slice(&(word(fixture, offset) + extra).to_be_bytes());
    }
    for parent in [moov, trak, mdia, minf] {
        let size = word(fixture, parent.start) + extra;
        raw[parent.start..parent.start + 4].copy_from_slice(&size.to_be_bytes());
    }
    raw.splice(at..at, added.iter().copied());
    raw
}

fn assert_refused(raw: &[u8], case: &str) {
    let result = open(raw, false).map(|_| ());
    assert!(
        matches!(
            result,
            Err(Error::Core(CoreError::Invalid(_) | CoreError::Truncated))
        ),
        "{case}: {result:?}"
    );
}

#[test]
fn strict_iso_rejects_extra_minf_handler() {
    let raw = with_minf_atoms(ISO, b"\0\0\0\x08hdlr", false);
    assert_refused(&raw, "header-only ISO minf handler");
    let raw = with_minf_atoms(ISO, DATA_HANDLER, false);
    assert_refused(&raw, "QuickTime data handler in ISO movie");
}

#[test]
fn quicktime_keeps_legacy_data_handler_separate_from_media_handler() {
    let raw = with_minf_atoms(QUICKTIME, DATA_HANDLER, false);
    for progressive in [false, true] {
        let movie = open(&raw, progressive).unwrap();
        assert_eq!(movie.major_brand(), Some(*b"qt  "));
        assert_eq!(
            matches!(
                movie.read_scope(),
                MetadataReadScope::FrontMetadataBeforeMdat(_)
            ),
            progressive
        );
        let [track] = movie.tracks() else {
            panic!("expected the fixture's single audio track");
        };
        assert_eq!(track.header().handler, *b"soun");
    }
}

#[test]
fn quicktime_rejects_malformed_minf_data_handler() {
    let mut wrong_type = DATA_HANDLER.to_vec();
    wrong_type[12..16].copy_from_slice(b"mhlr");
    let mut bad_name = DATA_HANDLER.to_vec();
    bad_name[32] += 1;
    let mut reserved = DATA_HANDLER.to_vec();
    reserved[20] = 1;
    for (case, handler) in [
        ("header-only handler", &b"\0\0\0\x08hdlr"[..]),
        ("media handler in minf", wrong_type.as_slice()),
        ("truncated component name", bad_name.as_slice()),
        ("nonzero reserved manufacturer", reserved.as_slice()),
    ] {
        let raw = with_minf_atoms(QUICKTIME, handler, false);
        assert_refused(&raw, case);
    }
    let mut alias = DATA_HANDLER.to_vec();
    alias[16..20].copy_from_slice(b"alis");
    let raw = with_minf_atoms(QUICKTIME, &alias, false);
    let result = open(&raw, false).map(|_| ());
    assert!(
        matches!(
            result,
            Err(Error::Core(CoreError::Unsupported(
                "QuickTime data handler reference type"
            )))
        ),
        "unsupported alias data handler: {result:?}"
    );
}

#[test]
fn quicktime_rejects_duplicate_or_late_minf_data_handler() {
    let duplicate = [DATA_HANDLER, DATA_HANDLER].concat();
    let raw = with_minf_atoms(QUICKTIME, &duplicate, false);
    assert_refused(&raw, "duplicate data handler");
    let raw = with_minf_atoms(QUICKTIME, DATA_HANDLER, true);
    assert_refused(&raw, "data handler after dinf");
}
