use triergon_core::Error;
use triergon_mpegts::mux::{MuxConfig, Muxer};
use triergon_mpegts::psi::PsiCrc32;
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
    assert_eq!(PsiCrc32::of(s), 0);
}
#[test]
fn one_byte_excess_is_rejected_without_emission() {
    assert!(matches!(
        Muxer::new(&MuxConfig {
            provider_name: "p".repeat(150),
            ..MuxConfig::default()
        }),
        Err(Error::Unsupported(_))
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
