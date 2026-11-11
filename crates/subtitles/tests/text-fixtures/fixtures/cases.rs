// Generated only from original authored components by author.py.
use super::*;
pub(super) static ALL: &[Case] = &[
    Case {
        id: "tx3g-basic",
        format: Format::Tx3g,
        encoding: Encoding::Tx3gInBand,
        payload: include_bytes!("input/tx3g-basic.tx3g"),
        codec_id: Some(include_bytes!("config/tx3g-basic.codec_id")),
        start: Some((1_i128, 1000_u128)),
        duration: Some((1001_i128, 1000_u128)),
        events: &[ExpectedEvent {
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
            raw: (0, 21),
            text: (0, 21),
            encoding: Encoding::Opaque,
            ..EMPTY_EVENT
        }],
        ..EMPTY
    },
];
