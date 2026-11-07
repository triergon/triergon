// Generated only from original authored components by author.py.
use super::*;
pub(super) static ALL: &[Case] = &[
    Case {
        id: "srt-basic",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-basic.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 51),
            text: (32, 51),
            decoded: Some(include_bytes!("expected/srt-basic.0.utf8")),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (0, 1),
            }),
            start: Some((1001_i128, 1000_u128)),
            end: Some((2509_i128, 1000_u128)),
            duration: Some((377_i128, 250_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-crlf-multiline",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-crlf-multiline.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 65),
            text: (35, 65),
            decoded: Some(include_bytes!("expected/srt-crlf-multiline.0.utf8")),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (0, 2),
            }),
            start: Some((0_i128, 1_u128)),
            end: Some((3_i128, 1_u128)),
            duration: Some((3_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-unicode-no-normalization",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-unicode-no-normalization.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 61),
            text: (32, 61),
            decoded: Some(include_bytes!(
                "expected/srt-unicode-no-normalization.0.utf8"
            )),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (0, 1),
            }),
            start: Some((3_i128, 1000_u128)),
            end: Some((999_i128, 1000_u128)),
            duration: Some((249_i128, 250_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-overlap-order",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-overlap-order.srt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (0, 50),
                text: (32, 50),
                decoded: Some(include_bytes!("expected/srt-overlap-order.0.utf8")),
                encoding: Encoding::Utf8,
                identifier: Some(ExpectedField {
                    owner: FieldOwner::Payload,
                    range: (0, 1),
                }),
                start: Some((5_i128, 1_u128)),
                end: Some((9_i128, 1_u128)),
                duration: Some((4_i128, 1_u128)),
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (52, 112),
                text: (84, 112),
                decoded: Some(include_bytes!("expected/srt-overlap-order.1.utf8")),
                encoding: Encoding::Utf8,
                identifier: Some(ExpectedField {
                    owner: FieldOwner::Payload,
                    range: (52, 53),
                }),
                start: Some((5_i128, 1_u128)),
                end: Some((6_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "srt-wide-hours-profile",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-wide-hours-profile.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 47),
            text: (34, 47),
            decoded: Some(include_bytes!("expected/srt-wide-hours-profile.0.utf8")),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (0, 1),
            }),
            start: Some((360000001_i128, 1000_u128)),
            end: Some((180000501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-utf8-bom",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-utf8-bom.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (3, 59),
            text: (35, 59),
            decoded: Some(include_bytes!("expected/srt-utf8-bom.0.utf8")),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (3, 4),
            }),
            start: Some((3_i128, 250_u128)),
            end: Some((253_i128, 250_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-utf16be-bom",
        format: Format::SubRip,
        encoding: Encoding::Utf16Bom,
        payload: include_bytes!("input/srt-utf16be-bom.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (2, 80),
            text: (66, 80),
            decoded: Some(include_bytes!("expected/srt-utf16be-bom.0.utf8")),
            encoding: Encoding::Utf16Be,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (2, 4),
            }),
            start: Some((3_i128, 250_u128)),
            end: Some((253_i128, 250_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-utf16le-bom",
        format: Format::SubRip,
        encoding: Encoding::Utf16Bom,
        payload: include_bytes!("input/srt-utf16le-bom.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (2, 102),
            text: (66, 102),
            decoded: Some(include_bytes!("expected/srt-utf16le-bom.0.utf8")),
            encoding: Encoding::Utf16Le,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (2, 4),
            }),
            start: Some((3_i128, 250_u128)),
            end: Some((253_i128, 250_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-explicit-utf16be",
        format: Format::SubRip,
        encoding: Encoding::Utf16Be,
        payload: include_bytes!("input/srt-explicit-utf16be.srt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 102),
            text: (64, 102),
            decoded: Some(include_bytes!("expected/srt-explicit-utf16be.0.utf8")),
            encoding: Encoding::Utf16Be,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Payload,
                range: (0, 2),
            }),
            start: Some((3_i128, 250_u128)),
            end: Some((253_i128, 250_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "srt-end-before-start",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-end-before-start.srt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "srt-sequence-missing",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-sequence-missing.srt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "srt-fraction-width",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-fraction-width.srt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "srt-overlong-utf8",
        format: Format::SubRip,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/srt-overlong-utf8.srt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "srt-lone-surrogate",
        format: Format::SubRip,
        encoding: Encoding::Utf16Bom,
        payload: include_bytes!("input/srt-lone-surrogate.srt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "vtt-short-timestamps",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-short-timestamps.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 47),
                text: (32, 47),
                decoded: Some(include_bytes!("expected/vtt-short-timestamps.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((5_i128, 4_u128)),
                end: Some((11_i128, 4_u128)),
                duration: Some((3_i128, 2_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-id-settings",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-id-settings.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 117),
                text: (97, 117),
                decoded: Some(include_bytes!("expected/vtt-id-settings.1.utf8")),
                encoding: Encoding::Utf8,
                identifier: Some(ExpectedField {
                    owner: FieldOwner::Payload,
                    range: (8, 13),
                }),
                settings: Some(ExpectedField {
                    owner: FieldOwner::Payload,
                    range: (44, 96),
                }),
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-crlf-multiline",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-crlf-multiline.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (10, 45),
                text: (41, 45),
                decoded: Some(include_bytes!("expected/vtt-crlf-multiline.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-cr-lines",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-cr-lines.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 48),
                text: (38, 48),
                decoded: Some(include_bytes!("expected/vtt-cr-lines.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-utf8-bom",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-utf8-bom.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 9),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (11, 58),
                text: (41, 58),
                decoded: Some(include_bytes!("expected/vtt-utf8-bom.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-payload-markup-exact",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-payload-markup-exact.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 91),
                text: (38, 91),
                decoded: Some(include_bytes!("expected/vtt-payload-markup-exact.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-metadata-order",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-metadata-order.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Note,
                raw: (8, 47),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Style,
                raw: (49, 77),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Region,
                raw: (79, 109),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (111, 161),
                text: (151, 161),
                decoded: Some(include_bytes!("expected/vtt-metadata-order.4.utf8")),
                encoding: Encoding::Utf8,
                settings: Some(ExpectedField {
                    owner: FieldOwner::Payload,
                    range: (141, 150),
                }),
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-note-between-cues",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-note-between-cues.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 44),
                text: (38, 44),
                decoded: Some(include_bytes!("expected/vtt-note-between-cues.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Note,
                raw: (46, 63),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (65, 100),
                text: (95, 100),
                decoded: Some(include_bytes!("expected/vtt-note-between-cues.3.utf8")),
                encoding: Encoding::Utf8,
                start: Some((1_i128, 1_u128)),
                end: Some((2_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-tab-separators",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-tab-separators.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 64),
                text: (38, 64),
                decoded: Some(include_bytes!("expected/vtt-tab-separators.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((1_i128, 1000_u128)),
                end: Some((999_i128, 1000_u128)),
                duration: Some((499_i128, 500_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-wide-hours",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-wide-hours.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 53),
                text: (40, 53),
                decoded: Some(include_bytes!("expected/vtt-wide-hours.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((360000001_i128, 1000_u128)),
                end: Some((180000501_i128, 500_u128)),
                duration: Some((1001_i128, 1000_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-overlap-order",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-overlap-order.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 6),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (8, 51),
                text: (38, 51),
                decoded: Some(include_bytes!("expected/vtt-overlap-order.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((3_i128, 1_u128)),
                duration: Some((3_i128, 1_u128)),
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (53, 97),
                text: (83, 97),
                decoded: Some(include_bytes!("expected/vtt-overlap-order.2.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((2_i128, 1_u128)),
                duration: Some((2_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-header-annotation",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-header-annotation.vtt"),
        events: &[
            ExpectedEvent {
                kind: ExpectedKind::Header,
                raw: (0, 30),
                text: (0, 0),
                encoding: Encoding::Utf8,
                ..EMPTY_EVENT
            },
            ExpectedEvent {
                kind: ExpectedKind::Cue,
                raw: (32, 79),
                text: (62, 79),
                decoded: Some(include_bytes!("expected/vtt-header-annotation.1.utf8")),
                encoding: Encoding::Utf8,
                start: Some((0_i128, 1_u128)),
                end: Some((1_i128, 1_u128)),
                duration: Some((1_i128, 1_u128)),
                ..EMPTY_EVENT
            },
        ],
        ..EMPTY
    },
    Case {
        id: "vtt-empty-document",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-empty-document.vtt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Header,
            raw: (0, 6),
            text: (0, 0),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "vtt-bad-signature",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-bad-signature.vtt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "vtt-utf16-refused",
        format: Format::WebVtt,
        encoding: Encoding::Utf16Be,
        payload: include_bytes!("input/vtt-utf16-refused.vtt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "vtt-zero-interval-strict",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-zero-interval-strict.vtt"),
        error: Some("Invalid"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Header,
            raw: (0, 6),
            text: (0, 0),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "vtt-surrogate-utf8",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-surrogate-utf8.vtt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "vtt-missing-fraction",
        format: Format::WebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/vtt-missing-fraction.vtt"),
        error: Some("Invalid"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Header,
            raw: (0, 6),
            text: (0, 0),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-basic",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-basic.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-basic.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 10),
            text: (2, 10),
            decoded: Some(include_bytes!("expected/tx3g-basic.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-byte-not-character-count",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-byte-not-character-count.tx3g"),
        codec_id: Some(include_bytes!(
            "config/tx3g-byte-not-character-count.codec_id"
        )),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 9),
            text: (2, 9),
            decoded: Some(include_bytes!(
                "expected/tx3g-byte-not-character-count.0.utf8"
            )),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-utf16be-bom",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-utf16be-bom.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-utf16be-bom.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 12),
            text: (2, 12),
            decoded: Some(include_bytes!("expected/tx3g-utf16be-bom.0.utf8")),
            encoding: Encoding::Utf16Be,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-empty-sample",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-empty-sample.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-empty-sample.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 2),
            text: (2, 2),
            decoded: Some(include_bytes!("expected/tx3g-empty-sample.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-embedded-nul",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-embedded-nul.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-embedded-nul.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 5),
            text: (2, 5),
            decoded: Some(include_bytes!("expected/tx3g-embedded-nul.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-line-codepoints-exact",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-line-codepoints-exact.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-line-codepoints-exact.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 10),
            text: (2, 10),
            decoded: Some(include_bytes!("expected/tx3g-line-codepoints-exact.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-style-highlight-raw",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-style-highlight-raw.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-style-highlight-raw.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 51),
            text: (2, 5),
            decoded: Some(include_bytes!("expected/tx3g-style-highlight-raw.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            modifiers: &[
                ([115, 116, 121, 108], (5, 27)),
                ([104, 108, 105, 116], (27, 39)),
                ([104, 99, 108, 114], (39, 51)),
            ],
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-unknown-modifier",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-unknown-modifier.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-unknown-modifier.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 19),
            text: (2, 3),
            decoded: Some(include_bytes!("expected/tx3g-unknown-modifier.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            modifiers: &[([122, 90, 48, 57], (3, 19))],
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-modifier-order",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-modifier-order.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-modifier-order.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 23),
            text: (2, 5),
            decoded: Some(include_bytes!("expected/tx3g-modifier-order.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            modifiers: &[
                ([116, 119, 114, 112], (5, 14)),
                ([116, 119, 114, 112], (14, 23)),
            ],
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-empty-with-modifier",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-empty-with-modifier.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-empty-with-modifier.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 18),
            text: (2, 2),
            decoded: Some(include_bytes!("expected/tx3g-empty-with-modifier.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            modifiers: &[([116, 98, 111, 120], (2, 18))],
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-u16-byte-count-boundary",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-u16-byte-count-boundary.tx3g"),
        codec_id: Some(include_bytes!(
            "config/tx3g-u16-byte-count-boundary.codec_id"
        )),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 65537),
            text: (2, 65537),
            decoded: Some(include_bytes!(
                "expected/tx3g-u16-byte-count-boundary.0.utf8"
            )),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 1000_u128)),
            end: Some((501_i128, 500_u128)),
            duration: Some((1001_i128, 1000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "tx3g-missing-length",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-missing-length.tx3g"),
        error: Some("Truncated"),
        ..EMPTY
    },
    Case {
        id: "tx3g-text-overrun",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-text-overrun.tx3g"),
        error: Some("Truncated"),
        ..EMPTY
    },
    Case {
        id: "tx3g-utf16-odd",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-utf16-odd.tx3g"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "tx3g-lone-surrogate",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-lone-surrogate.tx3g"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "tx3g-short-box",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-short-box.tx3g"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "tx3g-box-overrun",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-box-overrun.tx3g"),
        error: Some("Truncated"),
        ..EMPTY
    },
    Case {
        id: "tx3g-extended-box-gap",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-extended-box-gap.tx3g"),
        error: Some("Unsupported:tx3g extended box size"),
        ..EMPTY
    },
    Case {
        id: "utf8-packet-markup",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-markup.txt"),
        codec_id: Some(include_bytes!("config/utf8-packet-markup.codec_id")),
        start: Some((1_i128, 3_u128)),
        duration: Some((2_i128, 3_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 21),
            text: (0, 21),
            decoded: Some(include_bytes!("expected/utf8-packet-markup.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 3_u128)),
            end: Some((1_i128, 1_u128)),
            duration: Some((2_i128, 3_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-unknown-time",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-unknown-time.txt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 7),
            text: (0, 7),
            decoded: Some(include_bytes!("expected/utf8-packet-unknown-time.0.utf8")),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-known-zero-duration",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-known-zero-duration.txt"),
        start: Some((0_i128, 1_u128)),
        duration: Some((0_i128, 1_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 4),
            text: (0, 4),
            decoded: Some(include_bytes!(
                "expected/utf8-packet-known-zero-duration.0.utf8"
            )),
            encoding: Encoding::Utf8,
            start: Some((0_i128, 1_u128)),
            end: Some((0_i128, 1_u128)),
            duration: Some((0_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-negative-start",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-negative-start.txt"),
        start: Some((-5_i128, 7_u128)),
        duration: Some((1_i128, 3_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 4),
            text: (0, 4),
            decoded: Some(include_bytes!("expected/utf8-packet-negative-start.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((-5_i128, 7_u128)),
            end: Some((-8_i128, 21_u128)),
            duration: Some((1_i128, 3_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-raw-clock",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-raw-clock.txt"),
        start: Some((1_i128, 30000_u128)),
        duration: Some((1001_i128, 90000_u128)),
        clock: Some(RawClock {
            numerator: 2,
            denominator: 180000,
            pts_ticks: Some(3),
            dts_ticks: Some(-6),
            duration_ticks: Some(1001),
        }),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 5),
            text: (0, 5),
            decoded: Some(include_bytes!("expected/utf8-packet-raw-clock.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((1_i128, 30000_u128)),
            end: Some((251_i128, 22500_u128)),
            duration: Some((1001_i128, 90000_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-bom-nul-content",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-bom-nul-content.txt"),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 6),
            text: (0, 6),
            decoded: Some(include_bytes!(
                "expected/utf8-packet-bom-nul-content.0.utf8"
            )),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "utf8-packet-too-large-scalar",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-too-large-scalar.txt"),
        error: Some("Invalid"),
        ..EMPTY
    },
    Case {
        id: "utf8-packet-input-limit",
        format: Format::Utf8Packet,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/utf8-packet-input-limit.txt"),
        input_limit: Some(3),
        error: Some("Limit"),
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-no-addition",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-no-addition.block"),
        codec_id: Some(include_bytes!("config/mkv-vtt-no-addition.codec_id")),
        codec_private: Some(include_bytes!("config/mkv-vtt-no-addition.codec_private")),
        start: Some((5_i128, 2_u128)),
        duration: Some((3_i128, 4_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 11),
            text: (0, 11),
            decoded: Some(include_bytes!("expected/mkv-vtt-no-addition.0.utf8")),
            encoding: Encoding::Utf8,
            start: Some((5_i128, 2_u128)),
            end: Some((13_i128, 4_u128)),
            duration: Some((3_i128, 4_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-settings-id-comments",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-settings-id-comments.block"),
        codec_private: Some(include_bytes!(
            "config/mkv-vtt-settings-id-comments.codec_private"
        )),
        addition: Some((
            1,
            include_bytes!("addition/mkv-vtt-settings-id-comments.bin"),
        )),
        start: Some((1_i128, 1_u128)),
        duration: Some((1_i128, 2_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 4),
            text: (0, 4),
            decoded: Some(include_bytes!(
                "expected/mkv-vtt-settings-id-comments.0.utf8"
            )),
            encoding: Encoding::Utf8,
            identifier: Some(ExpectedField {
                owner: FieldOwner::Addition,
                range: (12, 24),
            }),
            settings: Some(ExpectedField {
                owner: FieldOwner::Addition,
                range: (0, 11),
            }),
            comments: Some(ExpectedField {
                owner: FieldOwner::Addition,
                range: (25, 51),
            }),
            start: Some((1_i128, 1_u128)),
            end: Some((3_i128, 2_u128)),
            duration: Some((1_i128, 2_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-present-empty-fields",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-present-empty-fields.block"),
        codec_private: Some(include_bytes!(
            "config/mkv-vtt-present-empty-fields.codec_private"
        )),
        addition: Some((
            1,
            include_bytes!("addition/mkv-vtt-present-empty-fields.bin"),
        )),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 12),
            text: (0, 12),
            decoded: Some(include_bytes!(
                "expected/mkv-vtt-present-empty-fields.0.utf8"
            )),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-relative-inline-time",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-relative-inline-time.block"),
        start: Some((10_i128, 1_u128)),
        duration: Some((1_i128, 1_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 19),
            text: (0, 19),
            decoded: Some(include_bytes!(
                "expected/mkv-vtt-relative-inline-time.0.utf8"
            )),
            encoding: Encoding::Utf8,
            start: Some((10_i128, 1_u128)),
            end: Some((11_i128, 1_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-unknown-addition-opaque",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-unknown-addition-opaque.block"),
        addition: Some((
            19,
            include_bytes!("addition/mkv-vtt-unknown-addition-opaque.bin"),
        )),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 6),
            text: (0, 6),
            decoded: Some(include_bytes!(
                "expected/mkv-vtt-unknown-addition-opaque.0.utf8"
            )),
            encoding: Encoding::Utf8,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "mkv-vtt-truncated-addition",
        format: Format::MatroskaWebVtt,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/mkv-vtt-truncated-addition.block"),
        addition: Some((1, include_bytes!("addition/mkv-vtt-truncated-addition.bin"))),
        error: Some("Truncated"),
        ..EMPTY
    },
    Case {
        id: "ass-raw-order-overrides",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-raw-order-overrides.ass-packet"),
        codec_id: Some(include_bytes!("config/ass-raw-order-overrides.codec_id")),
        charset_label: Some(include_bytes!(
            "config/ass-raw-order-overrides.charset_label"
        )),
        start: Some((0_i128, 1_u128)),
        duration: Some((1_i128, 1_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 59),
            text: (0, 59),
            encoding: Encoding::Opaque,
            start: Some((0_i128, 1_u128)),
            end: Some((1_i128, 1_u128)),
            duration: Some((1_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-ssa-empty-layer",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-ssa-empty-layer.ass-packet"),
        codec_id: Some(include_bytes!("config/ass-ssa-empty-layer.codec_id")),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 38),
            text: (0, 38),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-private-verbatim",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-private-verbatim.ass-packet"),
        codec_private: Some(include_bytes!("config/ass-private-verbatim.codec_private")),
        charset_label: Some(include_bytes!("config/ass-private-verbatim.charset_label")),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 26),
            text: (0, 26),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-unknown-charset-opaque",
        format: Format::OpaqueAss,
        encoding: Encoding::Opaque,
        payload: include_bytes!("input/ass-unknown-charset-opaque.ass-packet"),
        charset_label: Some(include_bytes!(
            "config/ass-unknown-charset-opaque.charset_label"
        )),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 26),
            text: (0, 26),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-empty-not-clear",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-empty-not-clear.ass-packet"),
        start: Some((0_i128, 1_u128)),
        duration: Some((0_i128, 1_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 0),
            text: (0, 0),
            encoding: Encoding::Opaque,
            start: Some((0_i128, 1_u128)),
            end: Some((0_i128, 1_u128)),
            duration: Some((0_i128, 1_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-known-duration-unknown-start",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-known-duration-unknown-start.ass-packet"),
        duration: Some((1_i128, 3_u128)),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 31),
            text: (0, 31),
            encoding: Encoding::Opaque,
            duration: Some((1_i128, 3_u128)),
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-private-present-empty",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-private-present-empty.ass-packet"),
        codec_private: Some(include_bytes!(
            "config/ass-private-present-empty.codec_private"
        )),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 32),
            text: (0, 32),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
    Case {
        id: "ass-invalid-utf8-unvalidated",
        format: Format::OpaqueAss,
        encoding: Encoding::Utf8,
        payload: include_bytes!("input/ass-invalid-utf8-unvalidated.ass-packet"),
        charset_label: Some(include_bytes!(
            "config/ass-invalid-utf8-unvalidated.charset_label"
        )),
        events: &[ExpectedEvent {
            kind: ExpectedKind::Cue,
            raw: (0, 21),
            text: (0, 21),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
];
