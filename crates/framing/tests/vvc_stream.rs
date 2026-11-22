use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceFailure, SourceId, Step,
    WorkBudget,
};
use triergon_framing::{RawAnnexBLimits, VvcAccessUnits, VvcAnnexBReader, compat};

static OFF: AtomicBool = AtomicBool::new(false);
const SOURCE: SourceId = SourceId(37);
const PICTURE: [u8; 7] = [0, 0, 0, 1, 0, 1, 0x80];
const SHORT: [u8; 6] = [0, 0, 1, 0, 1, 0x80];
const HEADER: [u8; 7] = [0, 0, 0, 1, 0, 0x99, 0x80];
const SLICE: [u8; 6] = [0, 0, 1, 0, 1, 0x40];

fn start(
    pool: &Budget,
    length: Option<u64>,
    limits: RawAnnexBLimits,
    compatible: bool,
) -> VvcAnnexBReader {
    let caps = SourceCapabilities {
        length,
        random_access: false,
    };
    if compatible {
        compat::vvc::access_unit_reader(pool, SOURCE, caps, limits).unwrap()
    } else {
        VvcAnnexBReader::new(pool, SOURCE, caps, limits).unwrap()
    }
}

type Output = (Range<u64>, Vec<u8>);

fn frames(
    data: &[u8],
    first_window: usize,
    window: usize,
    allowance: u64,
    compatible: bool,
    limits: RawAnnexBLimits,
) -> Result<Vec<Output>, Error> {
    let pool = Budget::new(data.len() * 4 + 4096).unwrap();
    let baseline = pool.used();
    let mut reader = start(&pool, Some(data.len() as u64), limits, compatible);
    let mut output = Vec::new();
    let mut total_work = 0;
    let mut finished = false;
    for _ in 0..data.len().saturating_mul(3) + 16 {
        let mut work = WorkBudget::new(allowance);
        let step = reader.step(&mut work, &OFF)?;
        total_work += work.spent();
        match step {
            Step::NeedInput(request) => {
                assert_eq!(request.source, SOURCE);
                let offset = request.offset as usize;
                if offset == data.len() {
                    assert!(!finished);
                    reader.finish(SOURCE, request.token)?;
                    finished = true;
                } else {
                    let window = if offset == 0 { first_window } else { window };
                    let end = data.len().min(offset + window.min(request.maximum));
                    let bytes = Bytes::copy_from_slice(&pool, &data[offset..end])?;
                    reader.provide_owned(InputWindow::new(
                        SOURCE,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
            }
            Step::Yield => assert_eq!(work.spent(), allowance),
            Step::Ready(unit) => {
                assert_eq!(unit.source, SOURCE);
                assert!(finished || unit.range.end < data.len() as u64);
                output.push((unit.range, unit.bytes.as_ref().to_vec()));
            }
            Step::End(location) => {
                assert_eq!(location.source, SOURCE);
                assert_eq!(location.offset, data.len() as u64);
                assert_eq!(total_work, data.len() as u64);
                assert_eq!(pool.used(), baseline);
                return Ok(output);
            }
            Step::Backpressure(never) => match never {},
        }
    }
    panic!("reader made no progress");
}

fn check_split(data: &[u8], compatible: bool) {
    let native = if compatible {
        compat::vvc::access_units(data, data.len())
    } else {
        VvcAccessUnits::with_picture_headers(data, data.len())
    }
    .unwrap()
    .map(|unit| {
        let unit = unit.unwrap();
        (
            unit.range.start as u64..unit.range.end as u64,
            unit.bytes.to_vec(),
        )
    })
    .collect::<Vec<_>>();
    for split in 1..=data.len() {
        for (window, work) in [(1, 1), (data.len(), 8)] {
            assert_eq!(
                frames(
                    data,
                    split,
                    window,
                    work,
                    compatible,
                    RawAnnexBLimits::default()
                )
                .unwrap(),
                native,
                "split {split}, window {window}, work {work}"
            );
        }
    }
}

#[test]
fn each_split_preserves_units_and_their_extents() {
    check_split(
        &[
            &[0, 0, 0][..],
            &HEADER,
            &SLICE,
            &SLICE,
            &[0, 0],
            &PICTURE,
            &PICTURE,
            &[0, 0],
        ]
        .concat(),
        false,
    );
    check_split(&[&PICTURE[..], &SHORT, &PICTURE].concat(), true);
}

#[test]
fn short_headers_damaged_payload_and_limits_refuse() {
    let limits = RawAnnexBLimits::default();
    for data in [
        [&PICTURE[..], &SHORT].concat(),
        [&PICTURE[..], &[0, 0, 0, 1, 0x80]].concat(),
        [&PICTURE[..], &[0, 0, 2, 0x40]].concat(),
        HEADER.to_vec(),
    ] {
        assert!(frames(&data, 1, 1, 1, false, limits).is_err(), "{data:?}");
    }
    let multi = [&HEADER[..], &SLICE, &SLICE].concat();
    for narrowed in [
        RawAnnexBLimits {
            maximum_access_unit_nals: 2,
            ..limits
        },
        RawAnnexBLimits {
            maximum_access_unit_bytes: multi.len() - 1,
            ..limits
        },
    ] {
        assert!(matches!(
            frames(&multi, 1, 1, 1, false, narrowed),
            Err(Error::Limit { .. })
        ));
    }
    let no_padding = RawAnnexBLimits {
        nal: triergon_framing::ReaderLimits {
            maximum_padding_bytes: 0,
            ..limits.nal
        },
        ..limits
    };
    assert_eq!(
        frames(&PICTURE.repeat(2), 1, 1, 1, false, no_padding)
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        frames(&[&[0][..], &PICTURE].concat(), 1, 1, 1, false, no_padding),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn pressure_during_scan_and_publication_preserves_input() {
    let pool = Budget::new(256).unwrap();
    let baseline = pool.used();
    let mut reader = start(&pool, None, RawAnnexBLimits::default(), false);
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("expected initial request");
    };
    let input = PICTURE.repeat(2);
    let bytes = Bytes::copy_from_slice(&pool, &input).unwrap();
    let retained = bytes.clone();
    reader
        .provide_owned(InputWindow::new(SOURCE, request.token, 0, bytes).unwrap())
        .unwrap();
    let hold = pool.reserve(256 - pool.used()).unwrap();
    let mut work = WorkBudget::new(32);
    assert!(matches!(
        reader.step(&mut work, &OFF),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(work.spent(), 0);
    drop(hold);

    for grant in [8, 5] {
        let mut work = WorkBudget::new(grant);
        assert!(matches!(reader.step(&mut work, &OFF), Ok(Step::Yield)));
        assert_eq!(work.spent(), grant);
    }
    let hold = pool.reserve(256 - pool.used() - 1).unwrap();
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        reader.step(&mut work, &OFF),
        Err(Error::Backpressure { .. })
    ));
    assert_eq!(work.spent(), 1);
    drop((hold, retained));
    let Step::Ready(first) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("retry lost the first output");
    };
    assert_eq!(first.bytes.as_ref(), PICTURE);
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("expected EOF request");
    };
    reader.finish(SOURCE, request.token).unwrap();
    let Step::Ready(second) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("retry lost the final output");
    };
    assert_eq!(second.range, PICTURE.len() as u64..input.len() as u64);
    assert_eq!(second.bytes.as_ref(), PICTURE);
    drop((first, second));
    assert_eq!(pool.used(), baseline);
}

#[test]
fn cancellation_discards_buffered_and_scanned_source() {
    let pool = Budget::new(256).unwrap();
    let baseline = pool.used();
    let mut reader = start(&pool, None, RawAnnexBLimits::default(), false);
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("expected initial request");
    };
    let bytes = Bytes::copy_from_slice(&pool, &PICTURE).unwrap();
    reader
        .provide_owned(InputWindow::new(SOURCE, request.token, 0, bytes).unwrap())
        .unwrap();
    assert!(matches!(
        reader.step(&mut WorkBudget::new(5), &OFF),
        Ok(Step::Yield)
    ));
    assert!(matches!(
        reader.step(&mut WorkBudget::new(0), &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(100), &OFF),
        Err(Error::Cancelled)
    ));
}

#[test]
fn a_transport_failure_discards_a_pending_picture() {
    let pool = Budget::new(256).unwrap();
    let baseline = pool.used();
    let mut reader = start(&pool, None, RawAnnexBLimits::default(), false);
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(0), &OFF).unwrap() else {
        panic!("expected initial request");
    };
    let bytes = Bytes::copy_from_slice(&pool, &PICTURE).unwrap();
    reader
        .provide_owned(InputWindow::new(SOURCE, request.token, 0, bytes).unwrap())
        .unwrap();
    let Step::NeedInput(request) = reader.step(&mut WorkBudget::new(20), &OFF).unwrap() else {
        panic!("a complete-looking NAL cannot establish EOF");
    };
    reader
        .fail(SOURCE, request.token, SourceFailure::Transport)
        .unwrap();
    assert_eq!(pool.used(), baseline);
    assert!(matches!(
        reader.step(&mut WorkBudget::new(20), &OFF),
        Err(Error::Transport)
    ));
}
