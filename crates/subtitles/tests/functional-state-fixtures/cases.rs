use super::{Case, Expected, Rect, SourcePacket};
pub(super) static ALL: &[Case] = &[
    Case {
        id: "multiple-regions-objects",
        packets: &[SourcePacket {
            body: include_bytes!("multiple-regions-objects.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[
                Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
                Rect {
                    geometry: [1, 1, 4, 2],
                    indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
            ],
        }],
    },
    Case {
        id: "palette-only-with-rcs",
        packets: &[
            SourcePacket {
                body: include_bytes!("palette-only-with-rcs.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("palette-only-with-rcs.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4278190080, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4278190080, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "placement-only",
        packets: &[
            SourcePacket {
                body: include_bytes!("placement-only.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("placement-only.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [4, 7, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [3, 2, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "clear-keeps-pixel-cache",
        packets: &[
            SourcePacket {
                body: include_bytes!("clear-keeps-pixel-cache.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("clear-keeps-pixel-cache.1.dvb"),
                pts: 2000,
            },
            SourcePacket {
                body: include_bytes!("clear-keeps-pixel-cache.2.dvb"),
                pts: 3000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[],
            },
            Expected {
                pts_us: 3000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "reuse-cached-object",
        packets: &[
            SourcePacket {
                body: include_bytes!("reuse-cached-object.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("reuse-cached-object.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "fill-without-object-data",
        packets: &[
            SourcePacket {
                body: include_bytes!("fill-without-object-data.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("fill-without-object-data.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[2, 2, 2, 2, 2, 2, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "mode-change-replaces-epoch",
        packets: &[
            SourcePacket {
                body: include_bytes!("mode-change-replaces-epoch.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("mode-change-replaces-epoch.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [0, 0, 2, 2],
                    indices: &[2, 2, 1, 1],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "fresh-acquisition",
        packets: &[SourcePacket {
            body: include_bytes!("fresh-acquisition.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[Rect {
                geometry: [1, 1, 4, 2],
                indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
            }],
        }],
    },
    Case {
        id: "ancillary-object",
        packets: &[SourcePacket {
            body: include_bytes!("ancillary-object.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[Rect {
                geometry: [1, 1, 2, 2],
                indices: &[1, 2, 2, 1],
                receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
            }],
        }],
    },
    Case {
        id: "reject-in-epoch-resize",
        packets: &[
            SourcePacket {
                body: include_bytes!("reject-in-epoch-resize.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("reject-in-epoch-resize.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "missing-object",
        packets: &[SourcePacket {
            body: include_bytes!("missing-object.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[],
        }],
    },
    Case {
        id: "same-pts-packet-split",
        packets: &[
            SourcePacket {
                body: include_bytes!("same-pts-packet-split.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("same-pts-packet-split.1.dvb"),
                pts: 1000,
            },
        ],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[
                Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
                Rect {
                    geometry: [1, 1, 4, 2],
                    indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
            ],
        }],
    },
    Case {
        id: "private-extension-preserved",
        packets: &[SourcePacket {
            body: include_bytes!("private-extension-preserved.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[
                Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
                Rect {
                    geometry: [1, 1, 4, 2],
                    indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
            ],
        }],
    },
    Case {
        id: "fresh-fill-no-refs",
        packets: &[SourcePacket {
            body: include_bytes!("fresh-fill-no-refs.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[],
        }],
    },
    Case {
        id: "missing-then-object",
        packets: &[
            SourcePacket {
                body: include_bytes!("missing-then-object.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("missing-then-object.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [1, 1, 2, 2],
                    indices: &[1, 2, 2, 1],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "missing-reference-with-prior-pixels",
        packets: &[
            SourcePacket {
                body: include_bytes!("missing-reference-with-prior-pixels.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("missing-reference-with-prior-pixels.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "mixed-present-missing",
        packets: &[SourcePacket {
            body: include_bytes!("mixed-present-missing.0.dvb"),
            pts: 1000,
        }],
        events: &[Expected {
            pts_us: 1000000,
            start: 0,
            end: 2000,
            canvas: [16, 12],
            rects: &[
                Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
                Rect {
                    geometry: [1, 1, 4, 2],
                    indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                },
            ],
        }],
    },
    Case {
        id: "resize-and-object",
        packets: &[
            SourcePacket {
                body: include_bytes!("resize-and-object.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("resize-and-object.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 5, 2],
                        indices: &[1, 2, 0, 0, 0, 2, 1, 0, 0, 0],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "resize-reacquire",
        packets: &[
            SourcePacket {
                body: include_bytes!("resize-reacquire.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("resize-reacquire.1.dvb"),
                pts: 2000,
            },
            SourcePacket {
                body: include_bytes!("resize-reacquire.2.dvb"),
                pts: 3000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
            Expected {
                pts_us: 3000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "resize-same-version",
        packets: &[
            SourcePacket {
                body: include_bytes!("resize-same-version.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("resize-same-version.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "missing-fill-only-later",
        packets: &[
            SourcePacket {
                body: include_bytes!("missing-fill-only-later.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("missing-fill-only-later.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[],
            },
        ],
    },
    Case {
        id: "height-change-no-ods",
        packets: &[
            SourcePacket {
                body: include_bytes!("height-change-no-ods.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("height-change-no-ods.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "width-shrink-no-ods",
        packets: &[
            SourcePacket {
                body: include_bytes!("width-shrink-no-ods.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("width-shrink-no-ods.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
        ],
    },
    Case {
        id: "height-change-with-object",
        packets: &[
            SourcePacket {
                body: include_bytes!("height-change-with-object.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("height-change-with-object.1.dvb"),
                pts: 2000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 3],
                        indices: &[1, 2, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
    Case {
        id: "shrink-then-regrow",
        packets: &[
            SourcePacket {
                body: include_bytes!("shrink-then-regrow.0.dvb"),
                pts: 1000,
            },
            SourcePacket {
                body: include_bytes!("shrink-then-regrow.1.dvb"),
                pts: 2000,
            },
            SourcePacket {
                body: include_bytes!("shrink-then-regrow.2.dvb"),
                pts: 3000,
            },
        ],
        events: &[
            Expected {
                pts_us: 1000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 2, 1, 2, 1, 1, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
            Expected {
                pts_us: 2000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[Rect {
                    geometry: [2, 5, 2, 2],
                    indices: &[1, 1, 2, 2],
                    receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                }],
            },
            Expected {
                pts_us: 3000000,
                start: 0,
                end: 2000,
                canvas: [16, 12],
                rects: &[
                    Rect {
                        geometry: [2, 5, 2, 2],
                        indices: &[1, 1, 2, 2],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                    Rect {
                        geometry: [1, 1, 4, 2],
                        indices: &[1, 2, 0, 0, 2, 1, 0, 0],
                        receiver_words: &[34816, 4294967295, 4278190080, 4294967295],
                    },
                ],
            },
        ],
    },
];
