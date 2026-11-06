//! One capture client session: RGBA in, MPEG-1-in-TS bytes out.
//!
//! A session starts with SDT/PAT/PMT and a sequence-header I picture, every call
//! produces that picture's complete TS bytes with advancing PTS/DTS/PCR and
//! continuity, and a sink failure poisons the session so the caller rebuilds.

use crate::{CaptureError, Result};
use triergon_codec_mpeg1::{EncodedPicture, Encoder, EncoderConfig};
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_pixel::{Converter, Geometry, Orientation, Picture};

/// Receives each frame's complete TS bytes. Returning `false` (refused or
/// partially written) fails the frame.
pub trait Sink {
    fn send(&mut self, bytes: &[u8]) -> bool;
}

impl Sink for Vec<u8> {
    fn send(&mut self, bytes: &[u8]) -> bool {
        self.extend_from_slice(bytes);
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub width: u32,
    pub height: u32,
    pub bit_rate: u32,
    pub max_rate: u32,
    pub vbv_bits: u32,
    pub gop: u32,
    pub mux: MuxConfig,
}

impl SessionConfig {
    /// The capture rate rule: maxrate = b·7/5, VBV buffer = b/7, GOP 30.
    pub fn with_rate_rule(width: u32, height: u32, bit_rate: u32) -> SessionConfig {
        SessionConfig {
            width,
            height,
            bit_rate,
            max_rate: (bit_rate as u64 * 7 / 5) as u32,
            vbv_bits: bit_rate / 7,
            gop: 30,
            mux: MuxConfig::default(),
        }
    }
}

/// Per-frame result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameReport {
    pub picture: EncodedPicture,
    pub ts_bytes: usize,
}

pub struct Session {
    converter: Converter,
    input: Picture,
    encoder: Encoder,
    muxer: Muxer,
    es: Vec<u8>,
    ts: Vec<u8>,
    failed: bool,
}

/// One frame period in the 90 kHz system clock at 30 pictures per second.
const FRAME_TICKS: i64 = 3000;

impl Session {
    pub fn open(cfg: &SessionConfig) -> Result<Session> {
        let geometry = Geometry::new(cfg.width, cfg.height)?;
        let encoder = Encoder::new(EncoderConfig {
            geometry,
            bit_rate: cfg.bit_rate,
            max_rate: cfg.max_rate,
            vbv_bits: cfg.vbv_bits,
            gop: cfg.gop,
        })?;
        Ok(Session {
            converter: Converter::new(geometry),
            input: Picture::new(geometry),
            encoder,
            muxer: Muxer::new(&cfg.mux)?,
            es: Vec::new(),
            ts: Vec::new(),
            failed: false,
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.input.geometry()
    }

    /// Convert, encode and mux one frame, then hand its bytes to `sink`.
    pub fn encode(
        &mut self,
        rgba: &[u8],
        orientation: Orientation,
        sink: &mut impl Sink,
    ) -> Result<FrameReport> {
        if self.failed {
            return Err(CaptureError::Closed);
        }
        self.converter.convert(rgba, orientation, &mut self.input)?;
        let picture = self.encoder.encode(&self.input, &mut self.es)?;
        let k = picture.index as i64;
        self.ts.clear();
        self.muxer.write_picture(
            &self.es,
            FRAME_TICKS * (k + 1),
            FRAME_TICKS * k,
            picture.key,
            &mut self.ts,
        );
        if !sink.send(&self.ts) {
            self.failed = true;
            return Err(CaptureError::Sink);
        }
        Ok(FrameReport {
            picture,
            ts_bytes: self.ts.len(),
        })
    }

    /// The converted input of the last frame (for conversion comparisons).
    pub fn converted(&self) -> &Picture {
        &self.input
    }

    /// The encoder's reconstruction of the last frame.
    pub fn reconstruction(&self) -> &Picture {
        self.encoder.reconstruction()
    }

    pub fn vbv_underflow_bits(&self) -> f64 {
        self.encoder.vbv_underflow_bits()
    }
}
