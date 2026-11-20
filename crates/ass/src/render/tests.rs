//! Final image work on frames whose drawings and composites are already cached.

use super::{Change, Fonts, ImageKind, Renderer, Script, Settings, Size};
use crate::Options;
use triergon_core::{Budget, Error, WorkBudget};

/// Returns the warm frame's charge and the work of its final image passes.
fn warm_image_work(side: u32, effect: &str) -> (u64, u64) {
    let budget = Budget::new(16 << 20).unwrap();
    let (_, font_data) =
        include_str!("../../../../harness/fixtures/ass-render/decorations-orientation.ass")
            .split_once("[Fonts]")
            .unwrap();
    let settings = Settings::new(Size::new(320, 320));
    let mut renderer = Renderer::new(&budget, &settings, &Fonts::default()).unwrap();
    let (style, mask) = match effect {
        "background" => (4, String::new()),
        clip => (1, format!(r"\{clip}(m 096 096 l 176 096 176 176 096 176)")),
    };
    let text = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 320\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, BorderStyle, Outline, Shadow, Alignment\n\
         Style: Default,Triergon Orientation,20,1,0,0,7\nStyle: Effect,Triergon Orientation,20,{style},0,0,7\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n\
         Dialogue: 0,0:00:00.00,0:00:01.00,Default,{{\\pos(5,5)}}A\n\
         Dialogue: 0,0:00:01.00,0:00:02.00,Effect,{{\\pos(100,100)\\p1{mask}}}m 0 0 l {side:03} 0 {side:03} {side:03} 0 {side:03}\n[Fonts]{font_data}"
    );
    let mut script = Script::parse(
        &budget,
        text.as_bytes(),
        &Options {
            extract_fonts: true,
            ..Options::default()
        },
    )
    .unwrap();
    let mut work = WorkBudget::new(settings.work);
    renderer.render(&mut script, 1000).unwrap();
    assert_eq!(
        renderer.render_frame(&mut script, 1000, &mut work),
        Ok(Change::Content)
    );
    let cost = work.spent();
    let image = &renderer.last_frame[0];
    let row_units = image.width().div_ceil(16) as u64;
    let area_units = row_units * image.height() as u64;
    let final_work = match effect {
        "background" => {
            assert_eq!(image.kind, ImageKind::Background);
            assert!(image.rows().flatten().all(|&v| v == 255));
            row_units
        }
        "clip" => {
            assert_eq!(image.kind, ImageKind::Fill);
            assert!(image.rows().flatten().any(|&v| v == 255));
            2 * area_units // copy and multiply the same overlap
        }
        _ => area_units, // copy the whole image; add the overlap in the caller
    };

    // A comparison with this plain frame distinguishes keeping it from
    // publishing even part of the refused frame. Both drawings stay cached.
    assert_eq!(renderer.render(&mut script, 0).unwrap().images.len(), 1);
    renderer.canvas.settings.work = cost - 1;
    assert!(matches!(
        renderer.render(&mut script, 1000),
        Err(Error::Policy(_))
    ));
    renderer.canvas.settings.work = settings.work;
    assert_eq!(
        renderer.render(&mut script, 0).unwrap().change,
        Change::None
    );
    renderer.canvas.settings.work = cost;
    assert_eq!(
        renderer.render(&mut script, 1000).unwrap().change,
        Change::Content
    );
    (cost, final_work)
}

#[test]
fn warm_clips_and_backgrounds_charge_the_pixels_they_touch() {
    // Same length event text and same number of drawing points and cache
    // lookups. Only the final pixel passes change with these dimensions.
    let costs = |side| {
        let (regular, both_passes) = warm_image_work(side, "clip");
        let (inverse, full_copy) = warm_image_work(side, "iclip");
        let (background, opaque_row) = warm_image_work(side, "background");
        [
            (regular, both_passes),
            (inverse, full_copy + both_passes / 2),
            (background, opaque_row),
        ]
    };
    for ((small, small_pixels), (large, large_pixels)) in costs(32).into_iter().zip(costs(128)) {
        assert!(large_pixels > small_pixels);
        assert_eq!(large - small, large_pixels - small_pixels);
    }
}

#[test]
fn a_shifted_background_keeps_one_row_and_checks_the_full_bitmap_limit() {
    use crate::Rgba;
    use triergon_raster::Rect;

    let budget = Budget::new(256).unwrap();
    let mut work = WorkBudget::new(2);
    let mut image = super::images::background(
        Rect::new(0, 0, 29, 4096),
        (29, 4096),
        Rgba(0),
        &budget,
        &mut work,
    )
    .unwrap()
    .unwrap();
    assert_eq!(work.spent(), 2);
    image.shift(-4093, 4096);
    assert_eq!((image.y, image.height()), (0, 3));
    assert!(image.rows().all(|r| r == [255; 29]));
    assert!(matches!(
        super::images::background(
            Rect::new(0, 0, 65536, 65536),
            (65536, 65536),
            Rgba(0),
            &budget,
            &mut work
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn consecutive_glyphs_follow_each_change_of_font_description() {
    let budget = Budget::new(32 << 20).unwrap();
    let (_, font_data) =
        include_str!("../../../../harness/fixtures/ass-render/decorations-orientation.ass")
            .split_once("[Fonts]")
            .unwrap();
    let text = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 320\nPlayResY: 320\n\
         [V4+ Styles]\nFormat: Name, Fontname, Fontsize, Bold, Italic\n\
         Style: Default,Triergon Orientation,20,0,0\n\
         [Events]\nFormat: Layer, Start, End, Style, Text\n\
         Dialogue: 0,0:00:00.00,0:00:01.00,Default,AA{{\\b1}}AA{{\\i1}}AA{{\\fn@Triergon Orientation}}AA{{\\fnOther}}AA{{\\r}}AA\n\
         [Fonts]{font_data}"
    );
    let mut script = Script::parse(
        &budget,
        text.as_bytes(),
        &Options {
            extract_fonts: true,
            ..Options::default()
        },
    )
    .unwrap();
    let fonts = Fonts {
        default_family: Some(b"Triergon Orientation".to_vec()),
        ..Fonts::default()
    };
    let mut renderer = Renderer::new(&budget, &Settings::new(Size::new(320, 320)), &fonts).unwrap();
    renderer.render(&mut script, 0).unwrap();
    let expected = [
        ("Triergon Orientation", 400, 0, false),
        ("Triergon Orientation", 700, 0, false),
        ("Triergon Orientation", 700, 100, false),
        ("Triergon Orientation", 700, 100, true),
        ("Other", 700, 100, false),
        ("Triergon Orientation", 400, 0, false),
    ];
    let chars = renderer.pipeline.text.chars();
    assert_eq!(chars.len(), 2 * expected.len());
    for (pair, (family, weight, slant, vertical)) in chars.chunks_exact(2).zip(expected) {
        assert_eq!(pair[0].font, pair[1].font);
        let font = renderer.pipeline.library.get(pair[0].font);
        assert_eq!(
            (font.family(), font.weight(), font.slant(), font.vertical()),
            (family.as_bytes(), weight, slant, vertical)
        );
    }
    assert_eq!(chars[0].font, chars[10].font);
}
