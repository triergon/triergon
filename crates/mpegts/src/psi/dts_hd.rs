//! DVB DTS-HD descriptor views from EN 300 468 Annex G.3.
use super::descriptor_tag::{DVB_DTS, DVB_DTS_HD_EXTENSION, DVB_EXTENSION};
use super::{Descriptor, DescriptorNamespace, ElementaryStream, stream_type};
use triergon_core::{Error, Result};

/// Signalled DTS-HD substreams and the original descriptor bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbDtsHd<'a> {
    /// The whole descriptor: tag, length and body.
    pub source: &'a [u8],
    /// The core substream, when signalled; it always has exactly one asset.
    pub core: Option<DvbDtsHdSubstream<'a>>,
    /// The four extension substreams, each present when its flag is set.
    pub extensions: [Option<DvbDtsHdSubstream<'a>>; 4],
    /// Trailing additional info bytes; may be empty.
    pub additional: &'a [u8],
}

/// One advertised DTS-HD substream. The flags describe all its assets together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbDtsHdSubstream<'a> {
    /// The 5-bit channel count field.
    pub channels: u8,
    /// Set when the substream signals a low-frequency effects channel.
    pub lfe: bool,
    /// Coded sample-rate index, 0–15, in the DVB DTS-HD sample-rate table.
    pub sample_rate_code: u8,
    /// Signalled sample depth exceeds 16 bits per sample.
    pub more_than_16_bits: bool,
    assets: &'a [u8],
}

/// One DTS-HD asset's signalling. The bit rate code keeps its source units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbDtsHdAsset {
    /// The 5-bit asset construction field.
    pub construction: u8,
    /// Set when the encoded rate describes a variable-rate asset.
    pub variable_bit_rate: bool,
    /// Selects the scaled interpretation of the coded rate.
    pub scaled_bit_rate: bool,
    /// Raw 13-bit rate value, interpreted with the scaling and variable-rate flags.
    pub bit_rate_code: u16,
    /// The component type byte, when its flag is set.
    pub component_type: Option<u8>,
    /// The ISO 639 language code bytes, when present.
    pub language: Option<[u8; 3]>,
}

impl<'a> DvbDtsHdSubstream<'a> {
    /// Iterates the substream's assets in source order; the parser checked their count and extent.
    pub fn assets(self) -> impl Iterator<Item = DvbDtsHdAsset> + 'a {
        let mut remaining = self.assets;
        std::iter::from_fn(move || {
            let (asset, rest) = read_asset(remaining)?;
            remaining = rest;
            Some(asset)
        })
    }
}

fn read_asset(bytes: &[u8]) -> Option<(DvbDtsHdAsset, &[u8])> {
    let (&[flags, high_rate, low_rate], mut rest) = bytes.split_first_chunk::<3>()?;
    let component_type = if flags & 1 != 0 {
        let (&component, tail) = rest.split_first()?;
        rest = tail;
        Some(component)
    } else {
        None
    };
    let language = if high_rate & 0x80 != 0 {
        let (&code, tail) = rest.split_first_chunk::<3>()?;
        rest = tail;
        Some(code)
    } else {
        None
    };
    Some((
        DvbDtsHdAsset {
            construction: flags >> 3,
            variable_bit_rate: flags & 4 != 0,
            scaled_bit_rate: flags & 2 != 0,
            bit_rate_code: u16::from_be_bytes([high_rate & 0x7f, low_rate]) >> 2,
            component_type,
            language,
        },
        rest,
    ))
}

impl<'a> Descriptor<'a> {
    /// Borrow the DTS-HD core, extension and asset fields of a DVB extension descriptor.
    /// Wrong tags, invalid substream lengths and multiple core assets return `Invalid`.
    /// Incomplete flagged fields or asset data return `Truncated`.
    pub fn dvb_dts_hd(self) -> Result<DvbDtsHd<'a>> {
        if self.tag != DVB_EXTENSION || self.data.first() != Some(&DVB_DTS_HD_EXTENSION) {
            return Err(Error::Invalid("DVB DTS-HD descriptor tag"));
        }
        let (&[_, flags], mut remaining) =
            self.data.split_first_chunk::<2>().ok_or(Error::Truncated)?;
        let mut substreams = [None; 5];
        for (index, slot) in substreams.iter_mut().enumerate() {
            if flags & (0x80 >> index) == 0 {
                continue;
            }
            let (&length, rest) = remaining.split_first().ok_or(Error::Truncated)?;
            let (body, following) = rest
                .split_at_checked(usize::from(length))
                .ok_or(Error::Truncated)?;
            remaining = following;
            let (&[first, second], assets) =
                body.split_first_chunk::<2>().ok_or(Error::Truncated)?;
            let count = (first >> 5) + 1;
            if index == 0 && count != 1 {
                return Err(Error::Invalid("DVB DTS-HD core asset count"));
            }
            let mut tail = assets;
            for _ in 0..count {
                (_, tail) = read_asset(tail).ok_or(Error::Truncated)?;
            }
            if !tail.is_empty() {
                return Err(Error::Invalid("DVB DTS-HD substream length"));
            }
            *slot = Some(DvbDtsHdSubstream {
                channels: first & 31,
                lfe: second & 0x80 != 0,
                sample_rate_code: (second >> 3) & 15,
                more_than_16_bits: second & 4 != 0,
                assets,
            });
        }
        let [core, ext0, ext1, ext2, ext3] = substreams;
        Ok(DvbDtsHd {
            source: self.source,
            core,
            extensions: [ext0, ext1, ext2, ext3],
            additional: remaining,
        })
    }
}

impl ElementaryStream {
    /// Borrow DTS-HD signalling for a DVB private-data elementary stream. `None`
    /// means the stream or descriptor is absent from this scope. A malformed, duplicate
    /// or conflicting descriptor returns an error.
    pub fn dvb_dts_hd(&self) -> Result<Option<DvbDtsHd<'_>>> {
        if self.stream_type != stream_type::PES_PRIVATE_DATA
            || !matches!(self.descriptor_namespace(), DescriptorNamespace::Dvb(_))
        {
            return Ok(None);
        }
        let mut hd = None;
        let mut core = false;
        for descriptor in self.descriptors() {
            let descriptor = descriptor?;
            if descriptor.tag == DVB_DTS {
                core = true;
            }
            if descriptor.tag == DVB_EXTENSION
                && descriptor.data.first() == Some(&DVB_DTS_HD_EXTENSION)
                && hd.replace(descriptor.dvb_dts_hd()?).is_some()
            {
                return Err(Error::Invalid("duplicate DVB DTS-HD descriptor"));
            }
        }
        if core && hd.is_some() {
            return Err(Error::Invalid("conflicting DVB DTS and DTS-HD descriptors"));
        }
        Ok(hd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(data: &[u8]) -> Descriptor<'_> {
        Descriptor {
            tag: DVB_EXTENSION,
            data,
            source: data,
        }
    }

    #[test]
    fn views_core_and_a_nonzero_extension_index_with_optional_asset_fields() {
        let data = [
            0x0e, 0xa0, 5, 2, 0x60, 0x08, 0x0f, 0, 9, 6, 0xec, 0x49, 0x85, 0x74, 0x3d, b'e', b'n',
            b'g', 0xaa,
        ];
        let view = descriptor(&data).dvb_dts_hd().unwrap();
        assert_eq!(view.source, data);
        assert_eq!(view.core.unwrap().channels, 2);
        assert_eq!(view.core.unwrap().sample_rate_code, 12);
        assert_eq!(view.core.unwrap().assets().count(), 1);
        assert!(view.extensions[0].is_none());
        let extension = view.extensions[1].unwrap();
        assert!(extension.lfe);
        assert!(extension.more_than_16_bits);
        assert_eq!(
            extension.assets().collect::<Vec<_>>(),
            [DvbDtsHdAsset {
                construction: 9,
                variable_bit_rate: false,
                scaled_bit_rate: false,
                bit_rate_code: 0x15d,
                component_type: Some(0x3d),
                language: Some(*b"eng"),
            }]
        );
        assert_eq!(view.additional, &[0xaa]);
    }

    #[test]
    fn enforces_the_declared_substream_and_asset_extents() {
        let complete = [0x0e, 0x40, 5, 2, 0x60, 8, 0, 0];
        for n in 1..complete.len() {
            assert_eq!(
                descriptor(&complete[..n]).dvb_dts_hd(),
                Err(Error::Truncated)
            );
        }
        let mut extra = complete.to_vec();
        extra[2] += 1;
        extra.push(0);
        assert_eq!(
            descriptor(&extra).dvb_dts_hd(),
            Err(Error::Invalid("DVB DTS-HD substream length"))
        );
    }
}
