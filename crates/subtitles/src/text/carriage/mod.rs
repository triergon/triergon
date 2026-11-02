//! Bounded versioned text carriage, separate from strict source views.
#![forbid(unsafe_code)]
mod clock;
mod document;
mod model;
mod native_read;
mod session;
mod utf16;
pub use model::*;
pub use native_read::NativeView;
pub use session::Session;
