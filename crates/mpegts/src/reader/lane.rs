mod aligned_audio;
mod dts_hd;
mod dvb;
mod eac3;
mod loas;
mod mpeg_audio;
mod unframed_audio;

mod build;
mod fragment;
mod storage;

use super::model::{ClockContext, PacketData, PayloadSources, Shared, UnframedData};
use super::seek::{PictureDelivery, SeekProbe};
use super::{
    Ac3AccessUnit, AccessUnit, AudioEofPolicy, DtsAudioFrame, DtsExtensionFrame, IncompleteVideo,
    LoasFrame, Packet, PacketClock, PayloadSource, ReaderLimits, SourceEvent, Track, UnframedAudio,
};
use crate::clock::{SourceTimePolicy, source_timestamp};
use crate::compat::adts::PacketPolicy;
use crate::compat::annex_b::VideoEofPolicy;
use crate::compat::latm;
use crate::compat::pes::{PrivateAudioCarriage, PrivateAudioDescriptors, UnframedAudioPolicy};
use crate::psi::{CodecIdentification, DescriptorNamespace};
use crate::stream::{StorageItem, TransportStorageCursor};
use crate::{SourceEpochs, table_storage};
use build::{Build, BuildOrigins, BuildUnit};
use fragment::{
    FragmentKind, Incomplete, LEADING_VIDEO_BYTES, ResidueScan, UnframedKind, VideoResidueScan,
};
use std::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
pub(super) use storage::StorageScan;
use triergon_core::container::CodecId;
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputRequest, InputWindow, Refused, Result,
    SourceCapabilities, SourceId, SourceLocation, Step, Time, WorkBudget,
};
use triergon_framing::{
    aac::{AacAdtsCarrierLimits, AacAdtsCarrierReader, AacAdtsStreamLimits, AacAdtsStreamReader},
    ac3::{Ac3FrameLimits, Ac3FrameReader, Ac3StreamSelection},
    dts,
    nal::{Codec, RawAnnexBReader},
};

// Keep concrete readers inline; their containing BudgetVec charges the whole state.
#[allow(clippy::large_enum_variant)]
enum Framer {
    Video(RawAnnexBReader),
    Adts(AacAdtsStreamReader),
    AdtsFragments(AacAdtsCarrierReader),
    Loas(aligned_audio::Reader<3, ()>),
    MpegAudio(mpeg_audio::Reader),
    Dts(aligned_audio::Reader<15, Time>),
    DtsHd(dts_hd::Reader),
    DtsExtension(dts_hd::ExtensionReader),
    LatmPes,
    Ac3(Ac3FrameReader),
    Eac3(eac3::Reader),
    Dvb(dvb::Reader),
}

impl Framer {
    fn input_wait_storage(&self) -> Result<usize> {
        Ok(match self {
            Self::Video(reader) => reader.input_wait_storage()?.0,
            Self::Adts(reader) => reader.input_wait_buffer_capacity(),
            Self::AdtsFragments(reader) => reader.input_wait_buffer_capacity(),
            Self::Loas(reader) => reader.input_wait_buffer_capacity(),
            Self::MpegAudio(reader) => reader.input_wait_buffer_capacity()?,
            Self::Dts(reader) => reader.input_wait_buffer_capacity(),
            Self::DtsHd(reader) => reader.input_wait_buffer_capacity(),
            Self::DtsExtension(reader) => reader.input_wait_buffer_capacity(),
            Self::Ac3(reader) => reader.input_wait_buffer_capacity(),
            Self::Eac3(reader) => reader.input_wait_storage()?,
            // Direct PES carriages do not keep a pending unit between inputs.
            Self::LatmPes | Self::Dvb(_) => 0,
        })
    }

    fn input_wait_backing(&self, index: usize) -> Result<Option<&Bytes>> {
        Ok(match self {
            Self::Video(reader) => reader
                .input_wait_storage()?
                .1
                .get(index)
                .map(triergon_framing::nal::NalUnit::bytes),
            Self::Eac3(reader) => reader.input_wait_backing(index),
            _ => None,
        })
    }

    fn is_audio(&self) -> bool {
        matches!(
            self,
            Self::Adts(_)
                | Self::Loas(_)
                | Self::MpegAudio(_)
                | Self::Dts(_)
                | Self::DtsHd(_)
                | Self::DtsExtension(_)
                | Self::Ac3(_)
                | Self::Eac3(_)
        )
    }
    fn new(pool: &Budget, track: &Track, limits: ReaderLimits) -> Result<Self> {
        // Private concatenated elementary view, never an external source request.
        let source = SourceId(0);
        let caps = SourceCapabilities {
            length: None,
            random_access: false,
        };
        let stream = track.stream();
        let namespace = stream.descriptor_namespace();
        match stream.stream_type {
            kind @ (0x1b | 0x24) => Ok(Self::Video(RawAnnexBReader::new(
                pool,
                if kind == 0x1b {
                    Codec::Avc
                } else {
                    Codec::Hevc
                },
                source,
                caps,
                limits.video,
            )?)),
            0x0f if limits.adts_packet_policy == PacketPolicy::ReportFragments => Ok(
                Self::AdtsFragments(AacAdtsCarrierReader::for_parser_packets(
                    pool,
                    source,
                    caps,
                    AacAdtsCarrierLimits {
                        maximum_packet_bytes: limits.maximum_audio_frame_bytes,
                        maximum_packets: limits.maximum_access_units,
                        maximum_input_window: limits.elementary_input_window,
                    },
                )?),
            ),
            0x0f => Ok(Self::Adts(AacAdtsStreamReader::new(
                pool,
                source,
                caps,
                AacAdtsStreamLimits {
                    maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                    maximum_frames: limits.maximum_access_units,
                    maximum_input_window: limits.elementary_input_window,
                },
            )?)),
            0x11 if limits.latm_packet_policy == latm::PacketPolicy::PreservePes => {
                Ok(Self::LatmPes)
            }
            // H.222.0 Table 2-34 assigns these streams to ISO/IEC 11172-3
            // and ISO/IEC 13818-3 audio respectively.
            0x03 | 0x04 => Ok(Self::MpegAudio(mpeg_audio::Reader::new(
                pool,
                source,
                caps,
                limits.maximum_audio_frame_bytes,
                limits.elementary_input_window,
            )?)),
            0x11 => Ok(Self::Loas(aligned_audio::Reader::new(
                pool,
                source,
                caps,
                limits.maximum_audio_frame_bytes,
                limits.elementary_input_window,
            )?)),
            0x06 if stream.codec_identification() == CodecIdentification::DvbSubtitles => {
                Ok(Self::Dvb(dvb::Reader::new(limits.dvb_subtitle_tail_policy)))
            }
            0x06 if track.unlabelled_dts_core || stream.codec() == Some(CodecId::Dts) => {
                if !track.unlabelled_dts_core
                    && stream.dvb_descriptor_codec() != Some(CodecId::Dts)
                    && let Some(descriptor) = stream.dvb_dts_hd()?
                {
                    if descriptor.core.is_none() {
                        return Ok(Self::DtsExtension(dts_hd::ExtensionReader::new(
                            pool,
                            source,
                            caps,
                            limits.maximum_dts_hd_pes_bytes,
                            limits.elementary_input_window,
                            descriptor,
                        )?));
                    }
                    Ok(Self::DtsHd(dts_hd::Reader::new(
                        pool,
                        source,
                        caps,
                        limits.maximum_dts_hd_pes_bytes,
                        limits.elementary_input_window,
                        descriptor,
                    )?))
                } else {
                    Ok(Self::Dts(aligned_audio::Reader::new(
                        pool,
                        source,
                        caps,
                        limits.maximum_audio_frame_bytes,
                        limits.elementary_input_window,
                    )?))
                }
            }
            0x06 if matches!(namespace, DescriptorNamespace::Dvb(_)) => {
                let descriptors = PrivateAudioDescriptors {
                    program: track.program_map().program_descriptors.as_ref(),
                    elementary: stream.descriptor_bytes.as_ref(),
                };
                let carriage =
                    PrivateAudioCarriage::from_observed_pmt(6, descriptors, limits.transport)
                        .or_else(|error| {
                            // An elementary registration can corroborate the DVB descriptor.
                            if matches!(error, Error::Unsupported(_)) {
                                PrivateAudioCarriage::from_hls_pmt(6, descriptors, limits.transport)
                                    .map_err(|_| error)
                            } else {
                                Err(error)
                            }
                        })?;
                if carriage == PrivateAudioCarriage::DvbEac3Tag {
                    Ok(Self::Eac3(eac3::Reader::new(
                        pool, track, limits, source, caps,
                    )?))
                } else {
                    Self::ac3(pool, limits, source, caps)
                }
            }
            0x81 if namespace == DescriptorNamespace::Atsc => Self::ac3(pool, limits, source, caps),
            0x87 if namespace == DescriptorNamespace::Atsc => Ok(Self::Eac3(eac3::Reader::new(
                pool, track, limits, source, caps,
            )?)),
            _ => Err(Error::Unsupported(
                "MPEG-TS carriage has no admitted access-unit framer",
            )),
        }
    }
    fn ac3(
        pool: &Budget,
        limits: ReaderLimits,
        source: SourceId,
        caps: SourceCapabilities,
    ) -> Result<Self> {
        Ok(Self::Ac3(Ac3FrameReader::new(
            pool,
            source,
            caps,
            Ac3StreamSelection::Ac3Only,
            Ac3FrameLimits {
                maximum_frame_bytes: limits.maximum_audio_frame_bytes,
                maximum_frames: limits.maximum_access_units,
                maximum_input_window: limits.elementary_input_window,
            },
        )?))
    }
    fn step(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<Step<AccessUnit>> {
        fn map<T>(step: Step<T>, wrap: impl FnOnce(T) -> AccessUnit) -> Step<AccessUnit> {
            match step {
                Step::Ready(unit) => Step::Ready(wrap(unit)),
                Step::Yield => Step::Yield,
                Step::NeedInput(request) => Step::NeedInput(request),
                Step::End(location) => Step::End(location),
                Step::Backpressure(never) => match never {},
            }
        }
        Ok(match self {
            Self::Video(reader) => map(reader.step(work, cancel)?, AccessUnit::Video),
            Self::Adts(reader) => map(reader.step(work, cancel)?, AccessUnit::Adts),
            Self::Loas(reader) => map(reader.step(work, cancel, loas::parse)?, |frame| {
                AccessUnit::Loas(LoasFrame {
                    bytes: frame.bytes,
                    offset: frame.offset,
                })
            }),
            Self::MpegAudio(reader) => map(reader.step(work, cancel)?, AccessUnit::MpegAudio),
            Self::Dts(reader) => map(
                reader.step(work, cancel, |prefix| {
                    let header = dts::probe_core_header(prefix)?;
                    Ok((header.window, header.frame_bytes))
                })?,
                |frame| {
                    AccessUnit::Dts(DtsAudioFrame {
                        core_bytes: frame.bytes.len(),
                        bytes: frame.bytes,
                        offset: frame.offset,
                        duration: frame.header,
                    })
                },
            ),
            Self::DtsHd(reader) => map(reader.step(work, cancel)?, AccessUnit::Dts),
            Self::DtsExtension(reader) => map(reader.step(work, cancel)?, AccessUnit::DtsExtension),
            Self::AdtsFragments(reader) => map(reader.step(work, cancel)?, |carrier| match carrier
                .into_selected_frame()
            {
                Ok(frame) => AccessUnit::Adts(frame),
                Err(fragment) => AccessUnit::AdtsFragment(fragment),
            }),
            Self::Ac3(reader) => map(reader.step(work, cancel)?, |frame| {
                AccessUnit::Ac3(Ac3AccessUnit::single(frame))
            }),
            Self::Eac3(reader) => map(reader.step(work, cancel)?, AccessUnit::Ac3),
            Self::Dvb(_) | Self::LatmPes => unreachable!("this route uses validated PES input"),
        })
    }
    fn provide(&mut self, window: InputWindow) -> Result<()> {
        match self {
            Self::Video(reader) => reader.provide_owned(window),
            Self::Adts(reader) => reader.provide_owned(window),
            Self::Loas(reader) => reader.provide(window),
            Self::MpegAudio(reader) => reader.provide(window),
            Self::Dts(reader) => reader.provide(window),
            Self::DtsHd(reader) => reader.provide(window),
            Self::DtsExtension(reader) => reader.provide(window),
            Self::AdtsFragments(reader) => reader.provide_owned(window),
            Self::Ac3(reader) => reader.provide_owned(window),
            Self::Eac3(reader) => reader.provide(window),
            Self::Dvb(_) | Self::LatmPes => unreachable!("this route uses validated PES input"),
        }
    }
    fn finish(&mut self, request: InputRequest) -> Result<()> {
        match self {
            Self::Video(reader) => reader.finish(request.source, request.token),
            Self::Adts(reader) => reader.finish(request.source, request.token),
            Self::Loas(reader) => reader.finish(request),
            Self::MpegAudio(reader) => reader.finish(request),
            Self::Dts(reader) => reader.finish(request),
            Self::DtsHd(reader) => reader.finish(request),
            Self::DtsExtension(reader) => reader.finish(request),
            Self::AdtsFragments(reader) => reader.finish(request.source, request.token),
            Self::Ac3(reader) => reader.finish(request.source, request.token),
            Self::Eac3(reader) => reader.finish(request),
            Self::Dvb(_) | Self::LatmPes => unreachable!("this route uses validated PES input"),
        }
    }
}

struct Piece {
    range: Range<u64>,
    source: SourceEvent,
    clock: ClockContext,
    serial: u64,
}

struct Input {
    span: usize,
    offset: usize,
}

/// Whether a PES can join a lane.
enum Prepared {
    /// The lane frames nothing from it.
    Ignored,
    /// The work grant is spent.
    Yield,
    /// Its piece is reserved and covers the lane's bytes up to `end`.
    Piece { end: u64, serial: u64 },
}

pub(super) enum LaneStep {
    Packet(Packet),
    PictureProbe(SeekProbe),
    IncompleteVideo(IncompleteVideo),
    LatmPes(latm::PesPacket),
    UnframedAudio(UnframedAudio),
    NeedPayload,
    Yield,
    End,
    DeferredFragment,
}

#[derive(Clone, Copy)]
pub(super) enum PesRun {
    Idle,
    BeforeBoundary(SourceLocation),
    Active(SourceEpochs, u64),
}

pub(super) struct Lane {
    pub track: Track,
    pub unsupported: Option<Error>,
    framer: Option<Framer>,
    pool: Budget,
    limits: ReaderLimits,
    input: Option<Input>,
    pieces: BudgetVec<Piece>,
    build: Option<Build>,
    incomplete: Option<Incomplete>,
    residue: Option<ResidueScan>,
    supplied: u64,
    serial: u64,
    last_anchor: u64,
    retire: usize,
    retire_credit: usize,
    eof: bool,
    completed: usize,
    published: u64,
    pub run: PesRun,
}

impl Lane {
    pub(super) fn new(
        pool: &Budget,
        track: Track,
        limits: ReaderLimits,
        frame: bool,
    ) -> Result<Self> {
        let (framer, unsupported, residue) = if frame {
            match Framer::new(pool, &track, limits) {
                Ok(framer) => (Some(framer), None, None),
                Err(error @ Error::Unsupported(_)) => {
                    let residue =
                        unframed_audio::Scan::for_track(&track, limits)?.map(ResidueScan::Audio);
                    (None, residue.is_none().then_some(error), residue)
                }
                Err(error) => return Err(error),
            }
        } else {
            (None, None, None)
        };
        Ok(Self {
            track,
            unsupported,
            framer,
            pool: pool.clone(),
            limits,
            input: None,
            pieces: BudgetVec::new(pool),
            build: None,
            incomplete: None,
            residue,
            supplied: 0,
            serial: 0,
            last_anchor: 0,
            retire: 0,
            retire_credit: 0,
            eof: false,
            completed: 0,
            published: 0,
            run: PesRun::Idle,
        })
    }
    pub(super) fn unpublished(&self) -> Option<Range<u64>> {
        (self.published < self.supplied).then_some(self.published..self.supplied)
    }

    /// Resume an independently framed PES with its proved original private
    /// coordinates, output bounds and one-use PES anchor numbering.
    pub(super) fn resume_independent(
        &mut self,
        offset: u64,
        frames: usize,
        nals: usize,
        map: SourceEvent,
    ) -> Result<()> {
        if self.supplied != 0 || self.serial != 0 || self.completed != 0 {
            return Err(Error::SourceMismatch);
        }
        match &mut self.framer {
            Some(Framer::MpegAudio(framer)) if nals == 0 => {
                framer.resume_at(offset, self.limits.elementary_input_window)?;
            }
            Some(Framer::Video(framer)) => framer.resume_at(offset, nals)?,
            _ => return Err(Error::SourceMismatch),
        }
        self.supplied = offset;
        self.published = offset;
        self.serial = frames as u64;
        self.last_anchor = frames as u64;
        self.completed = frames;
        self.track.source = map;
        Ok(())
    }
    pub(super) fn has_incomplete(&self) -> bool {
        self.incomplete.is_some()
    }
    #[expect(
        clippy::expect_used,
        reason = "typing the fragment copy measured +0.04 to +0.26% more instructions (lane notes)"
    )]
    pub(super) fn set_reader_storage(&mut self, bytes: usize) {
        self.incomplete
            .as_mut()
            .expect("pending fragment")
            .reader_storage = Some(bytes);
    }
    /// Admits the PES of `source`: `Ok(None)` once admitted or ignored, `Ok(Some(source))` when
    /// the work grant is spent; a refusal returns the source with the error.
    pub(super) fn push(
        &mut self,
        source: SourceEvent,
        clock: ClockContext,
        work: &mut WorkBudget,
    ) -> std::result::Result<Option<SourceEvent>, Refused<SourceEvent>> {
        let (end, serial) = match self.prepare_piece(source.pes(), work) {
            Ok(Prepared::Piece { end, serial }) => (end, serial),
            Ok(Prepared::Ignored) => return Ok(None),
            Ok(Prepared::Yield) => return Ok(Some(source)),
            Err(error) => {
                return Err(Refused {
                    error,
                    value: source,
                });
            }
        };
        let pes = source.pes();
        let payload_len = pes.payload_len;
        let run = PesRun::Active(pes.epochs, pes.byte_run_epoch);
        if let Err(rejected) = self.pieces.try_push(Piece {
            range: self.supplied..end,
            source,
            clock,
            serial,
        }) {
            return Err(Refused {
                error: rejected.error,
                value: rejected.value.source,
            });
        }
        match &mut self.framer {
            Some(Framer::DtsHd(reader)) => reader.begin_pes(payload_len),
            Some(Framer::DtsExtension(reader)) => reader.begin_pes(payload_len),
            _ => (),
        }
        self.serial = serial;
        self.run = run;
        self.input = self.framer.as_ref().map(|_| Input { span: 0, offset: 0 });
        Ok(None)
    }

    /// Checks `pes` against the lane, pays its work unit and reserves its piece.
    fn prepare_piece(&mut self, pes: &crate::pes::Pes, work: &mut WorkBudget) -> Result<Prepared> {
        if self.framer.is_none() && self.residue.is_none() {
            return Ok(Prepared::Ignored);
        }
        if self.input.is_some() {
            return Err(Error::Invalid("MPEG-TS PES mailbox occupied"));
        }
        match &self.framer {
            Some(Framer::DtsHd(reader)) => reader.check_pes(pes)?,
            Some(Framer::DtsExtension(reader)) => reader.check_pes(pes)?,
            Some(Framer::Dts(_))
                if self.track.unlabelled_dts_core && pes.header.stream_id != 0xbd =>
            {
                return Err(Error::Invalid("unlabelled DTS core PES stream ID"));
            }
            _ => (),
        }
        let stream_type = self.track.stream().stream_type;
        if !(0xc0..=0xdf).contains(&pes.header.stream_id) {
            match stream_type {
                0x03 | 0x04 => return Err(Error::Invalid("MPEG audio PES stream ID")),
                0x11 => return Err(Error::Invalid("LATM audio PES stream ID")),
                _ => (),
            }
        }
        if stream_type == 0x11
            && pes.payload_len == 0
            && matches!(self.framer, Some(Framer::LatmPes))
        {
            return Ok(Prepared::Ignored);
        }
        if pes.header.stream_id != 0xbd {
            if self.track.stream().descriptor_namespace() == DescriptorNamespace::Atsc
                && matches!(stream_type, 0x81 | 0x87)
            {
                return Err(Error::Invalid("ATSC private audio PES stream ID"));
            }
            if matches!(self.framer, Some(Framer::Eac3(_))) {
                return Err(Error::Invalid("DVB E-AC-3 PES stream ID"));
            }
        }
        if self.pieces.len() == self.limits.maximum_pes_per_access_unit {
            return Err(Error::Limit {
                requested: self.pieces.len().saturating_add(1),
                maximum: self.limits.maximum_pes_per_access_unit,
            });
        }
        if !work.try_charge(1) {
            return Ok(Prepared::Yield);
        }
        let end = self
            .supplied
            .checked_add(u64::try_from(pes.payload_len).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        match &self.residue {
            Some(ResidueScan::Video(_)) => self.check_video_residue_extent(end)?,
            Some(ResidueScan::Audio(_)) => unframed_audio::Scan::check_pes(
                pes,
                self.pieces.is_empty(),
                self.limits.maximum_audio_frame_bytes,
            )?,
            None => (),
        }
        self.pieces.try_reserve_exact(1)?;
        let serial = self.serial.checked_add(1).ok_or(Error::Overflow)?;
        Ok(Prepared::Piece { end, serial })
    }
    fn check_video_residue_extent(&self, end: u64) -> Result<()> {
        let size = usize::try_from(end).map_err(|_| Error::Overflow)?;
        let maximum = self.limits.video.maximum_access_unit_bytes;
        if size > maximum {
            return Err(crate::limit(size, maximum));
        }
        Ok(())
    }
    #[expect(
        clippy::expect_used,
        reason = "typing the fragment copy measured +0.04 to +0.26% more instructions (lane notes)"
    )]
    pub(super) fn step(
        &mut self,
        draining: bool,
        picture_delivery: PictureDelivery,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<LaneStep> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if work.remaining() == 0 {
                return Ok(LaneStep::Yield);
            }
            if self.retire != 0 {
                let cost = self.pieces.len() - self.retire_credit;
                self.retire_credit +=
                    usize::try_from(work.take(cost as u64)).map_err(|_| Error::Overflow)?;
                if self.retire_credit != self.pieces.len() {
                    return Ok(LaneStep::Yield);
                }
                self.pieces.as_mut_slice().rotate_left(self.retire);
                self.pieces.truncate(self.pieces.len() - self.retire);
                self.retire = 0;
                self.retire_credit = 0;
                continue;
            }
            if self.build.is_some() {
                return self.publish(picture_delivery, work, cancel);
            }
            if let Some(incomplete) = self.incomplete.as_mut() {
                if incomplete.reader_storage.is_none() {
                    return Ok(LaneStep::DeferredFragment);
                }
                if !incomplete.copy(self.pieces.as_slice(), work, cancel)? {
                    return Ok(LaneStep::Yield);
                }
                let incomplete = self.incomplete.take().expect("finished fragment copy");
                let bytes = incomplete.bytes.expect("copied fragment bytes");
                let mut build = match incomplete.kind {
                    FragmentKind::Audio => Build::new(
                        AccessUnit::IncompleteAudio {
                            range: incomplete.range,
                            bytes,
                            reason: Error::Truncated,
                        },
                        &self.pool,
                        self.last_anchor,
                    ),
                    FragmentKind::Unframed(kind) => Build::with_unit(
                        BuildUnit::Unframed(kind, incomplete.range),
                        Some(bytes),
                        &self.pool,
                        self.last_anchor,
                    ),
                };
                build.origins = BuildOrigins::ReserveTable(incomplete.origins);
                self.build = Some(build);
                continue;
            }
            if let Some(residue) = &mut self.residue {
                let pieces = self.pieces.as_slice();
                let (ready, kind) = match residue {
                    ResidueScan::Video(scan) => {
                        (scan.step(pieces, work, cancel)?, UnframedKind::Video)
                    }
                    ResidueScan::Audio(scan) => {
                        let [piece] = pieces else {
                            return Err(unframed_audio::NEED_PES);
                        };
                        (
                            scan.step(piece.source.pes(), work, cancel)?,
                            UnframedKind::Audio(scan.carriage),
                        )
                    }
                };
                if !ready {
                    return Ok(LaneStep::Yield);
                }
                self.supplied = pieces.last().expect("unframed PES").range.end;
                if !draining {
                    return Ok(LaneStep::NeedPayload);
                }
                self.residue = None;
                self.eof = true;
                self.incomplete = Some(Incomplete::new(
                    FragmentKind::Unframed(kind),
                    0..self.supplied,
                    &self.pool,
                ));
                continue;
            }
            if matches!(self.framer, Some(Framer::LatmPes)) {
                return self.preserve_latm_pes(draining, work);
            }
            let Some(framer) = &mut self.framer else {
                if draining && self.eof {
                    if !self.pieces.is_empty() {
                        self.retire = self.pieces.len();
                        continue;
                    }
                    self.pieces = BudgetVec::new(&self.pool);
                }
                return Ok(LaneStep::End);
            };
            if let Framer::Dvb(reader) = framer {
                if self.input.is_some() {
                    let Some(unit) = reader.step(&self.pieces, work, cancel)? else {
                        return Ok(LaneStep::Yield);
                    };
                    let end = unit.pes_end;
                    self.start_build(AccessUnit::DvbSubtitle(unit))?;
                    self.supplied = end;
                    self.input = None;
                    continue;
                }
                if draining && self.limits.audio_eof_policy == AudioEofPolicy::ReportIncomplete {
                    self.eof = true;
                    self.framer = None;
                    continue;
                }
                return Ok(if draining {
                    LaneStep::End
                } else {
                    LaneStep::NeedPayload
                });
            }
            let step = match framer.step(work, cancel) {
                Err(LEADING_VIDEO_BYTES)
                    if matches!(framer, Framer::Video(_))
                        && self.completed == 0
                        && self.limits.video_eof_policy == VideoEofPolicy::ReportUnframed =>
                {
                    let end = self
                        .pieces
                        .as_slice()
                        .last()
                        .expect("video input PES")
                        .range
                        .end;
                    self.check_video_residue_extent(end)?;
                    // Recheck all retained bytes, including the portion the
                    // framer already read and any unsupplied part of this PES.
                    self.residue = Some(ResidueScan::Video(VideoResidueScan::default()));
                    self.framer = None;
                    self.input = None;
                    continue;
                }
                Err(Error::Truncated)
                    if draining
                        && self.eof
                        && self.input.is_none()
                        && framer.is_audio()
                        && self.limits.audio_eof_policy == AudioEofPolicy::ReportIncomplete =>
                {
                    let maximum = match framer {
                        Framer::Eac3(reader) => reader.maximum_incomplete_bytes(),
                        Framer::DtsHd(_) | Framer::DtsExtension(_) => {
                            self.limits.maximum_dts_hd_pes_bytes
                        }
                        _ => self.limits.maximum_audio_frame_bytes,
                    };
                    let range = self.unpublished().ok_or(Error::Truncated)?;
                    if self.completed == self.limits.maximum_access_units {
                        return Err(Error::Limit {
                            requested: self.completed.saturating_add(1),
                            maximum: self.limits.maximum_access_units,
                        });
                    }
                    let size =
                        usize::try_from(range.end - range.start).map_err(|_| Error::Overflow)?;
                    if size > maximum {
                        return Err(Error::Limit {
                            requested: size,
                            maximum,
                        });
                    }
                    self.incomplete = Some(Incomplete::new(FragmentKind::Audio, range, &self.pool));
                    self.framer = None;
                    continue;
                }
                result => result?,
            };
            match step {
                Step::Ready(unit) => self.start_build(unit)?,
                Step::Yield => return Ok(LaneStep::Yield),
                Step::End(_) => {
                    if draining
                        && (self.limits.audio_eof_policy == AudioEofPolicy::ReportIncomplete
                            || self.limits.video_eof_policy == VideoEofPolicy::ReportUnframed
                            || self.limits.unframed_audio_policy
                                == UnframedAudioPolicy::ReportAtEof)
                    {
                        self.framer = None;
                        continue;
                    }
                    return Ok(LaneStep::End);
                }
                Step::NeedInput(request) => {
                    if request.offset != self.supplied {
                        return Err(Error::Invalid("MPEG-TS elementary input offset"));
                    }
                    if let Some(input) = &mut self.input {
                        // Retiring a consumed prefix leaves the current PES at
                        // the tail until its final byte has been handed off.
                        let Some(piece) = self.pieces.as_slice().last() else {
                            self.input = None;
                            continue;
                        };
                        let pes = piece.source.pes();
                        let mut request = request;
                        loop {
                            let Some(span) = pes.spans().get(input.span) else {
                                self.input = None;
                                break;
                            };
                            if !work.try_charge(1) {
                                return Ok(LaneStep::Yield);
                            }
                            if let Framer::Eac3(reader) = framer
                                && input.span == 0
                                && input.offset == 0
                                && reader.provide_complete(&request, pes, work, cancel)?
                            {
                                self.supplied = piece.range.end;
                                self.input = None;
                                break;
                            }
                            let payload = span.as_ref();
                            let length = payload.len();
                            let count = (length - input.offset).min(request.maximum);
                            if count == 0 {
                                return Err(Error::Invalid("empty MPEG-TS payload span"));
                            }
                            let end = self
                                .supplied
                                .checked_add(count as u64)
                                .ok_or(Error::Overflow)?;
                            let range = input.offset..input.offset + count;
                            let appended = if let Framer::Video(reader) = framer {
                                reader.append_payload(
                                    &request,
                                    &payload[range.clone()],
                                    work,
                                    cancel,
                                )?
                            } else {
                                false
                            };
                            if !appended {
                                let bytes = span.payload_bytes()?;
                                let bytes = if count == length {
                                    bytes
                                } else {
                                    bytes.into_slice(range)?
                                };
                                framer.provide(InputWindow::new(
                                    request.source,
                                    request.token.clone(),
                                    request.offset,
                                    bytes,
                                )?)?;
                            }
                            self.supplied = end;
                            input.offset += count;
                            if input.offset == length {
                                input.span += 1;
                                input.offset = 0;
                            }
                            if !appended {
                                break;
                            }
                            request.offset = end;
                        }
                    } else if draining {
                        if self.eof {
                            return Err(Error::Invalid("MPEG-TS framer requested input after EOF"));
                        }
                        if !work.try_charge(1) {
                            return Ok(LaneStep::Yield);
                        }
                        framer.finish(request)?;
                        self.eof = true;
                    } else {
                        return Ok(LaneStep::NeedPayload);
                    }
                }
            }
        }
    }
    fn check_output_limit(&self) -> Result<()> {
        if self.completed == self.limits.maximum_access_units {
            return Err(Error::Limit {
                requested: self.completed.saturating_add(1),
                maximum: self.limits.maximum_access_units,
            });
        }
        Ok(())
    }

    fn preserve_latm_pes(&mut self, draining: bool, work: &mut WorkBudget) -> Result<LaneStep> {
        let Some(piece) = self.pieces.as_slice().last() else {
            return Ok(if draining {
                LaneStep::End
            } else {
                LaneStep::NeedPayload
            });
        };
        if !work.try_charge(1) {
            return Ok(LaneStep::Yield);
        }
        self.check_output_limit()?;
        let packet = latm::PesPacket::new(
            self.track.clone(),
            piece.source.clone(),
            piece.clock.key,
            piece.clock.quarantined,
        );
        self.supplied = piece.range.end;
        self.published = piece.range.end;
        self.last_anchor = piece.serial;
        self.retire = self.pieces.len();
        self.completed += 1;
        self.input = None;
        Ok(LaneStep::LatmPes(packet))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod incomplete_audio_tests;
