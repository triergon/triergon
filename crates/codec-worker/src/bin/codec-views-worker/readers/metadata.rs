use std::io::{Cursor, Write};
use std::sync::atomic::AtomicBool;

use triergon::elementary::{self, Ac3, Adts, Flac, Format, MpegAudio};
use triergon::ogg;
use triergon::{CodecId, PacketInfo, TrackInfo};

use super::{Work, record, tags};

const ABSENT: u64 = u64::MAX;

fn codec(codec: CodecId) -> u64 {
    match codec {
        CodecId::Flac => 1,
        CodecId::MpegAudio => 2,
        CodecId::Aac => 3,
        CodecId::Ac3 => 4,
        CodecId::Eac3 => 5,
        CodecId::Vorbis => 8,
        CodecId::Opus => 9,
        CodecId::Theora => 10,
        CodecId::Speex => 11,
        _ => 0,
    }
}

fn xiph_laced(mut data: &[u8]) -> Option<(&[u8], &[u8])> {
    data = data.strip_prefix(&[2])?;
    let read = |data: &mut &[u8]| {
        let count = data.iter().position(|&byte| byte < 255)?;
        let len = count * 255 + usize::from(data[count]);
        *data = &data[count + 1..];
        Some(len)
    };
    let identification = read(&mut data)?;
    let comments = read(&mut data)?;
    Some((
        data.get(..identification)?,
        data.get(identification + comments..)?,
    ))
}

fn streaminfo(config: &[u8]) -> &[u8] {
    config
        .strip_prefix(b"fLaC")
        .and_then(|data| data.get(4..38))
        .unwrap_or_default()
}

fn comments(mut data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Some((size, rest)) = data.split_at_checked(4) else {
        return Vec::new();
    };
    let vendor = u32::from_le_bytes(size.try_into().unwrap()) as usize;
    let Some(rest) = rest.get(vendor..) else {
        return Vec::new();
    };
    data = rest;
    let Some((count, rest)) = data.split_at_checked(4) else {
        return Vec::new();
    };
    let count = u32::from_le_bytes(count.try_into().unwrap());
    data = rest;
    let mut entries = Vec::new();
    for _ in 0..count {
        let Some((size, rest)) = data.split_at_checked(4) else {
            break;
        };
        let size = u32::from_le_bytes(size.try_into().unwrap()) as usize;
        let Some((entry, rest)) = rest.split_at_checked(size) else {
            break;
        };
        data = rest;
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            continue;
        };
        let mut key = entry[..eq].to_ascii_lowercase();
        let value = &entry[eq + 1..];
        key = match key.as_slice() {
            b"albumartist" => b"album_artist".to_vec(),
            b"tracknumber" => b"track".to_vec(),
            b"discnumber" => b"disc".to_vec(),
            b"description" => b"comment".to_vec(),
            _ => key,
        };
        entries.push((key, value.to_vec()));
    }
    entries.sort();
    entries
}

fn emit_comments(
    pairs: &[(Vec<u8>, Vec<u8>)],
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    for (key, value) in pairs {
        let payload = [key.as_slice(), value.as_slice()].concat();
        record(
            work,
            trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
            5,
            &[key.len() as u64, value.len() as u64],
            &payload,
        )?;
    }
    Ok(())
}

fn cover_art(encoded: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut bits, mut count) = (0_u32, 0);
    for &char in encoded.iter().take_while(|&&char| char != b'=') {
        let sextet = alphabet.iter().position(|&value| value == char)? as u32;
        bits = (bits << 6) | sextet;
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
        }
    }
    // FLAC PICTURE: kind, two length-prefixed strings, four dimensions,
    // and a length-prefixed image (RFC 9639 §8.8).
    let take_length_prefixed = |data: &[u8]| -> Option<usize> {
        (u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize).checked_add(4)
    };
    let mime = 4_usize;
    let desc = mime.checked_add(take_length_prefixed(bytes.get(mime..)?)?)?;
    let image = desc
        .checked_add(take_length_prefixed(bytes.get(desc..)?)?)?
        .checked_add(16)?;
    let remaining = bytes.get(image..)?;
    let end = image.checked_add(take_length_prefixed(remaining)?)?;
    let payload = bytes.get(image + 4..end)?;
    (!payload.is_empty()).then(|| payload.to_vec())
}

pub(super) fn ogg_tracks(
    input: &[u8],
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let Ok(reader) = ogg::BlockingReader::open(Cursor::new(input), ogg::Limits::default()) else {
        return record(work, trace, 8, &[], &[]);
    };
    if reader.tracks().is_empty() {
        return record(work, trace, 8, &[], &[]);
    }
    let mut attachments = Vec::new();
    for track in reader.tracks() {
        let code = codec(track.codec());
        let config = track.configuration();
        let (identification, setup) = match track.codec() {
            CodecId::Vorbis | CodecId::Theora => xiph_laced(config).unwrap_or((config, &[])),
            CodecId::Flac => (streaminfo(config), &[][..]),
            _ => (config, &[][..]),
        };
        let frame = track
            .frame_rate()
            .and_then(|(num, den)| triergon::TimeBase::new(u64::from(den), u64::from(num)).ok());
        let fields = [
            code,
            u64::from(track.sample_rate().unwrap_or(0)),
            u64::from(track.channels().unwrap_or(0)),
            frame.map_or(0, triergon::TimeBase::denominator),
            frame.map_or(0, triergon::TimeBase::numerator),
            track.end().map_or(ABSENT, |end| end as u64),
            u64::from(track.header_error().is_some()),
            u64::from(track.is_configured()),
            track.codec_tag().len() as u64,
        ];
        record(
            work,
            trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
            2,
            &fields,
            track.codec_tag(),
        )?;
        for block in [identification, setup] {
            record(
                work,
                trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
                3,
                &[block.len() as u64],
                block,
            )?;
        }
        let prefix = match track.codec() {
            CodecId::Vorbis => b"\x03vorbis".as_slice(),
            CodecId::Theora => b"\x81theora",
            CodecId::Opus => b"OpusTags",
            _ => b"",
        };
        let mut pairs = track
            .comments()
            .and_then(|data| data.strip_prefix(prefix))
            .map_or_else(Vec::new, comments);
        pairs.retain(|(key, value)| {
            if key == b"metadata_block_picture" {
                if let Some(image) = cover_art(value) {
                    attachments.push(image);
                }
                false
            } else {
                true
            }
        });
        emit_comments(&pairs, work, &mut trace)?;
        record(
            work,
            trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
            7,
            &[pairs.len() as u64],
            &[],
        )?;
    }
    attachments.sort();
    for image in attachments {
        record(
            work,
            trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
            6,
            &[image.len() as u64],
            &image,
        )?;
    }
    Ok(())
}

type Opened<'a, F> = (
    elementary::BlockingReader<F, Cursor<&'a [u8]>>,
    Option<elementary::Packet>,
);

pub(super) fn opened<F: Format>(input: &[u8]) -> triergon::Result<Opened<'_, F>> {
    let mut reader = elementary::BlockingReader::<F, _>::open(
        Cursor::new(input),
        elementary::Limits::default(),
    )?;
    let first = reader.next_packet(&AtomicBool::new(false))?;
    Ok((reader, first))
}

fn elementary_fields<F: Format>(
    input: &[u8],
    family: &str,
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let Ok((mut reader, first)) = opened::<F>(input) else {
        return record(work, trace, 8, &[], &[]);
    };
    let Some(track) = reader.reader().stream() else {
        return record(work, trace, 8, &[], &[]);
    };
    let config = streaminfo(track.configuration());
    let fields = [
        match track.codec_tag() {
            b"mp1" => 6,
            b"mp2" => 7,
            _ => codec(track.codec()),
        },
        if family == "aac" {
            0
        } else {
            u64::from(track.sample_rate())
        },
        if matches!(family, "aac" | "eac3") {
            0
        } else {
            u64::from(track.channels().unwrap_or(0))
        },
        track.duration().unwrap_or(ABSENT),
        first
            .as_ref()
            .and_then(|packet| packet.trim().start)
            .unwrap_or(0) as u64,
        config.len() as u64,
    ];
    record(
        work,
        trace.as_mut().map(|writer| &mut **writer as &mut dyn Write),
        2,
        &fields,
        config,
    )?;
    // The push reader can only discover footer tags by reaching them.
    if family == "mp3" {
        let cancel = AtomicBool::new(false);
        while matches!(reader.next_packet(&cancel), Ok(Some(_))) {}
    }
    let metadata = reader.reader();
    let mut pairs = tags::collect(metadata.tags());
    if let Some(track) = metadata.stream() {
        let config = track.configuration();
        if let Some(header) = config.get(42..46)
            && header[0] & 0x7f == 4
        {
            let len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
            if let Some(body) = config.get(46..46 + len) {
                let flac_comments: Vec<_> = comments(body)
                    .into_iter()
                    .filter(|(key, _)| tags::selected(key))
                    .collect();
                if !flac_comments.is_empty() {
                    pairs = flac_comments.into_iter().collect();
                }
            }
        }
    }
    let pairs: Vec<_> = pairs.into_iter().collect();
    emit_comments(&pairs, work, &mut trace)?;
    record(work, trace, 7, &[pairs.len() as u64], &[])
}

pub(super) fn elementary(
    input: &[u8],
    format: &str,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    match format {
        "flac" => elementary_fields::<Flac>(input, format, work, trace),
        "mp3" => elementary_fields::<MpegAudio>(input, format, work, trace),
        "aac" => elementary_fields::<Adts>(input, format, work, trace),
        "ac3" | "eac3" => elementary_fields::<Ac3>(input, format, work, trace),
        _ => Err("unknown elementary format".into()),
    }
}
