use triergon_core::Error;
use triergon_framing::{
    DtsExtensionSubstreamExtent, DtsFrameExtent, DtsFrameExtentLimits, probe_dts_core_header,
    probe_dts_extension_substream_extent, probe_dts_frame_extent,
};

const LIMITS: DtsFrameExtentLimits = DtsFrameExtentLimits {
    maximum_header_bytes: 4096,
    maximum_frame_bytes: 1 << 20,
};
// Bit-authored size prefixes from TS 102 114 V1.6.1 Table 7-2. These are
// deliberately not complete headers: no CRC, asset metadata or body is implied.
const SHORT: &[u8] = &[0x64, 0x58, 0x20, 0x25, 0xa5, 0x03, 0xe0, 0x3f, 0xe0];
const LONG: &[u8] = &[0x64, 0x58, 0x20, 0x25, 0xff, 0xff, 0xff, 0xff, 0xff, 0xe0];
const CORE: &[u8] = include_bytes!("../../../harness/fixtures/mpegts/dts-core-mono.dts");

#[test]
fn reads_literal_short_and_long_size_fields() {
    for (input, expected) in [
        (
            SHORT,
            DtsExtensionSubstreamExtent {
                user_defined: 0xa5,
                substream_index: 0,
                long_sizes: false,
                header_bytes: 32,
                frame_bytes: 512,
            },
        ),
        (
            LONG,
            DtsExtensionSubstreamExtent {
                user_defined: 0xff,
                substream_index: 3,
                long_sizes: true,
                header_bytes: 4096,
                frame_bytes: 1 << 20,
            },
        ),
    ] {
        assert_eq!(
            probe_dts_extension_substream_extent(input, LIMITS),
            Ok(expected)
        );
        assert_eq!(
            probe_dts_frame_extent(input, LIMITS),
            Ok(DtsFrameExtent::Extension(expected))
        );
        assert_eq!(
            DtsFrameExtent::Extension(expected).frame_bytes(),
            expected.frame_bytes
        );
    }
    // Maximum short fields and a long frame beyond the short field's range.
    for (input, header, frame, index) in [
        (
            &[0x64, 0x58, 0x20, 0x25, 0x13, 0x9f, 0xff, 0xff, 0xe0][..],
            256,
            65536,
            2,
        ),
        (
            &[0x64, 0x58, 0x20, 0x25, 0, 0x60, 0x80, 0x20, 0, 0][..],
            65,
            65537,
            1,
        ),
    ] {
        let extent = probe_dts_extension_substream_extent(input, LIMITS).unwrap();
        assert_eq!(
            (
                extent.header_bytes,
                extent.frame_bytes,
                extent.substream_index
            ),
            (header, frame, index)
        );
    }
}

#[test]
fn every_incomplete_size_prefix_is_truncated() {
    for input in [SHORT, LONG] {
        for end in 0..input.len() {
            assert_eq!(
                probe_dts_extension_substream_extent(&input[..end], LIMITS),
                Err(Error::Truncated)
            );
            assert_eq!(
                probe_dts_frame_extent(&input[..end], LIMITS),
                Err(Error::Truncated)
            );
        }
    }
}

#[test]
fn checks_exact_limits_without_requiring_body_bytes() {
    for (input, header, frame) in [(SHORT, 32, 512), (LONG, 4096, 1 << 20)] {
        assert!(
            probe_dts_frame_extent(
                input,
                DtsFrameExtentLimits {
                    maximum_header_bytes: header,
                    maximum_frame_bytes: frame,
                }
            )
            .is_ok()
        );
        for (limits, requested, maximum) in [
            (
                DtsFrameExtentLimits {
                    maximum_header_bytes: header - 1,
                    ..LIMITS
                },
                header,
                header - 1,
            ),
            (
                DtsFrameExtentLimits {
                    maximum_frame_bytes: frame - 1,
                    ..LIMITS
                },
                frame,
                frame - 1,
            ),
        ] {
            assert_eq!(
                probe_dts_frame_extent(input, limits),
                Err(Error::Limit { requested, maximum })
            );
        }
        for limits in [
            DtsFrameExtentLimits {
                maximum_header_bytes: 0,
                ..LIMITS
            },
            DtsFrameExtentLimits {
                maximum_frame_bytes: 0,
                ..LIMITS
            },
        ] {
            assert!(matches!(
                probe_dts_frame_extent(input, limits),
                Err(Error::Invalid(_))
            ));
        }
    }
}

// Independent whole-integer construction of the two size fields, without the
// production bit reader. Used for invalid extents and field-boundary coverage.
fn prefix(long: bool, header: usize, frame: usize) -> Vec<u8> {
    let (header_bits, frame_bits, bytes) = if long { (12, 20, 10) } else { (8, 16, 9) };
    let value = (0x64582025_u128 << (11 + header_bits + frame_bits))
        | (u128::from(long) << (header_bits + frame_bits))
        | (((header - 1) as u128) << frame_bits)
        | (frame - 1) as u128;
    (value << 5).to_be_bytes()[16 - bytes..].to_vec()
}

#[test]
fn rejects_overlapping_size_fields_crc_and_payload() {
    for long in [false, true] {
        let minimum = if long { 12 } else { 11 };
        for header in 1..minimum {
            assert!(matches!(
                probe_dts_frame_extent(&prefix(long, header, 512), LIMITS),
                Err(Error::Invalid(_))
            ));
        }
        for frame in [1, 31, 32] {
            assert!(matches!(
                probe_dts_frame_extent(&prefix(long, 32, frame), LIMITS),
                Err(Error::Invalid(_))
            ));
        }
    }
}

#[test]
fn size_fields_cross_byte_and_short_long_boundaries() {
    for long in [false, true] {
        for header in [32, 63, 64, 127, 128, 255, 256] {
            for frame in [257, 511, 512, 1023, 1024, 32768, 65535, 65536] {
                let extent =
                    probe_dts_extension_substream_extent(&prefix(long, header, frame), LIMITS)
                        .unwrap();
                assert_eq!((extent.header_bytes, extent.frame_bytes), (header, frame));
            }
        }
    }
}

#[test]
fn leaves_source_bytes_and_opaque_trailing_data_untouched() {
    let mut input = vec![0xab; 8192];
    input[..SHORT.len()].copy_from_slice(SHORT);
    // Neither a sync inside the declared frame nor bytes after it are scanned.
    input[100..100 + LONG.len()].copy_from_slice(LONG);
    input[512..512 + LONG.len()].copy_from_slice(LONG);
    let before = input.clone();
    let extent = probe_dts_frame_extent(&input, LIMITS).unwrap();
    assert_eq!(extent.frame_bytes(), 512);
    assert_eq!(input, before);
    assert_eq!(
        probe_dts_frame_extent(&input[512..], LIMITS)
            .unwrap()
            .frame_bytes(),
        1 << 20
    );
    // The five remaining prefix bits and arbitrary following bytes are opaque.
    input[8] |= 0x1f;
    assert_eq!(probe_dts_frame_extent(&input, LIMITS), Ok(extent));
}

#[test]
fn reuses_core_validation_without_accepting_extensions_as_core() {
    for end in 4..=15 {
        assert_eq!(
            probe_dts_frame_extent(&CORE[..end], LIMITS),
            probe_dts_core_header(&CORE[..end]).map(DtsFrameExtent::Core)
        );
    }
    let header = probe_dts_core_header(CORE).unwrap();
    assert_eq!(
        probe_dts_frame_extent(CORE, LIMITS),
        Ok(DtsFrameExtent::Core(header))
    );
    assert_eq!(
        probe_dts_core_header(SHORT),
        Err(Error::Unsupported("DTS extension-substream input"))
    );
    let mut termination = CORE[..15].to_vec();
    termination[4] &= 0x7f;
    assert_eq!(
        probe_dts_frame_extent(&termination, LIMITS),
        probe_dts_core_header(&termination).map(DtsFrameExtent::Core)
    );
    assert!(matches!(
        probe_dts_frame_extent(
            CORE,
            DtsFrameExtentLimits {
                maximum_frame_bytes: header.frame_bytes - 1,
                ..LIMITS
            }
        ),
        Err(Error::Limit { .. })
    ));
    assert!(matches!(
        probe_dts_frame_extent(
            CORE,
            DtsFrameExtentLimits {
                maximum_header_bytes: 1,
                ..LIMITS
            }
        ),
        Err(Error::Limit { .. })
    ));
}

#[test]
fn rejects_unknown_sync_and_other_carriers_without_search() {
    for input in [
        &[0xfe, 0x7f, 0x01, 0x80][..], // 16-bit LE core
        &[0x1f, 0xff, 0xe8, 0][..],    // 14-bit BE core
        &[0xff, 0x1f, 0, 0xe8][..],    // 14-bit LE core
        &[0x25, 0x20, 0x58, 0x64][..], // word-swapped extension sync
        &[0, 0x64, 0x58, 0x20, 0x25][..],
    ] {
        assert!(matches!(
            probe_dts_frame_extent(input, LIMITS),
            Err(Error::Invalid(_))
        ));
    }
}
