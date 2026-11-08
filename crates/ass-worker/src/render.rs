//! The rendering operation of `harness/reference_ass_render.h`: a whole
//! script or a stream fixture rendered frame by frame with the fonts its
//! directives attach, every image folded in list order.

use std::path::Path;

use crate::fixture::{Command, Fixture, Frames};
use crate::perform::Result;

const USAGE: &str =
    "usage: ass-worker ass-render-fold FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H
       | ass-render-bench FILE ITERATIONS MODE ... | ass-render-memory FILE MODE ...
       | ass-render-dump FILE MODE ...
  MODE: script | chunks";

/// The largest frame or storage side either worker accepts.
const MAX_SIDE: u32 = 16384;
/// The largest font file either worker reads.
const MAX_FONT: u64 = 64 << 20;

/// A frame or storage size.
#[derive(Clone, Copy, Debug)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    fn read(width: &str, height: &str) -> Result<Self> {
        let side = |text: &str| match text.parse() {
            Ok(side) if (1..=MAX_SIDE).contains(&side) => Ok(side),
            _ => Err(USAGE),
        };
        Ok(Self {
            width: side(width)?,
            height: side(height)?,
        })
    }
}

/// A font file the fixture names, read.
pub struct FontFile {
    pub name: String,
    pub data: Vec<u8>,
}

fn read_font(fonts: &Path, path: &[u8]) -> Result<FontFile> {
    let name = std::str::from_utf8(path)?.to_owned();
    let path = fonts.join(&name);
    if std::fs::metadata(&path)?.len() > MAX_FONT {
        return Err(format!("{}: too large", path.display()).into());
    }
    Ok(FontFile {
        data: std::fs::read(&path)?,
        name,
    })
}

/// Everything one operation renders with, read before it.
pub struct Case<'a> {
    pub fixture: Fixture<'a>,
    pub stream: bool,
    pub attached: Vec<FontFile>,
    pub default_family: Option<&'a [u8]>,
    pub default_font: Option<FontFile>,
    pub frame: Size,
    pub storage: Size,
}

impl Case<'_> {
    /// The frames rendered, in order: the script's, or the stream's commands'.
    pub fn frames(&self) -> impl Iterator<Item = Frames> + '_ {
        let commands = self
            .fixture
            .commands
            .iter()
            .filter_map(|command| match command {
                Command::Frames(frames) => Some(*frames),
                _ => None,
            });
        let script = (!self.stream).then_some(self.fixture.frames.iter().copied());
        script.into_iter().flatten().chain(commands)
    }
}

/// `ass-render-*` with its arguments after the program name.
pub fn run(args: &[String]) -> Result<()> {
    let (action, rest) = args.split_first().ok_or(USAGE)?;
    let (path, rest) = match (action.as_str(), rest) {
        ("ass-render-bench", [path, count, rest @ ..]) => {
            let count: u64 = count.parse()?;
            if !(1..=1_000_000).contains(&count) {
                return Err(USAGE.into());
            }
            (path, rest)
        }
        ("ass-render-fold" | "ass-render-memory" | "ass-render-dump", [path, rest @ ..]) => {
            (path, rest)
        }
        _ => return Err(USAGE.into()),
    };
    let [mode, fonts, frame_w, frame_h, storage_w, storage_h] = rest else {
        return Err(USAGE.into());
    };
    let stream = match mode.as_str() {
        "script" => false,
        "chunks" => true,
        _ => return Err(USAGE.into()),
    };
    let data = std::fs::read(path)?;
    let fixture = Fixture::read(&data, stream);
    let fonts = Path::new(fonts);
    let case = Case {
        attached: fixture
            .attach
            .iter()
            .map(|path| read_font(fonts, path))
            .collect::<Result<_>>()?,
        default_family: fixture.default_family,
        default_font: fixture
            .default_font
            .map(|path| read_font(fonts, path))
            .transpose()?,
        frame: Size::read(frame_w, frame_h)?,
        storage: Size::read(storage_w, storage_h)?,
        stream,
        fixture,
    };
    let frames = case.frames().flat_map(Frames::times).count();
    Err(format!(
        "rendering is not supported ({frames} frames of {}x{}, storage {}x{}, {} fonts{}{})",
        case.frame.width,
        case.frame.height,
        case.storage.width,
        case.storage.height,
        case.attached.len(),
        case.default_family.map_or(String::new(), |f| format!(
            ", default family {}",
            String::from_utf8_lossy(f)
        )),
        case.default_font
            .as_ref()
            .map_or(String::new(), |f| format!(
                ", default font {} ({} bytes)",
                f.name,
                f.data.len()
            )),
    )
    .into())
}
