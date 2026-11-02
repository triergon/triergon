//! Matroska Colour and static HDR source facts (RFC 9559 §5.1.4.1.28).
//! No decoder, display policy or native library belongs in this module.
use crate::{Error, Result, ebml};

/// All ten source floats, without rational quantization or filling absent fields.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MasteringMetadata {
    /// CIE xy coordinates in R, G, B, white-point order.
    pub chromaticity: [Option<f64>; 8],
    pub luminance_max: Option<f64>,
    pub luminance_min: Option<f64>,
}

/// Numeric colour identifiers stay numeric, including future registry values.
/// Defaults apply only to elements for which the Matroska schema defines them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Colour {
    pub matrix_coefficients: u64,
    pub bits_per_channel: u64,
    pub chroma_subsampling: [Option<u64>; 2],
    pub cb_subsampling: [Option<u64>; 2],
    pub chroma_siting: [u64; 2],
    pub range: u64,
    pub transfer_characteristics: u64,
    pub primaries: u64,
    pub max_cll: Option<u64>,
    pub max_fall: Option<u64>,
    pub mastering: Option<MasteringMetadata>,
}

impl Default for Colour {
    fn default() -> Self {
        Self {
            matrix_coefficients: 2,
            bits_per_channel: 0,
            chroma_subsampling: [None; 2],
            cb_subsampling: [None; 2],
            chroma_siting: [0; 2],
            range: 0,
            transfer_characteristics: 2,
            primaries: 2,
            max_cll: None,
            max_fall: None,
            mastering: None,
        }
    }
}

fn mark(seen: &mut u16, index: u32) -> Result<()> {
    let bit = 1 << index;
    if *seen & bit != 0 {
        return Err(Error::Invalid("duplicate colour metadata"));
    }
    *seen |= bit;
    Ok(())
}

fn mastering(data: &[u8]) -> Result<MasteringMetadata> {
    let mut out = MasteringMetadata::default();
    let mut seen = 0;
    ebml::children(data, |id, bytes| {
        if !(0x55d1..=0x55da).contains(&id) {
            return Ok(());
        }
        let index = (id - 0x55d1) as u32;
        mark(&mut seen, index)?;
        let value = match bytes.len() {
            0 => 0.,
            4 => f64::from(f32::from_be_bytes(
                bytes
                    .try_into()
                    .map_err(|_| Error::Invalid("HDR float width"))?,
            )),
            8 => f64::from_be_bytes(
                bytes
                    .try_into()
                    .map_err(|_| Error::Invalid("HDR float width"))?,
            ),
            _ => return Err(Error::Invalid("HDR float width")),
        };
        if !value.is_finite() || value < 0. || (index < 8 && value > 1.) {
            return Err(Error::Invalid("HDR float range"));
        }
        match index {
            0..=7 => out.chromaticity[index as usize] = Some(value),
            8 => out.luminance_max = Some(value),
            _ => out.luminance_min = Some(value),
        }
        Ok(())
    })?;
    Ok(out)
}

pub(crate) fn parse(data: &[u8]) -> Result<Colour> {
    let mut out = Colour::default();
    let mut seen = 0;
    ebml::children(data, |id, bytes| {
        if id == 0x55d0 {
            mark(&mut seen, 13)?;
            out.mastering = Some(mastering(bytes)?);
            return Ok(());
        }
        if !(0x55b1..=0x55bd).contains(&id) {
            return Ok(());
        }
        mark(&mut seen, (id - 0x55b1) as u32)?;
        let v = if bytes.is_empty() && matches!(id, 0x55b1 | 0x55ba | 0x55bb) {
            2
        } else {
            ebml::uint(bytes)?
        };
        match id {
            0x55b1 => out.matrix_coefficients = v,
            0x55b2 => out.bits_per_channel = v,
            0x55b3..=0x55b4 => out.chroma_subsampling[(id - 0x55b3) as usize] = Some(v),
            0x55b5..=0x55b6 => out.cb_subsampling[(id - 0x55b5) as usize] = Some(v),
            0x55b7..=0x55b8 => out.chroma_siting[(id - 0x55b7) as usize] = v,
            0x55b9 => out.range = v,
            0x55ba => out.transfer_characteristics = v,
            0x55bb => out.primaries = v,
            0x55bc => out.max_cll = Some(v),
            _ => out.max_fall = Some(v),
        }
        Ok(())
    })?;
    Ok(out)
}

/// Bounded nonnegative rational for the pinned public numeric view.
/// Continued fractions operate on the exact IEEE binary value. Source floats are
/// never replaced by this explicit, optional compatibility representation.
fn bounded_rational(value: f64) -> Result<[i64; 2]> {
    const MAX: u128 = i32::MAX as u128;
    if !value.is_finite() || value < 0. || value > MAX as f64 {
        return Err(Error::Overflow);
    }
    let half_minimum = 0.5 / MAX as f64;
    if value < half_minimum {
        return Ok([0, 1]);
    }
    // The native public view rounds the representable half-minimum boundary
    // toward 1/MAX. Keep this exact comparison separate from source storage.
    if value == half_minimum {
        return Ok([1, MAX as i64]);
    }
    let bits = value.to_bits();
    let significand = u128::from((bits & ((1_u64 << 52) - 1)) | (1_u64 << 52));
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023 - 52;
    let (mut n, mut d) = if exponent >= 0 {
        (
            significand
                .checked_shl(exponent as u32)
                .ok_or(Error::Overflow)?,
            1,
        )
    } else {
        (
            significand,
            1_u128
                .checked_shl((-exponent) as u32)
                .ok_or(Error::Overflow)?,
        )
    };
    let original = (n, d);
    let (mut p0, mut q0, mut p1, mut q1) = (0_u128, 1_u128, 1_u128, 0_u128);
    loop {
        let a = n / d;
        let p2 = p0 + a * p1;
        let q2 = q0 + a * q1;
        if p2 > MAX || q2 > MAX {
            let pn = (MAX - p0).checked_div(p1).unwrap_or(MAX);
            let qn = (MAX - q0).checked_div(q1).unwrap_or(MAX);
            let k = pn.min(qn);
            let (p, q) = (p0 + k * p1, q0 + k * q1);
            let error = |p: u128, q: u128| (original.0 * q).abs_diff(original.1 * p);
            return if q1 != 0 && error(p1, q1) * q <= error(p, q) * q1 {
                Ok([p1 as i64, q1 as i64])
            } else {
                Ok([p as i64, q as i64])
            };
        }
        let remainder = n % d;
        if remainder == 0 {
            return Ok([p2 as i64, q2 as i64]);
        }
        (p0, q0, p1, q1) = (p1, q1, p2, q2);
        (n, d) = (d, remainder);
    }
}

impl Colour {
    /// Pinned FFmpeg 9 packet-metadata view, encoded as the 32 protocol integers.
    /// It omits unavailable bit depth, incomplete/zero primaries and incomplete
    /// light levels exactly as the native public API does. Use the source fields
    /// above for complete facts; this view does not delete or mutate them.
    pub fn ffmpeg9(&self) -> Result<[i64; 32]> {
        let mut out = [0; 32];
        let code = |v| i32::try_from(v).map(i64::from).map_err(|_| Error::Overflow);
        out[0] = code(self.matrix_coefficients)?;
        out[1] = if self.range <= 2 {
            self.range as i64
        } else {
            0
        };
        out[2] = code(self.transfer_characteristics)?;
        out[3] = code(self.primaries)?;
        out[5] = match self.chroma_siting {
            [1, 1] => 3,
            [1, 2] => 1,
            [2, 1] => 4,
            [2, 2] => 2,
            _ => 0,
        };
        if let (Some(cll), Some(fall)) = (self.max_cll, self.max_fall) {
            if cll > 0 && fall > 0 {
                out[6] = 1;
                out[7] = i64::from(u32::try_from(cll).map_err(|_| Error::Overflow)?);
                out[8] = i64::from(u32::try_from(fall).map_err(|_| Error::Overflow)?);
            }
        }
        if let Some(m) = self.mastering {
            let primaries = m.chromaticity.iter().all(|v| v.is_some_and(|v| v > 0.));
            let luminance = m.luminance_max.is_some_and(|v| v > 0.) && m.luminance_min.is_some();
            out[9] = i64::from(primaries || luminance);
            out[10] = i64::from(primaries);
            out[11] = i64::from(luminance);
            if primaries {
                for (i, value) in m.chromaticity.into_iter().enumerate() {
                    out[12 + 2 * i..14 + 2 * i].copy_from_slice(&bounded_rational(
                        value.ok_or(Error::Invalid("missing primary"))?,
                    )?);
                }
            }
            if luminance {
                out[28..30].copy_from_slice(&bounded_rational(
                    m.luminance_min
                        .ok_or(Error::Invalid("missing minimum luminance"))?,
                )?);
                out[30..32].copy_from_slice(&bounded_rational(
                    m.luminance_max
                        .ok_or(Error::Invalid("missing maximum luminance"))?,
                )?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_binary_rationals_and_bounded_approximation() {
        for (x, expected) in [
            (0., [0, 1]),
            (0.68, [17, 25]),
            (0.3127, [3127, 10000]),
            (f64::from(0.68_f32), [11408507, 16777216]),
            (1000., [1000, 1]),
            (f64::MIN_POSITIVE, [0, 1]),
        ] {
            assert_eq!(bounded_rational(x).unwrap(), expected);
        }
        let half = 0.5 / i32::MAX as f64;
        assert_eq!(bounded_rational(half).unwrap(), [1, i64::from(i32::MAX)]);
        assert_eq!(
            bounded_rational(f64::from_bits(half.to_bits() - 1)).unwrap(),
            [0, 1]
        );
        assert_eq!(
            bounded_rational(f64::from_bits(half.to_bits() + 1)).unwrap(),
            [1, i64::from(i32::MAX)]
        );
        for x in [f64::NAN, f64::INFINITY, -1., f64::MAX] {
            assert!(bounded_rational(x).is_err());
        }
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    fn e(id: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out: Vec<_> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        assert!(bytes.len() < 127);
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
        out
    }
    #[test]
    fn defaults_zero_widths_future_values_and_source_precision() {
        let data = [
            e(0x55b1, &[]),
            e(0x55ba, &[]),
            e(0x55bb, &[]),
            e(0x55b2, &[12]),
            e(0x55b3, &[1]),
            e(0x55bc, &[0]),
            e(
                0x55d0,
                &[e(0x55d1, &0.68_f32.to_be_bytes()), e(0x55da, &[])].concat(),
            ),
        ]
        .concat();
        let c = parse(&data).unwrap();
        assert_eq!(c.matrix_coefficients, 2);
        assert_eq!(c.transfer_characteristics, 2);
        assert_eq!(c.primaries, 2);
        assert_eq!(c.bits_per_channel, 12);
        assert_eq!(c.chroma_subsampling, [Some(1), None]);
        assert_eq!(c.max_cll, Some(0));
        assert_eq!(c.max_fall, None);
        let m = c.mastering.unwrap();
        assert_eq!(m.chromaticity[0], Some(f64::from(0.68_f32)));
        assert_ne!(m.chromaticity[0], Some(0.68));
        assert_eq!(m.luminance_min, Some(0.));
        assert_eq!(m.luminance_max, None);
        let c = parse(&e(0x55bb, &u64::MAX.to_be_bytes())).unwrap();
        assert_eq!(c.primaries, u64::MAX);
        assert!(c.ffmpeg9().is_err());
    }
    #[test]
    fn malformed_metadata_duplicates_and_non_finite_or_out_of_range_values_fail() {
        for data in [
            e(0x55b1, &[0; 9]),
            [e(0x55b1, &[1]), e(0x55b1, &[2])].concat(),
            [e(0x55d0, &[]), e(0x55d0, &[])].concat(),
            e(0x55d0, &[e(0x55d1, &[]), e(0x55d1, &[])].concat()),
            e(0x55d0, &e(0x55d1, &[1, 2, 3])),
            vec![0x55, 0xb1, 0x82, 0],
        ] {
            assert!(parse(&data).is_err(), "{data:?}");
        }
        for id in [0x55d1, 0x55d9, 0x55da] {
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.] {
                assert!(parse(&e(0x55d0, &e(id, &value.to_be_bytes()))).is_err());
            }
        }
        assert!(parse(&e(0x55d0, &e(0x55d1, &1.0001_f64.to_be_bytes()))).is_err());
        let c = parse(&e(0x55d0, &e(0x55d9, &f64::MAX.to_be_bytes()))).unwrap();
        assert_eq!(c.mastering.unwrap().luminance_max, Some(f64::MAX));
    }
    #[test]
    fn numerical_boundaries_do_not_panic_or_lose_source_values() {
        for bits in (0..2047_u64).map(|exponent| (exponent << 52) | 0x1234_5678_9abc) {
            let x = f64::from_bits(bits);
            let _ = bounded_rational(x);
        }
        for bits in [
            1,
            (1_u64 << 52) - 1,
            1_u64 << 52,
            0x3fefffffffffffff,
            (i32::MAX as f64).to_bits(),
        ] {
            let _ = bounded_rational(f64::from_bits(bits));
        }
    }
}
