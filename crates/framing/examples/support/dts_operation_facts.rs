//! Original scalar serialization of actual frozen typed header facts.
use triergon_framing::DtsCoreHeader;
#[derive(Clone, Copy)]
pub struct Scalar {
    pub bits: u128,
    pub negative: bool,
    pub present: bool,
}
pub const HEADER_NAMES: [&str; 36] = [
    "normal",
    "short",
    "cpf",
    "nblks",
    "fsize",
    "amode",
    "sfreq",
    "rate",
    "fixed",
    "dynamic",
    "timestamp",
    "auxiliary",
    "hdcd",
    "extension_id",
    "extension",
    "audio_sync",
    "lfe",
    "history",
    "hcrc",
    "filter",
    "version",
    "copy_history",
    "pcm_resolution",
    "sum_front",
    "sum_surround",
    "dialog_word",
    "header_bits",
    "frame_bytes",
    "core_rate_hz",
    "coding_window_samples",
    "full_bandwidth_channels",
    "nominal_bitrate",
    "open_bitrate",
    "dialog_gain_db",
    "window_numerator",
    "window_denominator",
];
pub const WIDTHS: [usize; 36] = [
    1, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 2, 8, 4, 4, 1, 4,
    1, 1, 16, 16,
];
fn u(v: u128) -> Scalar {
    Scalar {
        bits: v,
        negative: false,
        present: true,
    }
}
fn s(v: i128) -> Scalar {
    Scalar {
        bits: v.unsigned_abs(),
        negative: v < 0,
        present: true,
    }
}
fn none() -> Scalar {
    Scalar {
        bits: 0,
        negative: false,
        present: false,
    }
}
pub fn values(h: DtsCoreHeader) -> [Scalar; 36] {
    [
        u(u128::from(h.normal)),
        u(u128::from(h.short)),
        u(u128::from(h.cpf)),
        u(u128::from(h.nblks)),
        u(u128::from(h.fsize)),
        u(u128::from(h.amode)),
        u(u128::from(h.sfreq)),
        u(u128::from(h.rate)),
        u(u128::from(h.fixed)),
        u(u128::from(h.dynamic)),
        u(u128::from(h.timestamp)),
        u(u128::from(h.auxiliary)),
        u(u128::from(h.hdcd)),
        u(u128::from(h.extension_id)),
        u(u128::from(h.extension)),
        u(u128::from(h.audio_sync)),
        u(u128::from(h.lfe)),
        u(u128::from(h.history)),
        h.hcrc.map_or_else(none, |v| u(u128::from(v))),
        u(u128::from(h.filter)),
        u(u128::from(h.version)),
        u(u128::from(h.copy_history)),
        u(u128::from(h.pcm_resolution)),
        u(u128::from(h.sum_front)),
        u(u128::from(h.sum_surround)),
        u(u128::from(h.dialog_word)),
        u(u128::from(h.header_bits)),
        u(h.frame_bytes as u128),
        u(u128::from(h.core_rate_hz)),
        u(u128::from(h.coding_window_samples)),
        h.full_bandwidth_channels
            .map_or_else(none, |v| u(u128::from(v))),
        h.nominal_bitrate.map_or_else(none, |v| u(u128::from(v))),
        u(u128::from(h.open_bitrate)),
        h.dialog_gain_db.map_or_else(none, |v| s(i128::from(v))),
        s(h.window.numerator()),
        u(h.window.denominator()),
    ]
}
