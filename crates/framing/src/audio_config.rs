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
use crate::{OpusHead, VorbisHeaders, flac_configuration_from_dfla, flac_configuration_from_ogg};
use triergon_core::Result;

/// Write the canonical FLAC configuration of `configuration` to `out`.
pub fn canonical_flac_configuration(configuration: &[u8], out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    if configuration.starts_with(b"fLaC") {
        crate::flac_stream_info(configuration)?;
        out.extend_from_slice(configuration);
    } else if configuration.starts_with(b"\x7fFLAC") {
        flac_configuration_from_ogg(configuration, out)?;
    } else {
        flac_configuration_from_dfla(configuration, out)?;
        crate::flac_stream_info(out)?;
    }
    Ok(())
}

/// Write the canonical Opus configuration (an `OpusHead` packet) of `configuration`.
pub fn canonical_opus_configuration(configuration: &[u8], out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    let head = if configuration.starts_with(b"OpusHead") {
        OpusHead::parse(configuration)?
    } else {
        OpusHead::from_dops(configuration)?
    };
    head.to_bytes(out);
    Ok(())
}

/// Check a Vorbis configuration and copy it to `out`.
pub fn canonical_vorbis_configuration(configuration: &[u8], out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    VorbisHeaders::from_laced(configuration)?;
    out.extend_from_slice(configuration);
    Ok(())
}
