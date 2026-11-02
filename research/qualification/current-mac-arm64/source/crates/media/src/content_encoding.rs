//! Header-removal content encoding, RFC 9559 §5.1.4.1.31.
use crate::{Error, Result, ebml};

#[derive(Default, Debug, Clone)]
pub(crate) struct Prefixes {
    pub frame: Vec<u8>,
    pub private: Vec<u8>,
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
        if algorithm != 3 {
            return Err(Error::Unsupported("content compression algorithm"));
        }
        if encodings
            .iter()
            .any(|(previous, _, _): &(u64, u64, &[u8])| *previous == order)
        {
            return Err(Error::Invalid("duplicate content encoding order"));
        }
        encodings.push((order, scope, settings));
        Ok(())
    })?;
    if encodings.is_empty() {
        return Err(Error::Invalid("empty ContentEncodings"));
    }
    encodings.sort_by_key(|(order, _, _)| *order);
    // Decoding prepends from highest to lowest order; the final concatenated
    // prefix is therefore in ascending order. Build it once per track.
    let mut out = Prefixes::default();
    for (_, scope, header) in encodings {
        for (bit, destination) in [(1, &mut out.frame), (2, &mut out.private)] {
            if scope & bit != 0 {
                if destination
                    .len()
                    .checked_add(header.len())
                    .ok_or(Error::Overflow)?
                    > limit
                {
                    return Err(Error::Limit("restored header bytes"));
                }
                destination
                    .try_reserve_exact(header.len())
                    .map_err(|_| Error::Limit("header allocation"))?;
                destination.extend_from_slice(header);
            }
        }
    }
    Ok(out)
}

pub(crate) fn restore_private(prefix: &[u8], data: &mut Vec<u8>, limit: usize) -> Result<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    let total = prefix
        .len()
        .checked_add(data.len())
        .ok_or(Error::Overflow)?;
    if total > limit {
        return Err(Error::Limit("restored CodecPrivate bytes"));
    }
    data.try_reserve_exact(prefix.len())
        .map_err(|_| Error::Limit("private allocation"))?;
    let original = data.len();
    data.resize(total, 0);
    data.copy_within(0..original, prefix.len());
    data[..prefix.len()].copy_from_slice(prefix);
    Ok(())
}
