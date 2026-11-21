//! Transport packets, their framing and continuity.
pub(crate) mod carrier;
pub(crate) mod continuity;
pub(crate) mod framer;
pub(crate) mod packet;

pub(crate) use carrier::{OPCR_WITHOUT_PCR, SYNC_BYTE};
pub use carrier::{
    RejectedTransport, TransportInspection, TransportInspector, TransportObservation,
};
pub use continuity::{Continuity, ContinuityOutcome};
pub use framer::{FrameState, FrameStep, NullCarrierRun, TransportFramer};
pub use packet::{Adaptation, PacketFormat, Pcr, TransportPacket, parse_transport};
pub(crate) use packet::{PCR_ENCODING, SCRAMBLED_PAYLOAD, TRANSPORT_ERROR_INDICATOR};
