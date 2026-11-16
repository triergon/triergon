use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// The ARMv7 reference faults on these widths. Check the filtered fallback
// against the AArch64 Linux reference, which produces an output.
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
const FALLBACK_WIDTHS: &[usize] = &[18, 22, 34, 66, 258, 958, 4094];
#[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
const FALLBACK_WIDTHS: &[usize] = &[];

#[test]
#[ignore = "requires the pinned capture reference worker"]
fn short_filtered_frames_against_reference() {
    let reference = PathBuf::from(env::var_os("CAPTURE_REFERENCE").expect("set CAPTURE_REFERENCE"));
    let root = PathBuf::from(env::var_os("CAPTURE_PROBE_DIR").expect("set CAPTURE_PROBE_DIR"));
    fs::create_dir(&root).unwrap();
    let candidate = Path::new(env!("CARGO_BIN_EXE_capture-worker"));
    let mut random = 0x1fb7_1cdf_u32;
    for width in (2..16).step_by(2).chain(FALLBACK_WIDTHS.iter().copied()) {
        for height in [2, 4, 6] {
            for flip in [0, 1] {
                let geometry = format!("{width}x{height}-flip{flip}");
                let dir = root.join(&geometry);
                fs::create_dir(&dir).unwrap();
                let input = dir.join("input.rgba");
                let mut rgba = Vec::new();
                for pulse in 0..height {
                    for row in 0..height {
                        let pixel: [u8; 4] = if row == pulse {
                            [255, 0, 0, 255]
                        } else {
                            [128, 128, 128, 255]
                        };
                        rgba.extend(pixel.repeat(width));
                    }
                }
                for _ in 0..32 * width * height * 4 {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    rgba.push(random.to_le_bytes()[0]);
                }
                fs::write(&input, rgba).unwrap();
                let frames = height + 32;
                let args = [width, height, flip, frames].map(|value| value.to_string());
                let mut planes = Vec::new();
                for (role, worker) in [("reference", &*reference), ("candidate", candidate)] {
                    let output = dir.join(format!("{role}.yuv"));
                    let result = Command::new(worker)
                        .arg("convert")
                        .args(&args)
                        .arg(&input)
                        .arg(&output)
                        .status()
                        .unwrap();
                    assert!(result.success(), "{role} on {geometry}: {result}");
                    let data = fs::read(output).unwrap();
                    assert_eq!(data.len(), width * height * 3 / 2 * frames);
                    planes.push(data);
                }
                let max_error = planes[0]
                    .iter()
                    .zip(&planes[1])
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap();
                assert_eq!(max_error, 0, "{geometry}: maximum sample error");
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
#[ignore = "requires the pinned AArch64 capture reference worker"]
fn filtered_capture_resolutions_against_reference() {
    use triergon_capture::pixel::Orientation;
    use triergon_capture::{Session, SessionConfig};

    let reference = PathBuf::from(env::var_os("CAPTURE_REFERENCE").expect("set CAPTURE_REFERENCE"));
    let mut root = PathBuf::from(env::var_os("CAPTURE_PROBE_DIR").expect("set CAPTURE_PROBE_DIR"));
    root.set_extension("resolutions");
    fs::create_dir(&root).unwrap();
    let mut random = 0x401c_123b_u32;
    for (w, h) in [(480, 270), (960, 540)] {
        let mut rgba = vec![0; w as usize * h as usize * 4];
        for byte in &mut rgba {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random.to_le_bytes()[0];
        }
        let input = root.join(format!("{w}x{h}.rgba"));
        fs::write(&input, &rgba).unwrap();
        let cfg = SessionConfig::with_rate_rule(w, h, 2_500_000);
        for (flip, orientation) in [(0, Orientation::TopDown), (1, Orientation::BottomUp)] {
            let output = root.join(format!("{w}x{h}-flip{flip}.yuv"));
            let status = Command::new(&reference)
                .arg("convert")
                .args([w, h, flip, 1].map(|arg| arg.to_string()))
                .arg(&input)
                .arg(&output)
                .status()
                .unwrap();
            assert!(status.success(), "{w}x{h}, flip {flip}: {status}");

            let mut candidate = Session::open_filtered(&cfg).unwrap();
            candidate
                .encode(&rgba, orientation, &mut Vec::new())
                .unwrap();
            let expected = fs::read(&output).unwrap();
            let actual = candidate.converted().visible_yuv420();
            assert_eq!(actual.len(), expected.len());
            assert!(
                actual == expected,
                "{w}x{h}, flip {flip}: first different sample {:?}",
                actual
                    .iter()
                    .zip(expected)
                    .position(|(actual, expected)| *actual != expected)
            );
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
#[ignore = "requires the pinned AArch64 capture reference worker"]
fn recover_filtered_rounding_parameters() {
    let reference = PathBuf::from(env::var_os("CAPTURE_REFERENCE").expect("set CAPTURE_REFERENCE"));
    let mut root = PathBuf::from(env::var_os("CAPTURE_PROBE_DIR").expect("set CAPTURE_PROBE_DIR"));
    root.set_extension("rounding");
    fs::create_dir(&root).unwrap();
    let (pairs, chroma) = reference_chroma_rows(&reference, &root, 2, 220);

    let mut recovered = [[0; 3]; 2];
    // Rounded nominal coefficients at fifteen fractional bits. Search nearby
    // integer coefficients against the reference's single-row chroma output.
    for (plane, nominal) in [[-4857, -9535, 14392], [14392, -12052, -2340]]
        .into_iter()
        .enumerate()
    {
        let mut observations: Vec<_> = pairs[1]
            .iter()
            .copied()
            .zip(chroma[plane].iter().copied())
            .collect();
        observations.sort_unstable_by_key(|&(sample, _)| {
            // Colour sums near a rounding boundary constrain the candidates first.
            ((dot(nominal, sample) + 512) & 65535).abs_diff(32768)
        });
        let mut matches = Vec::new();
        for red in nominal[0] - 16..=nominal[0] + 16 {
            for green in nominal[1] - 16..=nominal[1] + 16 {
                for blue in nominal[2] - 16..=nominal[2] + 16 {
                    let matrix = [red, green, blue];
                    if observations
                        .iter()
                        .all(|&(sample, value)| (dot(matrix, sample) + 33280) >> 16 == value)
                    {
                        matches.push(matrix);
                    }
                }
            }
        }
        let [matrix] = matches.as_slice() else {
            panic!("expected one matrix row for plane {plane}, got {matches:?}");
        };
        recovered[plane] = *matrix;
    }
    assert_eq!(recovered, [[-4865, -9528, 14392], [14392, -12061, -2332]]);

    let (pairs, chroma) = reference_chroma_rows(&reference, &root, 6, 160);
    let mut observations = Vec::new();
    for (matrix, plane) in recovered.into_iter().zip(chroma) {
        observations.extend(plane.into_iter().enumerate().map(|(sample, value)| {
            (
                std::array::from_fn(|row| (dot(matrix, pairs[row][sample]) + 512) >> 10),
                value,
            )
        }));
    }
    let mut matches = Vec::new();
    // An impulse gives 3:3:1 at eight bits. Resolve the two nearby weights
    // at twelve bits and get the third from their normalized sum.
    for first in 1751..=1759 {
        for second in 1751..=1759 {
            let taps = [first, second, 4096 - first - second];
            if observations
                .iter()
                .all(|&(sample, value)| (dot(taps, sample) + (32 << 12)) >> 18 == value)
            {
                matches.push(taps);
            }
        }
    }
    assert_eq!(matches, [[1755, 1756, 585]]);
}

#[cfg(target_arch = "aarch64")]
fn dot(coefficients: [i32; 3], sample: [i32; 3]) -> i32 {
    coefficients
        .into_iter()
        .zip(sample)
        .map(|(a, b)| a * b)
        .sum()
}

#[cfg(target_arch = "aarch64")]
fn reference_chroma_rows(
    reference: &Path,
    dir: &Path,
    height: usize,
    frames: usize,
) -> ([Vec<[i32; 3]>; 3], [Vec<i32>; 2]) {
    const WIDTH: usize = 480;
    let pixels = WIDTH * height;
    let input = dir.join(format!("{height}-rows.rgba"));
    let output = dir.join(format!("{height}-rows.yuv"));
    let mut rgba = vec![0; pixels * frames * 4];
    let mut random = 0x23a9_0d79_u32;
    for byte in &mut rgba {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *byte = random.to_le_bytes()[0];
    }
    fs::write(&input, &rgba).unwrap();
    let status = Command::new(reference)
        .arg("convert")
        .args([WIDTH, height, 0, frames].map(|arg| arg.to_string()))
        .arg(&input)
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success(), "{height} rows: {status}");
    let yuv = fs::read(output).unwrap();
    assert_eq!(yuv.len(), pixels * frames * 3 / 2);
    let mut pairs = std::array::from_fn(|_| Vec::with_capacity(frames * WIDTH / 2));
    let mut chroma = std::array::from_fn(|_| Vec::with_capacity(frames * WIDTH / 2));
    for (rgba, yuv) in rgba
        .chunks_exact(pixels * 4)
        .zip(yuv.chunks_exact(pixels * 3 / 2))
    {
        for (input, pairs) in rgba.chunks_exact(WIDTH * 4).zip(&mut pairs) {
            pairs.extend(input.chunks_exact(8).map(|pair| {
                std::array::from_fn(|channel| {
                    i32::from(pair[channel]) + i32::from(pair[channel + 4])
                })
            }));
        }
        for (plane_index, samples) in chroma.iter_mut().enumerate() {
            let offset = pixels + plane_index * pixels / 4;
            samples.extend(
                yuv[offset..offset + WIDTH / 2]
                    .iter()
                    .map(|&value| i32::from(value) - 128),
            );
        }
    }
    (pairs, chroma)
}
