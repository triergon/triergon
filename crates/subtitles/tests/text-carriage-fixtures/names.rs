#[test]
fn srt_basic_carriage_header() { run_case(0); }
#[test]
fn srt_basic_carriage_streaminfo() { run_case(1); }
#[test]
fn srt_crlf_multiline_carriage_header() { run_case(2); }
#[test]
fn srt_crlf_multiline_carriage_streaminfo() { run_case(3); }
#[test]
fn srt_unicode_no_normalization_carriage_header() { run_case(4); }
#[test]
fn srt_unicode_no_normalization_carriage_streaminfo() { run_case(5); }
#[test]
fn srt_overlap_order_carriage_header() { run_case(6); }
#[test]
fn srt_overlap_order_carriage_streaminfo() { run_case(7); }
#[test]
fn srt_wide_hours_profile_carriage_header() { run_case(8); }
#[test]
fn srt_wide_hours_profile_carriage_streaminfo() { run_case(9); }
#[test]
fn srt_utf8_bom_carriage_header() { run_case(10); }
#[test]
fn srt_utf8_bom_carriage_streaminfo() { run_case(11); }
#[test]
fn srt_utf16be_bom_carriage_header() { run_case(12); }
#[test]
fn srt_utf16be_bom_carriage_streaminfo() { run_case(13); }
#[test]
fn srt_utf16le_bom_carriage_header() { run_case(14); }
#[test]
fn srt_utf16le_bom_carriage_streaminfo() { run_case(15); }
#[test]
fn srt_explicit_utf16be_carriage_header() { run_case(16); }
#[test]
fn srt_explicit_utf16be_carriage_streaminfo() { run_case(17); }
#[test]
fn srt_end_before_start_carriage_header() { run_case(18); }
#[test]
fn srt_end_before_start_carriage_streaminfo() { run_case(19); }
#[test]
fn srt_sequence_missing_carriage_header() { run_case(20); }
#[test]
fn srt_sequence_missing_carriage_streaminfo() { run_case(21); }
#[test]
fn srt_fraction_width_carriage_header() { run_case(22); }
#[test]
fn srt_fraction_width_carriage_streaminfo() { run_case(23); }
#[test]
fn srt_overlong_utf8_carriage_header() { run_case(24); }
#[test]
fn srt_overlong_utf8_carriage_streaminfo() { run_case(25); }
#[test]
fn srt_lone_surrogate_carriage_header() { run_case(26); }
#[test]
fn srt_lone_surrogate_carriage_streaminfo() { run_case(27); }
#[test]
fn vtt_short_timestamps_carriage_header() { run_case(28); }
#[test]
fn vtt_short_timestamps_carriage_streaminfo() { run_case(29); }
#[test]
fn vtt_id_settings_carriage_header() { run_case(30); }
#[test]
fn vtt_id_settings_carriage_streaminfo() { run_case(31); }
#[test]
fn vtt_crlf_multiline_carriage_header() { run_case(32); }
#[test]
fn vtt_crlf_multiline_carriage_streaminfo() { run_case(33); }
#[test]
fn vtt_cr_lines_carriage_header() { run_case(34); }
#[test]
fn vtt_cr_lines_carriage_streaminfo() { run_case(35); }
#[test]
fn vtt_utf8_bom_carriage_header() { run_case(36); }
#[test]
fn vtt_utf8_bom_carriage_streaminfo() { run_case(37); }
#[test]
fn vtt_payload_markup_exact_carriage_header() { run_case(38); }
#[test]
fn vtt_payload_markup_exact_carriage_streaminfo() { run_case(39); }
#[test]
fn vtt_metadata_order_carriage_header() { run_case(40); }
#[test]
fn vtt_metadata_order_carriage_streaminfo() { run_case(41); }
#[test]
fn vtt_note_between_cues_carriage_header() { run_case(42); }
#[test]
fn vtt_note_between_cues_carriage_streaminfo() { run_case(43); }
#[test]
fn vtt_tab_separators_carriage_header() { run_case(44); }
#[test]
fn vtt_tab_separators_carriage_streaminfo() { run_case(45); }
#[test]
fn vtt_wide_hours_carriage_header() { run_case(46); }
#[test]
fn vtt_wide_hours_carriage_streaminfo() { run_case(47); }
#[test]
fn vtt_overlap_order_carriage_header() { run_case(48); }
#[test]
fn vtt_overlap_order_carriage_streaminfo() { run_case(49); }
#[test]
fn vtt_header_annotation_carriage_header() { run_case(50); }
#[test]
fn vtt_header_annotation_carriage_streaminfo() { run_case(51); }
#[test]
fn vtt_empty_document_carriage_header() { run_case(52); }
#[test]
fn vtt_empty_document_carriage_streaminfo() { run_case(53); }
#[test]
fn vtt_bad_signature_carriage_header() { run_case(54); }
#[test]
fn vtt_bad_signature_carriage_streaminfo() { run_case(55); }
#[test]
fn vtt_utf16_refused_carriage_header() { run_case(56); }
#[test]
fn vtt_utf16_refused_carriage_streaminfo() { run_case(57); }
#[test]
fn vtt_zero_interval_strict_carriage_header() { run_case(58); }
#[test]
fn vtt_zero_interval_strict_carriage_streaminfo() { run_case(59); }
#[test]
fn vtt_surrogate_utf8_carriage_header() { run_case(60); }
#[test]
fn vtt_surrogate_utf8_carriage_streaminfo() { run_case(61); }
#[test]
fn vtt_missing_fraction_carriage_header() { run_case(62); }
#[test]
fn vtt_missing_fraction_carriage_streaminfo() { run_case(63); }
#[test]
fn tx3g_basic_none() { run_case(64); }
#[test]
fn tx3g_byte_not_character_count_none() { run_case(65); }
#[test]
fn tx3g_utf16be_bom_none() { run_case(66); }
#[test]
fn tx3g_empty_sample_none() { run_case(67); }
#[test]
fn tx3g_embedded_nul_none() { run_case(68); }
#[test]
fn tx3g_line_codepoints_exact_none() { run_case(69); }
#[test]
fn tx3g_style_highlight_raw_none() { run_case(70); }
#[test]
fn tx3g_unknown_modifier_none() { run_case(71); }
#[test]
fn tx3g_modifier_order_none() { run_case(72); }
#[test]
fn tx3g_empty_with_modifier_none() { run_case(73); }
#[test]
fn tx3g_u16_byte_count_boundary_none() { run_case(74); }
#[test]
fn tx3g_missing_length_none() { run_case(75); }
#[test]
fn tx3g_text_overrun_none() { run_case(76); }
#[test]
fn tx3g_utf16_odd_none() { run_case(77); }
#[test]
fn tx3g_lone_surrogate_none() { run_case(78); }
#[test]
fn tx3g_short_box_none() { run_case(79); }
#[test]
fn tx3g_box_overrun_none() { run_case(80); }
#[test]
fn tx3g_extended_box_gap_none() { run_case(81); }
#[test]
fn utf8_packet_markup_none() { run_case(82); }
#[test]
fn utf8_packet_unknown_time_none() { run_case(83); }
#[test]
fn utf8_packet_known_zero_duration_none() { run_case(84); }
#[test]
fn utf8_packet_negative_start_none() { run_case(85); }
#[test]
fn utf8_packet_raw_clock_none() { run_case(86); }
#[test]
fn utf8_packet_bom_nul_content_none() { run_case(87); }
#[test]
fn utf8_packet_too_large_scalar_none() { run_case(88); }
#[test]
fn utf8_packet_input_limit_none() { run_case(89); }
#[test]
fn mkv_vtt_no_addition_none() { run_case(90); }
#[test]
fn mkv_vtt_settings_id_comments_none() { run_case(91); }
#[test]
fn mkv_vtt_present_empty_fields_none() { run_case(92); }
#[test]
fn mkv_vtt_relative_inline_time_none() { run_case(93); }
#[test]
fn mkv_vtt_unknown_addition_opaque_none() { run_case(94); }
#[test]
fn mkv_vtt_truncated_addition_none() { run_case(95); }
#[test]
fn ass_raw_order_overrides_none() { run_case(96); }
#[test]
fn ass_ssa_empty_layer_none() { run_case(97); }
#[test]
fn ass_private_verbatim_none() { run_case(98); }
#[test]
fn ass_unknown_charset_opaque_none() { run_case(99); }
#[test]
fn ass_empty_not_clear_none() { run_case(100); }
#[test]
fn ass_known_duration_unknown_start_none() { run_case(101); }
#[test]
fn ass_private_present_empty_none() { run_case(102); }
#[test]
fn ass_invalid_utf8_unvalidated_none() { run_case(103); }
