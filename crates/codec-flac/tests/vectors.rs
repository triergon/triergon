//! The decoder test suite (`.research/flac-test-files`) and the generated streams
//! (`.research/flac-generated`), when present: the integer output must reproduce the
//! STREAMINFO MD5 the encoder recorded, the float output must be the integer output
//! scaled, and the damaged files of `faulty` must decode or fail without a panic.
//! Bit-exactness against FFmpeg is graded by `tools/flac_vectors.py`.
#[path = "../examples/decode_flac/native.rs"]
mod native;

use std::path::{Path, PathBuf};
use triergon_codec_flac::{AudioFrame, Decoder, IntegerFrame, Limits};
use triergon_framing::flac_stream_info;

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
            let data = std::fs::read(&path).unwrap();
            let (configuration, frames) = native::split_header(&data);
            let Ok(info) = flac_stream_info(configuration) else {
                continue;
            };
            let mut decoder =
                Decoder::from_configuration(configuration, Limits::default()).unwrap();
            let (mut integer, mut float) = (IntegerFrame::default(), AudioFrame::default());
            let mut md5 = Md5::default();
            let mut uniform = true;
            for au in native::frames(frames) {
                let Ok(frame) = decoder.decode_integer(au, &mut integer) else {
                    uniform = false;
                    continue;
                };
                uniform &= frame.layout.channels() == usize::from(info.channels)
                    && frame.sample_rate == info.sample_rate;
                decoder.decode_float(au, &mut float).unwrap();
                let width = usize::from(info.bits_per_sample).div_ceil(8);
                let scale = |x: i32| {
                    let bits = u32::from(info.bits_per_sample);
                    if bits <= 16 {
                        f32::from((x << (16 - bits)) as i16) / 32768.0
                    } else {
                        (x << (32 - bits)) as f32 / 2_147_483_648.0
                    }
                };
                let mut bytes = Vec::new();
                for i in 0..integer.samples_per_channel {
                    for (plane, floats) in integer.planes().zip(&float.planes) {
                        assert_eq!(floats[i], scale(plane[i]), "{}", path.display());
                        bytes.extend_from_slice(&plane[i].to_le_bytes()[..width]);
                    }
                }
                md5.update(&bytes);
            }
            if uniform && info.md5 != [0; 16] {
                assert_eq!(md5.finish(), info.md5, "{}", path.display());
                checked += 1;
            }
        }
    }
    eprintln!("{checked} streams match their MD5");
}

#[test]
fn damaged_files_never_panic() {
    for path in research("flac-test-files/faulty") {
        let data = std::fs::read(&path).unwrap();
        let (configuration, frames) = native::split_header(&data);
        let configuration = if flac_stream_info(configuration).is_ok() {
            configuration
        } else {
            &[]
        };
        let Ok(mut decoder) = Decoder::from_configuration(configuration, Limits::default()) else {
            continue;
        };
        let mut out = AudioFrame::default();
        for au in native::frames(frames) {
            let _ = decoder.decode_float(au, &mut out);
        }
    }
}

/// MD5 (RFC 1321), for the STREAMINFO signature.
struct Md5 {
    state: [u32; 4],
    buffer: Vec<u8>,
    len: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buffer: Vec::new(),
            len: 0,
        }
    }
}

impl Md5 {
    fn update(&mut self, bytes: &[u8]) {
        self.len += bytes.len() as u64;
        self.buffer.extend_from_slice(bytes);
        let whole = self.buffer.len() / 64 * 64;
        for block in self.buffer[..whole].chunks_exact(64) {
            compress(&mut self.state, block);
        }
        self.buffer.drain(..whole);
    }

    fn finish(mut self) -> [u8; 16] {
        let bits = self.len * 8;
        let mut tail = vec![0x80];
        tail.resize((119 - self.buffer.len() % 64) % 64 + 1, 0);
        tail.extend_from_slice(&bits.to_le_bytes());
        self.update(&tail);
        let mut out = [0; 16];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

fn compress(state: &mut [u32; 4], block: &[u8]) {
    const SHIFTS: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let mut words = [0u32; 16];
    for (word, bytes) in words.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes(bytes.try_into().unwrap());
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32;
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(k)
            .wrapping_add(words[g])
            .rotate_left(SHIFTS[i / 16 * 4 + i % 4]);
        (a, d, c) = (d, c, b);
        b = b.wrapping_add(rotated);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d]) {
        *s = s.wrapping_add(v);
    }
}

#[test]
fn md5_known_answer() {
    let mut md5 = Md5::default();
    md5.update(b"The quick brown fox jumps over the lazy dog");
    let hex: String = md5.finish().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "9e107d9d372bb6826bd81d3542a419d6");
}
