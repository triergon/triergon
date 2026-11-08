//! The `[Script Info]` section.

use crate::number::{header_bool, header_int, strtod};
use crate::scan::{is_space, trim_end, trim_start};

/// The script dialect, from `ScriptType` or the styles section header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ScriptType {
    /// Neither declared; a whole script without one is rejected.
    #[default]
    Unknown,
    /// v4.00+, `[V4+ Styles]`.
    Ass,
    /// v4.00, `[V4 Styles]`.
    Ssa,
}

/// How lines wrap (`WrapStyle`, `\q`). Values libass does not name wrap as
/// [`WrapStyle::Smart`] does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WrapStyle {
    /// 0: smart wrapping into lines of even width, the upper one wider.
    #[default]
    Smart,
    /// 1: wrapping at the end of a line; only `\N` breaks.
    EndOfLine,
    /// 2: no wrapping; `\n` and `\N` break.
    Manual,
    /// 3: smart wrapping, the lower line wider.
    SmartLower,
}

impl WrapStyle {
    /// The wrap style of libass's value.
    pub const fn from_value(value: i32) -> Self {
        match value {
            1 => Self::EndOfLine,
            2 => Self::Manual,
            3 => Self::SmartLower,
            _ => Self::Smart,
        }
    }
}

/// The `YCbCr Matrix` header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum YCbCrMatrix {
    /// No header, or an empty one.
    #[default]
    Default,
    /// A value libass does not know.
    Unknown,
    /// `None`: colours are not converted.
    None,
    /// `TV.601`.
    Bt601Tv,
    /// `PC.601`.
    Bt601Pc,
    /// `TV.709`.
    Bt709Tv,
    /// `PC.709`.
    Bt709Pc,
    /// `TV.240M`.
    Smpte240mTv,
    /// `PC.240M`.
    Smpte240mPc,
    /// `TV.FCC`.
    FccTv,
    /// `PC.FCC`.
    FccPc,
}

impl YCbCrMatrix {
    /// libass's `parse_ycbcr_matrix`: blanks trimmed, the first 15 bytes
    /// compared ignoring ASCII case.
    pub(crate) fn parse(value: &[u8]) -> Self {
        const NAMES: [(&[u8], YCbCrMatrix); 9] = [
            (b"none", YCbCrMatrix::None),
            (b"tv.601", YCbCrMatrix::Bt601Tv),
            (b"pc.601", YCbCrMatrix::Bt601Pc),
            (b"tv.709", YCbCrMatrix::Bt709Tv),
            (b"pc.709", YCbCrMatrix::Bt709Pc),
            (b"tv.240m", YCbCrMatrix::Smpte240mTv),
            (b"pc.240m", YCbCrMatrix::Smpte240mPc),
            (b"tv.fcc", YCbCrMatrix::FccTv),
            (b"pc.fcc", YCbCrMatrix::FccPc),
        ];
        let value = trim_end(trim_start(value));
        if value.is_empty() {
            return Self::Default;
        }
        let value = &value[..value.len().min(15)];
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(value))
            .map_or(Self::Unknown, |&(_, m)| m)
    }
}

/// The `[Script Info]` values libass reads. Unset integers are 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Info {
    /// The dialect.
    pub script_type: ScriptType,
    /// `PlayResX` as written; [`Info::play_res`] applies libass's defaults.
    pub play_res_x: i32,
    /// `PlayResY` as written.
    pub play_res_y: i32,
    /// `LayoutResX`.
    pub layout_res_x: i32,
    /// `LayoutResY`.
    pub layout_res_y: i32,
    /// `Timer`, which libass reads and does not apply.
    pub timer: f64,
    /// `WrapStyle`.
    pub wrap_style: WrapStyle,
    /// `ScaledBorderAndShadow`: borders and shadows scale with the video
    /// rather than the script resolution.
    pub scaled_border_and_shadow: bool,
    /// `Kerning`.
    pub kerning: bool,
    /// `YCbCr Matrix`.
    pub ycbcr_matrix: YCbCrMatrix,
    language: Option<([u8; 2], u8)>,
}

impl Info {
    /// The first two bytes of the `Language` header, if present.
    pub fn language(&self) -> Option<&[u8]> {
        self.language.as_ref().map(|(b, n)| &b[..usize::from(*n)])
    }

    /// `LayoutResX` and `LayoutResY` when both are positive; libass then lays
    /// out in that resolution, else in the renderer's storage size or the
    /// play resolution.
    pub const fn layout_res(&self) -> Option<(i32, i32)> {
        if self.layout_res_x > 0 && self.layout_res_y > 0 {
            Some((self.layout_res_x, self.layout_res_y))
        } else {
            None
        }
    }

    /// The play resolution libass renders with (`ass_lazy_track_init`): 384×288
    /// when neither dimension is positive, 1280×1024 from either half, else
    /// the missing one from a 4:3 aspect.
    pub const fn play_res(&self) -> (i32, i32) {
        let (x, y) = (self.play_res_x, self.play_res_y);
        if x > 0 && y > 0 {
            (x, y)
        } else if x <= 0 && y <= 0 {
            (384, 288)
        } else if y <= 0 && x == 1280 {
            (x, 1024)
        } else if y <= 0 {
            // libass computes `x * 3 / 4` without overflow as (x-1) - (x-1)/4.
            let x1 = x as u32 - 1;
            let y = (x1 - x1 / 4) as i32;
            (x, if y > 1 { y } else { 1 })
        } else if y == 1024 {
            (1280, y)
        } else {
            let sum = y as u32 + y as u32 / 3;
            (
                if sum > i32::MAX as u32 {
                    i32::MAX
                } else {
                    sum as i32
                },
                y,
            )
        }
    }

    /// libass's `process_info_line`: a `Key: value` header, keys compared
    /// exactly. Returns the header read.
    pub(super) fn read_line(&mut self, line: &[u8]) -> Option<Header> {
        let colon = line.iter().position(|&c| c == b':')?;
        let (key, value) = (&line[..colon], &line[colon + 1..]);
        let header = match key {
            b"PlayResX" => Header::PlayResX,
            b"PlayResY" => Header::PlayResY,
            b"LayoutResX" => Header::LayoutResX,
            b"LayoutResY" => Header::LayoutResY,
            b"Timer" => Header::Timer,
            b"WrapStyle" => Header::WrapStyle,
            b"ScaledBorderAndShadow" => Header::ScaledBorderAndShadow,
            b"Kerning" => Header::Kerning,
            b"YCbCr Matrix" => Header::ColourMatrix,
            b"Language" => {
                let value = &value[value
                    .iter()
                    .position(|&c| !is_space(c))
                    .unwrap_or(value.len())..];
                let mut bytes = [0; 2];
                let n = value.len().min(2);
                bytes[..n].copy_from_slice(&value[..n]);
                self.language = Some((bytes, n as u8));
                return Some(Header::Language);
            }
            b"ScriptType" => {
                if let Some(kind) = script_type(value) {
                    self.script_type = kind;
                }
                return Some(Header::ScriptType);
            }
            _ => return None,
        };
        self.set(header, value);
        Some(header)
    }

    /// Sets a header that a style override may also set, from its text.
    pub(super) fn set(&mut self, header: Header, value: &[u8]) {
        match header {
            Header::PlayResX => self.play_res_x = header_int(value),
            Header::PlayResY => self.play_res_y = header_int(value),
            Header::LayoutResX => self.layout_res_x = header_int(value),
            Header::LayoutResY => self.layout_res_y = header_int(value),
            Header::Timer => self.timer = strtod(value).0,
            Header::WrapStyle => self.wrap_style = WrapStyle::from_value(header_int(value)),
            Header::ScaledBorderAndShadow => self.scaled_border_and_shadow = header_bool(value),
            Header::Kerning => self.kerning = header_bool(value),
            Header::ColourMatrix => self.ycbcr_matrix = YCbCrMatrix::parse(value),
            Header::Language | Header::ScriptType | Header::Ffmpeg => {}
        }
    }
}

/// libass's `parse_script_type`: `4.00` before the last blank-trimmed byte,
/// optionally followed by `+`; no leading `v` is required.
fn script_type(value: &[u8]) -> Option<ScriptType> {
    let value = trim_end(value);
    if value.len() < 4 {
        return None;
    }
    let (kind, value) = match value.strip_suffix(b"+") {
        Some(rest) => (ScriptType::Ass, rest),
        None => (ScriptType::Ssa, value),
    };
    value.ends_with(b"4.00").then_some(kind)
}

/// A `[Script Info]` line libass records for its compatibility rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Header {
    Language,
    PlayResX,
    PlayResY,
    Timer,
    WrapStyle,
    ScaledBorderAndShadow,
    ColourMatrix,
    Kerning,
    ScriptType,
    LayoutResX,
    LayoutResY,
    /// A `; Script generated by FFmpeg/Lavc` comment.
    Ffmpeg,
}

impl Header {
    /// The headers a style override sets, by their name in any case.
    pub(super) fn overridable(name: &[u8]) -> Option<Self> {
        const NAMES: [(&[u8], Header); 9] = [
            (b"PlayResX", Header::PlayResX),
            (b"PlayResY", Header::PlayResY),
            (b"LayoutResX", Header::LayoutResX),
            (b"LayoutResY", Header::LayoutResY),
            (b"Timer", Header::Timer),
            (b"WrapStyle", Header::WrapStyle),
            (b"ScaledBorderAndShadow", Header::ScaledBorderAndShadow),
            (b"Kerning", Header::Kerning),
            (b"YCbCr Matrix", Header::ColourMatrix),
        ];
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, header)| header)
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// The headers seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Headers(u16);

impl Headers {
    pub(super) fn insert(&mut self, header: Header) {
        self.0 |= header.bit();
    }

    pub(super) const fn contains(self, header: Header) -> bool {
        self.0 & header.bit() != 0
    }

    /// Exactly the headers FFmpeg wrote before `ScaledBorderAndShadow`: the
    /// script type, the play resolution and its comment.
    pub(super) const fn is_legacy_ffmpeg(self) -> bool {
        self.0
            == Header::ScriptType.bit()
                | Header::PlayResX.bit()
                | Header::PlayResY.bit()
                | Header::Ffmpeg.bit()
    }
}

#[cfg(test)]
mod tests {
    use super::{Info, ScriptType, YCbCrMatrix, script_type};

    #[test]
    fn script_types_follow_vsfilter() {
        assert_eq!(script_type(b" v4.00+ "), Some(ScriptType::Ass));
        assert_eq!(script_type(b"4.00"), Some(ScriptType::Ssa));
        assert_eq!(script_type(b"x4.00"), Some(ScriptType::Ssa));
        assert_eq!(script_type(b"4.0+"), None);
        assert_eq!(script_type(b"v4.01"), None);
    }

    #[test]
    fn play_res_defaults_follow_libass() {
        let res = |x, y| {
            Info {
                play_res_x: x,
                play_res_y: y,
                ..Info::default()
            }
            .play_res()
        };
        assert_eq!(res(0, 0), (384, 288));
        assert_eq!(res(1280, 0), (1280, 1024));
        assert_eq!(res(640, 0), (640, 480));
        assert_eq!(res(1, 0), (1, 1));
        assert_eq!(res(0, 1024), (1280, 1024));
        assert_eq!(res(0, 480), (640, 480));
        assert_eq!(res(0, i32::MAX), (i32::MAX, i32::MAX));
        assert_eq!(YCbCrMatrix::parse(b" TV.709 "), YCbCrMatrix::Bt709Tv);
    }
}
