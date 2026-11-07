//! The `cc` operation: preparation and the timed decode.

use crate::container::Result;
use crate::{Args, Prepared};
use std::path::Path;

pub fn prepare(_file: &Path, _args: &Args) -> Result<Box<dyn Prepared>> {
    Err("cc: not implemented".into())
}
