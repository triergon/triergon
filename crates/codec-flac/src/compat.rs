//! Optional FLAC decoding conventions.

/// Frame CRC conventions of the FFmpeg 9.0.2 decoder.
pub mod ffmpeg {
    /// What to do with the CRC-16 that ends every frame.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum CrcPolicy {
        /// Decodes without validating the frame footer's CRC-16.
        #[default]
        Ignore,
        /// Refuse a frame whose CRC-16 does not match with [`DecodeError::CrcMismatch`](crate::DecodeError::CrcMismatch).
        Reject,
    }
}
