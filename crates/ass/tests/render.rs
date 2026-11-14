//! The renderer over a font built in memory: the order and kinds of the
//! images, collision handling across frames, clips and karaoke splits, and
//! mutated scripts that must render or fail with an error, never panic.
//! `cargo xtask ass compare` checks the pixels against libass.

#[path = "../../font/tests/support/mod.rs"]
mod fonts;

use triergon_ass::{
    CacheLimits, Change, Fonts, Image, ImageKind, Options, Renderer, Script, Settings, Size,
};
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
fn margins_come_from_the_last_reset_style() {
    // libass takes the margins an event leaves at 0 from the style in force
    // after its text: a trailing `\r` style moves the event, not its glyphs.
    let budget = Budget::new(64 << 20).unwrap();
    let text = "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, MarginV\n\
         Style: Default,Test Sans,40,10\nStyle: Raised,Test Sans,40,100\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n\
         Dialogue: 0,0:00:00.00,0:00:02.00,Default,A\n\
         Dialogue: 1,0:00:00.00,0:00:02.00,Default,A{\\rRaised}\n";
    let mut script = Script::parse(&budget, text.as_bytes(), &Options::default()).unwrap();
    let mut renderer = renderer(&budget);
    let images = renderer.render(&mut script, 1000).unwrap().images;
    let fills: Vec<i32> = images
        .iter()
        .filter(|i| i.kind == ImageKind::Fill)
        .map(|i| i.y)
        .collect();
    // 90 script pixels higher, at 2 frame pixels each.
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[0] - fills[1], 180);
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
    // A clip drawing without a contour empties every image as well; an
    // inverse clip leaves them whole. A square far away misses the text and
    // empties it too, unless it lies 125,000 frame pixels or more from the
    // origin, where libass cannot place the clip and does not apply it.
    const NEAR: &str = "m 50000 50000 l 50050 50000 50050 50050 50000 50050";
    const FAR: &str = "m 70000 70000 l 70050 70000 70050 70050 70000 70050";
    for (clip, drawing, empty) in [
        ("clip", "m 0 0 m 140 -24", true),
        ("iclip", "m 0 0 m 140 -24", false),
        ("clip", NEAR, true),
        ("clip", FAR, false),
    ] {
        let mut events = script(
            &budget,
            &format!(
                "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{{\\bord0\\shad0\\{clip}({drawing})}}ABC\n"
            ),
        );
        let images = renderer.render(&mut events, 500).unwrap().images;
        assert!(!images.is_empty());
        assert!(
            images.iter().all(|i| (i.width() == 0) == empty),
            "{clip}({drawing}): {images:?}"
        );
    }
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

/// The caches keep what the last frame shows however small their limits, so
/// a frame drawn again finds the same bitmaps, as libass's caches keep what
/// its last frame's images hold.
#[test]
fn caches_without_room_keep_what_the_last_frame_shows() {
    let budget = Budget::new(64 << 20).unwrap();
    let fonts = Fonts {
        attached: vec![test_sans(&budget)],
        ..Fonts::default()
    };
    let mut settings = Settings::new(Size::new(1280, 720));
    settings.storage = Some(Size::new(640, 360));
    settings.cache = CacheLimits::new(0, 0);
    let mut renderer = Renderer::new(&budget, &settings, &fonts).unwrap();
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\blur2}ABC\n",
    );
    let mut change = |now| renderer.render(&mut script, now).unwrap().change;
    assert_eq!(change(0), Change::Content);
    assert_eq!(change(500), Change::None);
    assert_eq!(change(1000), Change::None);
}

/// An image holds its pixels on its own: a clone may go on another thread
/// while the renderer draws on, and one kept after the renderer is gone
/// still shows its pixels.
#[test]
fn images_outlive_the_renderer_and_move_between_threads() {
    let budget = Budget::new(64 << 20).unwrap();
    let fonts = Fonts {
        attached: vec![test_sans(&budget)],
        ..Fonts::default()
    };
    let mut settings = Settings::new(Size::new(1280, 720));
    settings.storage = Some(Size::new(640, 360));
    settings.cache = CacheLimits::new(0, 0);
    let mut renderer = Renderer::new(&budget, &settings, &fonts).unwrap();
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:02.00,Default,{\\t(\\frz90)\\bord2}ABC\n",
    );
    let images = renderer.render(&mut script, 0).unwrap().images.to_vec();
    let rows = |images: &[Image]| -> Vec<Vec<u8>> {
        images
            .iter()
            .flat_map(Image::rows)
            .map(<[u8]>::to_vec)
            .collect()
    };
    let shown = rows(&images);
    assert!(shown.iter().flatten().any(|&a| a != 0));
    let elsewhere = images.clone();
    std::thread::spawn(move || drop(elsewhere)).join().unwrap();
    // Later frames draw anew, and with no room the caches cut whatever is
    // let go, the other thread's clones too.
    for now in [500, 1000, 1500] {
        renderer.render(&mut script, now).unwrap();
    }
    drop(renderer);
    assert_eq!(rows(&images), shown);
    // The pixels stay charged until the last image goes.
    let held = budget.used();
    drop(images);
    assert!(budget.used() < held);
}

/// The caches charge the budget for what they keep, and keep at most their
/// limits beyond what a renderer without room in them holds for the same
/// frame: the frame's working set and what its images show.
#[test]
fn caches_hold_at_most_their_limits_beyond_the_frame() {
    let limits = CacheLimits {
        bitmap_bytes: 64 << 10,
        composite_bytes: 32 << 10,
        ..CacheLimits::new(1000, 0)
    };
    let rendered = |limits: CacheLimits| {
        let budget = Budget::new(64 << 20).unwrap();
        let fonts = Fonts {
            attached: vec![test_sans(&budget)],
            ..Fonts::default()
        };
        let mut settings = Settings::new(Size::new(1280, 720));
        settings.storage = Some(Size::new(640, 360));
        settings.cache = limits;
        let mut renderer = Renderer::new(&budget, &settings, &fonts).unwrap();
        // Every frame turns the line further: new bitmaps and composites.
        let mut script = script(
            &budget,
            "Dialogue: 0,0:00:00.00,0:00:09.00,Default,{\\t(0,8000,\\frz360)\\blur1}ABC CAB\n",
        );
        (0..80)
            .map(|frame| {
                renderer.render(&mut script, frame * 100).unwrap();
                budget.used()
            })
            .collect::<Vec<_>>()
    };
    let bare = rendered(CacheLimits::new(1000, 0));
    let bound = limits.bitmap_bytes + limits.composite_bytes;
    let most = |kept: Vec<usize>| kept.iter().zip(&bare).map(|(k, b)| k - b).max();
    // Kept whole, the frames' bitmaps and composites outgrow the limits;
    // cut to them, they stay within them, a little scratch aside. libass
    // counts its records with each entry, more than a kept entry is charged.
    let unbounded = most(rendered(CacheLimits::new(1000, 24))).unwrap();
    assert!(unbounded > 2 * bound, "{unbounded} {bound}");
    let kept = most(rendered(limits)).unwrap();
    assert!(kept > bound / 4 && kept <= bound + 4096, "{kept} {bound}");
}

/// One renderer drawing two scripts by turns draws each as a renderer of
/// its own does: what one keeps in the caches the other does not find
/// under another's key.
#[test]
fn a_renderer_alternating_between_scripts_draws_each_as_alone() {
    let budget = Budget::new(64 << 20).unwrap();
    let first = "Dialogue: 0,0:00:00.00,0:00:04.00,Default,{\\blur1}ABC\n";
    let second = "Dialogue: 0,0:00:00.00,0:00:04.00,Default,{\\be2\\an7\\pos(20,20)}CAB\n";
    let frame = |renderer: &mut Renderer, script: &mut Script, now| {
        let frame = renderer.render(script, now).unwrap();
        let images: Vec<_> = frame
            .images
            .iter()
            .map(|i| {
                (
                    i.kind,
                    i.x,
                    i.y,
                    i.rows().flatten().copied().collect::<Vec<u8>>(),
                )
            })
            .collect();
        (frame.change, images)
    };
    let mut shared = renderer(&budget);
    let (mut a, mut b) = (script(&budget, first), script(&budget, second));
    let mut together = Vec::new();
    for now in [0, 500, 1000] {
        together.push(frame(&mut shared, &mut a, now).1);
        together.push(frame(&mut shared, &mut b, now).1);
    }
    let alone = |text| {
        let mut renderer = renderer(&budget);
        let mut script = script(&budget, text);
        [0, 500, 1000].map(|now| frame(&mut renderer, &mut script, now).1)
    };
    let (a_alone, b_alone) = (alone(first), alone(second));
    for (i, images) in together.iter().enumerate() {
        let expected = if i % 2 == 0 {
            &a_alone[i / 2]
        } else {
            &b_alone[i / 2]
        };
        assert_eq!(images, expected, "frame {i}");
    }
    // Each frame differs from the one before, the other script's.
    assert_eq!(frame(&mut shared, &mut a, 1500).0, Change::Content);
}

/// With no room in the caches a frame drawn again still finds what the
/// frame before shows, as libass's held entries stay: it costs no more than
/// the first, and a bound the first fits renders both.
#[test]
fn zero_limits_still_find_what_the_last_frame_shows() {
    let budget = Budget::new(256 << 20).unwrap();
    let events = coloured_lines(4, 30);
    let run = |work: u64, frames: &[i64]| {
        let mut settings = Settings::new(Size::new(1280, 720));
        settings.storage = Some(Size::new(640, 360));
        settings.cache = CacheLimits::new(0, 0);
        settings.work = work;
        let fonts = Fonts {
            attached: vec![test_sans(&budget)],
            ..Fonts::default()
        };
        let mut renderer = Renderer::new(&budget, &settings, &fonts).unwrap();
        let mut script = script(&budget, &events);
        frames
            .iter()
            .all(|&now| renderer.render(&mut script, now).is_ok())
    };
    let (mut refused, mut made) = (0, Settings::DEFAULT_WORK);
    while made - refused > 1 {
        let mid = refused + (made - refused) / 2;
        if run(mid, &[1000]) {
            made = mid;
        } else {
            refused = mid;
        }
    }
    assert!(run(made, &[1000, 1000, 1000]));
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

/// A frame refused past its work bound keeps no collision record: the next
/// frame places events as if it had not been asked for.
#[test]
fn a_refused_frame_leaves_collision_records_unchanged() {
    let budget = Budget::new(256 << 20).unwrap();
    // B arrives on A at 1 s and moves up; once A ends, B keeps the place it
    // was given. Layer 1 collides too, so its collision handling is the last
    // work of the frame, after layer 0's records are made. B's blurred line
    // costs more than the frame at 0 s, whose glyphs later frames find
    // drawn, so its frame is the one a bound can refuse.
    let events = "Dialogue: 0,0:00:00.00,0:00:02.00,Default,AB\n\
                  Dialogue: 0,0:00:01.00,0:00:09.00,Default,{\\blur6}BC DEFGH IJKLM\n\
                  Dialogue: 1,0:00:00.00,0:00:09.00,Default,CA\n\
                  Dialogue: 1,0:00:00.00,0:00:09.00,Default,AC\n";
    let run = |work: u64, times: &[i64]| {
        let mut settings = Settings::new(Size::new(1280, 720));
        settings.storage = Some(Size::new(640, 360));
        settings.work = work;
        let fonts = Fonts {
            attached: vec![test_sans(&budget)],
            ..Fonts::default()
        };
        let mut renderer = Renderer::new(&budget, &settings, &fonts).unwrap();
        let mut script = script(&budget, events);
        let mut frames = Vec::new();
        for &now in times {
            frames.push(renderer.render(&mut script, now).map(|frame| {
                frame
                    .images
                    .iter()
                    .map(|i| (i.kind, i.x, i.y))
                    .collect::<Vec<_>>()
            }));
        }
        frames
    };
    // The least bound under which the frame at 1 s is made after the one at 0.
    let (mut refused, mut made) = (0, Settings::DEFAULT_WORK);
    while made - refused > 1 {
        let mid = refused + (made - refused) / 2;
        if run(mid, &[0, 1000]).iter().all(Result::is_ok) {
            made = mid;
        } else {
            refused = mid;
        }
    }
    let frames = run(made - 1, &[0, 1000, 3000]);
    assert!(frames[0].is_ok());
    assert!(matches!(frames[1], Err(Error::Policy(_))));
    let skipped = run(made - 1, &[0, 3000]);
    assert_eq!(frames[2], skipped[1]);
    // Had the frame at 1 s been made, B would have kept its raised place.
    let kept = run(made, &[0, 1000, 3000]);
    assert_ne!(kept[2], skipped[1]);
}

/// The kind, place and size of each image a frame at `now` shows.
fn shown(
    renderer: &mut Renderer,
    script: &mut Script,
    now: i64,
) -> Vec<(ImageKind, i32, i32, usize, usize)> {
    let frame = renderer.render(script, now).unwrap();
    frame
        .images
        .iter()
        .map(|i| (i.kind, i.x, i.y, i.width(), i.height()))
        .collect()
}

#[test]
fn a_face_keeps_the_charmap_a_lookup_left_it_on() {
    let budget = Budget::new(64 << 20).unwrap();
    // "Test Sans" with a Macintosh Roman charmap after its Windows one, which
    // maps U+0091 to the bump and A to the composite.
    let mut tables = fonts::truetype_tables(400, 0x40, "Test Sans", "Regular");
    tables.push((*b"fpgm", vec![0xb0, 0x01]));
    let windows = fonts::cmap(&[(0x41, 1), (0x42, 2), (0x43, 3)])[12..].to_vec();
    let mac = fonts::cmap_format0(&[(0x41, 3), (0x91, 2)]);
    for table in &mut tables {
        if &table.0 == b"cmap" {
            table.1 = fonts::cmap_records(&[(3, 1, windows.clone()), (1, 0, mac.clone())]);
        }
    }
    let file = Bytes::copy_from_slice(&budget, &fonts::font(b"\0\x01\0\0", &tables)).unwrap();
    // Only the default font file draws the style's family.
    let fonts = Fonts {
        default_font: Some(file),
        ..Fonts::default()
    };
    let mut renderer = Renderer::new(&budget, &Settings::new(Size::new(640, 360)), &fonts).unwrap();
    let mut script = script(
        &budget,
        "Dialogue: 0,0:00:00.00,0:00:01.00,Default,A\n\
         Dialogue: 0,0:00:01.00,0:00:02.00,Default,\u{91}\n\
         Dialogue: 0,0:00:02.00,0:00:03.00,Default,A\n\
         Dialogue: 0,0:00:03.00,0:00:04.00,Default,C\n\
         Dialogue: 0,0:00:04.00,0:00:05.00,Default,A\n",
    );
    let square = shown(&mut renderer, &mut script, 0);
    // No Windows mapping: the face is tried through each charmap and keeps
    // the Macintosh one, through which A is the composite.
    assert!(!shown(&mut renderer, &mut script, 1000).is_empty());
    let composite = shown(&mut renderer, &mut script, 2000);
    assert_ne!(composite, square);
    // C maps through neither: the Windows charmap is selected again.
    assert_eq!(shown(&mut renderer, &mut script, 3000), composite);
    assert_eq!(shown(&mut renderer, &mut script, 4000), square);
}

#[test]
fn a_style_font_size_of_zero_draws_glyphs_unscaled() {
    let budget = Budget::new(64 << 20).unwrap();
    let mut renderer = renderer(&budget);
    // FreeType loads the glyphs in font units, which libass reads as 26.6
    // pixels, and the size no longer scales them to the frame: the square's
    // 200 units are 3.125 pixels tall, four rows.
    let text = "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\
                [V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Test Sans,0\n\
                [Events]\nFormat: Layer, Start, End, Style, Text\n\
                Dialogue: 0,0:00:00.00,0:00:01.00,Default,A\n";
    let mut script = Script::parse(&budget, text.as_bytes(), &Options::default()).unwrap();
    let images = shown(&mut renderer, &mut script, 0);
    let fill = images.iter().find(|i| i.0 == ImageKind::Fill).unwrap();
    assert_eq!(fill.4, 4, "{images:?}");
}

/// A script embedding `font` in its `[Fonts]` section, with one event in
/// the style of `family`.
fn embedding(budget: &Budget, family: &str, font: &[u8]) -> Script {
    let mut encoded = Vec::new();
    for group in font.chunks(3) {
        let mut value = 0_u32;
        for (i, &byte) in group.iter().enumerate() {
            value |= u32::from(byte) << (16 - 8 * i);
        }
        for i in 0..=group.len() {
            encoded.push(((value >> (18 - 6 * i)) & 63) as u8 + 33);
        }
    }
    let lines: Vec<String> = encoded
        .chunks(80)
        .map(|line| String::from_utf8(line.to_vec()).unwrap())
        .collect();
    let text = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 640\nPlayResY: 360\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,{family},40\n\
         [Fonts]\nfontname: {family}.ttf\n{}\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n\
         Dialogue: 0,0:00:00.00,0:00:02.00,Default,ABC\n",
        lines.join("\n")
    );
    let options = Options {
        extract_fonts: true,
        ..Options::default()
    };
    Script::parse(budget, text.as_bytes(), &options).unwrap()
}

#[test]
fn every_script_rendered_has_its_embedded_fonts_registered() {
    let budget = Budget::new(64 << 20).unwrap();
    let font = |family| {
        let mut tables = fonts::truetype_tables(400, 0x40, family, "Regular");
        tables.push((*b"fpgm", vec![0xb0, 0x01]));
        fonts::font(b"\0\x01\0\0", &tables)
    };
    let mut first = embedding(&budget, "Test Sans", &font("Test Sans"));
    let mut second = embedding(&budget, "Test Two", &font("Test Two"));
    assert_eq!((first.fonts().len(), second.fonts().len()), (1, 1));
    let mut renderer = Renderer::new(
        &budget,
        &Settings::new(Size::new(640, 360)),
        &Fonts::default(),
    )
    .unwrap();
    // The second script's font is registered though the first had as many,
    // and the first's stays selectable.
    for which in [0, 1, 0] {
        let script = if which == 0 { &mut first } else { &mut second };
        let images = renderer.render(script, 1000).unwrap().images;
        assert!(!images.is_empty(), "script {which}");
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
