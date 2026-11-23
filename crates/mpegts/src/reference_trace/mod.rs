//! Comparison-only projections, compiled with the `reference-trace` feature.
//!
//! The feature also exposes
//! [`DemuxClock::project_encoded_header`](crate::compat::pes::DemuxClock::project_encoded_header),
//! [`DemuxClock::project_diagnostic_header`](crate::compat::pes::DemuxClock::project_diagnostic_header)
//! and
//! [`ElementaryStream::dvb_single_service_view`](crate::psi::ElementaryStream::dvb_single_service_view).

pub mod recovery;

pub mod catalogue;

pub mod dvb {
    //! The demuxer's codec parameters for one DVB subtitling service.
    //!
    //! [`ElementaryStream::dvb_single_service_view`](crate::psi::ElementaryStream::dvb_single_service_view)
    //! returns the five extradata bytes FFmpeg derives from a stream that signals
    //! exactly one service in one descriptor.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::psi::dvb::{DvbSingleServiceView, DvbViewProfile};
}
