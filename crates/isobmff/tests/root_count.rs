//! Resource/error-priority guard.
//! No clock/measurement proves the bound: metadata discovery must not be needed
//! to refuse a slice whose header count already exceeds the explicit policy.
mod common;
use common::{child, roots};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Limits,
    compat::mov::recovery::{
        DescriptionContext, DescriptionRecoveryBuilder, DescriptionRecoveryProfile,
    },
    reference_trace::mov::transport::{RootWalkTransportBuilder, RootWalkTransportProfile},
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
    let mut table = DescriptionRecoveryBuilder::new(
        stbl,
        pool.clone(),
        Limits::default(),
        DescriptionRecoveryProfile,
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
        let error = RootWalkTransportBuilder::new(
            ftyp.clone(),
            movie.clone(),
            &headers,
            &table,
            &pool,
            DescriptionContext {
                physical_length: BASE.len() as u64,
                custom_read_maximum: 1,
            },
            RootWalkTransportProfile,
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
