//! Reference-compatible policies.
//!
//! The crate root reads and renders scripts as libass does; what it leaves to
//! its caller is how a renderer is configured. Where a consumer wants the
//! settings a player gives one renderer, the modules here give them. Each
//! module is one renderer; its documentation names the version it reproduces.

pub mod libass;
