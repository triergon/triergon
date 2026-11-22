#[path = "../tests/support/ebml.rs"]
mod ebml;

fn main() -> std::io::Result<()> {
    let path = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected an output path")
    })?;
    let kind = std::env::args_os().nth(2);
    std::fs::write(
        path,
        if kind.as_deref() == Some(std::ffi::OsStr::new("ac3")) {
            ebml::cued_ac3_coded()
        } else if kind.as_deref() == Some(std::ffi::OsStr::new("coded")) {
            ebml::cued_aac_coded()
        } else {
            ebml::cued_aac().0
        },
    )
}
