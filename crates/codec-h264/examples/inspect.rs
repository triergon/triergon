//! Print the H.264 features (from parameter sets and slice headers) of Annex-B files.
fn main() {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).expect("read");
        let f = triergon_codec_h264::inspect::inspect(&data);
        let name = std::path::Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        println!("{name}\t{f:?}");
    }
}
