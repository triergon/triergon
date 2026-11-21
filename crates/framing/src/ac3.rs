//! AC-3 and E-AC-3: syncframe headers and frames, the `dac3` and `dec3`
//! configuration records, and the complete E-AC-3 bit stream information.
//!
//! | Need | Entry points |
//! |---|---|
//! | Header of a syncframe | [`probe_header`], [`Ac3FrameHeader`] |
//! | Frames of an aligned stream | [`Ac3FrameReader`] |
//! | `dac3` or `dec3` record | [`Ac3ConfigParser`], [`Ac3Configuration`] |
//! | E-AC-3 bit stream information | [`Eac3BsiParser`], [`Eac3Bsi`] |

mod config;
mod eac3_bsi;
pub(crate) mod frame;

pub use config::{
    Ac3ConfigIssues, Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration,
    Dac3Configuration, Dac3Fields, Dec3Configuration, Dec3Substream,
};
pub use eac3_bsi::{
    Eac3BitRange, Eac3BitSpan, Eac3Bsi, Eac3BsiFacts, Eac3BsiLimits, Eac3BsiParser, Eac3Trailer,
};
pub use frame::{
    Ac3Family, Ac3Frame, Ac3FrameDetails, Ac3FrameFacts, Ac3FrameHeader, Ac3FrameLimits,
    Ac3FrameReader, Ac3HeaderIssues, Ac3StreamSelection, Eac3DualMono, probe_header,
};
