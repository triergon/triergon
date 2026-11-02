//! Shared media primitives. Experimental; the new core is not qualified yet.
#![forbid(unsafe_code)]

mod budget;
mod bytes;
mod error;
mod input;
mod time;
mod wide;
mod work;

pub use budget::{Budget, Reservation};
pub use bytes::{BudgetVec, Bytes, FreezeError, PushError, TransferError};
pub use error::{Error, Result};
pub use input::{
    InputCursor, InputRequest, InputToken, InputWindow, SourceCapabilities, SourceFailure,
    SourceId, SourceLocation, Step,
};
pub use time::{Rounding, Time, TimeBase};
pub use work::WorkBudget;
