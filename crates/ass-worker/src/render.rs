//! The rendering operation of `harness/reference_ass_render.h`: a whole
//! script or a stream fixture rendered frame by frame with the fonts its
//! directives attach, every image folded in list order.
//!
//! One operation does what a player embedding libass does: it hands the
//! attached fonts it has read to the renderer (`ass_add_font`, which copies
//! them), opens the default font file (which libass opens itself), creates
//! and configures the renderer, reads the script or runs the stream and
//! renders every frame. The timed span is the reading and the rendering, on
//! both sides: it starts once the renderer is configured, since every libass
//! setter empties the renderer's caches, and it ends before the renderer, the
//! script and the fonts are freed, so neither side times a cache reset or the
//! teardown.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use triergon_ass::{
    Change, Fonts, FrameMargins, Image, ImageKind, Options, Renderer, Script, Settings,
    compat::libass::cache_limits,
};
use triergon_core::{Budget, Bytes};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_harness_runner::{Boundary, Invocation, Mode, Totals, write_summary};

use crate::fixture::{Command, Fixture, Frames as Frames_, frames_error};
use crate::perform::{Result, parse};
use crate::record::Work;

const USAGE: &str =
    "usage: ass-worker ass-render-fold FILE MODE FONTS FRAME-W FRAME-H STORAGE-W STORAGE-H [KEY=VALUE ...]
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
    pub data: Vec<u8>,
}

fn font_path(fonts: &Path, path: &[u8]) -> Result<PathBuf> {
    Ok(fonts.join(std::str::from_utf8(path)?))
}

fn read_font(path: &Path) -> Result<FontFile> {
    if std::fs::metadata(path)?.len() > MAX_FONT {
        return Err(format!("{}: too large", path.display()).into());
    }
    Ok(FontFile {
        data: std::fs::read(path)?,
    })
}

/// Everything one operation renders with, read before it.
pub struct Case<'a> {
    pub fixture: Fixture<'a>,
    pub stream: bool,
    pub attached: Vec<FontFile>,
    pub default_family: Option<&'a [u8]>,
    /// The default font file, which the operation opens.
    pub default_font: Option<PathBuf>,
    pub settings: Settings,
}

fn settings(frame: Size, storage: Size, overrides: &[String]) -> Result<Settings> {
    let mut settings = Settings::new(triergon_ass::Size::new(frame.width, frame.height));
    settings.storage = Some(triergon_ass::Size::new(storage.width, storage.height));
    // A player's cache limits, as the libass worker sets them.
    settings.cache = cache_limits(1000, 24);
    for option in overrides {
        let (key, value) = option.split_once('=').ok_or(USAGE)?;
        let number = || match value.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(value),
            _ => Err(USAGE),
        };
        match (key, value) {
            ("margins", value) => {
                let mut values = value.split(',');
                let mut side = || {
                    values
                        .next()
                        .and_then(|n| n.parse::<i32>().ok())
                        .ok_or(USAGE)
                };
                settings.margins = FrameMargins {
                    top: side()?,
                    bottom: side()?,
                    left: side()?,
                    right: side()?,
                };
                if values.next().is_some() {
                    return Err(USAGE.into());
                }
            }
            ("use-margins", "0" | "1") => settings.use_margins = value == "1",
            ("pixel-aspect", _) => settings.pixel_aspect = Some(number()?),
            ("font-scale", _) => settings.font_scale = number()?,
            ("line-spacing", _) => settings.line_spacing = number()?,
            ("line-position", _) => settings.line_position = number()?,
            _ => return Err(USAGE.into()),
        }
    }
    Ok(settings)
}

/// libass's number for an image's kind (`IMAGE_TYPE_CHARACTER`, `_OUTLINE`,
/// `_SHADOW`), and 3 for the box behind a `BorderStyle` 4 event, whose type
/// libass leaves uninitialised: the libass worker tells the box by its buffer.
const fn image_type(kind: ImageKind) -> i64 {
    match kind {
        ImageKind::Fill => 0,
        ImageKind::Border => 1,
        ImageKind::Shadow => 2,
        ImageKind::Background => 3,
    }
}

/// libass's number for a frame's change, as `ass_render_frame` reports it.
const fn change_number(change: Change) -> i64 {
    match change {
        Change::None => 0,
        Change::Positions => 1,
        Change::Content => 2,
    }
}

/// The images of every frame of one operation, folded or printed.
trait Frames {
    fn frame(&mut self, now: i64, change: Change, images: &[Image]) -> io::Result<()>;
}

/// The protocol's digest of the images.
#[derive(Debug, Default)]
struct Folded(Work);

impl Frames for Folded {
    fn frame(&mut self, now: i64, change: Change, images: &[Image]) -> io::Result<()> {
        let work = &mut self.0;
        fold(&mut work.digest, signed(now));
        fold(&mut work.digest, signed(change_number(change)));
        fold(&mut work.digest, images.len() as u64);
        for image in images {
            for value in [
                image_type(image.kind),
                i64::from(image.x),
                i64::from(image.y),
                image.width() as i64,
                image.height() as i64,
                i64::from(image.colour.0),
            ] {
                fold(&mut work.digest, signed(value));
            }
            for row in image.rows() {
                fold_bytes(&mut work.digest, row);
            }
            work.packets += 1;
            work.bytes += (image.width() * image.height()) as u64;
        }
        Ok(())
    }
}

/// The dump lines, as the libass worker prints them.
struct Printed<W: Write>(W);

impl<W: Write> Frames for Printed<W> {
    fn frame(&mut self, now: i64, change: Change, images: &[Image]) -> io::Result<()> {
        writeln!(
            self.0,
            "frame {now} {} {}",
            change_number(change),
            images.len()
        )?;
        for image in images {
            write!(
                self.0,
                "image {} {} {} {} {} {:08x}",
                image_type(image.kind),
                image.x,
                image.y,
                image.width(),
                image.height(),
                image.colour.0
            )?;
            for row in image.rows() {
                self.0.write_all(b" ")?;
                for byte in row {
                    write!(self.0, "{byte:02x}")?;
                }
            }
            writeln!(self.0)?;
        }
        Ok(())
    }
}

/// What one operation rendered with, freed by its caller after the timed
/// span, and how long its reading and rendering took.
struct Rendered {
    _fonts: Fonts,
    _renderer: Renderer,
    _script: Option<Script>,
    elapsed: Duration,
}

impl Case<'_> {
    /// The renderer's fonts: copies of the attached files, and the default
    /// font file opened, as libass opens it, or none when it cannot be read.
    fn fonts(&self, budget: &Budget) -> Result<Fonts> {
        let default_font = match self.default_font.as_deref().map(read_font) {
            Some(Ok(file)) => {
                Some(Bytes::try_from_vec(budget, file.data).map_err(|refused| refused.error)?)
            }
            Some(Err(_)) | None => None,
        };
        Ok(Fonts {
            attached: self
                .attached
                .iter()
                .map(|file| Bytes::copy_from_slice(budget, &file.data))
                .collect::<std::result::Result<_, _>>()?,
            default_family: self.default_family.map(<[u8]>::to_vec),
            default_font,
        })
    }

    /// One operation: the fonts, a renderer, the script read or the stream
    /// run, and every frame rendered, timed from the configured renderer to
    /// the last frame; what it rendered with is returned, to be freed outside
    /// the timed span.
    fn render(&self, budget: &Budget, data: &[u8], out: &mut impl Frames) -> Result<Rendered> {
        let fonts = self.fonts(budget)?;
        let mut renderer = Renderer::new(budget, &self.settings, &fonts)?;
        let options = Options {
            extract_fonts: true,
            ..Options::default()
        };
        let start = Instant::now();
        let mut frames = |script: &mut Script, frames: Frames_| -> Result<()> {
            for now in frames.times() {
                let frame = renderer.render(script, now)?;
                out.frame(now, frame.change, frame.images)?;
            }
            Ok(())
        };
        let script = if self.stream {
            let mut script = Script::new(budget, &options)?;
            self.run_stream(&mut script, &mut frames)?;
            Some(script)
        } else {
            let mut script = parse(budget, data, &options)?;
            if let Some(script) = &mut script {
                for f in &self.fixture.frames {
                    frames(script, *f)?;
                }
            }
            script
        };
        let elapsed = start.elapsed();
        Ok(Rendered {
            _fonts: fonts,
            _renderer: renderer,
            _script: script,
            elapsed,
        })
    }

    /// The stream's commands on `script`, rendering frames where they stand.
    fn run_stream(
        &self,
        script: &mut Script,
        frames: &mut impl FnMut(&mut Script, Frames_) -> Result<()>,
    ) -> Result<()> {
        for command in &self.fixture.commands {
            match command {
                Command::Private(text) => script.process_codec_private(text)?,
                Command::Data(text) => script.process_data(text)?,
                Command::Styles(text) => script.process_styles(text)?,
                Command::Chunk {
                    start,
                    duration,
                    packet,
                } => script.process_chunk(packet, *start, *duration)?,
                Command::Flush => script.flush_events(),
                Command::Prune(deadline) => script.prune_events(*deadline),
                Command::CheckReadOrder(check) => script.set_check_read_order(*check),
                Command::Frames(f) => frames(script, *f)?,
                Command::Dump => {}
            }
        }
        Ok(())
    }
}

/// `ass-render-*` with its arguments after the program name.
pub fn run(args: &[String]) -> Result<()> {
    let (action, rest) = args.split_first().ok_or(USAGE)?;
    let action = action.strip_prefix("ass-render-").ok_or(USAGE)?;
    let invocation = Invocation::parse_mode(action, rest, Some("dump"), || USAGE)?;
    invocation.check_iterations(|| USAGE)?;
    let Invocation {
        mode: action,
        path,
        iterations,
        arguments: rest,
    } = invocation;
    let [
        mode,
        fonts,
        frame_w,
        frame_h,
        storage_w,
        storage_h,
        overrides @ ..,
    ] = rest
    else {
        return Err(USAGE.into());
    };
    let stream = match mode.as_str() {
        "script" => false,
        "chunks" => true,
        _ => return Err(USAGE.into()),
    };
    let data = std::fs::read(path)?;
    if let Some(error) = frames_error(&data, stream) {
        return Err(error.into());
    }
    let fixture = Fixture::read(&data, stream);
    let fonts = Path::new(fonts);
    let case = Case {
        attached: fixture
            .attach
            .iter()
            .map(|path| read_font(&font_path(fonts, path)?))
            .collect::<Result<_>>()?,
        default_family: fixture.default_family,
        // libass opens the default font file itself and goes without one it
        // cannot open (an attached font is read by the player, and a missing
        // one fails the case on both sides).
        default_font: fixture
            .default_font
            .map(|path| font_path(fonts, path))
            .transpose()?,
        settings: settings(
            Size::read(frame_w, frame_h)?,
            Size::read(storage_w, storage_h)?,
            overrides,
        )?,
        stream,
        fixture,
    };
    // An accounting ceiling far above any case of the corpus, so only parity
    // decides: 16 GiB, or all of a 32-bit address space.
    let budget = Budget::new(usize::try_from(1_u64 << 34).unwrap_or(usize::MAX))?;
    let mut out = io::stdout().lock();
    if action == Mode::Trace {
        let mut printed = Printed(BufWriter::new(out));
        case.render(&budget, &data, &mut printed)?;
        return Ok(printed.0.flush()?);
    }
    let memory = action == Mode::Memory;
    if memory {
        Boundary::Begin.mark(&mut out)?;
    }
    let bench = action == Mode::Bench;
    let (mut total, mut elapsed) = (Work::default(), Duration::ZERO);
    for _ in 0..iterations {
        let mut folded = Folded::default();
        let rendered = case.render(&budget, std::hint::black_box(&data), &mut folded)?;
        elapsed += rendered.elapsed;
        drop(rendered);
        total.accumulate(&folded.0);
    }
    let elapsed = if bench { elapsed.as_nanos() } else { 0 };
    if memory {
        Boundary::End.mark(&mut out)?;
    }
    Ok(write_summary(
        &mut out,
        "ass-render",
        iterations,
        elapsed,
        &Totals {
            packets: total.packets,
            bytes: total.bytes,
            digest: total.digest,
            refusal: None,
        },
    )?)
}
