//! Authored H.222.0 (08/2023) Tables 2-109, 2-110, 2-142 and 2-143 vectors.
use triergon_mpegts::psi::{Descriptor, Descriptors, LcevcVideo};

fn descriptor(tag: u8, data: &[u8]) -> Descriptor<'_> {
    Descriptor {
        tag,
        data,
        source: data,
    }
}

#[test]
fn video_exact_fields_and_source_bytes() {
    let raw = [0x3f, 5, 0x17, 0xa5, 0x3c, 0xaf, 0x89];
    let d = Descriptors::new(&raw).next().unwrap().unwrap();
    assert_eq!(d.source, raw);
    assert_eq!(
        d.lcevc_video().unwrap(),
        LcevcVideo {
            lcevc_stream_tag: 0xa5,
            profile_idc: 3,
            level_idc: 12,
            sublevel_idc: 2,
            processed_planes_type_flag: true,
            picture_type_bit_flag: false,
            field_type_bit_flag: true,
            hdr_wcg_idc: 2,
            video_properties_tag: 9,
        }
    );
    let v = descriptor(0x3f, &[0x17, 0, 0xff, 0x57, 0xcf])
        .lcevc_video()
        .unwrap();
    assert_eq!((v.profile_idc, v.level_idc, v.sublevel_idc), (15, 15, 1));
    assert!(!v.processed_planes_type_flag);
    assert!(v.picture_type_bit_flag);
    assert!(!v.field_type_bit_flag);
    assert_eq!((v.hdr_wcg_idc, v.video_properties_tag), (3, 15));
}

#[test]
fn video_rejects_every_truncation_extra_byte_and_reserved_bit_damage() {
    let good = [0x17, 1, 0x23, 7, 0];
    for end in 0..good.len() {
        assert!(descriptor(0x3f, &good[..end]).lcevc_video().is_err());
    }
    assert!(
        descriptor(0x3f, &[0x17, 1, 0x23, 7, 0, 0])
            .lcevc_video()
            .is_err()
    );
    for bit in 0..3 {
        let mut bad = good;
        bad[3] ^= 1 << bit;
        assert!(descriptor(0x3f, &bad).lcevc_video().is_err());
    }
    for bit in 4..6 {
        let mut bad = good;
        bad[4] |= 1 << bit;
        assert!(descriptor(0x3f, &bad).lcevc_video().is_err());
    }
}

#[test]
fn linkage_preserves_order_duplicates_zero_and_borrowed_storage() {
    let raw = [0x3f, 6, 0x18, 4, 0xff, 0, 0xff, 0x47];
    let d = Descriptors::new(&raw).next().unwrap().unwrap();
    let tags = d.lcevc_linkage().unwrap().lcevc_stream_tags;
    assert_eq!(tags, &[0xff, 0, 0xff, 0x47]);
    assert_eq!(tags.as_ptr(), raw[4..].as_ptr());
    assert_eq!(d.source, raw);
    assert!(
        descriptor(0x3f, &[0x18, 0])
            .lcevc_linkage()
            .unwrap()
            .lcevc_stream_tags
            .is_empty()
    );
}

#[test]
fn linkage_checks_exact_count_and_descriptor_bound() {
    for bad in [&[0x18][..], &[0x18, 1], &[0x18, 0, 1], &[0x18, 2, 1]] {
        assert!(descriptor(0x3f, bad).lcevc_linkage().is_err());
    }
    let mut maximum = [0xff; 255];
    maximum[0] = 0x18;
    maximum[1] = 253;
    assert_eq!(
        descriptor(0x3f, &maximum)
            .lcevc_linkage()
            .unwrap()
            .lcevc_stream_tags
            .len(),
        253
    );
    maximum[1] = 254;
    assert!(descriptor(0x3f, &maximum).lcevc_linkage().is_err());
    let mut oversized = [0; 256];
    oversized[0] = 0x18;
    oversized[1] = 254;
    assert!(descriptor(0x3f, &oversized).lcevc_linkage().is_err());
}

#[test]
fn unknown_and_dvb_extensions_remain_opaque_and_validation_unchanged() {
    for tag in [0x3f, 0x7f, 0x33, 0x36] {
        for data in [&[][..], &[0x19, 0xaa], &[0x17], &[0x18, 255]] {
            let d = descriptor(tag, data);
            assert!(d.validate_selected_syntax().is_ok());
            if tag != 0x3f || data.first() != Some(&0x17) {
                assert!(d.lcevc_video().is_err());
            }
            assert!(d.lcevc_linkage().is_err());
        }
    }
    let raw = [0x3f, 2, 0x19, 0xaa, 0x7f, 1, 0x18];
    let descriptors: Vec<_> = Descriptors::new(&raw).map(Result::unwrap).collect();
    assert_eq!(descriptors[0].source, &raw[..4]);
    assert_eq!(descriptors[1].source, &raw[4..]);
}
