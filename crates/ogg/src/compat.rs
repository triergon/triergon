//! Reference-compatible policies.
//!
//! The reader follows the specifications and keeps exact source facts. Where FFmpeg
//! reports an input differently and a consumer needs FFmpeg's result, the policies here
//! reproduce FFmpeg's behaviour for that input. Each module is one policy family; its
//! documentation names the FFmpeg build it reproduces.

pub mod clock;
