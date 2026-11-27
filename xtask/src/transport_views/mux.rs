use std::fs;

use crate::corpus::Paths;

const SEED: &str = "harness/features/ts-mux-readback/capture-pictures.hex";
const PICTURE_HEADER: usize = 21;

pub(super) fn fixtures(paths: &Paths) -> Result<(), String> {
    let seed = paths.root.join(SEED);
    let encoded =
        fs::read_to_string(&seed).map_err(|error| format!("{}: {error}", seed.display()))?;
    let dir = paths.fixtures("mux-views")?;
    let mut pictures = Vec::new();
    let mut continuation = Vec::new();
    for (index, line) in encoded.lines().enumerate() {
        let picture =
            picture(line).ok_or_else(|| format!("{SEED}: invalid picture {}", index + 1))?;
        pictures.extend_from_slice(&picture);
        // One continuous run of predicted pictures from the first GOP.
        if (1..12).contains(&index) {
            if picture[16] != 0 {
                return Err(format!(
                    "{SEED}: isolated GOP continuation contains a key picture"
                ));
            }
            continuation.extend_from_slice(&picture);
        }
    }
    if continuation.is_empty() {
        return Err(format!("{SEED}: missing GOP continuation"));
    }
    super::write(&dir, "capture.bin", &pictures)?;
    super::write(&dir, "capture-continuation.bin", &continuation)
}

fn picture(line: &str) -> Option<Vec<u8>> {
    let pairs = line.as_bytes().chunks_exact(2);
    if !pairs.remainder().is_empty() {
        return None;
    }
    let picture: Vec<u8> = pairs
        .map(|pair| u8::from_str_radix(str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<_>>()?;
    let length = u32::from_be_bytes(picture.get(17..PICTURE_HEADER)?.try_into().ok()?) as usize;
    (length > 0 && picture.len() == PICTURE_HEADER.checked_add(length)?).then_some(picture)
}
