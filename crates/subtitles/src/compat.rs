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
    //! The DVB receiver view.
    //!
    //! A [`dvb::Decoder`](crate::dvb::Decoder) built with
    //! [`ReceiverView::Receiver`](crate::dvb::ReceiverView::Receiver) or
    //! [`new_configured_with_geometry_policy`](crate::dvb::Decoder::new_configured_with_geometry_policy)
    //! presents display sets as FFmpeg's `dvbsub` decoder does: its region order,
    //! automatic palettes without a CDS, non-modifying-colour handling and
    //! end-of-display tolerance. Each event then carries a [`Publication`]
    //! (`DisplayEvent::receiver_publication`) and each region palette a receiver
    //! [`Palette`] (`PaletteSnapshot::receiver`). [`GeometryPolicy::OmitUnrendered`]
    //! omits regions without a raster and reports them as [`OmittedRegion`]s.
    //!
    //! [`PesParser`] cuts a selected service's PES packets into the codec packets
    //! FFmpeg's demuxer hands its decoder; decode them with
    //! [`Decoder::provide_pes`](crate::dvb::Decoder::provide_pes).
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::dvb::pes_parser::{PesFraming, PesPacket, PesParser, PesProgress};
    pub use crate::dvb::receiver::{
        GeometryPolicy, OmittedRegion, Palette, Publication, PublicationReason,
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
    pub use crate::pgs::recovery::{
        EofBehavior, IndexedRecoveryDecoder, RecoveryDecoder, RecoveryEof,
    };
}

pub mod dvd {
    //! The DVD receiver's crop, palette and presentation.
    //!
    //! Selected with [`ControlPolicy::Receiver`](crate::dvd::ControlPolicy::Receiver)
    //! for traditional subpictures and
    //! [`HdPolicy::Receiver`](crate::dvd::HdPolicy::Receiver) for extended ones.
    //! [`resolve_palette`] builds the receiver's four ARGB words from a supplied
    //! palette, or its automatic greys without one. [`CropJob`] removes
    //! transparent borders as FFmpeg's `dvdsub` decoder does and exposes the
    //! result as a [`CroppedView`] that borrows the source rows. [`Presentation`]
    //! places that view on the caller's packet timeline. [`hd_argb`] converts one
    //! extended-subpicture Y/Cr/Cb triple and contrast byte.
    //!
    //! Reference: FFmpeg git revision `d32b387f` on the 9.0 branch.
    pub use crate::dvd::hd::hd_argb;
    pub use crate::dvd::projection::{Crop, CropJob, CropMode, CropSelection, CroppedView};
    pub use crate::dvd::receiver_palette::{Palette, PaletteOrigin, resolve_palette};
    pub use crate::dvd::traditional::Presentation;
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
