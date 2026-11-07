mod common;
use common::{child, children, roots, table};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, WorkBudget};
use triergon_isobmff::{
    BuildStatus, Limits, SampleTable, TrackHeader,
    reference_trace::mov::sample_entry::{RawVisualEntriesJob, SampleEntryHeaderProfile},
};

const CARRIAGE: &[u8] = include_bytes!("fixtures/r-private-carriage.mov");

fn track(pool: &Budget, data: &[u8]) -> (TrackHeader, SampleTable) {
    let top = roots(pool, data).unwrap();
    let movie = top.iter().find(|x| x.header().kind == *b"moov").unwrap();
    let trak = children(movie, 0)
        .unwrap()
        .into_iter()
        .find(|x| x.header().kind == *b"trak")
        .unwrap();
    let mdia = child(&trak, b"mdia").unwrap();
    let header = TrackHeader::parse(
        &child(&trak, b"tkhd").unwrap(),
        &child(&mdia, b"mdhd").unwrap(),
        &child(&mdia, b"hdlr").unwrap(),
    )
    .unwrap();
    let stbl = child(&child(&mdia, b"minf").unwrap(), b"stbl").unwrap();
    (header, table(stbl, pool, 1).unwrap())
}

fn run(header: &TrackHeader, table: &SampleTable, quantum: u64) -> Result<(), Error> {
    let cancel = AtomicBool::new(false);
    let mut job =
        RawVisualEntriesJob::new(header, table, Limits::default(), SampleEntryHeaderProfile);
    for _ in 0..10_000 {
        if job.poll(&mut WorkBudget::new(quantum), &cancel)? == BuildStatus::Ready {
            return Ok(());
        }
    }
    panic!("bounded entry check stalled")
}

#[test]
fn a_rejected_raw_entry_fails_and_keeps_failing() {
    let pool = Budget::new(1 << 22).unwrap();
    let (header, table) = track(&pool, CARRIAGE);
    let refused = Err(Error::Invalid("sample entry configuration"));
    assert_eq!(run(&header, &table, 1), refused);
    assert_eq!(run(&header, &table, 4096), refused);

    let cancel = AtomicBool::new(false);
    let mut job =
        RawVisualEntriesJob::new(&header, &table, Limits::default(), SampleEntryHeaderProfile);
    assert!(job.poll(&mut WorkBudget::new(4096), &cancel).is_err());
    assert_eq!(
        job.poll(&mut WorkBudget::new(4096), &cancel),
        refused.map(|()| BuildStatus::Ready)
    );
}

#[test]
fn entries_without_a_rejected_configuration_pass() {
    let pool = Budget::new(1 << 22).unwrap();
    let mut control = CARRIAGE.to_vec();
    for kind in [b"dOps", b"ddts"] {
        let at = control.windows(4).position(|x| x == kind).unwrap();
        control[at..at + 4].copy_from_slice(b"free");
    }
    let (header, table) = track(&pool, &control);
    run(&header, &table, 1).unwrap();
}

#[test]
fn other_handlers_are_not_checked() {
    let pool = Budget::new(1 << 22).unwrap();
    let (mut header, table) = track(&pool, CARRIAGE);
    header.handler = *b"soun";
    run(&header, &table, 1).unwrap();
}
