use crate::common;
use triergon_core::Budget;
use triergon_isobmff::{BoxView, RecoveryKind};

pub fn boxed(pool: &Budget, kind: &[u8; 4], payload: &[u8], start: u64) -> BoxView {
    let mut wire = u32::try_from(payload.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    wire.extend(kind);
    wire.extend(payload);
    BoxView::new(common::bytes(pool, &wire), start).unwrap()
}

pub fn descriptions(
    pool: &Budget,
    kind: RecoveryKind,
    version: u8,
    length: u32,
    default: u32,
    values: &[i16],
    start: u64,
) -> BoxView {
    let mut body = vec![version, 0, 0, 0];
    body.extend(match kind {
        RecoveryKind::Roll => b"roll",
        RecoveryKind::Prol => b"prol",
    });
    if version >= 1 {
        body.extend(length.to_be_bytes());
    }
    if version >= 2 {
        body.extend(default.to_be_bytes());
    }
    body.extend(u32::try_from(values.len()).unwrap().to_be_bytes());
    for value in values {
        if version >= 1 && length == 0 {
            body.extend(2u32.to_be_bytes());
        }
        body.extend(value.to_be_bytes());
    }
    boxed(pool, b"sgpd", &body, start)
}

pub fn mappings(
    pool: &Budget,
    kind: RecoveryKind,
    version: u8,
    parameter: u32,
    runs: &[(u32, u32)],
) -> BoxView {
    let mut body = vec![version, 0, 0, 0];
    body.extend(match kind {
        RecoveryKind::Roll => b"roll",
        RecoveryKind::Prol => b"prol",
    });
    if version == 1 {
        body.extend(parameter.to_be_bytes());
    }
    body.extend(u32::try_from(runs.len()).unwrap().to_be_bytes());
    for (count, index) in runs {
        body.extend(count.to_be_bytes());
        body.extend(index.to_be_bytes());
    }
    boxed(pool, b"sbgp", &body, 2048)
}
