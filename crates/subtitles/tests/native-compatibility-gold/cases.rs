//! Frozen native C transcript bytes; no expected output is generated from this candidate.
pub struct FrozenCase {
    pub id: &'static str,
    pub input: &'static [u8],
    pub expected: &'static [u8],
    pub succeeded: bool,
}
pub const CASES: &[FrozenCase] = &[
    FrozenCase {
        id: "pgs-original140--old34-run-1-0",
        input: include_bytes!("pgs-original140--old34-run-1-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-1-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-1-1",
        input: include_bytes!("pgs-original140--old34-run-1-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-1-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-1-255",
        input: include_bytes!("pgs-original140--old34-run-1-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-1-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-2-0",
        input: include_bytes!("pgs-original140--old34-run-2-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-2-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-2-1",
        input: include_bytes!("pgs-original140--old34-run-2-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-2-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-2-255",
        input: include_bytes!("pgs-original140--old34-run-2-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-2-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-63-0",
        input: include_bytes!("pgs-original140--old34-run-63-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-63-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-63-1",
        input: include_bytes!("pgs-original140--old34-run-63-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-63-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-63-255",
        input: include_bytes!("pgs-original140--old34-run-63-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-63-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-64-0",
        input: include_bytes!("pgs-original140--old34-run-64-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-64-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-64-1",
        input: include_bytes!("pgs-original140--old34-run-64-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-64-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-64-255",
        input: include_bytes!("pgs-original140--old34-run-64-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-64-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-255-0",
        input: include_bytes!("pgs-original140--old34-run-255-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-255-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-255-1",
        input: include_bytes!("pgs-original140--old34-run-255-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-255-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-255-255",
        input: include_bytes!("pgs-original140--old34-run-255-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-255-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-256-0",
        input: include_bytes!("pgs-original140--old34-run-256-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-256-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-256-1",
        input: include_bytes!("pgs-original140--old34-run-256-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-256-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-256-255",
        input: include_bytes!("pgs-original140--old34-run-256-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-256-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-4096-0",
        input: include_bytes!("pgs-original140--old34-run-4096-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-4096-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-4096-1",
        input: include_bytes!("pgs-original140--old34-run-4096-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-4096-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-4096-255",
        input: include_bytes!("pgs-original140--old34-run-4096-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-4096-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-16383-0",
        input: include_bytes!("pgs-original140--old34-run-16383-0/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-16383-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-16383-1",
        input: include_bytes!("pgs-original140--old34-run-16383-1/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-16383-1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-16383-255",
        input: include_bytes!("pgs-original140--old34-run-16383-255/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-16383-255/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-mixed-two-rows",
        input: include_bytes!("pgs-original140--old34-mixed-two-rows/input.bin"),
        expected: include_bytes!("pgs-original140--old34-mixed-two-rows/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-forced",
        input: include_bytes!("pgs-original140--old34-forced/input.bin"),
        expected: include_bytes!("pgs-original140--old34-forced/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-cropped",
        input: include_bytes!("pgs-original140--old34-cropped/input.bin"),
        expected: include_bytes!("pgs-original140--old34-cropped/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-forced-and-cropped",
        input: include_bytes!("pgs-original140--old34-forced-and-cropped/input.bin"),
        expected: include_bytes!("pgs-original140--old34-forced-and-cropped/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-truncated-control",
        input: include_bytes!("pgs-original140--old34-truncated-control/input.bin"),
        expected: include_bytes!("pgs-original140--old34-truncated-control/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-truncated-long-colour",
        input: include_bytes!("pgs-original140--old34-truncated-long-colour/input.bin"),
        expected: include_bytes!("pgs-original140--old34-truncated-long-colour/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-short-line",
        input: include_bytes!("pgs-original140--old34-short-line/input.bin"),
        expected: include_bytes!("pgs-original140--old34-short-line/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-run-across-row",
        input: include_bytes!("pgs-original140--old34-run-across-row/input.bin"),
        expected: include_bytes!("pgs-original140--old34-run-across-row/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-zero-colour-run",
        input: include_bytes!("pgs-original140--old34-zero-colour-run/input.bin"),
        expected: include_bytes!("pgs-original140--old34-zero-colour-run/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--old34-trailing-rle",
        input: include_bytes!("pgs-original140--old34-trailing-rle/input.bin"),
        expected: include_bytes!("pgs-original140--old34-trailing-rle/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reuse-clear-resume",
        input: include_bytes!("pgs-original140--reuse-clear-resume/input.bin"),
        expected: include_bytes!("pgs-original140--reuse-clear-resume/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--partial-duplicate-palette",
        input: include_bytes!("pgs-original140--partial-duplicate-palette/input.bin"),
        expected: include_bytes!("pgs-original140--partial-duplicate-palette/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--same-palette-version-new-values",
        input: include_bytes!("pgs-original140--same-palette-version-new-values/input.bin"),
        expected: include_bytes!("pgs-original140--same-palette-version-new-values/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--empty-palette-version-update",
        input: include_bytes!("pgs-original140--empty-palette-version-update/input.bin"),
        expected: include_bytes!("pgs-original140--empty-palette-version-update/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-40-partial-palette",
        input: include_bytes!("pgs-original140--reset-state-40-partial-palette/input.bin"),
        expected: include_bytes!("pgs-original140--reset-state-40-partial-palette/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-40-missing-definitions",
        input: include_bytes!("pgs-original140--reset-state-40-missing-definitions/input.bin"),
        expected: include_bytes!(
            "pgs-original140--reset-state-40-missing-definitions/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-80-partial-palette",
        input: include_bytes!("pgs-original140--reset-state-80-partial-palette/input.bin"),
        expected: include_bytes!("pgs-original140--reset-state-80-partial-palette/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-80-missing-definitions",
        input: include_bytes!("pgs-original140--reset-state-80-missing-definitions/input.bin"),
        expected: include_bytes!(
            "pgs-original140--reset-state-80-missing-definitions/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-c0-partial-palette",
        input: include_bytes!("pgs-original140--reset-state-c0-partial-palette/input.bin"),
        expected: include_bytes!("pgs-original140--reset-state-c0-partial-palette/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--reset-state-c0-missing-definitions",
        input: include_bytes!("pgs-original140--reset-state-c0-missing-definitions/input.bin"),
        expected: include_bytes!(
            "pgs-original140--reset-state-c0-missing-definitions/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--window-crop-forced-real-pixels",
        input: include_bytes!("pgs-original140--window-crop-forced-real-pixels/input.bin"),
        expected: include_bytes!("pgs-original140--window-crop-forced-real-pixels/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--multiple-objects-wire-order",
        input: include_bytes!("pgs-original140--multiple-objects-wire-order/input.bin"),
        expected: include_bytes!("pgs-original140--multiple-objects-wire-order/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--forced-only-false",
        input: include_bytes!("pgs-original140--forced-only-false/input.bin"),
        expected: include_bytes!("pgs-original140--forced-only-false/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--forced-only-true",
        input: include_bytes!("pgs-original140--forced-only-true/input.bin"),
        expected: include_bytes!("pgs-original140--forced-only-true/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--duplicate-object-references",
        input: include_bytes!("pgs-original140--duplicate-object-references/input.bin"),
        expected: include_bytes!("pgs-original140--duplicate-object-references/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-00",
        input: include_bytes!("pgs-original140--every-ods-split-00/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-00/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-01",
        input: include_bytes!("pgs-original140--every-ods-split-01/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-01/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-02",
        input: include_bytes!("pgs-original140--every-ods-split-02/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-02/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-03",
        input: include_bytes!("pgs-original140--every-ods-split-03/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-03/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-04",
        input: include_bytes!("pgs-original140--every-ods-split-04/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-04/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-05",
        input: include_bytes!("pgs-original140--every-ods-split-05/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-05/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-06",
        input: include_bytes!("pgs-original140--every-ods-split-06/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-06/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-07",
        input: include_bytes!("pgs-original140--every-ods-split-07/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-07/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-08",
        input: include_bytes!("pgs-original140--every-ods-split-08/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-08/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--every-ods-split-09",
        input: include_bytes!("pgs-original140--every-ods-split-09/input.bin"),
        expected: include_bytes!("pgs-original140--every-ods-split-09/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--interleaved-object-fragments",
        input: include_bytes!("pgs-original140--interleaved-object-fragments/input.bin"),
        expected: include_bytes!("pgs-original140--interleaved-object-fragments/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--one-rle-byte-per-fragment",
        input: include_bytes!("pgs-original140--one-rle-byte-per-fragment/input.bin"),
        expected: include_bytes!("pgs-original140--one-rle-byte-per-fragment/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--object-replacement-version-0",
        input: include_bytes!("pgs-original140--object-replacement-version-0/input.bin"),
        expected: include_bytes!("pgs-original140--object-replacement-version-0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--object-replacement-version-2",
        input: include_bytes!("pgs-original140--object-replacement-version-2/input.bin"),
        expected: include_bytes!("pgs-original140--object-replacement-version-2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--matrix-height-480",
        input: include_bytes!("pgs-original140--matrix-height-480/input.bin"),
        expected: include_bytes!("pgs-original140--matrix-height-480/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--matrix-height-576",
        input: include_bytes!("pgs-original140--matrix-height-576/input.bin"),
        expected: include_bytes!("pgs-original140--matrix-height-576/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--matrix-height-577",
        input: include_bytes!("pgs-original140--matrix-height-577/input.bin"),
        expected: include_bytes!("pgs-original140--matrix-height-577/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--matrix-height-1080",
        input: include_bytes!("pgs-original140--matrix-height-1080/input.bin"),
        expected: include_bytes!("pgs-original140--matrix-height-1080/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--matrix-per-entry-at-update-time",
        input: include_bytes!("pgs-original140--matrix-per-entry-at-update-time/input.bin"),
        expected: include_bytes!("pgs-original140--matrix-per-entry-at-update-time/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-unknown",
        input: include_bytes!("pgs-original140--clock-unknown/input.bin"),
        expected: include_bytes!("pgs-original140--clock-unknown/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-zero",
        input: include_bytes!("pgs-original140--clock-zero/input.bin"),
        expected: include_bytes!("pgs-original140--clock-zero/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-one-tick",
        input: include_bytes!("pgs-original140--clock-one-tick/input.bin"),
        expected: include_bytes!("pgs-original140--clock-one-tick/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-negative",
        input: include_bytes!("pgs-original140--clock-negative/input.bin"),
        expected: include_bytes!("pgs-original140--clock-negative/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-large-exact",
        input: include_bytes!("pgs-original140--clock-large-exact/input.bin"),
        expected: include_bytes!("pgs-original140--clock-large-exact/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-prime-base",
        input: include_bytes!("pgs-original140--clock-prime-base/input.bin"),
        expected: include_bytes!("pgs-original140--clock-prime-base/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-native-unrepresentable-sentinel",
        input: include_bytes!("pgs-original140--clock-native-unrepresentable-sentinel/input.bin"),
        expected: include_bytes!(
            "pgs-original140--clock-native-unrepresentable-sentinel/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-native-unrepresentable-overflow",
        input: include_bytes!("pgs-original140--clock-native-unrepresentable-overflow/input.bin"),
        expected: include_bytes!(
            "pgs-original140--clock-native-unrepresentable-overflow/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-split-pcs-known-end-different",
        input: include_bytes!("pgs-original140--clock-split-pcs-known-end-different/input.bin"),
        expected: include_bytes!(
            "pgs-original140--clock-split-pcs-known-end-different/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-split-pcs-unknown-end-known",
        input: include_bytes!("pgs-original140--clock-split-pcs-unknown-end-known/input.bin"),
        expected: include_bytes!("pgs-original140--clock-split-pcs-unknown-end-known/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-split-pcs-known-end-unknown",
        input: include_bytes!("pgs-original140--clock-split-pcs-known-end-unknown/input.bin"),
        expected: include_bytes!("pgs-original140--clock-split-pcs-known-end-unknown/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--clock-split-both-unknown",
        input: include_bytes!("pgs-original140--clock-split-both-unknown/input.bin"),
        expected: include_bytes!("pgs-original140--clock-split-both-unknown/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--explicit-hard-context-reset",
        input: include_bytes!("pgs-original140--explicit-hard-context-reset/input.bin"),
        expected: include_bytes!("pgs-original140--explicit-hard-context-reset/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--normal-window-replacement",
        input: include_bytes!("pgs-original140--normal-window-replacement/input.bin"),
        expected: include_bytes!("pgs-original140--normal-window-replacement/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--unknown-segment-kept",
        input: include_bytes!("pgs-original140--unknown-segment-kept/input.bin"),
        expected: include_bytes!("pgs-original140--unknown-segment-kept/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--source-position-outside-canvas",
        input: include_bytes!("pgs-original140--source-position-outside-canvas/input.bin"),
        expected: include_bytes!("pgs-original140--source-position-outside-canvas/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--multiple-displays-one-packet",
        input: include_bytes!("pgs-original140--multiple-displays-one-packet/input.bin"),
        expected: include_bytes!("pgs-original140--multiple-displays-one-packet/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--three-source-objects-native-two-reference-limit",
        input: include_bytes!(
            "pgs-original140--three-source-objects-native-two-reference-limit/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--three-source-objects-native-two-reference-limit/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-missing-END",
        input: include_bytes!("pgs-original140--malformed-missing-END/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-missing-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-END-without-PCS",
        input: include_bytes!("pgs-original140--malformed-END-without-PCS/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-END-without-PCS/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-nonempty-END",
        input: include_bytes!("pgs-original140--malformed-nonempty-END/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-nonempty-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-missing-last-marker",
        input: include_bytes!("pgs-original140--malformed-missing-last-marker/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-missing-last-marker/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-early-last",
        input: include_bytes!("pgs-original140--malformed-early-last/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-early-last/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-overshoot",
        input: include_bytes!("pgs-original140--malformed-overshoot/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-overshoot/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-wrong-fragment-version",
        input: include_bytes!("pgs-original140--malformed-wrong-fragment-version/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-wrong-fragment-version/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-crop-outside-object",
        input: include_bytes!("pgs-original140--malformed-crop-outside-object/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-crop-outside-object/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-palette-entry-truncated",
        input: include_bytes!("pgs-original140--malformed-palette-entry-truncated/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-palette-entry-truncated/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--malformed-windows-truncated",
        input: include_bytes!("pgs-original140--malformed-windows-truncated/input.bin"),
        expected: include_bytes!("pgs-original140--malformed-windows-truncated/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-01",
        input: include_bytes!("pgs-original140--truncated-first-header-01/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-01/expected.bin"),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-02",
        input: include_bytes!("pgs-original140--truncated-first-header-02/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-02/expected.bin"),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-03",
        input: include_bytes!("pgs-original140--truncated-first-header-03/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-03/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-04",
        input: include_bytes!("pgs-original140--truncated-first-header-04/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-04/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-05",
        input: include_bytes!("pgs-original140--truncated-first-header-05/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-05/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-06",
        input: include_bytes!("pgs-original140--truncated-first-header-06/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-06/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-07",
        input: include_bytes!("pgs-original140--truncated-first-header-07/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-07/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-08",
        input: include_bytes!("pgs-original140--truncated-first-header-08/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-08/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-09",
        input: include_bytes!("pgs-original140--truncated-first-header-09/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-09/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-10",
        input: include_bytes!("pgs-original140--truncated-first-header-10/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-10/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-11",
        input: include_bytes!("pgs-original140--truncated-first-header-11/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-11/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-12",
        input: include_bytes!("pgs-original140--truncated-first-header-12/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-12/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--truncated-first-header-13",
        input: include_bytes!("pgs-original140--truncated-first-header-13/input.bin"),
        expected: include_bytes!("pgs-original140--truncated-first-header-13/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--cache-edge-pending-across-clear-END",
        input: include_bytes!("pgs-original140--cache-edge-pending-across-clear-END/input.bin"),
        expected: include_bytes!(
            "pgs-original140--cache-edge-pending-across-clear-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--cache-edge-pending-across-two-clear-ENDs",
        input: include_bytes!(
            "pgs-original140--cache-edge-pending-across-two-clear-ENDs/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--cache-edge-pending-across-two-clear-ENDs/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--cache-edge-pending-discarded-at-epoch",
        input: include_bytes!("pgs-original140--cache-edge-pending-discarded-at-epoch/input.bin"),
        expected: include_bytes!(
            "pgs-original140--cache-edge-pending-discarded-at-epoch/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--cache-edge-complete-unused-object-preload",
        input: include_bytes!(
            "pgs-original140--cache-edge-complete-unused-object-preload/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--cache-edge-complete-unused-object-preload/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--cache-edge-pending-at-explicit-EOF",
        input: include_bytes!("pgs-original140--cache-edge-pending-at-explicit-EOF/input.bin"),
        expected: include_bytes!(
            "pgs-original140--cache-edge-pending-at-explicit-EOF/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-replacement-00",
        input: include_bytes!("pgs-original140--compat-empty-first-replacement-00/input.bin"),
        expected: include_bytes!("pgs-original140--compat-empty-first-replacement-00/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-replacement-40",
        input: include_bytes!("pgs-original140--compat-empty-first-replacement-40/input.bin"),
        expected: include_bytes!("pgs-original140--compat-empty-first-replacement-40/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-replacement-80",
        input: include_bytes!("pgs-original140--compat-empty-first-replacement-80/input.bin"),
        expected: include_bytes!("pgs-original140--compat-empty-first-replacement-80/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-replacement-c0",
        input: include_bytes!("pgs-original140--compat-empty-first-replacement-c0/input.bin"),
        expected: include_bytes!("pgs-original140--compat-empty-first-replacement-c0/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-replacement-new-shape",
        input: include_bytes!(
            "pgs-original140--compat-empty-first-replacement-new-shape/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-empty-first-replacement-new-shape/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-partial-palette-recolours-old-image",
        input: include_bytes!(
            "pgs-original140--compat-empty-first-partial-palette-recolours-old-image/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-empty-first-partial-palette-recolours-old-image/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-empty-first-after-hard-reset",
        input: include_bytes!("pgs-original140--compat-empty-first-after-hard-reset/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-empty-first-after-hard-reset/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-oversized-object-first-rejected-by-native",
        input: include_bytes!(
            "pgs-original140--compat-oversized-object-first-rejected-by-native/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-oversized-object-first-rejected-by-native/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-packet-final-context-from-trailing-PCS",
        input: include_bytes!(
            "pgs-original140--compat-packet-final-context-from-trailing-PCS/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-packet-final-context-from-trailing-PCS/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-first-clear-then-complete-in-one-packet",
        input: include_bytes!(
            "pgs-original140--compat-first-clear-then-complete-in-one-packet/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-first-clear-then-complete-in-one-packet/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-first-image-then-clear-new-context",
        input: include_bytes!(
            "pgs-original140--compat-first-image-then-clear-new-context/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-first-image-then-clear-new-context/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-three-references-forced-false",
        input: include_bytes!("pgs-original140--compat-three-references-forced-false/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-three-references-forced-false/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-three-references-forced-true",
        input: include_bytes!("pgs-original140--compat-three-references-forced-true/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-three-references-forced-true/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-EOF-PCS-only",
        input: include_bytes!("pgs-original140--compat-EOF-PCS-only/input.bin"),
        expected: include_bytes!("pgs-original140--compat-EOF-PCS-only/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-EOF-complete-ODS-without-END",
        input: include_bytes!("pgs-original140--compat-EOF-complete-ODS-without-END/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-EOF-complete-ODS-without-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-EOF-palette-update-without-END",
        input: include_bytes!("pgs-original140--compat-EOF-palette-update-without-END/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-EOF-palette-update-without-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-EOF-complete-then-empty-first-pending",
        input: include_bytes!(
            "pgs-original140--compat-EOF-complete-then-empty-first-pending/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-EOF-complete-then-empty-first-pending/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-clock-unknown-PTS-sentinel-DTS",
        input: include_bytes!("pgs-original140--compat-clock-unknown-PTS-sentinel-DTS/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-clock-unknown-PTS-sentinel-DTS/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-clock-negative-half-microsecond",
        input: include_bytes!("pgs-original140--compat-clock-negative-half-microsecond/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-clock-negative-half-microsecond/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-clock-sentinel-on-END-fallback",
        input: include_bytes!("pgs-original140--compat-clock-sentinel-on-END-fallback/input.bin"),
        expected: include_bytes!(
            "pgs-original140--compat-clock-sentinel-on-END-fallback/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-hidden-third-before-rejected-first",
        input: include_bytes!(
            "pgs-original140--compat-cache-hidden-third-before-rejected-first/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-hidden-third-before-rejected-first/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-unused-complete-before-rejected-first",
        input: include_bytes!(
            "pgs-original140--compat-cache-unused-complete-before-rejected-first/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-unused-complete-before-rejected-first/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-unused-fragments-before-rejected-first",
        input: include_bytes!(
            "pgs-original140--compat-cache-unused-fragments-before-rejected-first/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-unused-fragments-before-rejected-first/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-oversized-replacement-keeps-old-native",
        input: include_bytes!(
            "pgs-original140--compat-cache-oversized-replacement-keeps-old-native/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-oversized-replacement-keeps-old-native/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-rejected-first-then-valid-replacement",
        input: include_bytes!(
            "pgs-original140--compat-cache-rejected-first-then-valid-replacement/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-rejected-first-then-valid-replacement/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-original140--compat-cache-unused-preload-epoch-then-rejected-first",
        input: include_bytes!(
            "pgs-original140--compat-cache-unused-preload-epoch-then-rejected-first/input.bin"
        ),
        expected: include_bytes!(
            "pgs-original140--compat-cache-unused-preload-epoch-then-rejected-first/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-complete",
        input: include_bytes!("pgs-independent89--rle-1x1-complete/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-complete/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-tail-after-EOL",
        input: include_bytes!("pgs-independent89--rle-1x1-tail-after-EOL/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-tail-after-EOL/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-truncated-control",
        input: include_bytes!("pgs-independent89--rle-1x1-truncated-control/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-truncated-control/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-truncated-long-colour",
        input: include_bytes!("pgs-independent89--rle-1x1-truncated-long-colour/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-truncated-long-colour/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-short-line",
        input: include_bytes!("pgs-independent89--rle-1x1-short-line/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-short-line/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-run-crossing",
        input: include_bytes!("pgs-independent89--rle-1x1-run-crossing/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-run-crossing/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-1x1-zero-run",
        input: include_bytes!("pgs-independent89--rle-1x1-zero-run/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-1x1-zero-run/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--no-final-EOL-1x1",
        input: include_bytes!("pgs-independent89--no-final-EOL-1x1/input.bin"),
        expected: include_bytes!("pgs-independent89--no-final-EOL-1x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-no-last-1x1",
        input: include_bytes!("pgs-independent89--fragment-no-last-1x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-no-last-1x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-version-1x1",
        input: include_bytes!("pgs-independent89--fragment-version-1x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-version-1x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-early-1x1",
        input: include_bytes!("pgs-independent89--fragment-early-1x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-early-1x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-overshoot-1x1",
        input: include_bytes!("pgs-independent89--fragment-overshoot-1x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-overshoot-1x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-complete",
        input: include_bytes!("pgs-independent89--rle-2x1-complete/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-complete/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-tail-after-EOL",
        input: include_bytes!("pgs-independent89--rle-2x1-tail-after-EOL/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-tail-after-EOL/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-truncated-control",
        input: include_bytes!("pgs-independent89--rle-2x1-truncated-control/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-truncated-control/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-truncated-long-colour",
        input: include_bytes!("pgs-independent89--rle-2x1-truncated-long-colour/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-truncated-long-colour/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-short-line",
        input: include_bytes!("pgs-independent89--rle-2x1-short-line/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-short-line/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-run-crossing",
        input: include_bytes!("pgs-independent89--rle-2x1-run-crossing/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-run-crossing/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-2x1-zero-run",
        input: include_bytes!("pgs-independent89--rle-2x1-zero-run/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-2x1-zero-run/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--no-final-EOL-2x1",
        input: include_bytes!("pgs-independent89--no-final-EOL-2x1/input.bin"),
        expected: include_bytes!("pgs-independent89--no-final-EOL-2x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-no-last-2x1",
        input: include_bytes!("pgs-independent89--fragment-no-last-2x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-no-last-2x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-version-2x1",
        input: include_bytes!("pgs-independent89--fragment-version-2x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-version-2x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-early-2x1",
        input: include_bytes!("pgs-independent89--fragment-early-2x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-early-2x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-overshoot-2x1",
        input: include_bytes!("pgs-independent89--fragment-overshoot-2x1/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-overshoot-2x1/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-complete",
        input: include_bytes!("pgs-independent89--rle-3x2-complete/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-complete/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-tail-after-EOL",
        input: include_bytes!("pgs-independent89--rle-3x2-tail-after-EOL/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-tail-after-EOL/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-truncated-control",
        input: include_bytes!("pgs-independent89--rle-3x2-truncated-control/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-truncated-control/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-truncated-long-colour",
        input: include_bytes!("pgs-independent89--rle-3x2-truncated-long-colour/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-truncated-long-colour/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-short-line",
        input: include_bytes!("pgs-independent89--rle-3x2-short-line/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-short-line/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-run-crossing",
        input: include_bytes!("pgs-independent89--rle-3x2-run-crossing/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-run-crossing/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-3x2-zero-run",
        input: include_bytes!("pgs-independent89--rle-3x2-zero-run/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-3x2-zero-run/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--no-final-EOL-3x2",
        input: include_bytes!("pgs-independent89--no-final-EOL-3x2/input.bin"),
        expected: include_bytes!("pgs-independent89--no-final-EOL-3x2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-no-last-3x2",
        input: include_bytes!("pgs-independent89--fragment-no-last-3x2/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-no-last-3x2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-version-3x2",
        input: include_bytes!("pgs-independent89--fragment-version-3x2/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-version-3x2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-early-3x2",
        input: include_bytes!("pgs-independent89--fragment-early-3x2/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-early-3x2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-overshoot-3x2",
        input: include_bytes!("pgs-independent89--fragment-overshoot-3x2/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-overshoot-3x2/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-complete",
        input: include_bytes!("pgs-independent89--rle-17x3-complete/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-complete/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-tail-after-EOL",
        input: include_bytes!("pgs-independent89--rle-17x3-tail-after-EOL/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-tail-after-EOL/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-truncated-control",
        input: include_bytes!("pgs-independent89--rle-17x3-truncated-control/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-truncated-control/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-truncated-long-colour",
        input: include_bytes!("pgs-independent89--rle-17x3-truncated-long-colour/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-truncated-long-colour/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-short-line",
        input: include_bytes!("pgs-independent89--rle-17x3-short-line/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-short-line/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-run-crossing",
        input: include_bytes!("pgs-independent89--rle-17x3-run-crossing/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-run-crossing/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--rle-17x3-zero-run",
        input: include_bytes!("pgs-independent89--rle-17x3-zero-run/input.bin"),
        expected: include_bytes!("pgs-independent89--rle-17x3-zero-run/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--no-final-EOL-17x3",
        input: include_bytes!("pgs-independent89--no-final-EOL-17x3/input.bin"),
        expected: include_bytes!("pgs-independent89--no-final-EOL-17x3/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-no-last-17x3",
        input: include_bytes!("pgs-independent89--fragment-no-last-17x3/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-no-last-17x3/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-version-17x3",
        input: include_bytes!("pgs-independent89--fragment-version-17x3/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-version-17x3/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-early-17x3",
        input: include_bytes!("pgs-independent89--fragment-early-17x3/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-early-17x3/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--fragment-overshoot-17x3",
        input: include_bytes!("pgs-independent89--fragment-overshoot-17x3/input.bin"),
        expected: include_bytes!("pgs-independent89--fragment-overshoot-17x3/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-1-END",
        input: include_bytes!("pgs-independent89--palette-tail-1-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-1-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-1-unknown-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-1-unknown-before-END/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--palette-tail-1-unknown-before-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-1-packet-end",
        input: include_bytes!("pgs-independent89--palette-tail-1-packet-end/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-1-packet-end/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-1-PDS-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-1-PDS-before-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-1-PDS-before-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-2-END",
        input: include_bytes!("pgs-independent89--palette-tail-2-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-2-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-2-unknown-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-2-unknown-before-END/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--palette-tail-2-unknown-before-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-2-packet-end",
        input: include_bytes!("pgs-independent89--palette-tail-2-packet-end/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-2-packet-end/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-2-PDS-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-2-PDS-before-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-2-PDS-before-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-3-END",
        input: include_bytes!("pgs-independent89--palette-tail-3-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-3-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-3-unknown-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-3-unknown-before-END/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--palette-tail-3-unknown-before-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-3-packet-end",
        input: include_bytes!("pgs-independent89--palette-tail-3-packet-end/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-3-packet-end/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-3-PDS-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-3-PDS-before-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-3-PDS-before-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-4-END",
        input: include_bytes!("pgs-independent89--palette-tail-4-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-4-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-4-unknown-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-4-unknown-before-END/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--palette-tail-4-unknown-before-END/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-4-packet-end",
        input: include_bytes!("pgs-independent89--palette-tail-4-packet-end/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-4-packet-end/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--palette-tail-4-PDS-before-END",
        input: include_bytes!("pgs-independent89--palette-tail-4-PDS-before-END/input.bin"),
        expected: include_bytes!("pgs-independent89--palette-tail-4-PDS-before-END/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-1",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-1/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-1/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-2",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-2/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-2/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-3",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-3/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-3/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-4",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-4/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-4/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-7",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-7/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-7/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-11",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-11/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-11/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-19-actual-13",
        input: include_bytes!("pgs-independent89--physical-tail-declared-19-actual-13/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-19-actual-13/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-1",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-1/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-1/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-2",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-2/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-2/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-3",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-3/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-3/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-4",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-4/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-4/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-7",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-7/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-7/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-11",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-11/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-11/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-71-actual-13",
        input: include_bytes!("pgs-independent89--physical-tail-declared-71-actual-13/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-71-actual-13/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-1",
        input: include_bytes!("pgs-independent89--physical-tail-declared-65535-actual-1/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-1/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-2",
        input: include_bytes!("pgs-independent89--physical-tail-declared-65535-actual-2/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-2/expected.bin"
        ),
        succeeded: false,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-3",
        input: include_bytes!("pgs-independent89--physical-tail-declared-65535-actual-3/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-3/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-4",
        input: include_bytes!("pgs-independent89--physical-tail-declared-65535-actual-4/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-4/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-7",
        input: include_bytes!("pgs-independent89--physical-tail-declared-65535-actual-7/input.bin"),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-7/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-11",
        input: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-11/input.bin"
        ),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-11/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--physical-tail-declared-65535-actual-13",
        input: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-13/input.bin"
        ),
        expected: include_bytes!(
            "pgs-independent89--physical-tail-declared-65535-actual-13/expected.bin"
        ),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--clock-unknown-zero",
        input: include_bytes!("pgs-independent89--clock-unknown-zero/input.bin"),
        expected: include_bytes!("pgs-independent89--clock-unknown-zero/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--clock-unknown-nonzero-raw",
        input: include_bytes!("pgs-independent89--clock-unknown-nonzero-raw/input.bin"),
        expected: include_bytes!("pgs-independent89--clock-unknown-nonzero-raw/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--clock-negative",
        input: include_bytes!("pgs-independent89--clock-negative/input.bin"),
        expected: include_bytes!("pgs-independent89--clock-negative/expected.bin"),
        succeeded: true,
    },
    FrozenCase {
        id: "pgs-independent89--clock-known-native-sentinel",
        input: include_bytes!("pgs-independent89--clock-known-native-sentinel/input.bin"),
        expected: include_bytes!("pgs-independent89--clock-known-native-sentinel/expected.bin"),
        succeeded: true,
    },
];
