//! The hex dumps `ffprobe -show_data` writes into its JSON output.

/// The bytes of a `-show_data` hex dump.
pub fn data_bytes(dump: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for line in dump.lines().filter(|line| !line.is_empty()) {
        let (_, text) = line.split_once(':').ok_or("invalid data dump")?;
        let (hex, _) = text
            .trim_start()
            .split_once("  ")
            .ok_or("invalid data dump hex")?;
        for word in hex.split_ascii_whitespace() {
            if !word.len().is_multiple_of(2) || !word.is_ascii() {
                return Err("invalid data dump hex word".into());
            }
            for at in (0..word.len()).step_by(2) {
                bytes.push(
                    u8::from_str_radix(&word[at..at + 2], 16).map_err(|error| error.to_string())?,
                );
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::data_bytes;

    #[test]
    fn a_data_dump_gives_its_bytes() {
        let dump = "\n00000000: 1190 56e5 00                             ..V..\n";
        assert_eq!(data_bytes(dump).unwrap(), [0x11, 0x90, 0x56, 0xe5, 0x00]);
        assert!(data_bytes("00000000 1190").is_err());
    }
}
