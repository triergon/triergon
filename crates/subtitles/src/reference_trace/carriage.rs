//! Text carriage in the form of FFmpeg's demuxer API.
//!
//! [`Session`] publishes one immutable [`Snapshot`] per input: the stream and
//! packet fields FFmpeg reports for it, with its codec identifiers, return
//! codes, packet positions and millisecond document timing. A
//! [`Source::Sample`] is one demuxed text sample as its packet; a
//! [`Source::Document`] is a SubRip or WebVTT document read by
//! [`compat::document::Reader`](crate::compat::document::Reader), whose cues
//! become the packets, with the format, stream and progress of opening and
//! reading it. [`Session::recover_pending`] returns an input after
//! cancellation or a resource failure.
//!
//! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.

mod document;
mod model;
mod session;

pub use model::*;
pub use session::Session;
