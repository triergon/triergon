//! The decoder test suite (`.research/flac-test-files`) and the generated streams
//! (`.research/flac-generated`), when present: the integer output must reproduce the
//! STREAMINFO MD5 the encoder recorded, the float output must be the integer output
//! scaled, and the damaged files of `faulty` must decode or fail without a panic.
//! Bit-exactness against FFmpeg is graded by `harness/codecs/flac/flac_vectors.py`.
use std::path::{Path, PathBuf};
use triergon::{Budget, ContainerReader, PacketInfo};
use triergon_codec_flac::{AudioFrame, Decoder};
use triergon_framing::flac;
use triergon_harness_protocol::md5;

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
    let mut checked = 0;
    for dir in [
        "flac-test-files/subset",
        "flac-test-files/uncommon",
        "flac-generated",
    ] {
        for path in research(dir) {
            if std::fs::metadata(&path).map_or(true, |m| m.len() > max_bytes) {
                continue;
            }
            let Some((configuration, frames)) = read(&path) else {
                continue;
            };
            let Ok(info) = flac::stream_info(&configuration) else {
                continue;
            };
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
            if uniform && info.md5 != [0; 16] {
                assert_eq!(md5::digest(&pcm), info.md5, "{}", path.display());
                checked += 1;
            }
        }
    }
    eprintln!("{checked} streams match their MD5");
}

#[test]
fn damaged_files_never_panic() {
    for path in research("flac-test-files/faulty") {
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
