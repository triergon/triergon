//! Storage that depends on the stream is charged to the decoder's budget: a refused charge
//! is a typed error, and output pictures keep their frame buffers charged.

use triergon_codec::{Budget, StorageError};
use triergon_codec_h264::{AnnexBNalIter, DecodeError, Decoder, Limits, Picture};

/// 80x48 (5x3 macroblocks), one IDR and one P picture.
fn stream() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../playback/tests/fixtures/raw-video/avc-80x48.h264"
    );
    std::fs::read(path).unwrap()
}

const FRAME_BYTES: usize = 80 * 48 * 3 / 2;

#[test]
fn a_budget_too_small_for_a_frame_is_a_typed_error() {
    let limits = Limits {
        live_storage_bytes: Budget::control_storage_bytes() + FRAME_BYTES / 2,
    };
    let mut decoder = Decoder::with_limits(limits).unwrap();
    let error = decoder.send(&stream(), None).unwrap_err();
    assert!(matches!(error, DecodeError::Budget(_)), "{error:?}");
    decoder.flush();
    while decoder.receive().is_some() {}
}

#[test]
fn a_shared_budget_returns_to_idle_once_the_decoder_and_its_pictures_drop() {
    let budget = Budget::new(16 << 20).unwrap();
    let idle = budget.used();
    let mut decoder = Decoder::new_in(&budget);
    decoder.send(&stream(), None).unwrap();
    decoder.flush();
    let pictures: Vec<Picture> = std::iter::from_fn(|| decoder.receive()).collect();
    assert_eq!(pictures.len(), 2);
    drop(decoder);
    assert!(budget.used() >= idle + FRAME_BYTES);
    drop(pictures);
    assert_eq!(budget.used(), idle);
}

#[test]
fn a_limit_below_the_pools_own_charge_is_refused() {
    let limits = Limits {
        live_storage_bytes: 0,
    };
    let Err(DecodeError::Budget(error)) = Decoder::with_limits(limits) else {
        panic!("a limit below the pool's own charge was accepted");
    };
    assert!(matches!(error, StorageError::Limit { .. }));
}

#[test]
fn decoding_resumes_once_the_pool_has_room() {
    let budget = Budget::new(16 << 20).unwrap();
    let blocker = budget
        .reserve(budget.available() - FRAME_BYTES / 2)
        .unwrap();
    let mut decoder = Decoder::new_in(&budget);
    let error = decoder.send(&stream(), None).unwrap_err();
    assert!(matches!(error, DecodeError::Budget(_)), "{error:?}");
    while decoder.receive().is_some() {}
    drop(blocker);
    // The stream again from its IDR picture: both pictures decode.
    decoder.send(&stream(), None).unwrap();
    decoder.flush();
    assert_eq!(std::iter::from_fn(|| decoder.receive()).count(), 2);
}

/// 4096x2304 (36864 macroblocks, the level 5.2 maximum frame size), level 5.2, five
/// reference frames, MBAFF (so stored motion is not compacted), eight frames of flat grey:
/// `ffmpeg -f lavfi -i color=c=gray:size=4096x2304:rate=25 -frames:v 8 -pix_fmt yuv420p
/// -c:v libx264 -preset medium -profile:v high -level 5.2 -refs 5 -bf 0 -flags +ildct
/// -x264-params tff=1:keyint=100 -f h264`
#[test]
fn the_default_limit_holds_a_full_level_5_2_decoded_picture_buffer() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/level52-4096x2304-mbaff.h264"
    );
    let data = std::fs::read(path).unwrap();
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    // The caller holds the latest picture, as a player does while it is shown.
    let mut shown = None;
    let mut pictures = 0;
    for nal in AnnexBNalIter::new(&data) {
        decoder.send_nal(nal, None).unwrap();
        while let Some(picture) = decoder.receive() {
            pictures += 1;
            shown = Some(picture);
        }
    }
    decoder.flush();
    while let Some(picture) = decoder.receive() {
        pictures += 1;
        shown = Some(picture);
    }
    assert_eq!(pictures, 8);
    assert!(shown.is_some());
    // Five stored frames of samples alone are 5 * 36864 * 384 bytes.
    let dpb_samples = 5 * 36864 * 384;
    assert!(budget.peak() > dpb_samples, "peak {}", budget.peak());
}
