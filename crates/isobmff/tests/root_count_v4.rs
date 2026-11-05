//! Resource/error-priority guard against the frozen, uncorrected0dd source.
//! No clock/measurement proves the bound: metadata discovery must not be needed
//! to refuse a slice whose header count already exceeds the explicit policy.
mod common;
use common::{child, roots};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Limits, Mov9RecoveryV3Context, Mov9RunRecoveryV3Builder,
    Mov9V4RootWalkTransportBuilder, PinnedMovD32RegularRecoveryV3, PinnedMovD32RootWalkTransportV4,
};

const BASE: &[u8] = include_bytes!("fixtures/r-front-stco.mov");

#[test]
fn excess_root_count_refuses_before_metadata_discovery_with_explicit_limit() {
    let pool = Budget::new(1 << 20).unwrap();
    let top = roots(&pool, BASE).unwrap();
    let ftyp = top.iter().find(|b| b.header().kind == *b"ftyp").unwrap();
    let movie = top.iter().find(|b| b.header().kind == *b"moov").unwrap();
    let mdat = top.iter().find(|b| b.header().kind == *b"mdat").unwrap();
    let trak = child(movie, b"trak").unwrap();
    let mdia = child(&trak, b"mdia").unwrap();
    let stbl = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
    let mut table = Mov9RunRecoveryV3Builder::new(
        stbl,
        pool.clone(),
        Limits::default(),
        PinnedMovD32RegularRecoveryV3,
    )
    .unwrap();
    while table
        .poll(&mut WorkBudget::new(1), &AtomicBool::new(false))
        .unwrap()
        != BuildStatus::Ready
    {}
    let table = table.finish().unwrap();
    for has_mdat in [false, true] {
        let mut headers = [*ftyp.header(); 17];
        if has_mdat {
            headers[16] = *mdat.header();
        }
        let before = pool.used();
        let error = Mov9V4RootWalkTransportBuilder::new(
            ftyp.clone(),
            movie.clone(),
            &headers,
            &table,
            &pool,
            Mov9RecoveryV3Context {
                physical_length: BASE.len() as u64,
                custom_read_maximum: 1,
            },
            PinnedMovD32RootWalkTransportV4,
        )
        .unwrap_err();
        assert_eq!(
            error,
            Error::Limit {
                requested: headers.len(),
                maximum: 16
            },
            "root-count policy must precede optional/invalid metadata discovery"
        );
        assert_eq!(pool.used(), before);
    }
}
