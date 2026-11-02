//! Original primary-derived Fraction oracle vectors; parent executes Rust tests.
#![forbid(unsafe_code)]
use safe_media_subtitles::dvb::{ColourProjection, ColourSource};

#[test]
fn independent_projection_vectors_preserve_source_and_unrepaired_views() {
    let vectors = [
        (
            ColourSource::YCrCbT {
                y: 0,
                cr: 0,
                cb: 0,
                transparency: 0,
                full_resolution: true,
            },
            [0, 0, 0, 0],
            [0, 136, 0, 0],
            (0_u32, 1_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 235,
                cr: 128,
                cb: 128,
                transparency: 0,
                full_resolution: true,
            },
            [255, 255, 255, 255],
            [255, 255, 255, 255],
            (1_u32, 1_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 16,
                cr: 128,
                cb: 128,
                transparency: 0,
                full_resolution: true,
            },
            [0, 0, 0, 255],
            [0, 0, 0, 255],
            (1_u32, 1_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 235,
                cr: 128,
                cb: 128,
                transparency: 128,
                full_resolution: true,
            },
            [255, 255, 255, 128],
            [255, 255, 255, 127],
            (1_u32, 2_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 16,
                cr: 128,
                cb: 128,
                transparency: 255,
                full_resolution: true,
            },
            [0, 0, 0, 1],
            [0, 0, 0, 0],
            (1_u32, 256_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 4,
                cr: 8,
                cb: 8,
                transparency: 2,
                full_resolution: false,
            },
            [0, 0, 0, 128],
            [0, 0, 0, 127],
            (1_u32, 2_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 81,
                cr: 240,
                cb: 90,
                transparency: 0,
                full_resolution: true,
            },
            [254, 0, 0, 255],
            [254, 0, 0, 255],
            (1_u32, 1_u32),
        ),
        (
            ColourSource::YCrCbT {
                y: 58,
                cr: 8,
                cb: 8,
                transparency: 0,
                full_resolution: false,
            },
            [252, 252, 252, 255],
            [252, 252, 252, 255],
            (1_u32, 1_u32),
        ),
    ];
    for (raw, source, unrepaired, exact) in vectors {
        assert_eq!(raw.rgba_source_nearest().unwrap(), source);
        assert_eq!(raw.rgba(ColourProjection::SourceNearest8).unwrap(), source);
        assert_eq!(
            raw.rgba(ColourProjection::StudioUnrepaired8).unwrap(),
            unrepaired
        );
        let (numerator, denominator) = raw.opacity().unwrap();
        assert_eq!(
            u32::from(numerator) * exact.1,
            u32::from(denominator) * exact.0
        );
    }
}

#[test]
fn transparent_rgb_views_do_not_change_raw_source_facts_or_opacity() {
    let raw = ColourSource::YCrCbT {
        y: 0,
        cr: 0,
        cb: 0,
        transparency: 0,
        full_resolution: true,
    };
    let old = raw;
    assert_eq!(
        raw.rgba(ColourProjection::SourceNearest8).unwrap(),
        [0, 0, 0, 0]
    );
    assert_eq!(
        raw.rgba(ColourProjection::StudioUnrepaired8).unwrap(),
        [0, 136, 0, 0]
    );
    assert_eq!(raw, old);
    assert_eq!(raw.opacity().unwrap(), (0, 1));
    assert_ne!(
        ColourProjection::SourceNearest8.identifier(),
        ColourProjection::StudioUnrepaired8.identifier()
    );
}
