//! Cues jobs: entry order and source positions, and the positions arena of each
//! CuePoint: peak and retained storage, work, yields and pressure.
use super::*;
use crate::ebml_kit::{element, uint};
use triergon_core::Bytes;

#[test]
fn cues_job_waits_for_late_time_keeps_every_source_position_and_does_not_sort_silently() {
    let point = |ticks| {
        element(
            0xbb,
            &[
                element(
                    0xb7,
                    &[uint(0xf7, 1), uint(0xf1, 10), uint(0xf0, 0)].concat(),
                ),
                element(
                    0xb7,
                    &[uint(0xf7, 2), uint(0xf1, 20), uint(0x5378, 1)].concat(),
                ),
                uint(0xb3, ticks),
            ]
            .concat(),
        )
    };
    let raw = [point(u64::MAX), point(0)].concat();
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
    let alias = source.clone();
    let mut job = CuesJob::new(source, 100, Some(1000), 4, &pool).unwrap();
    let mut result = None;
    for _ in 0..10_000 {
        let mut work = WorkBudget::new(1);
        match job.step(&mut work, &AtomicBool::new(false)).unwrap() {
            ParseStep::Yield => (),
            ParseStep::Event(value) => {
                result = Some(value);
                break;
            }
            other => panic!("Cues transaction: {other:?}"),
        }
        assert!(work.spent() <= 1);
    }
    let result = result.unwrap();
    let entries = result.entries.as_slice();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        (entries[0].ticks, entries[1].ticks, entries[2].ticks),
        (u64::MAX, u64::MAX, 0)
    );
    assert_eq!((entries[0].cluster, entries[1].cluster), (110, 120));
    assert_eq!(entries[0].relative, Some(0));
    assert_eq!(entries[1].block, Some(1));
    assert!(result.source.shares_storage_with(&alias));
    drop((job, alias));
    assert_eq!(result.source.as_ref(), raw.as_slice());
}

fn point(time: u8, positions: usize) -> Vec<u8> {
    let mut body = vec![0xb3, 0x81, time];
    for n in 0..positions {
        body.extend_from_slice(&[0xb7, 0x86, 0xf7, 0x81, 1, 0xf1, 0x81, (n % 100) as u8]);
    }
    let mut out = vec![0xbb, 0x40 | ((body.len() >> 8) as u8), body.len() as u8];
    out.extend(body);
    out
}

fn run(points: &[(u8, usize)], live: usize, chunk: u64) -> String {
    let pool = Budget::new(live).unwrap();
    let mut raw = Vec::new();
    for &(t, k) in points {
        raw.extend(point(t, k));
    }
    let source = SharedBytes::from_core(Bytes::copy_from_slice(&pool, &raw).unwrap());
    let base = (pool.used(), pool.peak());
    let mut job = CuesJob::new(source, 0, None, 100_000, &pool).unwrap();
    let cancel = AtomicBool::new(false);
    let (mut spent, mut yields, mut most) = (0, 0, 0);
    loop {
        let mut work = WorkBudget::new(chunk);
        let step = job.step(&mut work, &cancel);
        spent += work.spent();
        most = most.max(pool.used());
        match step {
            Ok(ParseStep::Yield) => yields += 1,
            Ok(ParseStep::Event(cues)) => {
                return format!(
                    "entries {} spent {spent} yields {yields} base {base:?} peak {} used_end {} maxused {most}",
                    cues.entries.len(),
                    pool.peak(),
                    pool.used()
                );
            }
            Ok(other) => return format!("{other:?} spent {spent} peak {}", pool.peak()),
            Err(e) => {
                return format!(
                    "err {e:?} spent {spent} yields {yields} peak {} used {}",
                    pool.peak(),
                    pool.used()
                );
            }
        }
    }
}

#[test]
fn cue_arena_peak_work_and_pressure_match_the_preimage() {
    let expected = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../harness/traces/cues-preimage.txt"
    ));
    let mut big_first = vec![(0_u8, 60_usize)];
    big_first.extend((1..200).map(|t| (t as u8, 1)));
    let uniform: Vec<_> = (0..100).map(|t| (t as u8, 3)).collect();
    let mut rows = Vec::new();
    for (name, points) in [("big_first", big_first), ("uniform", uniform)] {
        rows.push(format!(
            "M1CUES {name} unbounded: {}",
            run(&points, 1 << 20, u64::MAX)
        ));
        rows.push(format!(
            "M1CUES {name} chunk7: {}",
            run(&points, 1 << 20, 7)
        ));
        for live in [
            24_000_usize,
            26_000,
            28_000,
            30_000,
            32_000,
            34_000,
            36_000,
            40_000,
        ] {
            rows.push(format!(
                "M1CUES {name} live={live}: {}",
                run(&points, live, u64::MAX)
            ));
        }
    }
    let expected: Vec<_> = expected.lines().collect();
    if cfg!(target_pointer_width = "64") {
        assert_eq!(rows, expected);
    } else {
        // Pool bytes include storage headers sized by the pointer width, and the
        // preimage was recorded on a 64-bit target. Entries, work and yields of
        // the runs that never meet pressure do not depend on them; where the pool
        // is bounded, the pressure point itself does.
        let unpressured = |row: &str| {
            row.find(" base ")
                .filter(|_| row.contains(" unbounded: ") || row.contains(" chunk7: "))
                .map(|end| row[..end].to_owned())
        };
        let portable: Vec<_> = rows.iter().filter_map(|r| unpressured(r)).collect();
        assert_eq!(portable.len(), 4);
        assert_eq!(
            portable,
            expected
                .iter()
                .filter_map(|r| unpressured(r))
                .collect::<Vec<_>>()
        );
    }
}
