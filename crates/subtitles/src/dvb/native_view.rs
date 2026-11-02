//! Original versioned receiver facts, independent of the source-only view.
use triergon_core::{BudgetVec, Bytes, Rounding, Time, TimeBase};

pub const D32_PIN: &str = "d32b387f2b0a484599d4587d651891f0c63c4238";
pub const D32_VIEW: &str = "d32-default-explicit-CLUT-DVB-receiver-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverView {
    Source,
    NativeD32,
}

#[derive(Debug)]
pub struct NativePalette {
    pub identifier: &'static str,
    pub source_pin: &'static str,
    pub rgba: Bytes,
    pub words: BudgetVec<u32>,
    /// Source provenance only. Native defaults/automatic colours are a separate
    /// constructed view; this flag never certifies general native equality.
    pub all_entries_explicit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationReason {
    EndOfDisplay,
    CompletePrefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativePublication {
    pub identifier: &'static str,
    pub source_pin: &'static str,
    pub packet_sequence: u64,
    pub consumed_prefix_bytes: usize,
    pub reason: PublicationReason,
    /// Only an exactly representable sampled source instant is assigned here.
    /// Unknown/nonintegral/overflow clocks remain explicitly unresolved.
    pub exact_pts_microseconds: Option<i64>,
    pub start_display_ms: u32,
    pub end_display_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeFailure {
    pub packet_sequence: u64,
    pub packet_offset: usize,
    pub code: i32,
    pub unconsumed_bytes: usize,
}

pub(crate) fn exact_microseconds(pts: Option<Time>) -> Option<i64> {
    let time = pts?;
    let base = TimeBase::new(1, 1_000_000).ok()?;
    let ticks = time.to_ticks(base, Rounding::TowardZero).ok()?;
    if Time::from_ticks(ticks, base).ok()? != time {
        return None;
    }
    i64::try_from(ticks).ok()
}
