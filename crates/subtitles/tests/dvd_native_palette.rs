use triergon_subtitles::dvd::{NativePaletteOrigin, native_d32_palette};

#[test]
fn supplied_raw_colours_and_transparent_rgb_survive_the_native_view() {
    let rgb = std::array::from_fn(|i| 0xa000_0000 | (i as u32 * 0x0001_0203));
    let view = native_d32_palette(Some(&rgb), 0xf731, 0x084f);
    assert_eq!(view.source_rgb(), Some(&rgb));
    assert_eq!((view.palette_word(), view.alpha_word()), (0xf731, 0x084f));
    assert_eq!(
        view.argb(),
        &[0xff01_0203, 0x4403_0609, 0x8807_0e15, 0x000f_1e2d]
    );
    assert_eq!(
        view.origins()[3],
        NativePaletteOrigin::Supplied {
            selector: 15,
            raw_rgb: 0xa00f_1e2d
        }
    );
    assert_eq!(rgb[15], 0xa00f_1e2d);
}

#[test]
fn absent_palette_with_four_classes_matches_the_captured_receiver_greys() {
    let view = native_d32_palette(None, 0x3210, 0xffff);
    assert_eq!(
        view.argb(),
        &[0xff00_0000, 0xff54_5454, 0xffa9_a9a9, 0xfffe_fefe]
    );
    assert_eq!(view.source_rgb(), None);
    assert_eq!(
        view.origins()[3],
        NativePaletteOrigin::ReceiverGray {
            class: 3,
            classes: 4,
            level: 255
        }
    );
}

#[test]
fn one_two_and_three_opaque_classes_preserve_their_receiver_levels() {
    let cases = [
        (0x3210, 0x000f, [0xfffe_fefe, 0, 0, 0]),
        (0x3210, 0x00ff, [0xff00_0000, 0xfffe_fefe, 0, 0]),
        (0x3210, 0x0fff, [0xff00_0000, 0xff7f_7f7f, 0xfffe_fefe, 0]),
    ];
    for (word, alpha, expected) in cases {
        let view = native_d32_palette(None, word, alpha);
        assert_eq!(view.argb(), &expected);
        assert!(view.source_rgb().is_none());
    }
}

#[test]
fn selector_identity_order_and_per_slot_alpha_are_independent() {
    // LSB-first selectors are15,3,15,7: slot2 reuses class0 with another alpha.
    let view = native_d32_palette(None, 0x7f3f, 0x842f);
    assert_eq!(
        view.argb(),
        &[0xff00_0000, 0x227f_7f7f, 0x4400_0000, 0x88fe_fefe]
    );
    assert_eq!(view.origins()[0], view.origins()[2]);
    let permuted = native_d32_palette(None, 0xc414, 0x842f);
    assert_eq!(view.argb(), permuted.argb());
}

#[test]
fn transparent_slots_do_not_create_opaque_classes_or_fake_source_colours() {
    let view = native_d32_palette(None, 0x3210, 0xf0f0);
    assert_eq!(view.argb(), &[0, 0xff00_0000, 0, 0xfffe_fefe]);
    assert_eq!(view.origins()[0], NativePaletteOrigin::ReceiverTransparent);
    assert_eq!(view.origins()[2], NativePaletteOrigin::ReceiverTransparent);
    let empty = native_d32_palette(None, 0xfedc, 0);
    assert_eq!(empty.argb(), &[0; 4]);
    assert_eq!(
        empty.origins(),
        &[NativePaletteOrigin::ReceiverTransparent; 4]
    );
}

#[test]
fn shared_selector_still_preserves_each_alpha_nibble() {
    let view = native_d32_palette(None, 0xaaaa, 0xf821);
    assert_eq!(
        view.argb(),
        &[0x11fe_fefe, 0x22fe_fefe, 0x88fe_fefe, 0xfffe_fefe]
    );
    assert!(view.origins().iter().all(|origin| *origin
        == NativePaletteOrigin::ReceiverGray {
            class: 0,
            classes: 1,
            level: 255
        }));
}

#[test]
fn supplied_palette_and_automatic_palette_keep_distinct_zero_alpha_semantics() {
    let rgb = [0x0012_3456; 16];
    let supplied = native_d32_palette(Some(&rgb), 0, 0);
    let absent = native_d32_palette(None, 0, 0);
    assert_eq!(supplied.argb(), &[0x0012_3456; 4]);
    assert_eq!(absent.argb(), &[0; 4]);
    assert_ne!(supplied.origins(), absent.origins());
}

#[test]
fn returned_palette_keeps_all_original_supplied_words_after_the_input_drops() {
    let view = {
        let rgb = std::array::from_fn(|i| 0xf100_0000 | i as u32);
        native_d32_palette(Some(&rgb), 0x3210, 0xffff)
    };
    assert_eq!(view.source_rgb().unwrap()[15], 0xf100_000f);
    assert_eq!(
        view.argb(),
        &[0xff00_0000, 0xff00_0001, 0xff00_0002, 0xff00_0003]
    );
}
