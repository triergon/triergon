//! Native FLAC file splitting, until a reader crate provides it.
use triergon_bitstream::crc::crc16_a52;
use triergon_framing::{FlacFrameHeader, flac_metadata_blocks};

/// The longest span searched for the frame's end: a 65536-sample frame of eight
/// 32-bit verbatim channels is 2 MiB.
const MAX_FRAME_BYTES: usize = 4 << 20;

/// The configuration (`fLaC` and its metadata blocks) and the bytes after it. A file
/// without the marker has no configuration.
pub fn split_header(data: &[u8]) -> (&[u8], &[u8]) {
    let Ok(blocks) = flac_metadata_blocks(data) else {
        return (&[], data);
    };
    let mut end = 4;
    for block in blocks {
        match block {
            Ok(block) => end += 4 + block.body.len(),
            Err(_) => break,
        }
    }
    data.split_at(end)
}

/// The frames of `data`, starting at the first valid header. A frame ends at the next
/// valid header whose preceding bytes check against the CRC-16, else at the next
/// valid header at all.
pub fn frames(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = next_header(data, 0).map_or(&[][..], |start| &data[start..]);
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let window = &rest[..rest.len().min(MAX_FRAME_BYTES)];
        let mut fallback = None;
        let mut from = 1;
        let end = loop {
            match next_header(window, from) {
                Some(at) if crc16_a52(0, &rest[..at]) == 0 => break at,
                Some(at) => {
                    fallback.get_or_insert(at);
                    from = at + 1;
                }
                None => break fallback.unwrap_or(rest.len()),
            }
        };
        let (frame, tail) = rest.split_at(end);
        rest = tail;
        Some(frame)
    })
}

/// The first position at or after `from` where a frame header parses.
fn next_header(data: &[u8], from: usize) -> Option<usize> {
    (from..data.len().saturating_sub(1)).find(|&at| {
        data[at] == 0xff
            && data[at + 1] & 0xfe == 0xf8
            && FlacFrameHeader::parse(&data[at..]).is_ok()
    })
}
