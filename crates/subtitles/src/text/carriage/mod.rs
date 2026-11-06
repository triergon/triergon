//! Bounded text carriage in the reference's packet form, separate from strict
//! source views.
#![forbid(unsafe_code)]
mod clock;
mod document;
mod model;
mod reader_view;
mod session;
mod utf16;
pub use model::*;
pub use reader_view::ReaderView;
pub use session::Session;
