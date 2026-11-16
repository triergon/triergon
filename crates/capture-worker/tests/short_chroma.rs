use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "requires the pinned capture reference worker"]
fn short_filtered_frames_against_reference() {
    let reference = PathBuf::from(env::var_os("CAPTURE_REFERENCE").expect("set CAPTURE_REFERENCE"));
    let root = PathBuf::from(env::var_os("CAPTURE_PROBE_DIR").expect("set CAPTURE_PROBE_DIR"));
    fs::create_dir(&root).unwrap();
    let candidate = Path::new(env!("CARGO_BIN_EXE_capture-worker"));
    let mut random = 0x1fb7_1cdf_u32;
    for width in (2..16).step_by(2) {
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
                assert!(
                    max_error <= 1,
                    "{geometry}: maximum sample error {max_error}"
                );
            }
        }
    }
}
