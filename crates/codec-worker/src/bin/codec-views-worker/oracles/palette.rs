use std::ffi::OsString;

use triergon_pixel::{ChannelRounding, YcbcrMatrix};

use super::{Operation, row};

pub(super) fn prepare<'a>(
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let [matrix, rounding] = arguments else {
        return Err("expected palette matrix and rounding".into());
    };
    let matrix = match matrix.to_str() {
        Some("bt601") => YcbcrMatrix::BT601,
        Some("bt709") => YcbcrMatrix::BT709,
        _ => return Err("unknown palette matrix".into()),
    };
    let rounding = match rounding.to_str() {
        Some("nearest") => ChannelRounding::Nearest,
        Some("half-down") => ChannelRounding::HalfDown,
        _ => return Err("unknown palette rounding".into()),
    };
    let (colors, remainder) = input.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err("truncated Y'/Cr/Cb/alpha input tuple".into());
    }
    Ok(Operation {
        name: "ycbcr-palette",
        run: Box::new(move |work, mut trace| {
            for &[y, cr, cb, alpha] in colors {
                row(
                    &mut trace,
                    work,
                    3,
                    &[
                        u64::from(y),
                        u64::from(cr),
                        u64::from(cb),
                        u64::from(alpha),
                        u64::from(matrix.rgb(y, cr, cb, rounding)),
                        u64::from(matrix.argb(y, cr, cb, alpha, rounding)),
                    ],
                    &[],
                )?;
            }
            Ok(())
        }),
    })
}
