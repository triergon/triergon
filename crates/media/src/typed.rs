//! Resumable typed Matroska operations over shared core input, storage and time.
//! Development API: later sources and this operation need fresh parity and
//! numerical qualification. Existing blocking Demuxer compatibility is retained.
pub use crate::matroska::typed::{
    configuration::{CodecState, Configuration, Initialization, InitializationOrigin},
    hdr10_plus::{
        Hdr10Plus, Hdr10PlusGeometry, Hdr10PlusPeakGrid, Hdr10PlusPercentile, Hdr10PlusToneCurve,
        Hdr10PlusWindow,
    },
    mapping::{MappingExtension, MappingView, TrackExtensions, TrackExtensionsJob},
    native_additions::{BlockAdditionContainer, NativeBlockAddition},
    packet::TypedPacket as Packet,
    side_data::{
        AdditionView, BlockGroupSource, ExtensionScope, ExtensionView, Presence, SourcePosition,
        StateView,
    },
};
use crate::{incremental::ElementEvent, matroska::SeekReceipt, metadata::SharedDocument};
#[path = "typed/reader.rs"]
mod reader;
pub use reader::{BlockingReader, Reader};

#[derive(Clone, Debug)]
pub enum Event {
    /// Source structure or syntax; the other variants express typed acceptance.
    Envelope(ElementEvent),
    Configuration(Configuration),
    Document(SharedDocument),
    Packet(Packet),
    Seek(SeekReceipt),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekRequest {
    pub track: usize,
    pub requested_ns: i64,
    pub playback_preroll: bool,
}

pub use crate::storage::PayloadOwnership;
