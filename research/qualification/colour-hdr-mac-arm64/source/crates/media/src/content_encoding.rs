//! Ordered header-removal and bounded zlib content encoding, RFC 9559 §5.1.4.1.31.
use crate::{Error, Limits, Result, ebml};
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone)]
pub(crate) enum Decoder {
    Header(Vec<u8>),
    Zlib,
    Chain(Vec<Decoder>),
}
impl Decoder {
    pub(crate) fn contains_zlib(&self) -> bool {
        match self {
            Self::Zlib => true,
            Self::Header(_) => false,
            Self::Chain(steps) => steps.iter().any(Self::contains_zlib),
        }
    }
    fn append(slot: &mut Option<Self>, step: Self) -> Result<()> {
        *slot = Some(match slot.take() {
            None => step,
            Some(Self::Chain(mut steps)) => {
                steps
                    .try_reserve_exact(1)
                    .map_err(|_| Error::Limit("encoding allocation"))?;
                steps.push(step);
                Self::Chain(steps)
            }
            Some(previous) => {
                let mut steps = Vec::new();
                steps
                    .try_reserve_exact(2)
                    .map_err(|_| Error::Limit("encoding allocation"))?;
                steps.push(previous);
                steps.push(step);
                Self::Chain(steps)
            }
        });
        Ok(())
    }
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
            Self::Chain(steps) => {
                let mut steps = steps.iter();
                let first = steps
                    .next()
                    .ok_or(Error::Invalid("empty content decoder chain"))?;
                let mut decoded = first.decode(data, limits, output, cancel)?;
                for step in steps {
                    decoded = step.decode(&decoded, limits, output, cancel)?;
                }
                Ok(decoded)
            }
        }
    }
}
#[derive(Default, Debug, Clone)]
pub(crate) struct Encodings {
    pub frame_prefix: Vec<u8>,
    pub private: Option<Decoder>,
    pub frame: Option<Decoder>,
}

pub(crate) fn parse(bytes: &[u8], limit: usize) -> Result<Encodings> {
    let mut entries = [(0_u64, 0_u64, &[][..], 0_u64); 8];
    let mut count = 0;
    ebml::children(bytes, |id, entry| {
        if id != 0x6240 {
            return Ok(());
        }
        if count == entries.len() {
            return Err(Error::Limit("content encoding count"));
        }
        let (mut order, mut scope, mut kind, mut compression) = (0, 1, 0, None);
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
        let (mut algorithm, mut settings) = (0, &[][..]);
        ebml::children(compression, |id, data| {
            match id {
                0x4254 => algorithm = ebml::uint(data)?,
                0x4255 => settings = data,
                _ => (),
            };
            Ok(())
        })?;
        if algorithm != 0 && algorithm != 3 {
            return Err(Error::Unsupported("content compression algorithm"));
        }
        if entries[..count].iter().any(|entry| entry.0 == order) {
            return Err(Error::Invalid("duplicate content encoding order"));
        }
        entries[count] = (order, scope, settings, algorithm);
        count += 1;
        Ok(())
    })?;
    if count == 0 {
        return Err(Error::Invalid("empty ContentEncodings"));
    }
    let entries = &mut entries[..count];
    entries.sort_unstable_by_key(|entry| entry.0);
    let headers_only = entries
        .iter()
        .all(|(_, scope, _, algorithm)| scope & 1 == 0 || *algorithm == 3);
    let mut out = Encodings::default();
    for &(_, scope, header, algorithm) in entries.iter().rev() {
        if header.len() > limit {
            return Err(Error::Limit("content encoding settings"));
        }
        if scope & 2 != 0 || scope & 1 != 0 && !headers_only {
            let step = if algorithm == 0 {
                Decoder::Zlib
            } else {
                Decoder::Header(header.to_vec())
            };
            if scope & 2 != 0 {
                Decoder::append(&mut out.private, step.clone())?;
            }
            if scope & 1 != 0 && !headers_only {
                Decoder::append(&mut out.frame, step)?;
            }
        }
    }
    if headers_only {
        for &(_, scope, header, _) in entries.iter() {
            if scope & 1 == 0 {
                continue;
            }
            if out
                .frame_prefix
                .len()
                .checked_add(header.len())
                .ok_or(Error::Overflow)?
                > limit
            {
                return Err(Error::Limit("restored header bytes"));
            }
            out.frame_prefix
                .try_reserve_exact(header.len())
                .map_err(|_| Error::Limit("header allocation"))?;
            out.frame_prefix.extend_from_slice(header);
        }
    }
    Ok(out)
}
