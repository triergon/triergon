//! Ordered header-removal and bounded zlib content encoding, RFC 9559 §5.1.4.1.31.
use crate::{Error, Result, storage::SharedBytes};

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
            return Err(Error::Policy("content encoding count"));
        }
        decoder.steps[decoder.count] = Some(step);
        decoder.count += 1;
        Ok(())
    }
}
#[derive(Default, Debug, Clone)]
pub(crate) struct Encodings {
    pub frame_prefix: SharedBytes,
    pub private: Option<Decoder>,
    pub frame: Option<Decoder>,
}

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
