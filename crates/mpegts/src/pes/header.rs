//! The PES header fields: prefix, flags and timestamps.
use triergon_core::{Budget, Bytes, Error, Result, WorkBudget};

/// PTS_DTS_flags set to the forbidden value 01.
pub(crate) const FORBIDDEN_TIME_FLAGS: &str = "forbidden PES time flags";

/// A PTS or DTS field whose prefix or marker bits are wrong.
pub(crate) const TIMESTAMP_MARKERS: &str = "PES timestamp markers";

use super::{HeaderBytes, PesHeader};

fn take<'a>(bytes: &'a [u8], position: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = position.checked_add(count).ok_or(Error::Overflow)?;
    let value = bytes
        .get(*position..end)
        .ok_or(Error::Invalid("PES optional-field length"))?;
    *position = end;
    Ok(value)
}

pub(crate) fn timestamp(bytes: &[u8], prefix: u8) -> Result<u64> {
    if bytes.len() != 5
        || bytes[0] >> 4 != prefix
        || bytes[0] & 1 == 0
        || bytes[2] & 1 == 0
        || bytes[4] & 1 == 0
    {
        return Err(Error::Invalid(TIMESTAMP_MARKERS));
    }
    Ok(timestamp_bits(bytes))
}
/// The caller independently checks the five-byte field's applicability.
pub(crate) fn timestamp_bits(bytes: &[u8]) -> u64 {
    (u64::from(bytes[0] & 0x0e) << 29)
        | (u64::from(bytes[1]) << 22)
        | (u64::from(bytes[2] & 0xfe) << 14)
        | (u64::from(bytes[3]) << 7)
        | u64::from(bytes[4] >> 1)
}

/// Whether the three marker bits of a five-byte timestamp field are set.
pub(crate) fn timestamp_markers(bytes: &[u8]) -> bool {
    bytes[0] & 1 != 0 && bytes[2] & 1 != 0 && bytes[4] & 1 != 0
}

/// Parse a complete timestamp-only header without stepping its three stages.
/// Refusal leaves the caller free to use the ordinary parser and diagnostics.
pub(crate) fn parse_timed_header(
    bytes: &[u8],
    maximum: usize,
    work: &WorkBudget,
) -> Option<(PesHeader, Option<usize>)> {
    let prefix = bytes.first_chunk::<9>()?;
    if prefix[..3] != [0, 0, 1]
        || prefix[6] & 0xf0 != 0x80
        || !matches!((prefix[7], prefix[8]), (0x80, 5) | (0xc0, 10))
    {
        return None;
    }
    let used = 9 + usize::from(prefix[8]);
    if used > maximum || work.remaining() < used as u64 + 3 {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([prefix[4], prefix[5]]));
    let declared = (length != 0).then_some(length + 6);
    if declared.is_some_and(|total| total < used || total > maximum) {
        return None;
    }
    let bytes = bytes.get(..used)?;
    let timing = prefix[7] >> 6;
    let pts_raw33 = Some(timestamp(&bytes[9..14], timing).ok()?);
    let dts_raw33 = if timing == 3 {
        Some(timestamp(&bytes[14..19], 1).ok()?)
    } else {
        None
    };
    Some((
        PesHeader {
            stream_id: prefix[3],
            declared_packet_length: u16::from_be_bytes([prefix[4], prefix[5]]),
            pts_raw33,
            dts_raw33,
            data_alignment_indicator: prefix[6] & 4 != 0,
            priority: prefix[6] & 8 != 0,
            raw: HeaderBytes::inline(bytes)?,
        },
        declared,
    ))
}

pub(crate) fn parse_header(budget: &Budget, bytes: &[u8]) -> Result<PesHeader> {
    let used = bytes.len();
    if used < 9 || bytes[6] & 0xc0 != 0x80 || used != 9 + usize::from(bytes[8]) {
        return Err(Error::Invalid("PES optional-header prefix"));
    }
    if bytes[6] & 0x30 != 0 {
        return Err(Error::Unsupported("scrambled PES"));
    }
    let flags = bytes[7];
    let timing = flags >> 6;
    if timing == 1 {
        return Err(Error::Invalid(FORBIDDEN_TIME_FLAGS));
    }
    let mut position = 9;
    let pts_raw33 = if timing >= 2 {
        Some(timestamp(take(bytes, &mut position, 5)?, timing)?)
    } else {
        None
    };
    let dts_raw33 = if timing == 3 {
        Some(timestamp(take(bytes, &mut position, 5)?, 1)?)
    } else {
        None
    };
    if flags & 32 != 0 {
        let field = take(bytes, &mut position, 6)?;
        let extension = (u16::from(field[4] & 3) << 7) | u16::from(field[5] >> 1);
        if field[0] & 4 == 0
            || field[2] & 4 == 0
            || field[4] & 4 == 0
            || field[5] & 1 == 0
            || extension > 299
        {
            return Err(Error::Invalid("ESCR markers/extension"));
        }
    }
    if flags & 16 != 0 {
        let field = take(bytes, &mut position, 3)?;
        if field[0] & 128 == 0 || field[2] & 1 == 0 {
            return Err(Error::Invalid("ES rate markers"));
        }
    }
    if flags & 8 != 0 {
        take(bytes, &mut position, 1)?;
    }
    if flags & 4 != 0 && take(bytes, &mut position, 1)?[0] & 128 == 0 {
        return Err(Error::Invalid("copy-info marker"));
    }
    if flags & 2 != 0 {
        take(bytes, &mut position, 2)?;
    }
    if flags & 1 != 0 {
        let ext = take(bytes, &mut position, 1)?[0];
        if ext & 0x0e != 14 {
            return Err(Error::Invalid("PES extension reserved bits"));
        }
        if ext & 128 != 0 {
            take(bytes, &mut position, 16)?;
        }
        if ext & 64 != 0 {
            let size = usize::from(take(bytes, &mut position, 1)?[0]);
            take(bytes, &mut position, size)?;
        }
        if ext & 32 != 0 {
            let field = take(bytes, &mut position, 2)?;
            if field[0] & 128 == 0 || field[1] & 128 == 0 {
                return Err(Error::Invalid("PES sequence markers"));
            }
        }
        if ext & 16 != 0 && take(bytes, &mut position, 2)?[0] & 0xc0 != 0x40 {
            return Err(Error::Invalid("P-STD prefix"));
        }
        if ext & 1 != 0 {
            let length = take(bytes, &mut position, 1)?[0];
            if length & 128 == 0 {
                return Err(Error::Invalid("PES extension-2 marker"));
            }
            let field = take(bytes, &mut position, usize::from(length & 127))?;
            if let Some(first) = field.first() {
                if first & 128 != 0 && first & 1 == 0 {
                    if field.len() < 6 {
                        return Err(Error::Invalid("PES TREF length"));
                    }
                    // Reserved prefix is retained; validate the three marker bits.
                    let tref = &field[1..6];
                    if tref[0] & 1 == 0 || tref[2] & 1 == 0 || tref[4] & 1 == 0 {
                        return Err(Error::Invalid("PES TREF markers"));
                    }
                }
            } else {
                return Err(Error::Invalid("empty PES extension-2"));
            }
        }
    }
    if bytes[position..].iter().any(|byte| *byte != 255) {
        return Err(Error::Invalid("PES header stuffing"));
    }
    let raw = match HeaderBytes::inline(bytes) {
        Some(raw) => raw,
        None => HeaderBytes::Extended(Bytes::copy_from_slice(budget, bytes)?),
    };
    Ok(PesHeader {
        stream_id: bytes[3],
        declared_packet_length: u16::from_be_bytes([bytes[4], bytes[5]]),
        pts_raw33,
        dts_raw33,
        data_alignment_indicator: bytes[6] & 4 != 0,
        priority: bytes[6] & 8 != 0,
        raw,
    })
}

#[cfg(test)]
mod timed_header_tests {
    use super::*;
    use crate::pes::HEADER_MAX;
    use crate::pes::PesAssembler;
    use crate::{AssemblyStep, ParseLimits, SourceEpochs, SourceRange};
    use std::sync::atomic::AtomicBool;
    use triergon_core::SourceId;

    fn stamp(raw: u64, prefix: u8) -> [u8; 5] {
        [
            (prefix << 4) | u8::try_from((raw >> 29) & 0x0e).unwrap() | 1,
            ((raw >> 22) & 255).try_into().unwrap(),
            u8::try_from((raw >> 14) & 0xfe).unwrap() | 1,
            ((raw >> 7) & 255).try_into().unwrap(),
            u8::try_from((raw << 1) & 0xfe).unwrap() | 1,
        ]
    }

    fn header(pts: u64, dts: Option<u64>, control: u8) -> ([u8; HEADER_MAX], usize) {
        let mut raw = [0; HEADER_MAX];
        let (timing, used) = if dts.is_some() { (3, 19) } else { (2, 14) };
        raw[..9].copy_from_slice(&[
            0,
            0,
            1,
            0xe0,
            0,
            0,
            control,
            timing << 6,
            u8::try_from(used - 9).unwrap(),
        ]);
        raw[9..14].copy_from_slice(&stamp(pts, timing));
        if let Some(dts) = dts {
            raw[14..19].copy_from_slice(&stamp(dts, 1));
        }
        (raw, used)
    }

    #[test]
    fn complete_timestamps_match_general_headers_and_keep_admission_bounds() {
        let pool = Budget::new(4096).unwrap();
        for pts in [0, 1, 0x0001_0000_0011, (1 << 33) - 1] {
            for dts in [None, Some(0), Some((1 << 33) - 1)] {
                for control in 0x80..=0x8f {
                    let (mut raw, used) = header(pts, dts, control);
                    for length in [0, u16::try_from(used - 6).unwrap(), u16::MAX] {
                        raw[4..6].copy_from_slice(&length.to_be_bytes());
                        let work = WorkBudget::new(used as u64 + 3);
                        let (fast, total) =
                            parse_timed_header(&raw[..used], 1 << 20, &work).unwrap();
                        let slow = parse_header(&pool, &raw[..used]).unwrap();
                        assert_eq!(format!("{fast:?}"), format!("{slow:?}"));
                        assert_eq!(fast.pts_raw33, Some(pts));
                        assert_eq!(fast.dts_raw33, dts);
                        assert_eq!(total, (length != 0).then_some(usize::from(length) + 6));
                        assert_eq!(work.spent(), 0);
                        let too_small = if length == 0 {
                            used - 1
                        } else {
                            usize::from(length) + 5
                        };
                        assert!(parse_timed_header(&raw[..used], too_small, &work).is_none());
                        for units in [0, used as u64 + 2] {
                            assert!(
                                parse_timed_header(&raw[..used], 1 << 20, &WorkBudget::new(units))
                                    .is_none()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_and_incomplete_timestamps_fall_back_to_the_general_parser() {
        let pool = Budget::new(4096).unwrap();
        for dts in [None, Some((1 << 33) - 1)] {
            let (raw, used) = header(0x0001_0000_0011, dts, 0x84);
            let work = WorkBudget::new(4096);
            for length in 0..used {
                assert!(parse_timed_header(&raw[..length], 1 << 20, &work).is_none());
            }
            for at in std::iter::once(6).chain(9..used) {
                for bit in 0..8 {
                    let mut changed = raw;
                    changed[at] ^= 1 << bit;
                    let fast = parse_timed_header(&changed[..used], 1 << 20, &work);
                    match parse_header(&pool, &changed[..used]) {
                        Ok(slow) => {
                            assert_eq!(format!("{:?}", fast.unwrap().0), format!("{slow:?}"));
                        }
                        Err(_) => assert!(fast.is_none()),
                    }
                }
            }
            for at in [0, 1, 2, 7, 8] {
                let mut changed = raw;
                changed[at] ^= 1;
                assert!(parse_timed_header(&changed[..used], 1 << 20, &work).is_none());
            }
            let mut short = raw;
            short[4..6].copy_from_slice(&u16::try_from(used - 7).unwrap().to_be_bytes());
            assert!(parse_timed_header(&short[..used], 1 << 20, &work).is_none());
        }
        let (mut raw, used) = header(17, None, 0x84);
        raw[8] += 1;
        raw[used] = 0xff;
        assert!(parse_header(&pool, &raw[..used + 1]).is_ok());
        assert!(parse_timed_header(&raw[..used + 1], 1 << 20, &WorkBudget::new(4096)).is_none());
    }

    #[test]
    fn extended_header_retains_every_byte_after_a_pressure_retry() {
        let pool = Budget::new(4096).unwrap();
        let source = SourceId(1);
        let mut raw = [0xff; HEADER_MAX + 1];
        let length = u16::try_from(raw.len() - 6).unwrap().to_be_bytes();
        raw[..9].copy_from_slice(&[0, 0, 1, 0xe0, length[0], length[1], 0x80, 0, 255]);
        let mut assembler = PesAssembler::new(
            256,
            true,
            pool.clone(),
            ParseLimits::default(),
            SourceEpochs::default(),
        )
        .unwrap();
        assembler
            .push(
                Bytes::copy_from_slice(&pool, &raw).unwrap(),
                true,
                source,
                0,
            )
            .unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            assembler.poll(&mut WorkBudget::new(HEADER_MAX as u64 + 2), &cancel),
            Ok(AssemblyStep::Yield)
        ));
        assert_eq!(assembler.partial_header(), &raw[..HEADER_MAX]);
        let pressure = pool.reserve(pool.available()).unwrap();
        let mut work = WorkBudget::new(1);
        assert!(matches!(
            assembler.poll(&mut work, &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert!(assembler.blocked_growth_floor().is_some());
        assert!(matches!(
            assembler.poll(&mut WorkBudget::new(1), &cancel),
            Err(Error::Backpressure { .. })
        ));
        assert_eq!(assembler.partial_header(), &raw[..HEADER_MAX]);
        drop(pressure);
        let AssemblyStep::Output(pes) = assembler.poll(&mut WorkBudget::new(2), &cancel).unwrap()
        else {
            panic!("expected the completed PES");
        };
        assert_eq!(pes.header.as_bytes(), &raw[..HEADER_MAX]);
        assert_eq!(pes.payload().next(), Some(&raw[HEADER_MAX..]));
        assert_eq!(
            pes.header_source_ranges.as_slice(),
            &[SourceRange {
                entity: source,
                offset: 0,
                bytes: HEADER_MAX,
            }]
        );
        assert!(assembler.blocked_growth_floor().is_none());
    }
}
