//! AV1: open bitstream units, the `av1C` configuration record, and container
//! samples as decoder temporal units.
//!
//! | Need | Entry points |
//! |---|---|
//! | OBUs of a stream | [`Av1ObuReader`] with [`Av1ObuFraming`] |
//! | `av1C` record | [`Av1ConfigParser`], [`Av1Configuration`] |
//! | One sample as a temporal unit | [`Av1TemporalUnitConfig`], [`Av1TemporalUnitJob`] |

mod config;
pub(crate) mod obu;
mod temporal_unit;

pub use config::{
    Av1ConfigFields, Av1ConfigLimits, Av1ConfigParser, Av1ConfigSyntaxIssues, Av1Configuration,
};
pub use obu::{
    Av1Leb128, Av1Obu, Av1ObuExtension, Av1ObuFacts, Av1ObuFraming, Av1ObuHeader,
    Av1ObuHeaderIssues, Av1ObuLimits, Av1ObuReader,
};
pub use temporal_unit::{
    Av1TemporalUnit, Av1TemporalUnitConfig, Av1TemporalUnitJob, Av1TemporalUnitLimits,
};
