//! What the robustness test runs on every input: reading it as a script and
//! as a stream, evaluating every event, its drawings and karaoke. Errors are
//! expected; panics and hangs are not.

use triergon_ass::{
    Context, Drawing, KaraokeGlyph, Limits, Options, Script, Syllables, Symbol, Tags,
};
use triergon_core::Budget;
use triergon_raster::Outline;

const OVERRIDES: [&[u8]; 4] = [
    b"Default.Fontsize=30",
    b"ScaleX=2",
    b"PlayResY=0",
    b"x.y.Bold=",
];

fn evaluate(script: &Script, limits: Limits, outline: &mut Outline) {
    let layout = script
        .info()
        .layout_res()
        .unwrap_or(script.info().play_res());
    for event in script.events().iter().take(64) {
        for now in [
            event.start,
            event.start.wrapping_add(event.duration / 2),
            event.end(),
        ] {
            let Ok(mut context) = Context::new(script, event, now, layout) else {
                continue;
            };
            let mut glyphs = Vec::new();
            while let Ok(Some(glyph)) = context.next_glyph() {
                if let Symbol::Drawing { text, .. } = glyph.symbol {
                    let _ = Drawing::parse(text, outline, &limits);
                }
                glyphs.push(KaraokeGlyph {
                    starts_new_run: glyphs.is_empty() || glyph.karaoke.timing != 0,
                    mark: glyph.karaoke,
                    skip: false,
                    x: 64 * glyphs.len() as i32,
                    advance: 64,
                    rotate_z: glyph.state.rotate_z,
                });
            }
            if let Some(clip) = context.state().vector_clip {
                let _ = Drawing::parse(clip.drawing, outline, &limits);
            }
            for syllable in Syllables::new(&glyphs, now.wrapping_sub(event.start)) {
                for glyph in &glyphs[syllable.glyphs.clone()] {
                    std::hint::black_box(syllable.timing(glyph));
                }
            }
        }
    }
}

pub fn exercise(data: &[u8], limits: Limits) {
    let budget = Budget::new(16 << 20).expect("budget");
    let mut outline = Outline::new(&budget);
    let options = Options {
        limits,
        extract_fonts: true,
        style_overrides: &OVERRIDES,
    };
    if let Ok(script) = Script::parse(&budget, data, &options) {
        evaluate(&script, limits, &mut outline);
    }
    // The same bytes as a stream: the first half as codec private, every
    // line of the rest as a packet.
    if let Ok(mut script) = Script::new(&budget, &options) {
        let half = data.len() / 2;
        let _ = script.process_codec_private(&data[..half]);
        for (i, line) in data[half..].split(|&c| c == b'\n').enumerate().take(256) {
            let _ = script.process_chunk(line, i as i64 * 100, 150);
            if i % 50 == 49 {
                script.prune_events(i as i64 * 50);
            }
            if i == 100 {
                script.set_check_read_order(false);
            }
        }
        let _ = script.process_styles(&data[..half]);
        evaluate(&script, limits, &mut outline);
        script.flush_events();
    }
    // The whole input as one override block and one drawing.
    let _ = Tags::new(data, 0..data.len()).take(4096).count();
    let _ = Drawing::parse(data, &mut outline, &limits);
    // A budget far too small for the input.
    if let Ok(small) = Budget::new(16 << 10) {
        let _ = Script::parse(&small, data, &options);
    }
}
