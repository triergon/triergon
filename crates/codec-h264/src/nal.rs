//! NAL unit header and RBSP extraction (clauses 7.3.1, 7.4.1).

use triergon_codec::{BudgetVec, StorageError};

/// Zero bytes kept after an RBSP, for `BitReader`'s eight-byte loads.
pub(crate) const RBSP_PAD: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NalHeader {
    pub nal_ref_idc: u8,
    pub nal_unit_type: u8,
}

/// `None` for an empty unit or a set forbidden_zero_bit.
pub(crate) fn parse_header(nal: &[u8]) -> Option<NalHeader> {
    match triergon_framing::NalHeader::parse(triergon_framing::Codec::Avc, nal).ok()? {
        triergon_framing::NalHeader::Avc {
            nal_ref_idc,
            nal_unit_type,
        } => Some(NalHeader {
            nal_ref_idc,
            nal_unit_type,
        }),
        triergon_framing::NalHeader::Hevc { .. } => None,
    }
}

/// Remove emulation_prevention_three_byte (clause 7.4.1) from `src` into `dst`, append
/// zero padding, and return the RBSP length (trailing zero bytes trimmed). `dst` is
/// charged for the whole payload before the pieces are copied.
pub(crate) fn unescape(src: &[u8], dst: &mut BudgetVec<u8>) -> Result<usize, StorageError> {
    dst.clear();
    dst.try_reserve_exact(src.len() + RBSP_PAD)?;
    for run in triergon_bitstream::runs(src) {
        dst.try_extend_from_slice(run)?;
    }
    let mut len = dst.len();
    while len > 0 && dst[len - 1] == 0 {
        len -= 1;
    }
    dst.truncate(len);
    dst.try_resize(len + RBSP_PAD, 0)?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use triergon_codec::Budget;

    #[test]
    fn split_and_unescape() {
        let s = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 0, 0, 3, 1, 0, 0, 0, 0, 1, 0x65, 9,
        ];
        let v: Vec<&[u8]> = triergon_framing::AnnexBUnits::new(&s).collect();
        assert_eq!(
            v,
            vec![&[0x67u8, 1, 2][..], &[0x68, 0, 0, 3, 1][..], &[0x65, 9][..]]
        );
        let mut d = BudgetVec::new(&Budget::new(1 << 20).unwrap());
        let n = unescape(v[1], &mut d).unwrap();
        assert_eq!(&d[..n], &[0x68, 0, 0, 1]);
        assert!(d.len() >= n + RBSP_PAD);
    }
}
