//! Author the signal-bearing family 2 control with the FFmpeg CLI.

use std::fs;
use std::path::Path;
use std::process::Command;

pub(super) fn run(ffmpeg: &Path, output: &Path) -> Result<(), String> {
    let scratch =
        std::env::temp_dir().join(format!("triergon-opus-control-{}", std::process::id()));
    fs::create_dir(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let result = (|| {
        let source = scratch.join("source.opus");
        let signals = [211, 307, 401, 503, 601, 701]
            .iter()
            .map(|hz| format!("0.1*sin(2*PI*{hz}*t)"))
            .collect::<Vec<_>>()
            .join("|");
        let status = Command::new(ffmpeg)
            .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("aevalsrc={signals}:s=48000:d=0.1"))
            .args([
                "-c:a",
                "libopus",
                "-mapping_family",
                "255",
                "-b:a",
                "256k",
                "-flags",
                "+bitexact",
                "-fflags",
                "+bitexact",
                "-y",
            ])
            .arg(&source)
            .status()
            .map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
        if !status.success() {
            return Err(format!("FFmpeg exited with {status}"));
        }
        let mut data = fs::read(&source).map_err(|e| e.to_string())?;
        if data.len() < 55
            || &data[..4] != b"OggS"
            || data[26..28] != [1, 27]
            || &data[28..36] != b"OpusHead"
            || data[37] != 6
            || data[46..55] != [255, 6, 0, 0, 1, 2, 3, 4, 5]
        {
            return Err("unexpected FFmpeg Opus header".into());
        }
        data[44..46].copy_from_slice(&1536_i16.to_le_bytes());
        data[46] = 2;
        data[49..55].copy_from_slice(&[0, 1, 2, 3, 5, 4]);
        data[22..26].fill(0);
        let mut crc = 0_u32;
        for &byte in &data[..55] {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = (crc << 1)
                    ^ if crc & 0x8000_0000 != 0 {
                        0x04c1_1db7
                    } else {
                        0
                    };
            }
        }
        data[22..26].copy_from_slice(&crc.to_le_bytes());
        fs::write(output, data).map_err(|e| format!("{}: {e}", output.display()))
    })();
    let cleanup = fs::remove_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()));
    result.and(cleanup)
}
