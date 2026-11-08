//! The renderer over a font built in memory: the order and kinds of the
//! images, collision handling across frames, clips and karaoke splits, and
//! mutated scripts that must render or fail with an error, never panic.
//! `cargo xtask ass compare` checks the pixels against libass.

#[path = "../../font/tests/support/mod.rs"]
mod fonts;

use triergon_ass::{Change, Fonts, ImageKind, Options, Renderer, Script, Settings, Size};
use triergon_core::{Budget, Bytes, Error};

/// "Test Sans": A is a square, B a bump, C a composite of both. A font
/// program that leaves instruction control alone keeps FreeType from
/// auto-hinting its advances.
fn test_sans(budget: &Budget) -> Bytes {
    let mut tables = fonts::truetype_tables(400, 0x40, "Test Sans", "Regular");
    // PUSHB[0] 1: a program without effect.
    tables.push((*b"fpgm", vec![0xb0, 0x01]));
    let data = fonts::font(b"\0\x01\0\0", &tables);
    Bytes::copy_from_slice(budget, &data).unwrap()
}

fn script(budget: &Budget, events: &str) -> Script {
    let text = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, \
         OutlineColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, \
         MarginL, MarginR, MarginV\n\
         Style: Default,Test Sans,40,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,1,2,2,2,10,10,10\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n{events}"
    );
    Script::parse(budget, text.as_bytes(), &Options::default()).unwrap()
}

fn renderer(budget: &Budget) -> Renderer {
    let fonts = Fonts {
        attached: vec![test_sans(budget)],
        ..Fonts::default()
    };
    let mut settings = Settings::new(Size::new(1280, 720));
    settings.storage = Some(Size::new(640, 360));
    Renderer::new(budget, &settings, &fonts).unwrap()
}

#[test]
fn an_event_draws_its_shadows_then_borders_then_fills() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = renderer(&budget);
    let mut script = script(&budget, "Dialogue: 0,0:00:00.00,0:00:02.00,Default,ABC\n");
    let images = renderer.render(&mut script, 1000).unwrap().images;
    let kinds: Vec<ImageKind> = images.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [ImageKind::Shadow, ImageKind::Border, ImageKind::Fill]
    );
    for image in images {
        assert!(image.rows().flatten().any(|&a| a != 0), "{image:?}");
        assert!(
            image.y > 360 && image.y < 720,
            "bottom aligned: {}",
            image.y
        );
    }
    // Nothing is shown after the event ends.
    assert!(
        renderer
            .render(&mut script, 2000)
            .unwrap()
            .images
            .is_empty()
    );
}

#[test]
fn colliding_events_stack_and_keep_their_place() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = renderer(&budget);
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:04.00,Default,AB\n\
         Dialogue: 0,0:00:01.00,0:00:04.00,Default,CA\n",
    );
    let fill_tops = |renderer: &mut Renderer, script: &mut Script, now| -> Vec<i32> {
        let images = renderer.render(script, now).unwrap().images;
        images
            .iter()
            .filter(|i| i.kind == ImageKind::Fill)
            .map(|i| i.y)
            .collect()
    };
    let alone = fill_tops(&mut renderer, &mut script, 500);
    let stacked = fill_tops(&mut renderer, &mut script, 1500);
    assert_eq!(stacked.len(), 2);
    // The first keeps its place; the second moves up above it.
    assert_eq!(stacked[0], alone[0]);
    assert!(stacked[1] < stacked[0]);
    assert_eq!(fill_tops(&mut renderer, &mut script, 2500), stacked);
}

#[test]
fn clips_and_karaoke_split_images() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = renderer(&budget);
    let mut events = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\bord0\\shad0\\kf100}ABC\n",
    );
    // Halfway through the syllable the fill is split in two colours.
    let images = renderer.render(&mut events, 500).unwrap().images;
    let colours: Vec<u32> = images.iter().map(|i| i.colour.0).collect();
    assert_eq!(colours, [0xffff_ff00, 0xff00_0000]);
    let mut events = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\bord0\\shad0\\iclip(0,0,320,360)}ABC\n",
    );
    let images = renderer.render(&mut events, 500).unwrap().images;
    assert!(images.iter().all(|i| i.x >= 640), "{images:?}");
    let mut events = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\bord0\\shad0\\clip(m 0 0 l 1 0 1 1 0 1)}ABC\n",
    );
    let images = renderer.render(&mut events, 500).unwrap().images;
    // The clip misses the text: its image is kept, empty.
    assert!(images.iter().all(|i| i.width() == 0 && i.height() == 0));
}

#[test]
fn frames_report_what_changed_since_the_last() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = renderer(&budget);
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\move(100,100,300,100)}ABC\n\
         Dialogue: 0,0:00:00.00,0:00:04.00,Default,CAB\n",
    );
    let change = |renderer: &mut Renderer, script: &mut Script, now| {
        renderer.render(script, now).unwrap().change
    };
    assert_eq!(change(&mut renderer, &mut script, 0), Change::Content);
    assert_eq!(change(&mut renderer, &mut script, 0), Change::None);
    // 50 script pixels a frame: 100 frame pixels, the same bitmaps.
    assert_eq!(change(&mut renderer, &mut script, 500), Change::Positions);
    assert_eq!(change(&mut renderer, &mut script, 2500), Change::Content);
    assert_eq!(change(&mut renderer, &mut script, 3000), Change::None);
    // Without events nothing is rendered; the next frame is compared with
    // the last one that was.
    script.prune_events(5000);
    assert_eq!(change(&mut renderer, &mut script, 5000), Change::Content);
    let mut again = steady_line(&budget);
    assert_eq!(change(&mut renderer, &mut again, 1000), Change::None);
}

fn steady_line(budget: &Budget) -> Script {
    script(budget, "Dialogue: 0,0:00:00.00,0:00:04.00,Default,CAB\n")
}

#[test]
fn an_event_without_a_font_is_not_drawn() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = Renderer::new(
        &budget,
        &Settings::new(Size::new(640, 360)),
        &Fonts::default(),
    )
    .unwrap();
    let mut script = script(&budget, "Dialogue: 0,0:00:00.00,0:00:02.00,Default,ABC\n");
    assert!(renderer.render(&mut script, 0).unwrap().images.is_empty());
    assert!(
        Renderer::new(
            &budget,
            &Settings::new(Size::new(0, 360)),
            &Fonts::default()
        )
        .is_err()
    );
}

#[test]
fn an_exhausted_budget_is_an_error() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\blur20}ABC\n",
    );
    let small = Budget::new(256 << 10).unwrap();
    let mut renderer = renderer(&small);
    assert!(matches!(
        renderer.render(&mut script, 0),
        Err(Error::Limit { .. } | Error::Backpressure { .. })
    ));
}

/// `events` events of `\fe-1` text with a colour change at every glyph, so
/// each glyph is a run of its own shaped within the whole text, at their own
/// places so they do not collide.
fn coloured_lines(events: usize, glyphs: usize) -> String {
    let mut text = String::new();
    for event in 0..events {
        let y = 20 + event * 7 % 340;
        text.push_str(&format!(
            "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{{\\pos(320,{y})\\fe-1}}"
        ));
        for glyph in 0..glyphs {
            let letter = ['A', 'B', 'C'][glyph % 3];
            text.push_str(&format!("{{\\c&H{:06X}&}}{letter}", glyph * 0x0001_0103));
        }
        text.push('\n');
    }
    text
}

/// Whether a fresh renderer whose frames are bounded by `work` units renders
/// `events` at 1 s; a frame beyond the bound is refused as `Policy`.
fn renders_within(budget: &Budget, events: &str, work: u64) -> bool {
    let mut settings = Settings::new(Size::new(1280, 720));
    settings.storage = Some(Size::new(640, 360));
    settings.work = work;
    let fonts = Fonts {
        attached: vec![test_sans(budget)],
        ..Fonts::default()
    };
    let mut renderer = Renderer::new(budget, &settings, &fonts).unwrap();
    let mut script = script(budget, events);
    match renderer.render(&mut script, 1000) {
        Ok(frame) => {
            assert!(!frame.images.is_empty());
            true
        }
        Err(Error::Policy(_)) => false,
        Err(error) => panic!("{error:?}"),
    }
}

/// The work units a frame of `events` is charged: the least bound that
/// renders it.
fn frame_work(budget: &Budget, events: &str) -> u64 {
    let (mut refused, mut rendered) = (0, Settings::DEFAULT_WORK);
    assert!(renders_within(budget, events, rendered));
    while rendered - refused > 1 {
        let mid = refused + (rendered - refused) / 2;
        if renders_within(budget, events, mid) {
            rendered = mid;
        } else {
            refused = mid;
        }
    }
    rendered
}

#[test]
fn a_frame_is_bounded_by_its_work_setting_and_charged_linearly() {
    let budget = Budget::new(256 << 20).unwrap();
    let base = coloured_lines(4, 30);
    let work = frame_work(&budget, &base);
    // Every stage of the frame shares the bound: one unit less is refused.
    assert!(!renders_within(&budget, &base, work - 1));
    // Twice the events or twice the text cost about twice the work, not four
    // times: shaping a run reads only its own text and a few characters of
    // context, however long the text it is shaped within.
    let events = frame_work(&budget, &coloured_lines(8, 30));
    let longer = frame_work(&budget, &coloured_lines(4, 60));
    for (what, units) in [("events", events), ("glyphs", longer)] {
        let ratio = units as f64 / work as f64;
        assert!(
            (1.7..2.3).contains(&ratio),
            "{what}: {work} -> {units} units"
        );
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn mutated_events_render_or_fail_without_panicking() {
    const SYNTAX: &[u8] = b"{}()\\,.-0123456789mnlbspcABC tkfrNh&H";
    let events = "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\pos(320,180)\\frz30\\fax0.2\\blur2}AB\\NC\n\
                  Dialogue: 1,0:00:00.00,0:00:02.00,Default,{\\k20}A{\\kf30}B{\\ko20}C {\\p1}m 0 0 l 50 0 50 50{\\p0}\n\
                  Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\clip(1,m 0 0 l 300 0 300 300)\\be3\\xshad-3}ABC ABC ABC\n\
                  Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\an5\\fscx300\\fsp5\\u1\\s1\\3a&H80&}CAB\n";
    let budget = Budget::new(256 << 20).unwrap();
    let mut renderer = renderer(&budget);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..300 {
        let mut text = events.as_bytes().to_vec();
        for _ in 0..1 + rng.below(6) {
            let at = rng.below(text.len());
            if round % 2 == 0 {
                text[at] = SYNTAX[rng.below(SYNTAX.len())];
            } else {
                text.remove(at);
            }
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        let mut script = script(&budget, &text);
        for now in [0, 700, 1999] {
            let _ = renderer.render(&mut script, now);
        }
    }
}
