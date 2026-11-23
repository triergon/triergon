#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

mod aac_adts_stream;
mod aac_adts_wire;
mod aac_asc_wire;
mod ac3;
mod av1;
mod avc_feed;
mod avc_wire;
mod config_range_storage;
mod config_view;
mod declared_config;
mod dts14_carrier;
mod dts14_frame_acquisition;
mod dts14_sync;
mod dts_core;
mod dts_discovery;
mod dts_extent;
mod dts_full_fields;
mod dts_hcrc_presence;
mod dts_header_cache;
mod dts_inspect;
mod dts_normal;
#[cfg(feature = "reference-trace")]
mod dts_operation;
mod dts_packet_corpus;
mod dts_packing;
mod eac3_bsi;
mod framing;
mod framing_native_compat;
mod header_domain_regression;
mod hevc_feed;
mod hvcc_annexb_whole;
mod hvcc_wire;
mod nal_packed_ranges;
mod nal_packet;
mod nal_packet_receiver;
mod nal_passthrough_alias;
mod nal_passthrough_budget;
mod nal_prefix_bulk;
mod nal_step_into;
mod packet_view;
mod raw_annex_b;
