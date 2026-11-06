//! Isolated semantic view. No C layout/FFI.
use std::io::{self, Write};
use triergon_framing::{DtsCoreField as F, DtsCoreHeader, DtsInspection};

pub const NAMES: [&str; 24] = [
    "normal_frame",
    "deficit_samples",
    "crc_present",
    "npcmblocks",
    "frame_size",
    "audio_mode",
    "sr_code",
    "br_code",
    "drc_present",
    "ts_present",
    "aux_present",
    "hdcd_master",
    "ext_audio_type",
    "ext_audio_present",
    "sync_ssf",
    "lfe_present",
    "predictor_history",
    "filter_perfect",
    "encoder_rev",
    "copy_hist",
    "pcmr_code",
    "sumdiff_front",
    "sumdiff_surround",
    "dn_code",
];

pub fn from_core(h: &DtsCoreHeader) -> [u32; 24] {
    [
        u32::from(h.normal),
        u32::from(h.short) + 1,
        u32::from(h.cpf),
        u32::from(h.nblks) + 1,
        u32::from(h.fsize) + 1,
        u32::from(h.amode),
        u32::from(h.sfreq),
        u32::from(h.rate),
        u32::from(h.dynamic),
        u32::from(h.timestamp),
        u32::from(h.auxiliary),
        u32::from(h.hdcd),
        u32::from(h.extension_id),
        u32::from(h.extension),
        u32::from(h.audio_sync),
        u32::from(h.lfe),
        u32::from(h.history),
        u32::from(h.filter),
        u32::from(h.version),
        u32::from(h.copy_history),
        u32::from(h.pcm_resolution),
        u32::from(h.sum_front),
        u32::from(h.sum_surround),
        u32::from(h.dialog_word),
    ]
}

/// A complete physical source observation can exist with invalid/unsupported
/// semantics. It stays distinct from the strict source operation's acceptance.
pub fn from_inspection(i: &DtsInspection) -> Option<[u32; 24]> {
    if !i.syntax_complete() {
        return None;
    }
    let fields = [
        F::Normal,
        F::Short,
        F::CrcPresent,
        F::BlockCount,
        F::FrameSize,
        F::AudioMode,
        F::SampleRate,
        F::Rate,
        F::Dynamic,
        F::Timestamp,
        F::Auxiliary,
        F::Hdcd,
        F::ExtensionId,
        F::Extension,
        F::AudioSync,
        F::Lfe,
        F::History,
        F::Filter,
        F::Version,
        F::CopyHistory,
        F::PcmResolution,
        F::SumFront,
        F::SumSurround,
        F::DialogWord,
    ];
    let mut out = [0_u32; 24];
    for (at, field) in fields.into_iter().enumerate() {
        out[at] = i.field(field).value()?;
    }
    for at in [1, 3, 4] {
        out[at] = out[at].checked_add(1)?;
    }
    Some(out)
}

pub fn write<W: Write>(out: &mut W, fields: Option<[u32; 24]>) -> io::Result<()> {
    let Some(fields) = fields else {
        return write!(out, "null");
    };
    write!(out, "{{")?;
    for (i, (name, value)) in NAMES.into_iter().zip(fields).enumerate() {
        if i != 0 {
            write!(out, ",")?;
        }
        write!(out, "\"{name}\":{value}")?;
    }
    write!(out, "}}")
}
