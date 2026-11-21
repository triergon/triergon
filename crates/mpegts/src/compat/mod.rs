//! Reference-compatible policies.
//!
//! The types at the crate root follow H.222.0 and report every source fact. Where
//! FFmpeg's demuxer cuts packets, positions or timestamps differently and a consumer
//! needs FFmpeg's result, the policies here reproduce that demuxer for the input.
//! They are fed from the same transport packets and never rewrite source facts. Each
//! module is one policy family; its documentation names the FFmpeg build it
//! reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies. They
//! are compiled only with the `reference-trace` feature.

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
    //! Explicit selection of DTS core on private-data routes without a descriptor.
    //! The comparison uses the same codec declaration with FFmpeg git revision
    //! `d32b387f` on the 9.0 branch.

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
    //! Packet boundaries for LATM in MPEG-TS, as observed with FFmpeg git
    //! revision `d32b387f` on the 9.0 branch with parsers and NOFILLIN.
    use crate::Track;
    use crate::clock::ClockKey;
    use crate::pes::Pes;
    use crate::reader::SourceEvent;
    use crate::stream::TraceEvent;

    /// Choose LOAS envelope framing or the demuxer's unsplit PES boundaries.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum PacketPolicy {
        /// Frame each LOAS envelope as an access unit.
        #[default]
        Strict,
        /// Emit each validated, nonempty LATM PES as a separate observation.
        PreservePes,
    }

    /// An unsplit PES observation with its exact source and clock facts.
    #[derive(Clone, Debug)]
    pub struct PesPacket {
        track: Track,
        source: SourceEvent,
        clock_key: ClockKey,
        clock_quarantined: bool,
    }

    impl PesPacket {
        pub(crate) fn new(
            track: Track,
            source: SourceEvent,
            clock_key: ClockKey,
            clock_quarantined: bool,
        ) -> Self {
            Self {
                track,
                source,
                clock_key,
                clock_quarantined,
            }
        }

        /// The track the PES belongs to.
        pub fn track_info(&self) -> &Track {
            &self.track
        }

        /// The original PES event.
        pub fn source(&self) -> &SourceEvent {
            &self.source
        }

        /// The clock the PES timestamps belong to.
        pub fn clock_key(&self) -> ClockKey {
            self.clock_key
        }

        /// Whether the program clock was quarantined at this PES, leaving its timestamps
        /// unresolved.
        pub fn clock_is_quarantined(&self) -> bool {
            self.clock_quarantined
        }

        /// The original PES.
        pub fn pes(&self) -> &Pes {
            let TraceEvent::Pes(pes) = self.source.event() else {
                unreachable!("LATM packet owns its source PES");
            };
            pes
        }
    }
}

pub mod opaque_video;

pub mod pes;

pub mod stream;

pub mod input;
