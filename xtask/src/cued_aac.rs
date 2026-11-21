//! Functional comparison of an indexed audio seek using public APIs.
use std::{path::Path, process::Command};

pub fn main(root: &Path, args: &[String]) -> Result<(), String> {
    let [install] = args else {
        return Err("usage: cargo xtask cued-aac-compare FFMPEG_INSTALL".into());
    };
    let install = Path::new(install);
    let out = root.join(".research/cued-aac-compare");
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let reference = out.join("reference");
    let status = Command::new("cc")
        .arg(root.join("harness/reference_cued_aac.c"))
        .arg("-I")
        .arg(install.join("include"))
        .arg(install.join("lib/libavformat.a"))
        .arg(install.join("lib/libavcodec.a"))
        .arg(install.join("lib/libavutil.a"))
        .args(["-lm", "-lz", "-o"])
        .arg(&reference)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("reference build failed".into());
    }
    let status = Command::new("cargo")
        .current_dir(root)
        .env("CARGO_INCREMENTAL", "0")
        .args([
            "run",
            "--quiet",
            "-p",
            "triergon-matroska",
            "--example",
            "cued_aac_comparison",
            "--",
        ])
        .arg(out.join("fixture.mkv"))
        .output()
        .map_err(|e| e.to_string())?;
    if !status.status.success() {
        return Err(format!(
            "candidate failed: {}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }
    let candidate = String::from_utf8(status.stdout).map_err(|e| e.to_string())?;
    let result = Command::new(reference)
        .arg(out.join("fixture.mkv"))
        .output()
        .map_err(|e| e.to_string())?;
    if !result.status.success() {
        return Err(format!(
            "reference operation failed: {}; candidate:\n{candidate}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    let reference = String::from_utf8(result.stdout).map_err(|e| e.to_string())?;
    let mut left = candidate.lines();
    let mut right = reference.lines();
    for index in 1.. {
        match (left.next(), right.next()) {
            (None, None) => break,
            (a, b) if a == b => (),
            (a, b) => {
                return Err(format!(
                    "first mismatch at line {index}:\ncandidate: {}\nreference: {}\ncomplete candidate:\n{candidate}complete reference:\n{reference}",
                    a.unwrap_or("<end>"),
                    b.unwrap_or("<end>")
                ));
            }
        }
    }
    println!("cued AAC seek comparison matches:\n{candidate}");
    Ok(())
}
