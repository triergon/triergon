//! Optional AC-3 and E-AC-3 output conventions.

/// Output conventions of the FFmpeg 9.0.2 decoder.
pub mod ffmpeg {
    /// How to interpret syntax where the reference and A/52 differ.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum Policy {
        /// Uses the reference's dither, HEBAP-4 and first-frame conventions.
        /// Refuses E-AC-3 reduced sample rates.
        #[default]
        Reference,
        /// Uses the A/52 conventions and decodes E-AC-3 reduced sample rates.
        Specification,
    }
}
