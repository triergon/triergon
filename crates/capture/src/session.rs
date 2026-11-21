//! One capture session: RGBA in, MPEG-1-in-TS bytes out.

use crate::{CaptureError, Result};
use triergon_codec_mpeg1::{EncodedPicture, Encoder, EncoderConfig};
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_pixel::{Converter, Geometry, Orientation, Picture, PictureView};

/// Receives each frame's complete transport stream bytes.
pub trait Sink {
    /// Takes the bytes of one frame; `false` means they were refused or only partly
    /// written, which fails the frame and closes the session.
    fn send(&mut self, bytes: &[u8]) -> bool;
}

impl Sink for Vec<u8> {
    /// Appends the bytes; never refuses.
    fn send(&mut self, bytes: &[u8]) -> bool {
        self.extend_from_slice(bytes);
        true
    }
}

/// The parameters of a capture session. [`Session::open`] refuses values outside
/// the ranges given here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    /// Frame width in pixels: even, 2 to 4094.
    pub width: u32,
    /// Frame height in pixels: even, 2 to 4094.
    pub height: u32,
    /// Average video rate in bit/s, as [`EncoderConfig::bit_rate`].
    pub bit_rate: u32,
    /// VBV fill rate in bit/s, as [`EncoderConfig::max_rate`].
    pub max_rate: u32,
    /// VBV buffer size in bits, as [`EncoderConfig::vbv_bits`].
    pub vbv_bits: u32,
    /// Intra period in frames, as [`EncoderConfig::gop`].
    pub gop: u32,
    /// The stream's program, PIDs and service description.
    pub mux: MuxConfig,
}

impl SessionConfig {
    /// The capture rate rule at `bit_rate`: `max_rate` = `bit_rate`·7/5 (saturating
    /// at `u32::MAX`), `vbv_bits` = `bit_rate`/7, a GOP of 30 and the default
    /// multiplex.
    pub fn with_rate_rule(width: u32, height: u32, bit_rate: u32) -> SessionConfig {
        SessionConfig {
            width,
            height,
            bit_rate,
            max_rate: u32::try_from(u64::from(bit_rate) * 7 / 5).unwrap_or(u32::MAX),
            vbv_bits: bit_rate / 7,
            gop: 30,
            mux: MuxConfig::default(),
        }
    }
}

/// What [`Session::encode`] did with one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameReport {
    /// The coded picture.
    pub picture: EncodedPicture,
    /// Transport stream bytes handed to the sink, a multiple of 188.
    pub ts_bytes: usize,
}

/// A capture session: RGBA frames in, the transport stream of one MPEG-1 video
/// program out.
///
/// [`open`](Self::open) builds the converter, encoder and multiplexer. Each
/// [`encode`](Self::encode) converts, codes and multiplexes one frame and hands its
/// complete bytes to a [`Sink`] before it returns; the first frame's bytes start
/// with the service and program tables and a sequence-header I picture. Frame k
/// (counting every successful call from 0) has PTS 3000·(k+1) and DTS 3000·k on the
/// 90 kHz clock. A conversion or encoder error leaves the session as it was; a
/// refused sink closes it, and every later call is [`CaptureError::Closed`].
#[derive(Debug)]
pub struct Session {
    converter: Converter,
    input: Picture,
    encoder: Encoder,
    muxer: Muxer,
    es: Vec<u8>,
    ts: Vec<u8>,
    closed: bool,
}

/// One frame period in the 90 kHz system clock at 30 pictures per second.
const FRAME_TICKS: i64 = 3000;

impl Session {
    /// A session converting with the chroma filter chosen by width
    /// ([`Converter::new`]). Fails with [`CaptureError::Pixel`] for a geometry
    /// outside the ranges of [`SessionConfig`], [`CaptureError::Encoder`] for rates
    /// or an intra period the encoder refuses, and [`CaptureError::Mux`] for a
    /// multiplex it cannot write.
    pub fn open(cfg: &SessionConfig) -> Result<Session> {
        Self::open_with_converter(cfg, Converter::new)
    }

    /// A session converting with the centre-sited chroma filter at every width
    /// ([`Converter::filtered`]); fails as [`open`](Self::open) does.
    pub fn open_filtered(cfg: &SessionConfig) -> Result<Session> {
        Self::open_with_converter(cfg, Converter::filtered)
    }

    fn open_with_converter(
        cfg: &SessionConfig,
        converter: fn(Geometry) -> Converter,
    ) -> Result<Session> {
        let geometry = Geometry::new(cfg.width, cfg.height)?;
        let encoder = Encoder::new(EncoderConfig {
            geometry,
            bit_rate: cfg.bit_rate,
            max_rate: cfg.max_rate,
            vbv_bits: cfg.vbv_bits,
            gop: cfg.gop,
        })?;
        Ok(Session {
            converter: converter(geometry),
            input: Picture::new(geometry),
            encoder,
            muxer: Muxer::new(&cfg.mux).map_err(CaptureError::Mux)?,
            es: Vec::new(),
            ts: Vec::new(),
            closed: false,
        })
    }

    /// The frame size and the coded size it is padded to.
    pub fn geometry(&self) -> Geometry {
        self.input.geometry()
    }

    /// Convert, encode and multiplex one frame of `width × height × 4` RGBA bytes
    /// (more are ignored), then hand its bytes to `sink`. Fails with
    /// [`CaptureError::Pixel`] for a shorter frame, [`CaptureError::Encoder`] when
    /// the encoder's storage is refused, [`CaptureError::Sink`] when the sink refuses
    /// the bytes, and [`CaptureError::Closed`] after that.
    pub fn encode(
        &mut self,
        rgba: &[u8],
        orientation: Orientation,
        sink: &mut impl Sink,
    ) -> Result<FrameReport> {
        if self.closed {
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
            self.closed = true;
            return Err(CaptureError::Sink);
        }
        Ok(FrameReport {
            picture,
            ts_bytes: self.ts.len(),
        })
    }

    /// The last frame as converted to YUV 4:2:0, before encoding.
    pub fn converted(&self) -> &Picture {
        &self.input
    }

    /// What a decoder reconstructs from the last frame.
    pub fn reconstruction(&self) -> PictureView<'_> {
        self.encoder.reconstruction()
    }

    /// Bits by which frames so far exceeded the encoder's modelled VBV occupancy,
    /// summed ([`Encoder::vbv_underflow_bits`]).
    pub fn vbv_underflow_bits(&self) -> f64 {
        self.encoder.vbv_underflow_bits()
    }
}
