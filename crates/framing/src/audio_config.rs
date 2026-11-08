//! One canonical decoder configuration per audio codec, whichever container carried it.
//!
//! | Codec | Canonical form | Also accepted |
//! |---|---|---|
//! | FLAC | `fLaC` and metadata blocks (native stream header, Matroska CodecPrivate) | ISO BMFF `dfLa` payload, first Ogg FLAC packet |
//! | Opus | `OpusHead` packet (Ogg, Matroska CodecPrivate) | ISO BMFF `dOps` payload |
//! | Vorbis | the three header packets, Xiph-laced (Matroska CodecPrivate) | — |
//!
//! Each accepted form is recognised by its own leading bytes, never by guessing from the
//! container.
use crate::flac_config::FLAC_MARKER;
use crate::{OpusHead, VorbisHeaders, flac_stream_info};
use triergon_core::{BudgetVec, Error, Result};

/// Replace `out` with the concatenation of `parts`, charging their whole length before
/// any byte is copied.
fn fill(out: &mut BudgetVec<u8>, parts: &[&[u8]]) -> Result<()> {
    out.clear();
    let len = parts
        .iter()
        .try_fold(0usize, |len, part| len.checked_add(part.len()))
        .ok_or(Error::Overflow)?;
    out.try_reserve_exact(len)?;
    for part in parts {
        out.try_extend_from_slice(part)?;
    }
    Ok(())
}

/// Write the canonical FLAC configuration of `configuration` to `out`. A stream that
/// starts at a frame header carries none; it stays empty and the frames describe
/// themselves.
pub fn canonical_flac_configuration(configuration: &[u8], out: &mut BudgetVec<u8>) -> Result<()> {
    if configuration.is_empty() {
        out.clear();
        return Ok(());
    }
    if configuration.starts_with(FLAC_MARKER) {
        flac_stream_info(configuration)?;
        return fill(out, &[configuration]);
    }
    if configuration.starts_with(b"\x7fFLAC") {
        let (native, _) = crate::flac_config::ogg_native(configuration)?;
        return fill(out, &[native]);
    }
    let blocks = crate::flac_config::dfla_blocks(configuration)?;
    fill(out, &[FLAC_MARKER, blocks])?;
    flac_stream_info(out)?;
    Ok(())
}

/// Write the canonical Opus configuration (an `OpusHead` packet) of `configuration`.
pub fn canonical_opus_configuration(configuration: &[u8], out: &mut BudgetVec<u8>) -> Result<()> {
    let head = if configuration.starts_with(b"OpusHead") {
        OpusHead::parse(configuration)?
    } else {
        OpusHead::from_dops(configuration)?
    };
    let mut packet = [0u8; OpusHead::MAX_BYTES];
    let len = head.write(&mut packet);
    fill(out, &[&packet[..len]])
}

/// Check a Vorbis configuration and copy it to `out`.
pub fn canonical_vorbis_configuration(configuration: &[u8], out: &mut BudgetVec<u8>) -> Result<()> {
    VorbisHeaders::from_laced(configuration)?;
    fill(out, &[configuration])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flac_stream_without_a_header_keeps_an_empty_configuration() {
        let budget = triergon_core::Budget::new(1 << 16).unwrap();
        let mut out = BudgetVec::try_filled(&budget, 1, 3).unwrap();
        canonical_flac_configuration(&[], &mut out).unwrap();
        assert!(out.is_empty());
        assert!(canonical_flac_configuration(b"fLa", &mut out).is_err());
    }
}
