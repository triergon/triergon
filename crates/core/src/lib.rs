#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(
    missing_debug_implementations,
    unreachable_pub,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used
)]
// Pending: `missing_docs`, `clippy::expect_used`.

pub mod blocking;
mod budget;
mod bytes;
pub mod container;
mod error;
mod input;
mod round;
mod time;
mod wide;
mod work;

pub use budget::{Budget, Charged, Reservation};
pub use bytes::{
    BudgetVec, BudgetVecIntoIter, ByteStorageAddress, Bytes, BytesView, BytesWriter, FreezeError,
    OwnedInputBytes, PushError, TransferError, nonzero_prefix_len,
};
pub use error::{Error, Result};
pub use input::{
    Backpressure, InputCursor, InputRequest, InputToken, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, SourceLocation, Step,
};
pub use round::round_ties_even_i32;
pub use time::{Rounding, Time, TimeBase};
pub use work::WorkBudget;
