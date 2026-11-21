//! Reference-compatible policies.
//!
//! The crate root reads and renders scripts with libass's results, its
//! departures from VSFilter included; what it leaves to its caller is the
//! renderer's settings. Where a consumer needs the choices one renderer
//! makes, the modules here reproduce them. Each module is one renderer; its
//! documentation names the version it reproduces.

pub mod libass;
