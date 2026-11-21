use std::ffi::OsString;
use std::sync::atomic::AtomicBool;

use triergon_core::{Budget, Bytes, WorkBudget};
use triergon_subtitles::compat::charset::{AssPacketJob, PacketStatus};
use triergon_subtitles::text::charset::multibyte::{
    DecodeStep, MultibyteCharset, MultibyteDecoder,
};
use triergon_subtitles::text::charset::{Charset, CharsetJob, CharsetStatus};
use triergon_subtitles::text::{Format, Limits};

use super::protocol::Operation;
use super::text::{Records, packet_input};

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [profile, label, mode] = arguments else {
        return Err(format!("usage: {prefix} PROFILE ICONV_LABEL MODE"));
    };
    let label = label
        .to_str()
        .ok_or("charset label is not UTF-8")?
        .as_bytes()
        .to_vec();
    let cancel = AtomicBool::new(false);
    let pool = Budget::new(32 * 1024 * 1024).map_err(|error| error.to_string())?;
    if prefix == "charset-packets-" {
        if mode != "packet" {
            return Err("charset-packets expects packet mode".into());
        }
        let glibc = profile == "glibc";
        return Ok(Operation {
            name: "charset-packets",
            run: Box::new(move |work, trace| {
                let mut records = Records { work, trace };
                match packet_conversion(input, &label, glibc, &pool, &cancel) {
                    Ok(Some(text)) => records.row(3, &[text.len() as u64], &text),
                    Ok(None) | Err(_) => records.refused(),
                }
            }),
        });
    }
    let profile = match profile.to_str() {
        Some("big5-receiver") => Charset::Multibyte(MultibyteCharset::ReceiverBig5Glibc239),
        Some("kr-c1") => Charset::Multibyte(MultibyteCharset::ReceiverEucKrC1),
        Some("kr-receiver") => Charset::Multibyte(MultibyteCharset::ReceiverEucKrGlibc239),
        Some("cp932-receiver") => Charset::Multibyte(MultibyteCharset::ReceiverWindows932Pua),
        Some(profile) => Charset::from_label(profile.as_bytes())
            .or_else(|_| MultibyteCharset::from_label(profile.as_bytes()).map(Charset::Multibyte))
            .map_err(|error| error.to_string())?,
        None => return Err("invalid charset profile name".into()),
    };
    let mode = mode.to_str().ok_or("invalid charset mode")?.to_owned();
    if !matches!(mode.as_str(), "bytes" | "pairs" | "stream") {
        return Err("expected bytes, pairs or stream mode".into());
    }
    Ok(Operation {
        name: "charset-streams",
        run: Box::new(move |work, trace| {
            let mut records = Records { work, trace };
            if mode == "stream" {
                return stream_conversion(&mut records, input, 0, profile, &pool, &cancel);
            }
            for byte in 0..=u8::MAX {
                if mode == "bytes" {
                    stream_conversion(&mut records, &[byte], byte.into(), profile, &pool, &cancel)?;
                } else {
                    for trail in 0..=u8::MAX {
                        let index = u16::from_be_bytes([byte, trail]);
                        stream_conversion(
                            &mut records,
                            &[byte, trail],
                            index.into(),
                            profile,
                            &pool,
                            &cancel,
                        )?;
                    }
                }
            }
            Ok(())
        }),
    })
}

fn packet_conversion(
    input: &[u8],
    label: &[u8],
    glibc: bool,
    pool: &Budget,
    cancel: &AtomicBool,
) -> triergon_core::Result<Option<Vec<u8>>> {
    let mut input = packet_input(Bytes::copy_from_slice(pool, input)?, Format::OpaqueAss);
    input.charset_label = Some(Bytes::copy_from_slice(pool, label)?);
    let mut job = if glibc {
        AssPacketJob::new_with_glibc_euc_kr(input, pool, Limits::default(), 0)?
    } else {
        AssPacketJob::new(input, pool, Limits::default(), 0)?
    };
    while job.poll(&mut WorkBudget::new(1024), cancel)? == PacketStatus::Pending {}
    Ok(job.take_output(cancel)?.visible().map(<[u8]>::to_vec))
}

fn stream_conversion(
    records: &mut Records<'_, '_>,
    input: &[u8],
    index: u64,
    charset: Charset,
    pool: &Budget,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let convert = || -> triergon_core::Result<Bytes> {
        let source = Bytes::copy_from_slice(pool, input)?;
        let mut job = CharsetJob::new(&source, None, charset, pool, Limits::default())?;
        while job.poll(&mut WorkBudget::new(1024), cancel)? == CharsetStatus::Pending {}
        Ok(job.take_output(cancel)?.utf8)
    };
    match convert() {
        Ok(bytes) => records.row(3, &[index, bytes.len() as u64], &bytes),
        Err(_) => records.row(
            8,
            &[index, refused_offset(input, charset, pool, cancel)? as u64],
            &[],
        ),
    }
}

fn refused_offset(
    input: &[u8],
    charset: Charset,
    pool: &Budget,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    if let Charset::Multibyte(profile) = charset {
        let mut decoder = MultibyteDecoder::new(profile);
        let mut start = 0;
        for &byte in input {
            match decoder.push(byte, &mut WorkBudget::new(1), cancel) {
                Ok(DecodeStep::Scalar(scalar)) => start = scalar.range.end,
                Ok(_) => {}
                Err(_) => return Ok(start),
            }
        }
        decoder.finish(cancel).map_or(Ok(start), |()| {
            Err("multibyte job and decoder disagree".into())
        })
    } else if charset == Charset::Utf8 {
        std::str::from_utf8(input).map_or_else(
            |error| Ok(error.valid_up_to()),
            |_| Err("UTF-8 validation disagrees with the charset job".into()),
        )
    } else if matches!(
        charset,
        Charset::Utf16Bom | Charset::Utf16Be | Charset::Utf16Le
    ) {
        Ok(utf16_offset(input, charset))
    } else {
        for (offset, &byte) in input.iter().enumerate() {
            let one = Bytes::copy_from_slice(pool, &[byte]).map_err(|error| error.to_string())?;
            let mut job = CharsetJob::new(&one, None, charset, pool, Limits::default())
                .map_err(|error| error.to_string())?;
            if job.poll(&mut WorkBudget::new(16), cancel).is_err() {
                return Ok(offset);
            }
        }
        Err("single-byte charset refused without a bad byte".into())
    }
}

fn utf16_offset(input: &[u8], charset: Charset) -> usize {
    let (mut at, little) = match charset {
        Charset::Utf16Bom => match input {
            [0xff, 0xfe, ..] => (2, true),
            [0xfe, 0xff, ..] => (2, false),
            _ => return 0,
        },
        _ => (0, charset == Charset::Utf16Le),
    };
    let units = input[at..].chunks_exact(2).map(|bytes| {
        if little {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        }
    });
    for decoded in char::decode_utf16(units) {
        match decoded {
            Ok(value) => at += 2 * value.len_utf16(),
            Err(_) => return at,
        }
    }
    at
}
