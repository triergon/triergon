//! Source-domain projection fixed by the original 24-member diagnostic plan.
use std::io::{self, Write};
use triergon_framing::{DtsCoreField as F, DtsPackedHeader};
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
pub fn fields(header: &DtsPackedHeader) -> Option<[u32; 24]> {
    if !header.syntax_complete() {
        return None;
    }
    let source = [
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
    for (i, field) in source.into_iter().enumerate() {
        out[i] = header.value(field)?;
    }
    for i in [1, 3, 4] {
        out[i] = out[i].checked_add(1)?;
    }
    Some(out)
}
pub fn write<W: Write>(out: &mut W, values: Option<[u32; 24]>) -> io::Result<()> {
    let Some(values) = values else {
        return write!(out, "null");
    };
    write!(out, "{{")?;
    for (i, (name, value)) in NAMES.into_iter().zip(values).enumerate() {
        if i != 0 {
            write!(out, ",")?;
        }
        write!(out, "\"{name}\":{value}")?;
    }
    write!(out, "}}")
}
