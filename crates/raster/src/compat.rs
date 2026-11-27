//! Reference-compatible policies.
//!
//! The crate root holds the outline and bitmap stages, with libass's results;
//! what it leaves to its caller is which shapes are drawn under which
//! transforms and how their bitmaps are combined and clipped. Where a
//! consumer needs the choices one renderer makes, the modules here reproduce
//! them. Each module is one renderer; its documentation names the version it
//! reproduces.

pub mod libass;
