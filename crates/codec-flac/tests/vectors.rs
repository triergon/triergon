//! The decoder test suite (`.research/flac-test-files`) and the generated streams
//! (`.research/flac-generated`), kept outside the repository; without them the tests check
//! nothing and say so on the test run's standard error. The integer output must reproduce the
//! STREAMINFO MD5 the encoder recorded, the float output must be the integer output
//! scaled, and the damaged files of `faulty` must decode or fail without a panic.
//! Bit-exactness against FFmpeg is graded by `harness/codecs/flac/flac_vectors.py`.
use std::path::{Path, PathBuf};
use triergon::{Budget, ContainerReader, PacketInfo};
use triergon_codec_flac::{AudioFrame, Decoder};
use triergon_framing::flac;
use triergon_harness_protocol::md5;
use triergon_harness_protocol::test_report::skipped;

/// The configuration and frames of a native FLAC file, read by the elementary reader up
/// to its first error; `None` when it does not open as FLAC.
fn read(path: &Path) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let pool = Budget::new(256 << 20).unwrap();
    let file = std::io::BufReader::new(std::fs::File::open(path).unwrap());
    let mut source = triergon::open_blocking(file, &pool).ok()?;
    let mut frames = Vec::new();
    while let Ok(Some(packet)) = source.next_packet() {
        frames.push(packet.payload().to_vec());
    }
    let track = source.reader().track(0)?;
    let budget = Budget::new(1 << 20).ok()?;
    let mut configuration = triergon_codec::BudgetVec::new(&budget);
    if flac::canonical_configuration(track.configuration(), &mut configuration).is_err() {
        configuration.clear();
    }
    Some((configuration.to_vec(), frames))
}

fn research(dir: &str) -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.research")
        .join(dir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "flac"))
        .collect();
    files.sort();
    files
}

/// Files up to this size are checked by default; an unoptimised build decodes the whole
/// suite (about 500 MB) in minutes.
const DEFAULT_MAX_BYTES: u64 = 3 << 20;

#[test]
fn small_vectors_match_their_streaminfo_md5() {
    check_md5(DEFAULT_MAX_BYTES);
}

#[test]
#[ignore = "decodes about 500 MB: cargo test --release -- --ignored"]
fn every_vector_matches_its_streaminfo_md5() {
    check_md5(u64::MAX);
}

fn check_md5(max_bytes: u64) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.research");
    let vectors = include_str!("data/md5-vectors.txt");
    let mut checked = 0;
    for dir in [
        "flac-test-files/subset",
        "flac-test-files/uncommon",
        "flac-generated",
    ] {
        let files = research(dir);
        if files.is_empty() && !root.join(dir).is_dir() {
            skipped(format_args!("the FLAC MD5 input directory {dir}: absent"));
            continue;
        }
        let mut expected: Vec<_> = vectors
            .lines()
            .map(|line| line.split_once('\t').expect("expectation and path"))
            .filter(|&(_, relative)| Path::new(relative).parent() == Some(Path::new(dir)))
            .map(|(expectation, relative)| (root.join(relative), expectation))
            .collect();
        expected.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        assert!(
            files.iter().eq(expected.iter().map(|(path, _)| path)),
            "unexpected files in {dir}: found {}, expected {}",
            files.len(),
            expected.len()
        );
        for (path, expectation) in expected {
            let bytes = std::fs::metadata(&path).unwrap().len();
            if bytes <= max_bytes {
                checked += usize::from(check_vector(&path, expectation));
            }
        }
    }
    eprintln!("{checked} streams match their MD5");
}

fn check_vector(path: &Path, expectation: &str) -> bool {
    let (configuration, frames) =
        read(path).unwrap_or_else(|| panic!("{} did not open as FLAC", path.display()));
    let info = flac::stream_info(&configuration);
    if expectation == "no-streaminfo" {
        // These streams start at a frame header or with junk before a frame header.
        assert!(info.is_err(), "{} has STREAMINFO", path.display());
        return false;
    }
    let info = info.unwrap_or_else(|error| panic!("{}: {error:?}", path.display()));
    let mut decoder = Decoder::new(&configuration).unwrap();
    let (mut integer, mut float) = (decoder.integer_frame(), AudioFrame::default());
    let mut pcm = Vec::new();
    let mut uniform = true;
    for au in &frames {
        let Ok(frame) = decoder.decode_integer(au, &mut integer) else {
            uniform = false;
            continue;
        };
        uniform &= frame.layout.channels() == usize::from(info.channels)
            && frame.sample_rate == info.sample_rate;
        decoder.decode(au, &mut float).unwrap();
        let width = usize::from(info.bits_per_sample).div_ceil(8);
        let scale = |x: i32| {
            let bits = u32::from(info.bits_per_sample);
            if bits <= 16 {
                f32::from((x << (16 - bits)) as i16) / 32768.0
            } else {
                (x << (32 - bits)) as f32 / 2_147_483_648.0
            }
        };
        for i in 0..integer.samples_per_channel {
            for (plane, floats) in integer.planes().zip(&float.planes) {
                assert_eq!(floats[i], scale(plane[i]), "{}", path.display());
                pcm.extend_from_slice(&plane[i].to_le_bytes()[..width]);
            }
        }
    }
    match expectation {
        "md5" => {
            assert!(
                uniform,
                "{} did not decode at a uniform format",
                path.display()
            );
            assert_ne!(info.md5, [0; 16], "{} has no recorded MD5", path.display());
            assert_eq!(md5::digest(&pcm), info.md5, "{}", path.display());
            true
        }
        "variable-format" => {
            assert_eq!(info.md5, [0; 16], "{}", path.display());
            assert!(!pcm.is_empty(), "{} decoded no samples", path.display());
            false
        }
        _ => panic!("invalid vector expectation {expectation}"),
    }
}

#[test]
fn damaged_files_never_panic() {
    let faulty = research("flac-test-files/faulty");
    if faulty.is_empty() {
        skipped(format_args!(
            "the damaged FLAC files: .research/flac-test-files/faulty absent"
        ));
    }
    for path in faulty {
        let mut out = AudioFrame::default();
        let Some((configuration, frames)) = read(&path) else {
            // A file the reader refuses still reaches a decoder without a configuration,
            // in arbitrary pieces.
            let bytes = std::fs::read(&path).unwrap();
            let mut decoder = Decoder::new(&[]).unwrap();
            for piece in bytes.chunks(4096) {
                let _ = decoder.decode(piece, &mut out);
            }
            continue;
        };
        let Ok(mut decoder) = Decoder::new(&configuration) else {
            continue;
        };
        for au in &frames {
            let _ = decoder.decode(au, &mut out);
        }
    }
}
