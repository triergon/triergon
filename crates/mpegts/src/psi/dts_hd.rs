//! DVB DTS-HD descriptor views from EN 300 468 Annex G.3.
use super::{Descriptor, DescriptorNamespace, ElementaryStream, PES_PRIVATE_DATA};
use triergon_core::{Error, Result};

const EXTENSION_DESCRIPTOR: u8 = 0x7f;
const DTS_HD_EXTENSION: u8 = 0x0e;

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
    /// Whether an LFE channel is present.
    pub lfe: bool,
    /// The 4-bit sample rate code.
    pub sample_rate_code: u8,
    /// Whether samples have more than 16 bits.
    pub more_than_16_bits: bool,
    assets: &'a [u8],
}

/// One DTS-HD asset's signalling. The bit rate code keeps its source units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DvbDtsHdAsset {
    /// The 5-bit asset construction field.
    pub construction: u8,
    /// Whether the asset uses a variable bit rate.
    pub variable_bit_rate: bool,
    /// Whether the bit rate code is scaled.
    pub scaled_bit_rate: bool,
    /// The 13-bit bit rate code.
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
    /// Parse an explicitly interpreted DVB DTS-HD extension descriptor.
    pub fn dvb_dts_hd(self) -> Result<DvbDtsHd<'a>> {
        if self.tag != EXTENSION_DESCRIPTOR || self.data.first() != Some(&DTS_HD_EXTENSION) {
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
    /// DTS-HD signalling on DVB private-data PES. The elementary payload is
    /// handled independently of the descriptor envelope.
    pub fn dvb_dts_hd(&self) -> Result<Option<DvbDtsHd<'_>>> {
        if self.stream_type != PES_PRIVATE_DATA
            || !matches!(self.descriptor_namespace(), DescriptorNamespace::Dvb(_))
        {
            return Ok(None);
        }
        let mut hd = None;
        let mut core = false;
        for descriptor in self.descriptors() {
            let descriptor = descriptor?;
            if descriptor.tag == 0x7b {
                core = true;
            }
            if descriptor.tag == EXTENSION_DESCRIPTOR
                && descriptor.data.first() == Some(&DTS_HD_EXTENSION)
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
            tag: 0x7f,
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
