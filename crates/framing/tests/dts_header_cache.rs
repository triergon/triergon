//! New static resource regression + source-semantic equivalence controls.
use triergon_framing::{
    DtsCoreField as F, DtsInspectionViews, DtsNormalSourceFrame, DtsPackedHeader,
    inspect_dts_header_bits,
};

fn authored(cpf: bool) -> Vec<u8> {
    let mut fields = vec![
        (32, 0x7ffe8001_u32),
        (1, 1),
        (5, 31),
        (1, u32::from(cpf)),
        (7, 15),
        (14, 95),
        (6, 2),
        (4, 13),
        (5, 15),
        (1, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (1, 0),
        (3, 0),
        (1, 0),
        (1, 0),
        (2, 0),
        (1, 1),
    ];
    if cpf {
        fields.push((16, 0xbeef));
    }
    fields.extend([(1, 0), (4, 7), (2, 0), (3, 0), (1, 0), (1, 0), (4, 11)]);
    let bits: String = fields
        .into_iter()
        .map(|(width, value)| format!("{value:0width$b}"))
        .collect();
    assert_eq!(bits.len(), if cpf { 120 } else { 104 });
    bits.as_bytes()
        .as_chunks::<8>()
        .0
        .iter()
        .map(|part| u8::from_str_radix(std::str::from_utf8(part).unwrap(), 2).unwrap())
        .collect()
}

// Preserve the existing const accessor shape, not just its runtime values.
const fn cached_views(header: &DtsPackedHeader) -> DtsInspectionViews {
    header.views()
}

#[test]
fn retained_header_and_frame_metadata_fit_the_new_static_capacity_contract() {
    assert!(
        size_of::<DtsPackedHeader>() <= 128,
        "DtsPackedHeader {} exceeds128",
        size_of::<DtsPackedHeader>()
    );
    assert!(
        size_of::<DtsNormalSourceFrame>() <= 384,
        "DtsNormalSourceFrame {} exceeds384",
        size_of::<DtsNormalSourceFrame>()
    );
}

#[test]
fn all226_prefixes_ignore_every_unobserved_bit_in_equality_and_all27_descriptors() {
    let mut controls = 0;
    for cpf in [false, true] {
        let source = authored(cpf);
        for available in 0..=source.len() * 8 {
            controls += 1;
            let baseline = inspect_dts_header_bits(&source, available).unwrap();
            let mut poisoned = source.clone();
            for bit in available..source.len() * 8 {
                poisoned[bit / 8] ^= 1 << (7 - bit % 8);
            }
            let observed = inspect_dts_header_bits(&poisoned, available).unwrap();
            assert_eq!(observed, baseline, "cpf{cpf} bits{available}");
            for field in F::ALL {
                assert_eq!(observed.field(field), baseline.field(field));
            }
            assert_eq!(cached_views(&observed), cached_views(&baseline));
        }
    }
    assert_eq!(controls, 226);
}

#[test]
fn cached_header_excludes_complete_body_lookahead_after104_or120_syntax_bits() {
    for cpf in [false, true] {
        let source = authored(cpf);
        let header = inspect_dts_header_bits(&source, source.len() * 8).unwrap();
        for tail in [[0, 0], [0xff, 0xff], [0x7f, 0xfe]] {
            let mut body = source.clone();
            body.extend(tail);
            let observed = inspect_dts_header_bits(&body, body.len() * 8).unwrap();
            assert_eq!(observed, header);
            assert_eq!(
                observed.required_header_bits(),
                Some(if cpf { 120 } else { 104 })
            );
            for field in F::ALL {
                assert_eq!(observed.field(field), header.field(field));
            }
            assert_eq!(cached_views(&observed), cached_views(&header));
        }
    }
}
