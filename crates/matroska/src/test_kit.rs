//! Fixtures for the unit tests that need the crate's private jobs.
use crate::configuration::{Configuration, ConfigurationJob};
use crate::ebml_kit::{element, mkv, track};
use crate::{BlockingReader, step::ParseStep};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec, WorkBudget};

/// The configuration of one TrackEntry with `extra` fields, read by the blocking
/// reader, so packet-job tests do not depend on the Tracks job they may test.
pub(crate) fn seed_configuration(pool: &Budget, extra: &[u8]) -> Configuration {
    seed_configuration_tracks(pool, &track(1, 1, extra))
}

/// The configuration of the TrackEntry elements `entries`, read the same way.
pub(crate) fn seed_configuration_tracks(pool: &Budget, entries: &[u8]) -> Configuration {
    let raw = mkv(&[element(0x1654ae6b, entries), element(0x1f43b675, &[])].concat());
    let reader =
        BlockingReader::open_in(std::io::Cursor::new(raw), crate::Limits::default(), pool).unwrap();
    let mut tracks = BudgetVec::new(pool);
    for track in reader.tracks() {
        tracks.try_push(track.clone()).unwrap();
    }
    let mut seal = ConfigurationJob::from_validated_vec(tracks, pool, 7, 3);
    match seal
        .step(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
    {
        ParseStep::Event(value) => value,
        _ => panic!("seal"),
    }
}
