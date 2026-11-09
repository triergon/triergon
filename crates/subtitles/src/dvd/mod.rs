//! DVD subpicture carriage.
//!
//! [`program_stream`] reads the MPEG-2 program streams that carry subpictures
//! (`.vob`, VobSub `.sub`) and [`vobsub`] the lines of a VobSub `.idx`. The
//! reference's receiver over packets, its demuxers' packets and its palette
//! sources are in [`crate::compat::dvd`].
pub(crate) mod demux;
pub mod program_stream;
pub(crate) mod receiver;
pub mod vobsub;
pub(crate) mod wire;
