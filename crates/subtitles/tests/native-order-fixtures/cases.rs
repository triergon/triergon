use super::*;
pub(super) static CASES: &[Case] = &[
    Case {
        id: "original-pcs12-rcs12",
        payload: include_bytes!("input/original-pcs12-rcs12.dvb"),
        pts_raw: 1000000,
        start_ms: 0,
        end_ms: 2000,
        rects: &[
            ExpectedRect {
                geometry: [2, 5, 2, 2],
                stride: 2,
                indices: include_bytes!("input/original-pcs12-rcs12.0.indices"),
                rgba: include_bytes!("input/original-pcs12-rcs12.0.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
            ExpectedRect {
                geometry: [1, 1, 4, 2],
                stride: 4,
                indices: include_bytes!("input/original-pcs12-rcs12.1.indices"),
                rgba: include_bytes!("input/original-pcs12-rcs12.1.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
        ],
    },
    Case {
        id: "same-pcs12-swapped-rcs21",
        payload: include_bytes!("input/same-pcs12-swapped-rcs21.dvb"),
        pts_raw: 1000000,
        start_ms: 0,
        end_ms: 2000,
        rects: &[
            ExpectedRect {
                geometry: [2, 5, 2, 2],
                stride: 2,
                indices: include_bytes!("input/same-pcs12-swapped-rcs21.0.indices"),
                rgba: include_bytes!("input/same-pcs12-swapped-rcs21.0.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
            ExpectedRect {
                geometry: [1, 1, 4, 2],
                stride: 4,
                indices: include_bytes!("input/same-pcs12-swapped-rcs21.1.indices"),
                rgba: include_bytes!("input/same-pcs12-swapped-rcs21.1.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
        ],
    },
    Case {
        id: "renamed-pcs92-rcs92",
        payload: include_bytes!("input/renamed-pcs92-rcs92.dvb"),
        pts_raw: 1000000,
        start_ms: 0,
        end_ms: 2000,
        rects: &[
            ExpectedRect {
                geometry: [2, 5, 2, 2],
                stride: 2,
                indices: include_bytes!("input/renamed-pcs92-rcs92.0.indices"),
                rgba: include_bytes!("input/renamed-pcs92-rcs92.0.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
            ExpectedRect {
                geometry: [1, 1, 4, 2],
                stride: 4,
                indices: include_bytes!("input/renamed-pcs92-rcs92.1.indices"),
                rgba: include_bytes!("input/renamed-pcs92-rcs92.1.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
        ],
    },
    Case {
        id: "reversed-pcs21-rcs12",
        payload: include_bytes!("input/reversed-pcs21-rcs12.dvb"),
        pts_raw: 1000000,
        start_ms: 0,
        end_ms: 2000,
        rects: &[
            ExpectedRect {
                geometry: [1, 1, 4, 2],
                stride: 4,
                indices: include_bytes!("input/reversed-pcs21-rcs12.0.indices"),
                rgba: include_bytes!("input/reversed-pcs21-rcs12.0.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
            ExpectedRect {
                geometry: [2, 5, 2, 2],
                stride: 2,
                indices: include_bytes!("input/reversed-pcs21-rcs12.1.indices"),
                rgba: include_bytes!("input/reversed-pcs21-rcs12.1.rgba"),
                words: &[34816, 4294967295, 4278190080, 4294967295],
            },
        ],
    },
];
