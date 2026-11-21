#[path = "../tests/support/ebml.rs"]
mod ebml;

fn main() -> std::io::Result<()> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected an output path")
    })?;
    std::fs::write(path, ebml::cued_aac().0)
}
