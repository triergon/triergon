use triergon_capture::mpeg1::{Encoder, EncoderConfig};
use triergon_capture::ts::{MuxConfig, Muxer, crc32_mpeg};
use triergon_capture::{
    CaptureError, Geometry, Orientation, Picture, Session, SessionConfig, rgba_to_yuv420,
};
fn cfg() -> EncoderConfig {
    EncoderConfig {
        geometry: Geometry::new(16, 16).unwrap(),
        bit_rate: 1_000_000,
        max_rate: 1_400_000,
        vbv_bits: 142_857,
        gop: 30,
    }
}
#[test]
fn direct_zero_gop_is_rejected_before_allocation() {
    assert!(matches!(
        Encoder::new(EncoderConfig { gop: 0, ..cfg() }),
        Err(CaptureError::Gop(0))
    ));
}
#[test]
fn session_and_direct_encoder_share_gop_validation() {
    let mut c = SessionConfig::streem(16, 16, 1_000_000);
    c.gop = 0;
    assert!(matches!(Session::open(&c), Err(CaptureError::Gop(0))));
}
#[test]
fn private_geometry_has_checked_dimensions_and_coded_extents() {
    let g = Geometry::new(18, 34).unwrap();
    assert_eq!(
        (
            g.width(),
            g.height(),
            g.macroblock_width(),
            g.macroblock_height()
        ),
        (18, 34, 2, 3)
    );
    assert_eq!(
        (g.luma_stride(), g.chroma_stride(), g.rgba_len()),
        (32, 16, 18 * 34 * 4)
    );
}
#[test]
fn largest_valid_geometry_is_32bit_representable_without_allocation() {
    let g = Geometry::new(4094, 4094).unwrap();
    assert_eq!((g.luma_stride(), g.chroma_stride()), (4096, 2048));
    assert!(g.rgba_len() < u32::MAX as usize);
}
#[test]
fn invalid_geometry_stays_explicit() {
    for (w, h) in [
        (0, 2),
        (2, 0),
        (1, 2),
        (2, 1),
        (4096, 2),
        (2, 4096),
        (u32::MAX, 2),
    ] {
        assert!(matches!(
            Geometry::new(w, h),
            Err(CaptureError::Geometry { .. })
        ));
    }
}
#[test]
fn mutable_planes_keep_exact_layout_and_convert_safely() {
    let mut p = Picture::new(Geometry::new(2, 2).unwrap());
    let before = (p.y().len(), p.cb().len(), p.cr().len());
    p.y_mut().fill(19);
    p.cb_mut().fill(128);
    p.cr_mut().fill(128);
    rgba_to_yuv420(&[0; 16], Orientation::TopDown, &mut p).unwrap();
    assert_eq!((p.y().len(), p.cb().len(), p.cr().len()), before);
    assert_eq!(p.geometry(), Geometry::new(2, 2).unwrap());
    assert_eq!(p.visible_yuv420(), [16, 16, 16, 16, 128, 128]);
}
#[test]
fn row_accessors_reject_end_and_overflow_indices() {
    let p = Picture::new(Geometry::new(18, 34).unwrap());
    assert_eq!(p.y_row(33).unwrap().len(), 18);
    assert!(p.y_row(34).is_none());
    assert!(p.y_row(usize::MAX).is_none());
    assert_eq!(p.cb_row(16).unwrap().len(), 9);
    assert!(p.cb_row(17).is_none());
    assert!(p.cr_row(usize::MAX).is_none());
}
#[test]
fn mismatched_picture_returns_owner_unchanged_before_encoder_mutation() {
    let mut e = Encoder::new(cfg()).unwrap();
    let p = Picture::new(Geometry::new(32, 16).unwrap());
    let mut out = vec![1, 2, 3];
    assert!(matches!(
        e.encode(&p, &mut out),
        Err(CaptureError::PictureGeometry { .. })
    ));
    assert_eq!(out, [1, 2, 3]);
    let p = Picture::new(Geometry::new(16, 16).unwrap());
    assert_eq!(e.encode(&p, &mut out).unwrap().index, 0);
}
#[test]
fn maximal_single_packet_sdt_is_complete_and_has_valid_crc() {
    let mut mux = Muxer::new(&MuxConfig {
        provider_name: "p".repeat(149),
        ..MuxConfig::default()
    })
    .unwrap();
    let mut out = Vec::new();
    mux.write_picture(&[0; 7], 3000, 0, true, &mut out);
    assert_eq!(out.len() % 188, 0);
    let s = &out[5..188];
    assert_eq!(s.len(), 183);
    assert_eq!(crc32_mpeg(s), 0);
}
#[test]
fn one_byte_excess_is_rejected_without_emission() {
    assert!(matches!(
        Muxer::new(&MuxConfig {
            provider_name: "p".repeat(150),
            ..MuxConfig::default()
        }),
        Err(CaptureError::MuxConfig(_))
    ));
}
#[test]
fn oversized_encoded_lengths_are_rejected() {
    assert!(
        Muxer::new(&MuxConfig {
            provider_name: "p".repeat(256),
            service_name: String::new(),
            ..MuxConfig::default()
        })
        .is_err()
    );
    assert!(
        Muxer::new(&MuxConfig {
            provider_name: String::new(),
            service_name: "s".repeat(256),
            ..MuxConfig::default()
        })
        .is_err()
    );
}
#[test]
fn unsupported_name_encoding_is_rejected() {
    for n in ["é", "line\n", "nul\0"] {
        assert!(
            Muxer::new(&MuxConfig {
                provider_name: n.into(),
                ..MuxConfig::default()
            })
            .is_err()
        );
    }
}
#[test]
fn empty_and_printable_ascii_names_are_supported() {
    assert!(
        Muxer::new(&MuxConfig {
            provider_name: String::new(),
            service_name: String::new(),
            ..MuxConfig::default()
        })
        .is_ok()
    );
    assert!(
        Muxer::new(&MuxConfig {
            provider_name: "AZaz09 ~".into(),
            ..MuxConfig::default()
        })
        .is_ok()
    );
}
#[test]
fn reserved_and_out_of_width_pids_are_rejected() {
    for pid in [0, 0x11, 0x1f, 0x1fff, 0x2000, u16::MAX] {
        assert!(
            Muxer::new(&MuxConfig {
                pmt_pid: pid,
                ..MuxConfig::default()
            })
            .is_err()
        );
        assert!(
            Muxer::new(&MuxConfig {
                video_pid: pid,
                ..MuxConfig::default()
            })
            .is_err()
        );
    }
}
#[test]
fn pmt_and_video_pid_must_be_distinct() {
    assert!(
        Muxer::new(&MuxConfig {
            pmt_pid: 0x100,
            ..MuxConfig::default()
        })
        .is_err()
    );
}
#[test]
fn pid_boundary_values_and_full_width_identifiers_are_valid() {
    assert!(
        Muxer::new(&MuxConfig {
            pmt_pid: 0x20,
            video_pid: 0x1ffe,
            transport_stream_id: u16::MAX,
            original_network_id: u16::MAX,
            program_number: u16::MAX,
            ..MuxConfig::default()
        })
        .is_ok()
    );
}
#[test]
fn zero_program_and_unselected_stream_types_are_rejected() {
    assert!(
        Muxer::new(&MuxConfig {
            program_number: 0,
            ..MuxConfig::default()
        })
        .is_err()
    );
    for t in [0, 3, 0xff] {
        assert!(
            Muxer::new(&MuxConfig {
                stream_type: t,
                ..MuxConfig::default()
            })
            .is_err()
        );
    }
    assert!(
        Muxer::new(&MuxConfig {
            stream_type: 2,
            ..MuxConfig::default()
        })
        .is_ok()
    );
}
#[test]
fn invalid_session_mux_configuration_is_an_error() {
    let mut c = SessionConfig::streem(16, 16, 1_000_000);
    c.mux.video_pid = c.mux.pmt_pid;
    assert!(matches!(Session::open(&c), Err(CaptureError::MuxConfig(_))));
}
