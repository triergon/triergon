//! Integration tests of the public reader, cursor and comparison APIs.
mod advancing_sync;
mod common;
mod container_reader;
mod cts_append;
mod demux_clock;
mod description_recovery;
mod direct_recovery;
mod dvd_subtitle_track;
mod edit_occurrences;
mod encoded_configs;
mod encoded_contexts;
mod encoded_dependency;
mod encoded_edits;
mod encoded_metadata;
mod encoded_recovery;
mod encoded_stream;
mod field_recovery;
mod fragment_append;
#[cfg(feature = "reference-trace")]
mod fragment_open_clock;
mod fragment_random_access;
mod fragment_time;
mod fragments;
mod global_seek;
mod index_distance;
mod index_seek;
mod input;
mod interleaving;
mod legacy_quicktime;
mod metadata_direct;
mod metadata_states;
mod modern_syntax;
mod movie_seek;
mod packet;
mod packet_cursor;
mod packet_direct;
mod packet_view;
mod payload_recovery;
mod pcm;
mod physical_read;
mod plain_seek;
#[cfg(feature = "reference-trace")]
mod private_catalogue;
mod qt_sound;
#[cfg(feature = "reference-trace")]
mod raw_visual_entries;
#[cfg(feature = "reference-trace")]
mod receiver_clock;
mod recovery_groups;
#[cfg(feature = "reference-trace")]
mod recovery_transport;
mod regular;
#[cfg(feature = "reference-trace")]
mod rejected_packet_seek;
#[cfg(feature = "reference-trace")]
mod root_count;
#[cfg(feature = "reference-trace")]
mod root_walk;
mod run_recovery;
mod selected_fields;
mod sidx;
mod source_association;
mod source_seek;
mod syntax;
mod time;
