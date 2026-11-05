//! The pure-Rust codec crates behind the session traits.
use crate::codec::{
    AudioCodec, AudioDecode, AudioView, Colour, DecodeFailure, PictureView, Plane, Speaker,
    VideoCodec, VideoDecode,
};
use crate::mix::{MixLevels, SurroundEncoding};

pub fn video_decoder(codec: VideoCodec) -> Box<dyn VideoDecode> {
    match codec {
        VideoCodec::H264 => Box::new(H264(triergon_codec_h264::Decoder::new())),
        VideoCodec::Hevc => Box::new(Hevc(triergon_codec_hevc::Decoder::new())),
    }
}

pub fn audio_decoder(codec: AudioCodec) -> Box<dyn AudioDecode> {
    match codec {
        AudioCodec::Ac3 | AudioCodec::Eac3 => Box::new(Ac3 {
            decoder: triergon_codec_ac3::Decoder::new(),
            frame: Default::default(),
            speakers: Vec::new(),
        }),
        AudioCodec::Aac => Box::new(Aac {
            decoder: triergon_codec_aac::Decoder::new(),
            frame: Default::default(),
            speakers: Vec::new(),
        }),
        AudioCodec::Dts => Box::new(Dca {
            decoder: triergon_codec_dca::Decoder::new(),
            frame: Default::default(),
            speakers: Vec::new(),
        }),
    }
}

fn failure(error: impl std::fmt::Debug) -> DecodeFailure {
    DecodeFailure(format!("{error:?}"))
}

struct H264(triergon_codec_h264::Decoder);
impl VideoDecode for H264 {
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeFailure> {
        self.0.send(au, pts).map_err(failure)
    }
    fn receive(&mut self, sink: &mut dyn FnMut(PictureView<'_>)) -> bool {
        let Some(picture) = self.0.receive() else {
            return false;
        };
        fn plane(p: &triergon_codec_h264::Plane) -> Plane<'_> {
            let (data, stride, offset) = p.raw();
            Plane::U8 {
                data,
                stride,
                offset,
            }
        }
        sink(PictureView {
            width: picture.width,
            height: picture.height,
            bit_depth: picture.bit_depth as u8,
            planes: [
                plane(&picture.planes[0]),
                plane(&picture.planes[1]),
                plane(&picture.planes[2]),
            ],
            pts: picture.pts,
            sar: picture.sar,
            colour: Colour {
                matrix: picture.colour.matrix,
                full_range: picture.colour.full_range,
            },
        });
        true
    }
    fn flush(&mut self) {
        self.0.flush();
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

struct Hevc(triergon_codec_hevc::Decoder);
impl VideoDecode for Hevc {
    fn send(&mut self, au: &[u8], pts: Option<i64>) -> Result<(), DecodeFailure> {
        self.0.send(au, pts).map_err(failure)
    }
    fn receive(&mut self, sink: &mut dyn FnMut(PictureView<'_>)) -> bool {
        let Some(picture) = self.0.receive() else {
            return false;
        };
        fn plane(p: &triergon_codec_hevc::Plane) -> Plane<'_> {
            match p.raw() {
                (triergon_codec_hevc::Row::U8(data), stride, offset) => Plane::U8 {
                    data,
                    stride,
                    offset,
                },
                (triergon_codec_hevc::Row::U16(data), stride, offset) => Plane::U16 {
                    data,
                    stride,
                    offset,
                },
            }
        }
        sink(PictureView {
            width: picture.width as usize,
            height: picture.height as usize,
            bit_depth: picture.bit_depth as u8,
            planes: [
                plane(&picture.planes[0]),
                plane(&picture.planes[1]),
                plane(&picture.planes[2]),
            ],
            pts: picture.pts,
            sar: picture.sar,
            colour: Colour {
                matrix: picture.colour.matrix,
                full_range: picture.colour.full_range,
            },
        });
        true
    }
    fn flush(&mut self) {
        self.0.flush();
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

macro_rules! audio_adapter {
    ($name:ident, $crate_:ident, $levels:expr) => {
        struct $name {
            decoder: $crate_::Decoder,
            frame: $crate_::AudioFrame,
            speakers: Vec<Speaker>,
        }
        impl AudioDecode for $name {
            fn decode(
                &mut self,
                au: &[u8],
                sink: &mut dyn FnMut(AudioView<'_>),
            ) -> Result<(), DecodeFailure> {
                self.frame.planes.iter_mut().for_each(Vec::clear);
                self.frame.samples_per_channel = 0;
                #[allow(clippy::redundant_closure_call)]
                let levels = ($levels)(self.decoder.decode(au, &mut self.frame).map_err(failure)?);
                if self.frame.samples_per_channel == 0 {
                    return Ok(());
                }
                self.speakers.clear();
                self.speakers.extend(
                    self.frame
                        .layout
                        .speakers()
                        .iter()
                        .map(|s| Speaker::from_index(*s as u8)),
                );
                sink(AudioView {
                    sample_rate: self.frame.sample_rate,
                    speakers: &self.speakers,
                    planes: &self.frame.planes,
                    samples: self.frame.samples_per_channel,
                    levels,
                });
                Ok(())
            }
            fn reset(&mut self) {
                self.decoder.reset();
            }
        }
    };
}

audio_adapter!(
    Ac3,
    triergon_codec_ac3,
    |info: triergon_codec_ac3::FrameInfo| info.downmix.map(ac3_levels)
);
audio_adapter!(Aac, triergon_codec_aac, |(): ()| None);
audio_adapter!(Dca, triergon_codec_dca, |(): ()| None);

/// FFmpeg's resampler takes the Lt/Rt levels and surround encoding when the
/// stream prefers an Lt/Rt (or Pro Logic II) downmix, else the Lo/Ro levels.
fn ac3_levels(info: triergon_codec_ac3::DownmixInfo) -> MixLevels {
    use triergon_codec_ac3::DownmixType;
    let default = MixLevels::default();
    let (center, surround, encoding) = match info.preferred_downmix_type {
        Some(DownmixType::LtRt) => (
            info.center_mix_level_ltrt,
            info.surround_mix_level_ltrt,
            SurroundEncoding::Dolby,
        ),
        Some(DownmixType::DplII) => (
            info.center_mix_level_ltrt,
            info.surround_mix_level_ltrt,
            SurroundEncoding::ProLogicII,
        ),
        _ => (
            info.center_mix_level,
            info.surround_mix_level,
            SurroundEncoding::None,
        ),
    };
    MixLevels {
        center: center.map_or(default.center, |v| v as f32),
        surround: surround.map_or(default.surround, |v| v as f32),
        lfe: info.lfe_mix_level.map_or(default.lfe, |v| v as f32),
        encoding,
    }
}
