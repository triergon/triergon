//! Bounded NAL/OBU framing, AV1/VP9 records and AC-3/E-AC-3 source facts.
//! These development slices are not qualified O21.
//!
//! Payloads are opaque encoded bytes: no RBSP unescaping, access-unit
//! assembly, parameter-set injection, decoder, or display policy is applied.
//! See `README.md` for the exact configuration and validation boundary.
#![forbid(unsafe_code)]

mod ac3;
mod ac3_config;
mod annexb;
mod av1;
mod av1_config;
mod config;
mod header;
mod reader;
mod record_input;
mod vp9_config;

pub use ac3::{
    Ac3Family, Ac3Frame, Ac3FrameDetails, Ac3FrameFacts, Ac3FrameHeader, Ac3FrameLimits,
    Ac3FrameReader, Ac3HeaderIssues, Ac3StreamSelection, Eac3DualMono, probe_ac3_header,
};
pub use ac3_config::{
    Ac3ConfigIssues, Ac3ConfigKind, Ac3ConfigLimits, Ac3ConfigParser, Ac3Configuration,
    Dac3Configuration, Dac3Fields, Dec3Configuration, Dec3Substream,
};
pub use annexb::{AnnexBOutput, AnnexBWriter, BuildStep};
pub use av1::{
    Av1Leb128, Av1Obu, Av1ObuExtension, Av1ObuFacts, Av1ObuFraming, Av1ObuHeader,
    Av1ObuHeaderIssues, Av1ObuLimits, Av1ObuReader,
};
pub use av1_config::{
    Av1ConfigFields, Av1ConfigLimits, Av1ConfigParser, Av1ConfigSyntaxIssues, Av1Configuration,
};
pub use config::{
    AnnexBConfigParser, ConfigEncoding, ConfigInspection, ConfigLimits, Configuration, ParameterSet,
};
pub use header::{Codec, NalHeader, ParameterSetKind};
pub use reader::{Framing, LengthSize, NalFacts, NalFraming, NalReader, NalUnit, ReaderLimits};
pub use vp9_config::{
    Vp9ConfigContext, Vp9ConfigFields, Vp9ConfigLimits, Vp9ConfigParser, Vp9ConfigSyntaxIssues,
    Vp9Configuration,
};
