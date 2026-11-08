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
        matches!(refused, Err(DecodeError::InvalidData(_))),
        "{refused:?}"
    );
    let _ = decoder.send_nal(&bare_nal(1, &slice_segment(1, 8, 0)), None);
    decoder.flush();
    while decoder.receive().is_some() {}
}
