//! Diagnostics consumed by the harness test gate.

use std::fmt;
use std::io::{self, Write};

/// Report an unavailable vector check on uncaptured stderr.
///
/// The gate reads the `skipped: ` prefix and expects one line per check.
pub fn skipped(what: fmt::Arguments<'_>) {
    let _ = io::stderr().write_all(format!("skipped: {what}\n").as_bytes());
}
