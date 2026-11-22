//! Dictionary projection of the reader's opaque ID3 and APE envelopes.

use std::collections::BTreeMap;

type Tags = BTreeMap<Vec<u8>, Vec<u8>>;

const ID3_NAMES: &[(&[u8], &[u8])] = &[
    (b"TIT2", b"title"),
    (b"TPE1", b"artist"),
    (b"TPE2", b"album_artist"),
    (b"TALB", b"album"),
    (b"TDRC", b"date"),
    (b"TYER", b"date"),
    (b"TRCK", b"track"),
    (b"TPOS", b"disc"),
    (b"TCOM", b"composer"),
    (b"TCOP", b"copyright"),
    (b"TENC", b"encoded_by"),
    (b"TPUB", b"publisher"),
    (b"TSSE", b"encoder"),
];

fn synchsafe(bytes: &[u8]) -> Option<usize> {
    bytes.iter().try_fold(0usize, |value, &byte| {
        (byte < 128).then_some((value << 7) | usize::from(byte))
    })
}

fn text(bytes: &[u8]) -> Option<String> {
    let (&encoding, bytes) = bytes.split_first()?;
    let latin = |bytes: &[u8]| {
        bytes
            .iter()
            .copied()
            .take_while(|&byte| byte != 0)
            .map(char::from)
            .collect::<String>()
    };
    match encoding {
        0 => Some(latin(bytes)),
        1 | 2 => {
            let (bytes, little) = if encoding == 2 {
                (bytes, false)
            } else if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
                (bytes, true)
            } else {
                (bytes.strip_prefix(&[0xfe, 0xff])?, false)
            };
            let words: Vec<_> = bytes
                .chunks_exact(2)
                .map(|pair| {
                    let pair = [pair[0], pair[1]];
                    if little {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    }
                })
                .take_while(|&word| word != 0)
                .collect();
            Some(String::from_utf16_lossy(&words))
        }
        3 => Some(String::from_utf8_lossy(bytes.split(|&byte| byte == 0).next()?).into_owned()),
        _ => None,
    }
}

fn unsynchronised(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut data = bytes.iter().copied().peekable();
    while let Some(value) = data.next() {
        result.push(value);
        if value == 255 && data.peek() == Some(&0) {
            data.next();
        }
    }
    result
}

fn id3v2(input: &[u8], tags: &mut Tags) -> Option<()> {
    let header = input.get(..10)?;
    let version = header[3];
    if !matches!(version, 3 | 4) {
        return None;
    }
    let size = synchsafe(&header[6..])?;
    let bytes = input.get(10..10 + size)?;
    let buffer;
    let mut bytes = if version == 3 && header[5] & 128 != 0 {
        buffer = unsynchronised(bytes);
        buffer.as_slice()
    } else {
        bytes
    };
    if header[5] & 64 != 0 {
        let len = bytes.get(..4)?;
        let size = if version == 4 {
            synchsafe(len)?
        } else {
            (u32::from_be_bytes(len.try_into().ok()?) as usize).checked_add(4)?
        };
        bytes = bytes.get(size..)?;
    }
    while let Some((head, rest)) = bytes.split_at_checked(10) {
        let name = &head[..4];
        if name[0] == 0 {
            break;
        }
        let size = if version == 4 {
            synchsafe(&head[4..8])?
        } else {
            u32::from_be_bytes(head[4..8].try_into().ok()?) as usize
        };
        let (body, remain) = rest.split_at_checked(size)?;
        bytes = remain;
        let Some((_, key)) = ID3_NAMES.iter().find(|(id, _)| *id == name) else {
            continue;
        };
        if (version == 3 && head[9] & 0xe0 != 0) || (version == 4 && head[9] & 0x4d != 0) {
            continue;
        }
        let buffer;
        let body = if version == 4 && (header[5] & 128 != 0 || head[9] & 2 != 0) {
            buffer = unsynchronised(body);
            buffer.as_slice()
        } else {
            body
        };
        if let Some(value) = text(body) {
            tags.insert(key.to_vec(), value.into_bytes());
        }
    }
    Some(())
}

fn id3v1(input: &[u8], tags: &mut Tags) {
    if input.len() != 128 {
        return;
    }
    let track = input[125] == 0 && input[126] != 0;
    for (name, bytes) in [
        (b"title".as_slice(), &input[3..33]),
        (b"artist", &input[33..63]),
        (b"album", &input[63..93]),
        (b"date", &input[93..97]),
        (b"comment", &input[97..if track { 125 } else { 127 }]),
    ] {
        let value: String = bytes
            .iter()
            .copied()
            .take_while(|&byte| byte != 0)
            .map(char::from)
            .collect();
        let value = value.trim_end().as_bytes();
        if !value.is_empty() {
            tags.entry(name.to_vec()).or_insert_with(|| value.to_vec());
        }
    }
    if track {
        tags.entry(b"track".to_vec())
            .or_insert_with(|| input[126].to_string().into_bytes());
    }
}

fn ape(input: &[u8], tags: &mut Tags) -> Option<()> {
    let header = input.get(..32)?;
    let count = u32::from_le_bytes(header[16..20].try_into().ok()?);
    let mut bytes = &input[32..];
    for _ in 0..count {
        let (size, rest) = bytes.split_at_checked(8)?;
        let size_value = u32::from_le_bytes(size[..4].try_into().ok()?) as usize;
        let flags = u32::from_le_bytes(size[4..].try_into().ok()?);
        let name_end = rest.iter().position(|&b| b == 0)?;
        let (value, remain) = rest[name_end + 1..].split_at_checked(size_value)?;
        bytes = remain;
        if flags & 6 != 0 {
            continue;
        }
        let mut key = rest[..name_end].to_ascii_lowercase();
        if key == b"year" {
            key = b"date".to_vec();
        }
        if selected(&key) {
            let value = value.split(|&b| b == 0).next()?.to_vec();
            tags.insert(key, value);
        }
    }
    Some(())
}

pub(super) fn selected(key: &[u8]) -> bool {
    key == b"comment" || ID3_NAMES.iter().any(|(_, name)| *name == key)
}

pub(super) fn collect(tags: &[triergon::elementary::Tag]) -> Tags {
    use triergon::elementary::TagKind;

    let mut result = BTreeMap::new();
    for tag in tags {
        let Some(bytes) = tag.bytes.as_deref() else {
            continue;
        };
        match tag.kind {
            TagKind::Id3v2 => {
                id3v2(bytes, &mut result);
            }
            TagKind::Id3v1 => id3v1(bytes, &mut result),
            TagKind::Ape => {
                ape(bytes, &mut result);
            }
        }
    }
    result
}
