//! Compare two interleaved f32le files per channel against the frozen D4 numeric rules
//! (per-channel RMS <= 8.8e-6, max |diff| <= 6.1035e-5, identical sample counts).
//!
//! usage: compare CANDIDATE.f32 REFERENCE.f32 CHANNELS
fn read(p: &str) -> Vec<f32> {
    let b = std::fs::read(p).expect("read");
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: compare CAND REF CHANNELS");
        std::process::exit(2);
    }
    let c = read(&a[1]);
    let r = read(&a[2]);
    let ch: usize = a[3].parse().expect("channels");
    let nc = c.len() / ch;
    let nr = r.len() / ch;
    let n = nc.min(nr);
    let mut ok = nc == nr && c.len() % ch == 0 && r.len() % ch == 0;
    let mut first_bad: Option<usize> = None;
    let mut parts = Vec::new();
    for k in 0..ch {
        let (mut se, mut mx) = (0f64, 0f64);
        for i in 0..n {
            let d = (c[i * ch + k] as f64 - r[i * ch + k] as f64).abs();
            se += d * d;
            if d > mx {
                mx = d;
            }
            if d > 6.1035e-5 && first_bad.is_none_or(|f| i < f) {
                first_bad = Some(i);
            }
        }
        let rms = if n > 0 { (se / n as f64).sqrt() } else { 0.0 };
        if rms > 8.8e-6 || mx > 6.1035e-5 {
            ok = false;
        }
        parts.push(format!("{{\"ch\":{k},\"rms\":{rms:.3e},\"max\":{mx:.3e}}}"));
    }
    println!(
        "{{\"pass\":{ok},\"cand_samples\":{nc},\"ref_samples\":{nr},\"first_bad_sample\":{},\"channels\":[{}]}}",
        first_bad.map_or("null".to_string(), |f| f.to_string()),
        parts.join(",")
    );
    std::process::exit(if ok { 0 } else { 1 });
}
