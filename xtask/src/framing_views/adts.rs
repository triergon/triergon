//! Aligned ADTS inputs and explicit buffer-boundary controls.
use std::fs;
use std::path::Path;

use crate::{codecs::derived, corpus::Paths};

use super::{Result, StreamCorpus};

pub(super) const IDS: [&str; 2] = ["aac-adts-frames", "aac-adts-header"];
const PS: &str = "harness/codecs/aac/ps/vectors";
const TAGGED: [&str; 6] = [
    "ct_File1_fixed_raw.adts",
    "ct_File1_raw.adts",
    "ct_sbr_i-ps_i_raw.adts",
    "id3v1.adts",
    "id3v2.adts",
    "id3v2_two_tags.adts",
];

fn frame(
    id: bool,
    crc: Option<u16>,
    rate: u8,
    channel: u8,
    fullness: u16,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let length = payload.len() + if crc.is_some() { 9 } else { 7 };
    let mut bytes = vec![
        0xff,
        0xf0 | u8::from(id) << 3 | u8::from(crc.is_none()),
        0x40 | rate << 2 | (flags & 1) << 1 | channel >> 2,
        (channel & 3) << 6 | (flags >> 1) << 2 | (length >> 11) as u8,
        (length >> 3) as u8,
        (length as u8 & 7) << 5 | (fullness >> 6) as u8,
        (fullness as u8 & 63) << 2,
    ];
    if let Some(word) = crc {
        bytes.extend(word.to_be_bytes());
    }
    bytes.extend(payload);
    bytes
}

pub(super) fn authored(paths: &Paths, corpus: &mut StreamCorpus) -> Result<()> {
    let payload = [0x21, 0x10];
    let base = frame(false, None, 3, 2, 2047, 0, &payload);
    let protected = frame(false, Some(0xabcd), 3, 2, 2047, 0, &payload);
    let mut control = |name: &str, data: &[u8]| {
        let file = format!("{name}.adts");
        let pin = corpus.write(&file, data)?;
        corpus.case(name, "authored", &file, &pin, "authored-synthetic", &[]);
        Ok::<(), String>(())
    };
    control("aligned", &base.repeat(4))?;
    control("protected-single", &protected)?;
    for size in 0..protected.len() {
        control(&format!("truncated-crc-{size}"), &protected[..size])?;
    }
    for size in 1..base.len() {
        control(&format!("truncated-{size}"), &base[..size])?;
    }
    for crc in [None, Some(0xabcd)] {
        let label = if crc.is_some() {
            "protected"
        } else {
            "unprotected"
        };
        let mut bytes = frame(false, crc, 3, 2, 2047, 0, &[]);
        control(&format!("empty-payload-{label}"), &bytes)?;
        bytes[5] -= 32;
        control(&format!("short-extent-{label}"), &bytes)?;
        let mut bytes = frame(false, crc, 3, 2, 2047, 0, &payload);
        bytes[6] |= 1;
        control(&format!("two-blocks-{label}"), &bytes)?;
    }
    for reserved in 0..3 {
        let mut bytes = base.clone();
        bytes[2] = (bytes[2] & 0xc3) | ((13 + reserved) << 2);
        control(&format!("frequency-{}", 13 + reserved), &bytes)?;
    }
    for (name, index, mask) in [("bad-sync", 0, 0x80), ("layer-1", 1, 2)] {
        let mut bytes = base.clone();
        bytes[index] ^= mask;
        control(name, &bytes)?;
    }
    for profile in [0, 2, 3] {
        let mut bytes = base.clone();
        bytes[2] = (bytes[2] & 63) | profile << 6;
        control(&format!("profile-{profile}"), &bytes)?;
    }
    for rate in 0..13 {
        control(
            &format!("rate-{rate}"),
            &frame(false, None, rate, 2, 2047, 0, &payload),
        )?;
    }
    for channel in 0..8 {
        control(
            &format!("channel-{channel}"),
            &frame(false, None, 3, channel, 2047, 0, &payload),
        )?;
    }
    for word in [0, 1, 0x8000, 0xffff] {
        control(
            &format!("crc-{word:04x}"),
            &frame(true, Some(word), 4, 0, 0, 31, &[0xff, 0, 0x81, 0]),
        )?;
    }
    let flags = [false, true]
        .into_iter()
        .flat_map(|id| (0..32).flat_map(move |flags| frame(id, None, 12, 7, 0x456, flags, &[0x80])))
        .collect::<Vec<_>>();
    control("header-flags", &flags)?;
    let fullness = [0, 1, 1024, 2047]
        .into_iter()
        .flat_map(|fullness| frame(false, None, 3, 2, fullness, 0, &payload))
        .collect::<Vec<_>>();
    control("buffer-fullness", &fullness)?;
    control(
        "maximum-frame",
        &frame(false, None, 3, 2, 2047, 0, &vec![0x80; 8184]),
    )?;
    for file in [
        "ps-change-while-off",
        "ps-ipd-after-iid34",
        "ps-no-sbr-element",
    ] {
        let path = paths.root.join(format!("{PS}/{file}.adts"));
        control(
            file,
            &fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )?;
    }
    Ok(())
}

pub(super) fn fate(root: &Path, corpus: &mut StreamCorpus) -> Result<()> {
    let recipes = derived::streams_matching(root, ".adts")?;
    if recipes.len() != 36
        || TAGGED
            .iter()
            .any(|name| !recipes.iter().any(|recipe| recipe.name == *name))
    {
        return Err("ADTS source membership changed; review the tagged inputs".into());
    }
    for recipe in recipes
        .iter()
        .filter(|recipe| !TAGGED.contains(&recipe.name.as_str()))
    {
        let path = root.join(&recipe.path);
        if !path.is_file() && !derived::make(root, &root.join(super::DECODER), recipe)? {
            return Err(format!("{} failed its derivation pin", recipe.name));
        }
        super::verify_hash(&path, &recipe.sha256)?;
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let pin = corpus.write(&recipe.name, &bytes)?;
        let id = recipe.name.trim_end_matches(".adts");
        corpus.case(
            id,
            "fate",
            &recipe.name,
            &pin,
            "fetched-not-redistributed",
            &[],
        );
    }
    Ok(())
}
