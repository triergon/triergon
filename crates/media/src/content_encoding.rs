//! Ordered header-removal and bounded zlib content encoding, RFC 9559 §5.1.4.1.31.
use crate::{Error, Limits, Result, ebml, storage::SharedBytes};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, BudgetVec};

#[derive(Debug, Clone)]
pub(crate) enum DecodeStep {
    Header(SharedBytes),
    Zlib,
}
/// Eight is the existing explicit ContentEncoding policy. Inline steps avoid
/// uncharged recursive Vec clones in retained tracks.
#[derive(Debug, Clone)]
pub(crate) struct Decoder {
    pub(crate) steps: [Option<DecodeStep>; 8],
    pub(crate) count: usize,
}
impl Decoder {
    pub(crate) fn contains_zlib(&self) -> bool {
        self.steps[..self.count]
            .iter()
            .any(|s| matches!(s, Some(DecodeStep::Zlib)))
    }
    pub(crate) fn is_zlib_only(&self) -> bool {
        self.count == 1 && matches!(self.steps[0], Some(DecodeStep::Zlib))
    }
    fn append(slot: &mut Option<Self>, step: DecodeStep) -> Result<()> {
        let decoder = slot.get_or_insert_with(|| Self {
            steps: std::array::from_fn(|_| None),
            count: 0,
        });
        if decoder.count == decoder.steps.len() {
            return Err(Error::Limit("content encoding count"));
        }
        decoder.steps[decoder.count] = Some(step);
        decoder.count += 1;
        Ok(())
    }
    pub(crate) fn decode(
        &self,
        data: &SharedBytes,
        limits: Limits,
        output: usize,
        cancel: &AtomicBool,
        pool: &Budget,
    ) -> Result<SharedBytes> {
        let mut decoded: Option<SharedBytes> = None;
        for step in &self.steps[..self.count] {
            let input = decoded.as_ref().unwrap_or(data);
            let next = match step.as_ref().ok_or(Error::Invalid("empty decoder step"))? {
                DecodeStep::Header(prefix) => restore(prefix, input, output, pool)?,
                DecodeStep::Zlib => crate::zlib::decompress_in(
                    input,
                    crate::zlib::InflateLimits {
                        output_bytes: output,
                        expansion_ratio: limits.decompression_ratio,
                        work: limits.decompression_work,
                    },
                    cancel,
                    pool,
                )?,
            };
            decoded = Some(next);
        }
        decoded.ok_or(Error::Invalid("empty content decoder chain"))
    }
}
pub(crate) fn restore(
    prefix: &[u8],
    input: &[u8],
    limit: usize,
    pool: &Budget,
) -> Result<SharedBytes> {
    let size = prefix
        .len()
        .checked_add(input.len())
        .ok_or(Error::Overflow)?;
    if size > limit {
        return Err(Error::Limit("restored content bytes"));
    }
    let mut bytes = BudgetVec::new(pool);
    bytes.try_reserve_exact(size)?;
    bytes.try_extend_from_slice(prefix)?;
    bytes.try_extend_from_slice(input)?;
    Ok(SharedBytes::from_core(
        bytes.freeze().map_err(|e| Error::from(e.error))?,
    ))
}
#[derive(Default, Debug, Clone)]
pub(crate) struct Encodings {
    pub frame_prefix: SharedBytes,
    pub private: Option<Decoder>,
    pub frame: Option<Decoder>,
}

pub(crate) fn parse(bytes: &[u8], limit: usize, pool: &Budget) -> Result<Encodings> {
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
                DecodeStep::Zlib
            } else {
                DecodeStep::Header(SharedBytes::copy_from_slice(pool, header)?)
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
        let mut prefix = BudgetVec::new(pool);
        for &(_, scope, header, _) in entries.iter() {
            if scope & 1 == 0 {
                continue;
            }
            if prefix
                .len()
                .checked_add(header.len())
                .ok_or(Error::Overflow)?
                > limit
            {
                return Err(Error::Limit("restored header bytes"));
            }
            prefix.try_extend_from_slice(header)?;
        }
        if !prefix.is_empty() {
            out.frame_prefix =
                SharedBytes::from_core(prefix.freeze().map_err(|e| Error::from(e.error))?);
        }
    }
    Ok(out)
}

#[path = "typed/encodings.rs"]
pub(crate) mod typed;

impl crate::matroska::typed::usage::Storage for Decoder {
    fn storage_slots(&self) -> Result<usize> {
        Ok(8)
    }
    fn storage_owner(&self, index: usize) -> Result<Option<crate::matroska::typed::usage::Owner>> {
        match self.steps.get(index).and_then(Option::as_ref) {
            Some(DecodeStep::Header(bytes)) => {
                crate::matroska::typed::usage::Storage::storage_owner(bytes, 0)
            }
            _ => Ok(None),
        }
    }
}
crate::matroska::typed::usage::storage!(Encodings, |s| [s.frame_prefix, s.private, s.frame]);
