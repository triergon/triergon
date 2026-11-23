//! Optional AAC decoding policies.

/// Output conventions of the FFmpeg 9.0.2 AAC decoder.
pub mod ffmpeg {
    /// Handling of in-band delay signatures when a container provides priming.
    pub mod priming {
        /// Source of leading PCM discard for ADTS.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub enum Policy {
            /// Apply the default delay signalled by a recognised encoder signature.
            #[default]
            EncoderSignature,
            /// Leave leading discard to the caller's container edit.
            Container,
        }
    }

    /// Channel mappings for fixed AAC-LC access units.
    ///
    /// `ConventionalSurround` uses the reference speaker mapping for packets with an
    /// explicit AudioSpecificConfig.
    pub mod layout {
        /// How to interpret the fixed channel configuration of a raw access unit.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub enum Policy {
            /// Speaker assignments in ISO/IEC 14496-3 Table 1.19.
            #[default]
            Specification,
            /// Use the back pair in configurations 5, 6, 11 and 14. Interpret
            /// configuration 7 as the conventional 7.1 back layout.
            ConventionalSurround,
        }
    }
}
