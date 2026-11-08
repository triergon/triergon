//! Writes the authored corpus fonts (features the freely licensed fonts do not
//! exercise) into a directory, for `cargo xtask fonts fetch`.
//!
//! Usage: authored-fonts DIR

#[path = "../tests/support/mod.rs"]
mod support;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [dir] = &args[..] else {
        eprintln!("usage: authored-fonts DIR");
        std::process::exit(2);
    };
    std::fs::create_dir_all(dir).expect("directory");
    for (name, data) in support::authored_fonts() {
        std::fs::write(std::path::Path::new(dir).join(name), data).expect("font file");
    }
}
