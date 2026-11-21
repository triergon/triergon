use super::*;
use triergon_core::Budget;

fn lace(data: &[u8], mode: u8) -> Result<Vec<Vec<u8>>> {
    let pool = Budget::new(1 << 16)?;
    let source = SharedBytes::try_from_vec(&pool, data.to_vec())?;
    let mut job = LaceJob::new(source, 0..data.len(), mode)?;
    loop {
        match job.step_descriptors(&mut WorkBudget::new(u64::MAX), &AtomicBool::new(false))? {
            ParseStep::Event(()) => break,
            ParseStep::Yield => continue,
            other => panic!("unexpected lace step {other:?}"),
        }
    }
    let mut frames = Vec::new();
    for index in 0..job.frame_count().unwrap() {
        let Some(range) = job.frame_range(
            index,
            &mut WorkBudget::new(u64::MAX),
            &AtomicBool::new(false),
        )?
        else {
            panic!("unlimited lace work yielded");
        };
        frames.push(data[range].to_vec());
    }
    Ok(frames)
}

#[test]
fn all_lacing_schemes_and_malformed_lengths() {
    assert_eq!(lace(&[1, 1, 4, 5, 6], 1).unwrap(), [&[4][..], &[5, 6]]);
    assert_eq!(lace(&[1, 4, 5, 6, 7], 2).unwrap(), [&[4, 5][..], &[6, 7]]);
    assert_eq!(
        lace(&[2, 0x81, 0xc0, 4, 5, 6, 7, 8, 9], 3).unwrap(),
        [&[4][..], &[5, 6], &[7, 8, 9]]
    );
    for b in [&[][..], &[0][..], &[1, 255][..], &[2, 0x81, 0x80][..]] {
        assert!(lace(b, 3).is_err(), "{b:?}");
    }
    assert!(lace(&[1, 0, 1, 2], 2).is_err());
}

#[test]
fn xiph_range_resumes_each_octet_and_checks_cancellation_when_cached() {
    let mut data = vec![1, 255, 255, 255, 0];
    data.resize(771, b'A');
    let pool = Budget::new(64 * 1024).unwrap();
    let source = SharedBytes::try_from_vec(&pool, data).unwrap();
    let mut job = LaceJob::new(source, 0..771, 1).unwrap();
    let running = AtomicBool::new(false);
    assert!(matches!(
        job.step_descriptors(&mut WorkBudget::new(32), &running)
            .unwrap(),
        ParseStep::Event(())
    ));
    for _ in 0..3 {
        assert!(
            job.frame_range(0, &mut WorkBudget::new(1), &running)
                .unwrap()
                .is_none()
        );
    }
    assert!(matches!(
        job.frame_range(0, &mut WorkBudget::new(1), &running).unwrap(),
        Some(range) if range == (5..770)
    ));
    assert_eq!(
        job.frame_range(0, &mut WorkBudget::new(0), &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert!(
        job.frame_range(0, &mut WorkBudget::new(1), &running)
            .is_err()
    );
}

#[test]
#[cfg(target_pointer_width = "64")]
fn ebml_size_delta_uses_full_width_until_the_lace_is_rejected() {
    let size = 1_u64 << 40;
    let delta = (1_u64 << 55) - 1 - size;
    let marker = 1_u64 << 56;
    let mut data = vec![2];
    data.extend_from_slice(&(marker | size).to_be_bytes());
    data.extend_from_slice(&(marker | delta).to_be_bytes());
    assert_eq!(lace(&data, 3), Err(Error::Invalid("lace exceeds block")));
}

#[test]
fn lace_job_resumes_descriptors_and_exposes_checked_source_ranges() {
    for (mode, raw, expected) in [
        (1, vec![1, 1, 4, 5, 6], vec![vec![4], vec![5, 6]]),
        (2, vec![1, 4, 5, 6, 7], vec![vec![4, 5], vec![6, 7]]),
        (
            3,
            vec![2, 0x81, 0xc0, 4, 5, 6, 7, 8, 9],
            vec![vec![4], vec![5, 6], vec![7, 8, 9]],
        ),
    ] {
        let pool = Budget::new(64 * 1024).unwrap();
        let source = SharedBytes::copy_from_slice(&pool, &raw).unwrap();
        let mut job = LaceJob::new(source.clone(), 0..raw.len(), mode).unwrap();
        let mut completed = false;
        for _ in 0..1000 {
            let mut work = WorkBudget::new(1);
            match job
                .step_descriptors(&mut work, &AtomicBool::new(false))
                .unwrap()
            {
                ParseStep::Yield => (),
                ParseStep::Event(()) => {
                    completed = true;
                    break;
                }
                _ => panic!("bounded known lace"),
            }
            assert!(work.spent() <= 1);
        }
        assert!(completed);
        assert_eq!(job.frame_count(), Some(expected.len()));
        for (index, bytes) in expected.into_iter().enumerate() {
            let mut range = None;
            for _ in 0..1000 {
                let mut work = WorkBudget::new(1);
                let step = job
                    .frame_range(index, &mut work, &AtomicBool::new(false))
                    .unwrap();
                assert!(work.spent() <= 1);
                if let Some(value) = step {
                    range = Some(value);
                    break;
                }
            }
            assert_eq!(&source[range.unwrap()], bytes.as_slice());
        }
    }
}

#[test]
fn lace_range_cursor_keeps_full_width_offsets_in_constant_storage() {
    let ceiling = 40 * size_of::<usize>();
    assert!(
        size_of::<LaceJob>() <= ceiling,
        "lace workspace={} ceiling={ceiling}",
        size_of::<LaceJob>()
    );
}
