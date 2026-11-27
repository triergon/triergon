//! OpenType layout: the GDEF, GSUB, GPOS and `kern` tables and their application.
//! Translated from HarfBuzz 14.6.0 (see `docs/provenance.md`).

pub(crate) mod apply;
mod context;
pub(crate) mod data;
pub(crate) mod digest;
pub(crate) mod gdef;
pub(crate) mod gpos;
pub(crate) mod gsub;
pub(crate) mod kern;
pub(crate) mod layout;
pub(crate) mod would_apply;
