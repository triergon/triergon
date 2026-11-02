//! Ordered header-removal and bounded zlib content encoding, RFC 9559 §5.1.4.1.31.
use crate::{Error, Limits, Result, ebml};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone)]
pub(crate) enum Compression {
    Header(Vec<u8>),
    Zlib,
}

impl Compression {
    pub(crate) fn decode(
        &self,
        data: &[u8],
        limits: Limits,
        output: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Header(prefix) => {
                let size = prefix
                    .len()
                    .checked_add(data.len())
                    .ok_or(Error::Overflow)?;
                if size > output {
                    return Err(Error::Limit("restored content bytes"));
                }
                let mut restored = Vec::new();
                restored
                    .try_reserve_exact(size)
                    .map_err(|_| Error::Limit("content allocation"))?;
                restored.extend_from_slice(prefix);
                restored.extend_from_slice(data);
                Ok(restored)
            }
            Self::Zlib => crate::zlib::decompress(
                data,
                crate::zlib::InflateLimits {
                    output_bytes: output,
                    expansion_ratio: limits.decompression_ratio,
                    work: limits.decompression_work,
                },
                cancel,
            ),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub(crate) struct Prefixes {
    pub frame: Vec<u8>,
    pub private: Vec<Compression>,
    pub frame_steps: Vec<Compression>,
}

pub(crate) fn parse(bytes: &[u8], limit: usize) -> Result<Prefixes> {
    let mut encodings = Vec::new();
    ebml::children(bytes, |id, entry| {
        if id != 0x6240 {
            return Ok(());
        }
        if encodings.len() >= 8 {
            return Err(Error::Limit("content encoding count"));
        }
        let mut order = 0;
        let mut scope = 1;
        let mut kind = 0;
        let mut compression = None;
        ebml::children(entry, |id, data| {
            match id {
                0x5031 => order = ebml::uint(data)?,
                0x5032 => scope = ebml::uint(data)?,
                0x5033 => kind = ebml::uint(data)?,
                0x5034 => {
                    if compression.replace(data).is_some() {
                        return Err(Error::Invalid("duplicate ContentCompression"));
                    }
                }
                0x5035 => return Err(Error::Unsupported("encrypted track")),
                _ => (),
            }
            Ok(())
        })?;
        if kind != 0 {
            return Err(Error::Unsupported("content encoding type"));
        }
        if scope == 0 {
            return Err(Error::Invalid("empty content encoding scope"));
        }
        if scope & !3 != 0 {
            return Err(Error::Unsupported("next-content encoding scope"));
        }
        let compression = compression.ok_or(Error::Invalid("missing ContentCompression"))?;
        let mut algorithm = 0;
        let mut settings = &[][..];
        ebml::children(compression, |id, data| {
            match id {
                0x4254 => algorithm = ebml::uint(data)?,
                0x4255 => settings = data,
                _ => (),
            };
            Ok(())
        })?;
        if algorithm != 3 && algorithm != 0 {
            return Err(Error::Unsupported("content compression algorithm"));
        }
        if encodings
            .iter()
            .any(|(previous, _, _, _): &(u64, u64, &[u8], u64)| *previous == order)
        {
            return Err(Error::Invalid("duplicate content encoding order"));
        }
        encodings.push((order, scope, settings, algorithm));
        Ok(())
    })?;
    if encodings.is_empty() {
        return Err(Error::Invalid("empty ContentEncodings"));
    }
    encodings.sort_by_key(|(order, _, _, _)| *order);
    let mut out = Prefixes::default();
    let frame_headers_only = encodings
        .iter()
        .all(|(_, scope, _, algorithm)| scope & 1 == 0 || *algorithm == 3);
    for (_, scope, header, algorithm) in encodings.iter().rev() {
        if header.len() > limit {
            return Err(Error::Limit("content encoding settings"));
        }
        let decoder = if *algorithm == 0 {
            Compression::Zlib
        } else {
            Compression::Header(header.to_vec())
        };
        if scope & 2 != 0 {
            out.private.push(decoder.clone());
        }
        if scope & 1 != 0 && !frame_headers_only {
            out.frame_steps.push(decoder);
        }
    }
    if frame_headers_only {
        // Highest encoding order decodes first; prepending produces ascending prefixes.
        for (_, scope, header, _) in encodings {
            if scope & 1 != 0 {
                if out
                    .frame
                    .len()
                    .checked_add(header.len())
                    .ok_or(Error::Overflow)?
                    > limit
                {
                    return Err(Error::Limit("restored header bytes"));
                }
                out.frame
                    .try_reserve_exact(header.len())
                    .map_err(|_| Error::Limit("header allocation"))?;
                out.frame.extend_from_slice(header);
            }
        }
    }
    Ok(out)
}
