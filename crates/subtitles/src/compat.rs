//! Reference-compatible policies.
//!
//! The decoders in the format modules follow the specifications and keep every
//! source byte, clock and palette fact. Where FFmpeg presents a subtitle
//! differently and a consumer needs FFmpeg's result, the policies here reproduce
//! FFmpeg's receiver for that input. They never rewrite the source output. Each
//! module is one policy family; its documentation names the FFmpeg build it
//! reproduces.
//!
//! Projections that exist only to compare traces with FFmpeg are not policies.
//! They are compiled only with the `reference-trace` feature.

pub mod dvb {
    //! The DVB receiver: FFmpeg's `dvbsub` decoder.
    //!
    //! [`ReceiverDecoder`] decodes the codec packets of one stream and outputs
    //! the subtitles FFmpeg's decoder outputs for them: when a display set is
    //! presented, its rectangles in the receiver's order, their palettes
    //! (including the [`AutomaticPalette`]) and index planes, and the
    //! subtitle's clock. [`Options`] carries the decoder options that change
    //! the output (`compute_edt`, `compute_clut`, `dvb_substream`).
    //!
    //! [`PesParser`] cuts a selected service's PES data fields into the codec
    //! packets FFmpeg's demuxer hands its decoder.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::dvb::pes_parser::{PesFraming, PesPacket, PesParser, PesProgress};
    pub use crate::dvb::receiver_decoder::{
        AutomaticPalette, Options, ReceiverDecoder, Rect, Subtitle,
    };
}

pub mod pgs {
    //! The PGS receiver.
    //!
    //! [`RecoveryDecoder`] and [`IndexedRecoveryDecoder`] run the strict source
    //! decoder and FFmpeg's `pgssub` receiver over the same packets and report
    //! both, including the receiver's packet recovery. [`ReceiverDecoder`] runs
    //! only the receiver, for a consumer of its events.
    //!
    //! The receiver returns what the reference returns for every packet it
    //! decodes, malformed ones included: the first display of each packet, its
    //! presentation PTS, rectangles, palettes and pixels, forced flags and the
    //! `forced_subs_only` palette. A packet the reference refuses carries a
    //! [`Refusal`]; the receiver keeps the state its earlier segments left and
    //! decodes the next packet.
    //!
    //! The display sets of [`pgs::DisplayDecoder`](crate::pgs::DisplayDecoder)
    //! also carry the receiver's ARGB words and clock beside the source facts:
    //! [`ColourMatrix`] converts Y/Cr/Cb as the receiver does, [`ColourOrigin`]
    //! and [`ClockSource`] say where a word or clock came from, and [`ArgbView`]
    //! writes an object's receiver ARGB plane on demand.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::pgs::display_model::{ClockSource, ColourMatrix, ColourOrigin};
    pub use crate::pgs::indexed_argb::ArgbView;
    pub use crate::pgs::receiver_decoder::{ReceiverDecoder, ReceiverEof, ReceiverPacket};
    pub use crate::pgs::recovery::{IndexedRecoveryDecoder, RecoveryDecoder, RecoveryEof, Refusal};
}

pub mod dvd {
    //! FFmpeg's DVD subtitle receiver and the packets its demuxers deliver.
    //!
    //! [`Decoder`] is the `dvdsub` decoder over packets: reassembly, control
    //! sequences, 2-bit and 8-bit pixels, colours carried between units,
    //! transparent-border crop and `forced_subs_only`, configured by
    //! [`Settings`] from the stream configuration and the `palette` and
    //! `ifo_palette` options ([`ifo_palette`]). Its packets come from
    //! [`VobSubIndex`] (the `vobsub` demuxer's `.idx`/`.sub` reading), from
    //! [`SubpictureParser`] over a program stream's subpicture packets (the
    //! `mpegps` demuxer and DVD subtitle parser, streams numbered by
    //! [`SubpictureStreams`]), or from Matroska and MP4 tracks, whose
    //! colour table [`mp4_configuration`] turns into the configuration the MP4
    //! demuxer hands the decoder. [`YcbcrRounding`] names the two Y'CbCr-to-RGB
    //! conversions.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::dvd::demux::{
        ParsedUnit, SubpictureParser, SubpictureStreams, VobSubEntry, VobSubIndex,
        mp4_configuration,
    };
    pub use crate::dvd::receiver::{
        Bitmap, Decoder, Limits, Settings, Status, Subtitle, YcbcrRounding, ifo_palette,
    };
}

pub mod charset {
    //! Charset conversion of opaque ASS packets.
    //!
    //! [`AssPacketJob`] converts one opaque ASS packet with the charset label the
    //! receiver accepts ([`receiver_charset`]) and reports the converted text and
    //! the prefix the receiver keeps. [`receiver_charset_with_glibc_euc_kr`] and
    //! [`AssPacketJob::new_with_glibc_euc_kr`] add glibc's EUC-KR mapping of
    //! `A2E8`. The repertoire is the one the reference build's Linux/glibc `iconv`
    //! accepts.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::text::receiver_charset::{
        AssPacketJob, Outcome, PacketStatus, PacketView, receiver_charset,
        receiver_charset_with_glibc_euc_kr,
    };
}

pub mod mov_text {
    //! mov_text to ASS conversion.
    //!
    //! [`Decoder`] converts tx3g samples to the ASS events and header FFmpeg's
    //! `mov_text` decoder emits, from the caller's codec extradata and packet
    //! clock. The strict [`tx3g_semantics`](crate::text::tx3g_semantics) view is
    //! unchanged.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::text::mov_text::{Clock, Decoder, Limits, Options, Output, Status, Subtitle};
}

pub mod cc {
    //! FFmpeg's closed-caption receiver.
    //!
    //! [`Decoder`] turns `cc_data` packets into the ASS events and header of
    //! FFmpeg's `ccaption` decoder, with its `real_time`,
    //! `real_time_latency_msec` and `data_field` [`Options`].
    //! [`CaptionExtractor`] gives, packet by packet, the caption bytes FFmpeg's
    //! MPEG-2, H.264 and HEVC decoders attach to their output frames, in its
    //! order and with its times.
    //! [`SccPackets`] reads a Scenarist SCC file into the packets FFmpeg's `scc`
    //! demuxer produces.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::cc::extraction::{
        CaptionExtractor, CaptionFrame, RAW_MPEG2_TIME_BASE, VideoCoding, VideoPacket,
    };
    pub use crate::cc::receiver::{ASS_HEADER, Clock, DataField, Decoder, Options, Subtitle};
    pub use crate::cc::scc::{SccPacket, SccPackets};
}

pub mod teletext {
    //! FFmpeg's `libzvbi_teletextdec` decoder in its text and ASS formats.
    //!
    //! [`Decoder`] takes the PES payloads FFmpeg's demuxer hands its decoder
    //! and returns at most one [`Subtitle`] per call, as the reference does:
    //! pages are selected ([`PageSelection`]), laid out as libzvbi does (41
    //! columns, its page number in the header row), written as plain text or
    //! ASS ([`Format`]) and queued. The `bitmap` format, which renders libzvbi's
    //! own font, is not provided.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch, with
    //! libzvbi 0.2.46.
    pub use crate::teletext::receiver::{
        Decoded, Decoder, Format, Options, PageSelection, Refusal, Subtitle,
    };
}
