//! DVB subtitle packets as a container delivers them.
//!
//! [`SourceConfiguration`] reads a service's configuration from Matroska
//! `CodecPrivate` or an MPEG-TS subtitling descriptor and binds each packet to
//! it ([`DvbPacket`]) with its original clock values. The receiver that decodes
//! the packets as the reference does is
//! [`compat::dvb::ReceiverDecoder`](crate::compat::dvb::ReceiverDecoder).
mod boundary;
mod model;
pub(crate) mod pes_parser;
pub(crate) mod receiver_decoder;

pub use boundary::{
    ClockOrigin, ClockValue, ConfigurationSource, PacketIdentity, PacketSourceFacts, PacketTiming,
    SourceClock, SourceConfiguration,
};
pub use boundary::{DvbPacket, PacketEnvelope, Service};
pub use model::{Depth, Size};
