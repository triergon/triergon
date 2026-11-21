//! A subtitle track's header, attachments and metadata, collected by a
//! [`Builder`] into an owned [`Context`] for a renderer; no parsing or
//! presentation policy.
#![forbid(unsafe_code)]
mod model;
mod session;
pub use model::*;
pub use session::Builder;
