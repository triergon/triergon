//! The operations, in the order of `harness/reference_ass.h`: recording a
//! track, running a stream fixture's commands, and walking every event at
//! every time with its drawings, vector clip and karaoke.

use std::io;
use triergon_ass::{
    Context, Drawing, Glyph, Karaoke, KaraokeGlyph, Placement, Script, ScriptType, ScrollDirection,
    Style, Syllables, Symbol, WrapStyle, YCbCrMatrix,
};
use triergon_core::{Budget, Error};
use triergon_raster::Outline;

use crate::fixture::Command;
use crate::record::{Kind, Sink, fnv};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const fn wrap_code(wrap: WrapStyle) -> i64 {
    match wrap {
        WrapStyle::Smart => 0,
        WrapStyle::EndOfLine => 1,
        WrapStyle::Manual => 2,
        WrapStyle::SmartLower => 3,
    }
}

const fn justify_code(justify: triergon_ass::Justify) -> i64 {
    match justify {
        triergon_ass::Justify::Auto => 0,
        triergon_ass::Justify::Left => 1,
        triergon_ass::Justify::Center => 2,
        triergon_ass::Justify::Right => 3,
    }
}

const fn karaoke_code(kind: Option<Karaoke>) -> i64 {
    match kind {
        None => 0,
        Some(Karaoke::Switch) => 1,
        Some(Karaoke::Fill) => 2,
        Some(Karaoke::Outline) => 3,
    }
}

fn style(sink: &mut impl Sink, index: usize, s: &Style) -> io::Result<()> {
    sink.begin(Kind::Style)?;
    sink.int(index as i64)?;
    sink.text(s.name())?;
    sink.text(s.font_name())?;
    sink.real(s.font_size)?;
    let c = s.colours;
    sink.ints(&[
        c.primary.0.into(),
        c.secondary.0.into(),
        c.outline.0.into(),
        c.back.0.into(),
        s.bold.0.into(),
        s.italic.0.into(),
    ])?;
    sink.flag(s.underline)?;
    sink.flag(s.strike_out)?;
    sink.reals(&[s.scale_x, s.scale_y, s.spacing, s.angle])?;
    sink.int(s.border_style.0.into())?;
    sink.reals(&[s.outline, s.shadow])?;
    let m = s.margins;
    sink.ints(&[
        s.alignment.bits().into(),
        m.left.into(),
        m.right.into(),
        m.vertical.into(),
        s.encoding.into(),
    ])?;
    sink.real(s.blur)?;
    sink.int(justify_code(s.justify))?;
    sink.end()
}

/// The track: header values, formats, styles, events and embedded fonts.
pub fn record_script(sink: &mut impl Sink, script: &Script) -> io::Result<()> {
    let info = script.info();
    sink.begin(Kind::Track)?;
    let kind = match info.script_type {
        ScriptType::Unknown => 0,
        ScriptType::Ass => 1,
        ScriptType::Ssa => 2,
    };
    let matrix = match info.ycbcr_matrix {
        YCbCrMatrix::Default => 0,
        YCbCrMatrix::Unknown => 1,
        YCbCrMatrix::None => 2,
        YCbCrMatrix::Bt601Tv => 3,
        YCbCrMatrix::Bt601Pc => 4,
        YCbCrMatrix::Bt709Tv => 5,
        YCbCrMatrix::Bt709Pc => 6,
        YCbCrMatrix::Smpte240mTv => 7,
        YCbCrMatrix::Smpte240mPc => 8,
        YCbCrMatrix::FccTv => 9,
        YCbCrMatrix::FccPc => 10,
    };
    sink.ints(&[
        kind,
        info.play_res_x.into(),
        info.play_res_y.into(),
        info.layout_res_x.into(),
        info.layout_res_y.into(),
    ])?;
    sink.real(info.timer)?;
    sink.int(wrap_code(info.wrap_style))?;
    sink.flag(info.scaled_border_and_shadow)?;
    sink.flag(info.kerning)?;
    sink.int(matrix)?;
    sink.optional(info.language())?;
    sink.ints(&[
        script.default_style() as i64,
        script.styles().len() as i64,
        script.events().len() as i64,
    ])?;
    sink.end()?;
    sink.begin(Kind::Formats)?;
    sink.optional(script.style_format())?;
    sink.optional(script.event_format())?;
    sink.end()?;
    for (i, s) in script.styles().iter().enumerate() {
        style(sink, i, s)?;
    }
    for (i, e) in script.events().iter().enumerate() {
        sink.begin(Kind::Event)?;
        sink.ints(&[
            i as i64,
            e.start,
            e.duration,
            e.read_order.into(),
            e.layer.into(),
            e.style as i64,
        ])?;
        sink.optional(e.name())?;
        let m = e.margins;
        sink.ints(&[m.left.into(), m.right.into(), m.vertical.into()])?;
        sink.optional(e.effect())?;
        sink.text(e.text())?;
        sink.end()?;
    }
    for (i, font) in script.fonts().iter().enumerate() {
        sink.begin(Kind::Font)?;
        sink.int(i as i64)?;
        sink.text(&font.name)?;
        sink.ints(&[
            font.data.len() as i64,
            fnv(0xcbf2_9ce4_8422_2325, &font.data) as i64,
        ])?;
        sink.end()?;
    }
    Ok(())
}

/// A whole script as `ass_read_memory` reads it: `None` (recorded as no
/// track) when libass returns none.
pub fn parse(
    budget: &Budget,
    data: &[u8],
    options: &triergon_ass::Options<'_>,
) -> Result<Option<Script>> {
    match Script::parse(budget, data, options) {
        Ok(script) => Ok(Some(script)),
        Err(Error::Invalid("no script type")) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// A stream fixture's commands on a new track, recording it at every
/// `@dump` and at the end.
pub fn run_stream(
    sink: &mut impl Sink,
    budget: &Budget,
    options: &triergon_ass::Options<'_>,
    commands: &[Command<'_>],
) -> Result<Script> {
    let mut script = Script::new(budget, options)?;
    for command in commands {
        match command {
            Command::Private(text) => script.process_codec_private(text)?,
            Command::Data(text) => script.process_data(text)?,
            Command::Styles(text) => script.process_styles(text)?,
            Command::Chunk {
                start,
                duration,
                packet,
            } => script.process_chunk(packet, *start, *duration)?,
            Command::Dump => record_script(sink, &script)?,
            Command::Flush => script.flush_events(),
            Command::Prune(deadline) => script.prune_events(*deadline),
            Command::CheckReadOrder(check) => script.set_check_read_order(*check),
            Command::Frames(_) => {}
        }
    }
    record_script(sink, &script)?;
    Ok(script)
}

/// What a walk keeps of each glyph for karaoke.
#[derive(Debug)]
struct Walked {
    karaoke: KaraokeGlyph,
    drawing: bool,
    primary: u32,
    secondary: u32,
}

/// Storage reused across walks.
#[derive(Debug)]
pub struct Walker {
    outline: Outline,
    glyphs: Vec<Walked>,
    karaoke: Vec<KaraokeGlyph>,
}

impl Walker {
    pub fn new(budget: &Budget) -> Self {
        Self {
            outline: Outline::new(budget),
            glyphs: Vec::new(),
            karaoke: Vec::new(),
        }
    }

    /// The play resolution, then every event at every time.
    pub fn walk_all(&mut self, sink: &mut impl Sink, script: &Script, times: &[i64]) -> Result<()> {
        let (x, y) = script.info().play_res();
        sink.begin(Kind::PlayRes)?;
        sink.ints(&[x.into(), y.into()])?;
        sink.end()?;
        for index in 0..script.events().len() {
            for &now in times {
                self.walk(sink, script, index, now)?;
            }
        }
        Ok(())
    }

    fn drawing(
        &mut self,
        sink: &mut impl Sink,
        kind: Kind,
        text: &[u8],
        script: &Script,
    ) -> Result<()> {
        sink.begin(kind)?;
        let drawing = Drawing::parse(text, &mut self.outline, script.limits())?;
        sink.outline(drawing.map(|d| (d.outline, d.cbox)))?;
        Ok(sink.end()?)
    }

    fn walk(
        &mut self,
        sink: &mut impl Sink,
        script: &Script,
        index: usize,
        now: i64,
    ) -> Result<()> {
        sink.begin(Kind::Walk)?;
        sink.ints(&[index as i64, now])?;
        sink.end()?;
        let event = &script.events()[index];
        let layout = script
            .info()
            .layout_res()
            .unwrap_or(script.info().play_res());
        let mut context = Context::new(script, event, now, layout)?;
        self.glyphs.clear();
        while let Some(glyph) = context.next_glyph()? {
            record_glyph(sink, &glyph)?;
            if let Symbol::Drawing { text, .. } = glyph.symbol {
                self.drawing(sink, Kind::Outline, text, script)?;
            }
            self.glyphs.push(Walked {
                karaoke: KaraokeGlyph {
                    starts_new_run: false,
                    mark: glyph.karaoke,
                    skip: glyph.symbol == Symbol::Char(u32::from(b' ')),
                    x: 0,
                    advance: 0,
                    rotate_z: glyph.state.rotate_z,
                },
                drawing: matches!(glyph.symbol, Symbol::Drawing { .. }),
                primary: glyph.state.colours.primary.0,
                secondary: glyph.state.colours.secondary.0,
            });
        }
        record_state(sink, &context)?;
        if let Some(clip) = context.state().vector_clip {
            self.drawing(sink, Kind::Clip, clip.drawing, script)?;
        }
        self.karaoke(sink, now.wrapping_sub(event.start))
    }

    /// Karaoke over synthetic positions: run starts where libass's style-run
    /// split puts them for karaoke alone, every glyph 10 pixels plus its
    /// index mod 3 wide, spaces invisible.
    fn karaoke(&mut self, sink: &mut impl Sink, elapsed: i64) -> Result<()> {
        let mut last_kind = self.glyphs.first().and_then(|g| g.karaoke.mark.kind);
        let mut x = 0;
        self.karaoke.clear();
        for (i, g) in self.glyphs.iter().enumerate() {
            let kind = g.karaoke.mark.kind;
            let starts_new_run = i == 0
                || g.karaoke.mark.timing != 0
                || (kind.is_some() && kind != last_kind)
                || g.drawing
                || self.glyphs[i - 1].drawing;
            if kind.is_some() {
                last_kind = kind;
            }
            let advance = 64 * (10 + (i % 3) as i32);
            self.karaoke.push(KaraokeGlyph {
                starts_new_run,
                x,
                advance,
                ..g.karaoke
            });
            x += advance;
        }
        let mut syllables = Syllables::new(&self.karaoke, elapsed).peekable();
        for (i, (glyph, walked)) in self.karaoke.iter().zip(&self.glyphs).enumerate() {
            while syllables.next_if(|s| s.glyphs.end <= i).is_some() {}
            let (mut kind, mut timing) = (glyph.mark.kind, glyph.mark.timing);
            let (mut c0, mut c1) = (walked.primary, walked.secondary);
            if let Some(syllable) = syllables.peek().filter(|s| s.glyphs.contains(&i)) {
                kind = Some(syllable.effect);
                timing = syllable.timing(glyph);
                if syllable.swap_colours {
                    (c0, c1) = (c1, c0);
                }
            }
            sink.begin(Kind::Karaoke)?;
            sink.ints(&[
                i as i64,
                karaoke_code(kind),
                timing.into(),
                c0.into(),
                c1.into(),
            ])?;
            sink.end()?;
        }
        Ok(())
    }
}

fn record_glyph(sink: &mut impl Sink, g: &Glyph<'_>) -> io::Result<()> {
    sink.begin(Kind::Glyph)?;
    sink.int(g.offset as i64)?;
    match g.symbol {
        Symbol::Char(code) => sink.ints(&[0, code.into()])?,
        Symbol::Drawing {
            text,
            scale,
            baseline,
        } => {
            sink.int(1)?;
            sink.text(text)?;
            sink.ints(&[scale.into(), baseline.into()])?;
        }
    }
    let s = &g.state;
    sink.text(s.family)?;
    sink.ints(&[(s.bold.0 as u32).into(), (s.italic.0 as u32).into()])?;
    sink.real(s.font_size)?;
    let c = s.colours;
    sink.ints(&[
        c.primary.0.into(),
        c.secondary.0.into(),
        c.outline.0.into(),
        c.back.0.into(),
        i64::from(s.underline) | i64::from(s.strike_out) << 1,
        s.border_style.0.into(),
    ])?;
    sink.reals(&[s.border_x, s.border_y, s.scale_x, s.scale_y, s.spacing])?;
    sink.int(s.blur_edges.into())?;
    sink.reals(&[
        s.blur, s.shadow_x, s.shadow_y, s.rotate_x, s.rotate_y, s.rotate_z, s.shear_x, s.shear_y,
    ])?;
    let k = g.karaoke;
    sink.ints(&[
        s.encoding.into(),
        g.fade.into(),
        karaoke_code(k.kind),
        k.timing.into(),
        k.skip_timing.into(),
    ])?;
    sink.flag(k.reset)?;
    sink.end()
}

fn record_state(sink: &mut impl Sink, context: &Context<'_>) -> io::Result<()> {
    let s = context.state();
    sink.begin(Kind::State)?;
    sink.ints(&[
        s.alignment.bits().into(),
        justify_code(s.justify),
        wrap_code(s.wrap_style),
    ])?;
    let (positioned, (x, y)) = match s.placement {
        Placement::Margins => (false, (0.0, 0.0)),
        Placement::Positioned(x, y) => (true, (x, y)),
    };
    sink.flag(positioned)?;
    sink.reals(&[x, y])?;
    let (ox, oy) = s.origin.unwrap_or((0.0, 0.0));
    sink.flag(s.origin.is_some())?;
    sink.reals(&[ox, oy])?;
    let c = s.clip;
    sink.ints(&[c.x0.into(), c.y0.into(), c.x1.into(), c.y1.into()])?;
    sink.flag(c.inverse)?;
    match s.vector_clip {
        Some(v) => {
            sink.text(v.drawing)?;
            sink.int(v.scale.into())?;
            sink.flag(v.inverse)?;
        }
        None => sink.absent()?,
    }
    sink.flag(s.detect_collisions)?;
    sink.flag(s.explicit)?;
    sink.ints(&[s.fade.into(), s.drawing_scale.into()])?;
    sink.real(s.drawing_baseline)?;
    match s.scroll {
        Some(scroll) => {
            let direction = match scroll.direction {
                ScrollDirection::LeftToRight => 0,
                ScrollDirection::RightToLeft => 1,
                ScrollDirection::TopToBottom => 2,
                ScrollDirection::BottomToTop => 3,
            };
            sink.int(direction)?;
            sink.real(scroll.shift)?;
            match scroll.band {
                Some((y0, y1)) => sink.ints(&[y0.into(), y1.into()])?,
                None => sink.absent()?,
            }
        }
        None => sink.absent()?,
    }
    sink.text(context.style().name())?;
    sink.end()
}
