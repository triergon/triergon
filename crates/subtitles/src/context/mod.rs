//! Independent owned subtitle-context handoff; no parsing or presentation policy.
#![forbid(unsafe_code)]
mod model;
mod session;
pub use model::*;
pub use session::Builder;
