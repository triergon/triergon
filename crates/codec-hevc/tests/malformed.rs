//! Malformed input is a typed error and leaves the decoder usable.

use triergon_codec_hevc::{DecodeError, Decoder};

mod common;
use common::{access_unit, bare_nal, slice_segment};

/// A picture whose first slice references nothing, then a P slice of it that does: the
/// P slice is refused, and a later slice of the same picture, fed NAL unit by NAL unit,
/// must not reach slice state the refused one never made.
#[test]
fn a_refused_slice_leaves_later_slices_of_its_picture_decodable() {
    let mut decoder = Decoder::new();
    let _ = decoder.send(&access_unit(), None);
    let _ = decoder.send_nal(&bare_nal(1, &slice_segment(1, 0, 0)), None);
    let refused = decoder.send_nal(&bare_nal(1, &slice_segment(1, 4, 1)), None);
    assert!(
        matches!(refused, Err(DecodeError::Invalid(_))),
        "{refused:?}"
    );
    let _ = decoder.send_nal(&bare_nal(1, &slice_segment(1, 8, 0)), None);
    decoder.flush();
    while decoder.receive().is_some() {}
}

#[test]
fn a_refused_first_irap_allows_trailing_pictures_but_suppresses_rasl() {
    let trail = bare_nal(1, &[0xc0]); // first slice, PPS 0
    let rasl = bare_nal(8, &[0xc0]);
    let cra = bare_nal(21, &[0xa0]); // first slice, no_output_of_prior_pics = 0, PPS 0
    let eos = bare_nal(36, &[0x80]);
    let mut decoder = Decoder::new();

    assert_eq!(decoder.send_nal(&trail, None), Ok(()));
    let missing_pps = Err(DecodeError::MissingParameterSet("PPS"));
    assert_eq!(decoder.send_nal(&cra, None), missing_pps);
    assert_eq!(decoder.send_nal(&rasl, None), Ok(()));
    assert_eq!(decoder.send_nal(&trail, None), missing_pps);
    assert_eq!(decoder.send_nal(&eos, None), Ok(()));
    assert_eq!(decoder.send_nal(&rasl, None), Ok(()));
    assert_eq!(decoder.send_nal(&trail, None), missing_pps);
    decoder.flush();
    assert!(decoder.receive().is_none());

    decoder.reset();
    assert_eq!(decoder.send_nal(&trail, None), Ok(()));
    assert_eq!(decoder.send_nal(&cra, None), missing_pps);
    assert_eq!(decoder.send_nal(&rasl, None), Ok(()));
    assert_eq!(decoder.send_nal(&trail, None), missing_pps);
}
