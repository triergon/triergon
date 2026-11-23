//! The integration tests of `triergon-mpegts`, one test binary.
#![forbid(unsafe_code)]

use triergon_harness_test_support::sha256;

mod support;

mod clock_views;
mod composed;
mod container_reader;
mod continuity_reader;
mod continuity_recovery;
mod demux_input_context;
mod demux_pes_packets;
mod demux_streams;
mod direct_append;
mod dvb_demux_pes;
mod dvb_signalling;
mod malformed_completion;
mod mux;
mod opaque_recovery;
mod original_program;
mod pes_cutter;
mod private_audio_carriage;
mod registered_carriage;
mod regressions;
#[cfg(feature = "reference-trace")]
mod rejected_carrier;
#[cfg(feature = "reference-trace")]
mod selected_carriage;
#[cfg(feature = "reference-trace")]
mod selected_faults;
#[cfg(feature = "reference-trace")]
mod selected_lifecycle;
mod signalling;
mod source_boundaries;
mod transport_fault_policy;
mod video_pes;
