#[test]
fn state_multiple_regions_objects(){run(0);}
#[test]
fn state_palette_only_with_rcs(){run(1);}
#[test]
fn state_placement_only(){run(2);}
#[test]
fn state_clear_keeps_pixel_cache(){run(3);}
#[test]
fn state_reuse_cached_object(){run(4);}
#[test]
fn state_fill_without_object_data(){run(5);}
#[test]
fn state_mode_change_replaces_epoch(){run(6);}
#[test]
fn state_fresh_acquisition(){run(7);}
#[test]
fn state_ancillary_object(){run(8);}
#[test]
fn state_reject_in_epoch_resize(){run(9);}
#[test]
fn state_missing_object(){run(10);}
#[test]
fn state_same_pts_packet_split(){run(11);}
#[test]
fn state_private_extension_preserved(){run(12);}
#[test]
fn state_fresh_fill_no_refs(){run(13);}
#[test]
fn state_missing_then_object(){run(14);}
#[test]
fn state_missing_reference_with_prior_pixels(){run(15);}
#[test]
fn state_mixed_present_missing(){run(16);}
#[test]
fn state_resize_and_object(){run(17);}
#[test]
fn state_resize_reacquire(){run(18);}
#[test]
fn state_resize_same_version(){run(19);}
#[test]
fn state_missing_fill_only_later(){run(20);}
#[test]
fn state_height_change_no_ods(){run(21);}
#[test]
fn state_width_shrink_no_ods(){run(22);}
#[test]
fn state_height_change_with_object(){run(23);}
#[test]
fn state_shrink_then_regrow(){run(24);}
