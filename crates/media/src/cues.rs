//! Bounded Matroska Cues (RFC 9559 §5.1.5 and §22).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cue {
    pub ticks: u64,
    pub track: u64,
    pub cluster: u64,
    pub relative: Option<u64>,
    pub block: Option<u64>,
    pub codec_state: u64,
}
