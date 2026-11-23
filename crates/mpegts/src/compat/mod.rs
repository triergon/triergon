//! Reference-compatible policies.
//!
//! These policies reproduce the named demuxer's packet boundaries, positions and
//! timestamps while preserving the H.222.0 source facts. Comparison projections
//! require the `reference-trace` feature.

pub mod adts {
    //! Parser packet boundaries for ADTS carried in MPEG-TS. Original PES
    //! timing and extents remain available on every result.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.

    use crate::pes::Pes;
    use triergon_core::Error;

    /// Select how a transport reader handles bytes outside complete ADTS frames.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum PacketPolicy {
        /// Use the specification reader and the selected audio EOF policy.
        #[default]
        Strict,
        /// Use bounded parser framing and report opaque prefixes and EOF tails
        /// through `ReaderEvent::AdtsFragment`.
        ReportFragments,
    }

    /// Admission of an ADTS PES with a zero packet-length field.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum PesPolicy {
        /// Refuse a length-zero ADTS PES with the specification's unbounded non-video PES error.
        #[default]
        Strict,
        /// Assemble the original bytes until a following PES start or logical
        /// EOF, and report the wire violation with the assembled source.
        ReportUnbounded,
    }

    /// Original header, bytes and source ranges of a length-zero ADTS PES.
    #[derive(Debug)]
    pub struct UnboundedPes(pub(crate) Pes);

    impl UnboundedPes {
        /// The assembled PES: original header, payload spans and source ranges.
        pub fn pes(&self) -> &Pes {
            &self.0
        }

        /// The specification error a strict reader reports for this PES.
        pub fn rejection(&self) -> Error {
            crate::pes::UNBOUNDED_NON_VIDEO
        }
    }
}

pub mod annex_b;

pub mod dts {
    //! DTS core on a private-data route without a DTS descriptor, which the demuxer opens as
    //! DTS when it is selected so.
    //!
    //! [`UnlabelledCore`] names the route; `ReaderLimits::probe_unlabelled_dts_core` instead
    //! admits it when its program has empty program descriptors and exactly one private-data
    //! entry with an empty descriptor loop, and the route's first `private_stream_1` PES, at
    //! elementary offset zero, starts with two complete canonical big-endian core frames whose
    //! sample rate, channel mode and block count agree. The probe reads at most 32,768 payload
    //! bytes, one work unit per byte or span boundary, without allocating or searching for a
    //! sync word; a failed probe reports the track unsupported at that PES, and a new map,
    //! clock epoch or byte run probes again. It proves the frame envelopes, not decodability.
    //! Word-swapped and 14-bit packings are not admitted, and DTS is not inferred from a
    //! registration or a Blu-ray stream type.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.

    /// A caller-identified DTS core stream with empty program and elementary
    /// descriptor loops. The reader validates each frame and keeps the original
    /// PMT and PES data.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct UnlabelledCore {
        /// The program number whose PMT lists the stream; must be nonzero.
        pub program: u16,
        /// The 13-bit elementary PID of the stream, in the elementary PID range.
        pub pid: u16,
    }
}

pub mod latm {
    //! LOAS framing or preservation of nonempty LATM PES packet boundaries.

    /// Choose LOAS envelope framing or the demuxer's unsplit PES boundaries.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum PacketPolicy {
        /// Frame each LOAS envelope as an access unit.
        #[default]
        Strict,
        /// Emit each validated, nonempty LATM PES as a separate observation.
        PreservePes,
    }
}

pub mod opaque_video;

pub mod pes;

pub mod stream;

pub mod input;
