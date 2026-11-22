#[path = "../tests/support/ebml.rs"]
mod ebml;

fn main() -> std::io::Result<()> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected an output path")
    })?;
    let coded = std::env::args_os().nth(2).is_some_and(|arg| arg == "coded");
    std::fs::write(
        path,
        if coded {
            ebml::cued_aac_coded()
        } else {
            ebml::cued_aac().0
        },
    )
}
