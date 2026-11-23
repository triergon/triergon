//! Synthesis conventions of the FFmpeg 9.0.2 DTS core decoder.

#[path = "../pr_table.rs"]
#[rustfmt::skip]
mod pr_table;

/// Coefficients for the 32-band perfect-reconstruction bank (FILTS = 1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    /// The normative Annex D.8 prototype.
    #[default]
    Specification,
    /// The effective prototype measured from the reference decoder's output.
    Reference,
}

impl Policy {
    pub(crate) fn prototype(self) -> &'static [f64; 512] {
        match self {
            Self::Specification => &crate::tables::QMF_PERFECT,
            Self::Reference => &pr_table::QMF_PERFECT_REF,
        }
    }
}
