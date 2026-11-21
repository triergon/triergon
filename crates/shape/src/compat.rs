//! Reference-compatible policies.
//!
//! The crate root shapes with the font functions it is given, with HarfBuzz's
//! results; what it leaves to its caller is which font functions a face gets
//! and how their answers are cached. Where a consumer needs the choices one
//! renderer makes, the modules here reproduce them. Each module is one
//! renderer; its documentation names the version it reproduces.

pub mod libass;
