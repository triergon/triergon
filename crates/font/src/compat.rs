//! Reference-compatible policies.
//!
//! The crate root reads faces and loads, scales and hints glyphs, with
//! FreeType's results; what it leaves to its caller is which face, charmap,
//! size and synthetic style a piece of text gets. Where a consumer needs the
//! choices one renderer makes, the modules here reproduce them. Each module is
//! one renderer; its documentation names the version it reproduces.

pub mod libass;
