#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

use core::marker::PhantomData;

mod division;
mod error;
pub mod layout;
pub mod mdct;
mod window;

pub use division::{Divisor, divide, divide_u32};
pub use error::DecodeError;
pub use layout::{ChannelLayout, Speaker};
/// [`Error`] where a decoder names a refused storage charge, as in
/// [`DecodeError::Budget`].
pub use triergon_core::Error as StorageError;
pub use triergon_core::{Budget, BudgetVec, Charged, Error, Reservation};
pub use triergon_pixel::{Colour, PictureView, Plane};
pub use window::{kbd_window, vorbis_window};

/// A video codec a workspace decoder decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
}

/// An audio codec a workspace decoder decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    /// AC-3 (Dolby Digital).
    Ac3,
    /// E-AC-3 (Dolby Digital Plus).
    Eac3,
    /// AAC.
    Aac,
    /// DTS (the core substream).
    Dts,
    /// FLAC.
    Flac,
    /// Opus.
    Opus,
    /// Vorbis.
    Vorbis,
    /// MPEG-1/2 audio Layer I, II or III.
    MpegAudio,
}

/// The sample format a decoder produces natively, before its samples become the
/// frame's `f32` planes. Mixing follows it: FFmpeg's resampler mixes 16-bit input in
/// fixed point.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit float; the planes hold the decoder's samples as they are.
    #[default]
    Float,
    /// Signed 16-bit; each sample is exactly `x / 2^15`.
    S16,
    /// Signed 32-bit, left-justified; each sample is `x / 2^31` rounded to `f32`,
    /// exact for up to 24 significant bits.
    S32,
}

/// Decoded audio of one access unit: planar `f32` at full scale 1.0.
#[derive(Clone, Debug, Default)]
pub struct AudioFrame {
    /// Samples per second per channel.
    pub sample_rate: u32,
    /// The speaker of each plane.
    pub layout: ChannelLayout,
    /// Samples in each plane; 0 when the access unit produced none.
    pub samples_per_channel: usize,
    /// What the planes are images of.
    pub sample_format: SampleFormat,
    /// One plane per speaker of `layout`, in its order, `samples_per_channel` samples each.
    pub planes: Vec<Vec<f32>>,
}

impl AudioFrame {
    /// Drop all samples (keeps allocations).
    pub fn clear(&mut self) {
        for plane in &mut self.planes {
            plane.clear();
        }
        self.samples_per_channel = 0;
    }

    /// Size the frame to `channels` planes of `len` samples each, new samples zero.
    ///
    /// The capacity the frame needs is charged to `charge` before any plane grows,
    /// and the charge then matches the frame's whole capacity, so a decoder's budget
    /// covers the output it fills. A refused charge leaves `charge` unchanged; the
    /// frame may have dropped planes beyond `channels`.
    pub fn try_resize(
        &mut self,
        channels: usize,
        len: usize,
        charge: &mut Reservation,
    ) -> Result<(), Error> {
        self.planes.truncate(channels);
        let planes = |count: usize| {
            count
                .checked_mul(size_of::<Vec<f32>>())
                .ok_or(Error::Overflow)
        };
        let samples = |count: usize| count.checked_mul(size_of::<f32>()).ok_or(Error::Overflow);
        let mut bytes = planes(self.planes.capacity().max(channels))?;
        for i in 0..channels {
            let capacity = self.planes.get(i).map_or(0, Vec::capacity).max(len);
            bytes = bytes
                .checked_add(samples(capacity)?)
                .ok_or(Error::Overflow)?;
        }
        charge.resize_to(bytes)?;
        let mut grew = false;
        if channels > self.planes.len() {
            self.planes
                .try_reserve_exact(channels - self.planes.len())
                .map_err(|_| Error::Allocation)?;
            self.planes.resize_with(channels, Vec::new);
            grew = true;
        }
        for plane in &mut self.planes {
            if len > plane.capacity() {
                plane
                    .try_reserve_exact(len - plane.len())
                    .map_err(|_| Error::Allocation)?;
                grew = true;
            }
            plane.resize(len, 0.0);
        }
        if grew {
            let mut bytes = planes(self.planes.capacity())?;
            for plane in &self.planes {
                bytes = bytes
                    .checked_add(samples(plane.capacity())?)
                    .ok_or(Error::Overflow)?;
            }
            charge.resize_to(bytes)?;
        }
        Ok(())
    }
}

/// Stereo downmix parameters a stream carries (AC-3/E-AC-3 centre/surround/LFE mix
/// levels and Dolby/Pro Logic II surround encoding). The default is the conventional
/// -3 dB centre and surrounds with LFE omitted, used when a stream carries none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DownmixLevels {
    /// Linear gain of the centre channel into each side.
    pub center: f32,
    /// Linear gain of the surround channels.
    pub surround: f32,
    /// Linear gain of the LFE channel, before its -3 dB into each side; 0 omits it.
    pub lfe: f32,
    /// How the surround channels are matrixed into the stereo pair.
    pub encoding: SurroundEncoding,
}

impl Default for DownmixLevels {
    fn default() -> Self {
        Self {
            center: core::f32::consts::FRAC_1_SQRT_2,
            surround: core::f32::consts::FRAC_1_SQRT_2,
            lfe: 0.0,
            encoding: SurroundEncoding::None,
        }
    }
}

/// How a stereo downmix carries the surround channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurroundEncoding {
    /// Each surround channel to its own side.
    None,
    /// Dolby Surround: the surrounds summed out of phase into both sides.
    Dolby,
    /// Dolby Pro Logic II: the surrounds out of phase into both sides, weighted to
    /// their own side.
    ProLogicII,
}

/// An Annex-B video decoder with caller timestamps carried through reordering.
pub trait VideoDecoder: Send {
    /// Why the decoder refuses an access unit: [`DecodeError`], or its crate's own enum
    /// with the same classes and more detail. Every decoder of this workspace converts it
    /// to [`Error`], keeping its class and its detail text.
    type Error;

    /// One Annex-B access unit with its caller timestamp. A refused access unit leaves
    /// the decoder usable.
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), Self::Error>;
    /// The next picture in output order, if ready. The view borrows the decoder until
    /// the next call; it returns `None` once no picture is ready.
    fn receive(&mut self) -> Option<PictureView<'_>>;
    /// End of stream: every remaining picture becomes receivable.
    fn flush(&mut self);
    /// Discontinuity: drop all decoder state except parameter sets.
    fn reset(&mut self);

    /// This decoder boxed, its errors converted to `E`: the form for a caller that
    /// chooses the codec at run time. With [`Error`] for `E` it is a
    /// [`DynVideoDecoder`]. Every call forwards to this decoder unchanged; only a
    /// refusal changes type, through `Into`.
    fn boxed<E: 'static>(self) -> Box<dyn VideoDecoder<Error = E>>
    where
        Self: Sized + 'static,
        Self::Error: Into<E>,
    {
        Box::new(Converting {
            decoder: self,
            error: PhantomData,
        })
    }
}

/// An audio decoder taking one access unit at a time.
pub trait AudioDecoder: Send {
    /// Why the decoder refuses an access unit: [`DecodeError`], or its crate's own enum
    /// with the same classes and more detail. Every decoder of this workspace converts it
    /// to [`Error`], keeping its class and its detail text.
    type Error;

    /// Replace `out` with the output of `au` (`samples_per_channel == 0` when it
    /// produced none). Returns the stream's downmix metadata, if it carries any. A
    /// refused access unit leaves the decoder usable.
    fn decode(
        &mut self,
        au: &[u8],
        out: &mut AudioFrame,
    ) -> Result<Option<DownmixLevels>, Self::Error>;
    /// Samples at the start of the stream that carry no signal and are discarded unless
    /// the container says otherwise (the Opus pre-skip); 0 for most decoders.
    fn delay(&self) -> usize {
        0
    }
    /// End of stream: replace `out` with any samples the decoder still holds back
    /// (`samples_per_channel == 0` when there are none, as for most decoders).
    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), Self::Error> {
        out.clear();
        Ok(())
    }
    /// Discontinuity: drop all signal history.
    fn reset(&mut self);

    /// This decoder boxed, its errors converted to `E`: the form for a caller that
    /// chooses the codec at run time. With [`Error`] for `E` it is a
    /// [`DynAudioDecoder`]. Every call forwards to this decoder unchanged, the provided
    /// [`delay`](Self::delay) and [`flush`](Self::flush) included; only a refusal changes
    /// type, through `Into`.
    fn boxed<E: 'static>(self) -> Box<dyn AudioDecoder<Error = E>>
    where
        Self: Sized + 'static,
        Self::Error: Into<E>,
    {
        Box::new(Converting {
            decoder: self,
            error: PhantomData,
        })
    }
}

/// A video decoder of a codec chosen at run time. It refuses with the workspace
/// [`Error`]; [`VideoDecoder::boxed`] makes one from a codec crate's decoder.
pub type DynVideoDecoder = dyn VideoDecoder<Error = Error>;

/// An audio decoder of a codec chosen at run time. It refuses with the workspace
/// [`Error`]; [`AudioDecoder::boxed`] makes one from a codec crate's decoder.
pub type DynAudioDecoder = dyn AudioDecoder<Error = Error>;

/// The decoder behind [`VideoDecoder::boxed`] and [`AudioDecoder::boxed`]: every method
/// forwards, and a refusal converts to `E` on the way out.
struct Converting<D, E> {
    decoder: D,
    // `fn() -> E`: the wrapper holds no `E`, so it is `Send` whenever the decoder is.
    error: PhantomData<fn() -> E>,
}

impl<D: VideoDecoder, E> VideoDecoder for Converting<D, E>
where
    D::Error: Into<E>,
{
    type Error = E;

    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), E> {
        self.decoder.send(au, pts).map_err(Into::into)
    }
    fn receive(&mut self) -> Option<PictureView<'_>> {
        self.decoder.receive()
    }
    fn flush(&mut self) {
        self.decoder.flush();
    }
    fn reset(&mut self) {
        self.decoder.reset();
    }
}

impl<D: AudioDecoder, E> AudioDecoder for Converting<D, E>
where
    D::Error: Into<E>,
{
    type Error = E;

    fn decode(&mut self, au: &[u8], out: &mut AudioFrame) -> Result<Option<DownmixLevels>, E> {
        self.decoder.decode(au, out).map_err(Into::into)
    }
    fn delay(&self) -> usize {
        self.decoder.delay()
    }
    fn flush(&mut self, out: &mut AudioFrame) -> Result<(), E> {
        self.decoder.flush(out).map_err(Into::into)
    }
    fn reset(&mut self) {
        self.decoder.reset();
    }
}
