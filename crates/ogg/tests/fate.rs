//! The reader over the FATE Vorbis samples, when they are fetched into
//! `.research/fate-samples/vorbis` (see the repository's fixture tooling): small windows
//! read the same packets, and seeks resume the linear packet sequence.
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use triergon_core::container::{Event, PacketInfo, SeekMethod, SeekPolicy, SeekRequest, TrackInfo};
use triergon_core::{
    Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, Time, WorkBudget,
};
use triergon_ogg::{BlockingReader, Limits, Reader};

fn samples() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.research/fate-samples/vorbis");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.retain(|path| path.extension().is_some_and(|ext| ext == "ogg"));
    files.sort();
    files
}

type Row = (u64, Vec<u8>, Option<i64>, Option<i64>);

fn read(file: &[u8], window: usize) -> Vec<Row> {
    let pool = Budget::new(64 << 20).unwrap();
    let capabilities = SourceCapabilities {
        length: Some(file.len() as u64),
        random_access: true,
    };
    let mut reader =
        Reader::new(&pool, SourceId(1), capabilities, Limits::default(), window).unwrap();
    let cancel = AtomicBool::new(false);
    let mut rows = Vec::new();
    loop {
        use triergon_core::container::ContainerReader;
        match reader.step(&mut WorkBudget::new(1 << 16), &cancel).unwrap() {
            Step::Ready(Event::Packet(p)) => {
                rows.push((p.track().0, p.payload().to_vec(), p.pts(), p.duration()));
            }
            Step::Ready(_) | Step::Yield => {}
            Step::NeedInput(request) => {
                let start = request.offset as usize;
                let end = (start + request.maximum).min(file.len());
                if start >= end {
                    reader.finish(request.source, request.token).unwrap();
                } else {
                    let bytes = Bytes::copy_from_slice(&pool, &file[start..end]).unwrap();
                    reader
                        .provide(
                            &InputWindow::new(request.source, request.token, request.offset, bytes)
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            Step::Backpressure(_) => panic!("backpressure"),
            Step::End(_) => return rows,
        }
    }
}

#[test]
fn small_windows_read_the_same_packets() {
    for path in samples() {
        let file = std::fs::read(&path).unwrap();
        let expected = read(&file, 1 << 16);
        assert!(!expected.is_empty(), "{}", path.display());
        for window in [1, 333, 4096] {
            assert!(
                read(&file, window) == expected,
                "{} window {window}",
                path.display()
            );
        }
    }
}

#[test]
fn seeks_resume_the_linear_packets() {
    let cancel = AtomicBool::new(false);
    for path in samples() {
        let file = std::fs::read(&path).unwrap();
        let linear = read(&file, 1 << 16);
        let mut reader =
            BlockingReader::open(std::fs::File::open(&path).unwrap(), Limits::default()).unwrap();
        let track = reader.tracks()[0].id();
        let rate = i128::from(reader.tracks()[0].sample_rate().unwrap());
        let last = linear.last().and_then(|row| row.2).unwrap();
        for fraction in [0, 1, 3, 5, 7] {
            let target = i128::from(last) * fraction / 8;
            for policy in [
                SeekPolicy::PreviousRandomAccess,
                SeekPolicy::PlaybackPreroll,
            ] {
                let target = Time::new(target, rate as u128).unwrap();
                let receipt = reader
                    .seek(
                        SeekRequest {
                            track,
                            target,
                            policy,
                        },
                        &cancel,
                    )
                    .unwrap();
                assert_eq!(receipt.method, SeekMethod::BoundedScan);
                let first = reader.next_packet(&cancel).unwrap().unwrap();
                let second = reader.next_packet(&cancel).unwrap();
                let row = (
                    first.track().0,
                    first.payload().to_vec(),
                    first.pts(),
                    first.duration(),
                );
                let at = linear.iter().position(|r| *r == row).unwrap_or_else(|| {
                    panic!("{} {fraction}/8: {:?}", path.display(), first.pts())
                });
                if let Some(second) = second {
                    assert_eq!(
                        linear[at + 1].1,
                        second.payload(),
                        "{} {fraction}/8",
                        path.display()
                    );
                    // The packet after the landing holds the target.
                    let end = second.end().unwrap();
                    assert!(
                        Time::new(i128::from(end), rate as u128).unwrap() > target
                            || at + 2 >= linear.len()
                    );
                }
                assert!(receipt.landing <= target.max(receipt.landing));
                assert!(receipt.scanned_bytes <= file.len() as u64);
            }
        }
    }
}
