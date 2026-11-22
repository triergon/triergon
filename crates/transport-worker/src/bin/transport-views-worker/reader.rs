use std::ffi::OsString;

use super::protocol::Operation;

pub(super) fn operation<'a>(
    prefix: &str,
    _input: &'a [u8],
    _arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    Err(format!(
        "usage: {prefix}{{fold,trace,memory}} FILE [ARG...] | {prefix}bench FILE ITERATIONS [ARG...]"
    ))
}
