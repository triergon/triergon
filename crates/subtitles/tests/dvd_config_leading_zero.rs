use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_subtitles::dvd::{Canvas, ConfigJob, ConfigLimits, ConfigStatus};

#[test]
fn leading_zero_canvas_numbers_use_only_declared_limits() {
    for count in [255, 256, 4096] {
        let mut raw = b"size: ".to_vec();
        raw.extend(std::iter::repeat_n(b'0', count));
        raw.extend_from_slice(b"1x");
        raw.extend(std::iter::repeat_n(b'0', count));
        raw.extend_from_slice(b"2\n");
        for quantum in [1, 7, 65536] {
            let budget = Budget::new(128 * 1024).unwrap();
            let input = Bytes::copy_from_slice(&budget, &raw).unwrap();
            let mut job = ConfigJob::new(input, &budget, ConfigLimits::default()).unwrap();
            let cancel = AtomicBool::new(false);
            while job.poll(&mut WorkBudget::new(quantum), &cancel).unwrap() != ConfigStatus::Ready {
            }
            assert_eq!(job.work_spent(), raw.len() + 1);
            let result = job.try_finish(&cancel).unwrap();
            assert_eq!(
                result.canvas,
                Some(Canvas {
                    width: 1,
                    height: 2
                })
            );
            assert_eq!(result.raw().as_ref(), raw);
        }
    }
}
