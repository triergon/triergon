//! Storage budget: self-authored streams (SPS, PPS, an IDR slice and P slices built bit by
//! bit from the Rec. ITU-T H.265 syntax tables; the slice data is filler) show a refused
//! picture allocation as a typed error, decoding resuming after it, a shared pool returning
//! to idle, and the default limit holding the largest DPB level 6.2 allows.

use triergon_codec::{Budget, StorageError};
use triergon_codec_hevc::{DecodeError, Decoder, Limits};

mod common;
use common::{access_unit, idr_access_unit, nal, p_slice};

/// Send `au`, which the filler slice data may make invalid, but not over budget.
fn send_within_budget(decoder: &mut Decoder, au: &[u8]) {
    let result = decoder.send(au, None);
    assert!(!matches!(result, Err(DecodeError::Budget(_))), "{result:?}");
}

#[test]
fn a_budget_too_small_for_the_picture_is_a_typed_error() {
    let mut decoder = Decoder::with_limits(Limits {
        live_storage_bytes: 4096,
    })
    .unwrap();
    let error = decoder.send(&access_unit(), None).unwrap_err();
    assert!(matches!(
        error,
        DecodeError::Budget(StorageError::Limit { .. })
    ));
    decoder.flush();
    assert!(decoder.receive().is_none());
}

#[test]
fn decoding_resumes_once_a_refused_charge_fits() {
    let budget = Budget::new(1 << 20).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    // Leave room for the parameter sets and scan tables but not for a picture.
    let held = budget.reserve(budget.available() - 4096).unwrap();
    let error = decoder.send(&access_unit(), None).unwrap_err();
    assert!(matches!(
        error,
        DecodeError::Budget(StorageError::Backpressure { .. })
    ));
    assert!(decoder.receive().is_none());
    drop(held);
    send_within_budget(&mut decoder, &access_unit());
    decoder.flush();
    let picture = decoder.receive().unwrap();
    assert_eq!((picture.width, picture.height, picture.poc), (64, 64, 0));
    assert!(decoder.receive().is_none());
}

/// The README's derivation of the default: the largest picture the size cap accepts, in
/// 10-bit storage, in the 6-picture DPB level 6.2 allows at that size, while the caller
/// holds three more output pictures (nine frames, as with the frame pool and two held
/// pictures), with 16x16 CTBs for the largest per-CTB maps.
#[test]
fn the_default_limit_holds_the_largest_level_6_2_dpb() {
    let (width, height) = (8192, 4352);
    let budget = Budget::new(Limits::default().live_storage_bytes).unwrap();
    let mut decoder = Decoder::new_in(&budget);
    let mut held = std::collections::VecDeque::new();
    for poc in 0..10 {
        let au = if poc == 0 {
            idr_access_unit(width, height, 10, 6)
        } else {
            let mut au = Vec::new();
            nal(&mut au, 1, &p_slice(poc, poc.min(5)));
            au
        };
        send_within_budget(&mut decoder, &au);
        while let Some(picture) = decoder.receive() {
            held.push_back(picture);
            if held.len() > 8 {
                held.pop_front();
            }
        }
    }
    let pocs: Vec<i32> = held.iter().map(|p| p.poc).collect();
    assert_eq!(pocs, [2, 3, 4, 5, 6, 7, 8, 9]);
    let frame = (8192 * 4352 + 2 * 4096 * 2176) * size_of::<u16>();
    assert!(budget.peak() >= 9 * frame, "{}", budget.peak());
}

#[test]
fn a_shared_budget_carries_held_pictures_until_they_drop() {
    let budget = Budget::new(1 << 20).unwrap();
    let idle = budget.used();
    let mut decoder = Decoder::new_in(&budget);
    // The filler slice data may be refused; the picture is output either way.
    send_within_budget(&mut decoder, &access_unit());
    decoder.flush();
    let picture = decoder.receive().unwrap();
    assert_eq!((picture.width, picture.height), (64, 64));
    drop(decoder);
    // 64x64 luma and two 32x32 chroma planes, rows padded to 64 and 32 samples.
    assert!(budget.used() >= idle + 64 * 64 + 2 * 32 * 32);
    drop(picture);
    assert_eq!(budget.used(), idle);
}
