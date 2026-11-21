//! The MP4 engine: indexed rows, buffered samples, the demux view and the
//! forward continuation of a progressive source.

pub(crate) mod demux_replay;
pub mod progressive;

use crate::engine::mp4::demux_replay::{DemuxReplay, DemuxReplayStep};
use crate::event::{
    EngineStep, Event, Mp4EmptyPacket, Mp4Event, Mp4MissingSample, Mp4Packet, Mp4ShortPacket,
};
use crate::{
    Result,
    mp4_open::PendingMedia,
    mp4_plan::{Mp4Configuration, PreparedMp4, SourceRow},
};
use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{Error, InputCursor, Step, WorkBudget};
use triergon_isobmff::{
    CursorStatus,
    compat::mov::{
        front::FrontReadBoundary,
        packets::{PacketCursor, PacketOccurrence},
        recovery::{RecoverySampleExtent, RecoverySampleRead, RunRecoveryProfile},
    },
    payload::{SampleReader, SourcePacket},
};
#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedDescription {
    pub(crate) track: u32,
    pub(crate) description: u32,
    pub(crate) revision: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Mp4SyncLanding {
    pub(crate) first_row: usize,
    pub(crate) sync_row: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Mp4RangeLanding<'a> {
    Source(Mp4SyncLanding),
    Demux(&'a DemuxReplay),
    DemuxCursor(u64),
}

/// How an MP4 engine chooses rows after a seek.
#[expect(
    clippy::large_enum_variant,
    reason = "the replay is built once per seek and polled in place"
)]
pub(crate) enum SeekReplay {
    /// In plan order.
    Forward,
    /// After a sync-sample seek: rows of track `video` before row `sync` are skipped.
    SkipVideoUntil {
        /// `track_ID` of the video track.
        video: u32,
        /// The row of the sync sample.
        sync: usize,
    },
    /// After a demux-view seek: in the order the replay yields.
    DemuxOrder(DemuxReplay),
}

pub struct Mp4Engine {
    pub(crate) progressive: Option<progressive::ProgressiveState>,
    pub(crate) media_cache_fill: Option<PendingMedia>,
    pub(crate) front_boundary: Option<FrontReadBoundary>,
    pub(crate) empty_run: Option<(Mp4EmptyPacket, u32)>,
    pub(crate) ended_short: bool,
    pub(crate) input: InputCursor,
    pub plan: PreparedMp4,
    pub(crate) row: usize,
    pub(crate) seek: SeekReplay,
    pub(crate) packet: Option<(usize, SampleReader)>,
    pub(crate) configs: [Option<CachedDescription>; 64],
    pub(crate) revision: u64,
    pub(crate) maximum_packet: usize,
}

impl Mp4Engine {
    pub(crate) fn is_progressive(&self) -> bool {
        self.progressive.is_some()
    }

    pub(crate) fn supports_range_replay(&self) -> bool {
        self.progressive.is_none()
            && self.plan.unavailable_first.is_none()
            && self.input.capabilities().random_access
    }

    fn packet_config(&self, at_track: usize, description: u32) -> Result<Mp4Configuration> {
        let revision = self.configs[at_track]
            .ok_or(Error::Invalid("configuration missing"))?
            .revision;
        self.plan.tracks.as_slice()[at_track].configuration(description, revision)
    }

    fn skip_samples(
        &self,
        at_track: usize,
        occurrence: &PacketOccurrence,
    ) -> Result<Option<[u8; 10]>> {
        if occurrence.source.fragment_flags.is_some() && occurrence.source.ordinal == 1 {
            let samples = self.plan.tracks.as_slice()[at_track].fragment_aac_trim;
            return Ok((samples != 0).then(|| {
                let mut trim = [0; 10];
                trim[..4].copy_from_slice(&samples.to_le_bytes());
                trim
            }));
        }
        match &self.plan.encoded_edits {
            Some(plans) => plans.as_slice()[at_track]
                .as_ref()
                .map_or(Ok(None), |plan| plan.skip_samples(&occurrence.source)),
            None => Ok(None),
        }
    }

    fn emit_packet(
        &mut self,
        at_track: usize,
        source: SourcePacket,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        let cfg = self.packet_config(at_track, source.sample.description_index)?;
        let occurrence = match cursor {
            Some(cursor) => Some(cursor.take_occurrence()?),
            None => self.plan.demux_row(self.row).cloned(),
        };
        let encoded_skip_samples = occurrence.as_ref().map_or(Ok(None), |occurrence| {
            self.skip_samples(at_track, occurrence)
        })?;
        if occurrence.is_some() && source.sample.range.is_empty() {
            self.front_boundary = self.plan.front_read_boundary().filter(|boundary| {
                boundary.track_id == cfg.track.track_id
                    && boundary.source_ordinal == source.sample.ordinal
                    && boundary.source_offset == source.sample.range.start
            });
        }
        if let SeekReplay::DemuxOrder(replay) = &mut self.seek {
            replay.delivered(self.row)?;
        } else {
            self.row += 1;
        }
        Ok(EngineStep::Event(Event::Mp4(Mp4Event::Packet(Mp4Packet {
            front_read_boundary: self.front_boundary,
            source,
            configuration: cfg,
            occurrence,
            encoded_skip_samples,
        }))))
    }

    fn poll_progressive(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        let Some(state) = self.progressive.as_mut() else {
            return Ok(EngineStep::End);
        };
        let mut continuation = progressive::Continuation {
            input: &mut self.input,
            plan: &mut self.plan,
            row: &mut self.row,
            media_cache_fill: &mut self.media_cache_fill,
            empty_run: &mut self.empty_run,
        };
        state.poll(&mut continuation, w, c)
    }

    fn poll_unavailable_first(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        let start = self
            .plan
            .unavailable_first
            .as_ref()
            .ok_or(Error::Invalid("missing first sample declaration"))?
            .sample
            .range
            .start;
        let within_source = if self.plan.demux_recovery {
            Error::Invalid("native first sample starts before physical end")
        } else {
            crate::mp4_plan::BACKWARD_LAYOUT_REFUSAL
        };
        if self.input.position() > start {
            return Err(within_source);
        }
        let step = self.poll_progressive(w, c)?;
        if !matches!(step, EngineStep::End) {
            return Ok(step);
        }
        match self.input.read_discard(1, w, c)? {
            Step::End(end) => {
                if end.offset > start {
                    return Err(within_source);
                }
                if !self.plan.demux_recovery {
                    return Err(Error::Invalid("sample outside media extent"));
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                let row = self
                    .plan
                    .unavailable_first
                    .as_ref()
                    .ok_or(Error::Invalid("missing first sample declaration"))?;
                self.ended_short = true;
                Ok(EngineStep::Event(Event::Mp4(Mp4Event::MissingSample(
                    Mp4MissingSample {
                        declared: row.sample.clone(),
                        track_id: row.track_id,
                        end,
                    },
                ))))
            }
            Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
            Step::Yield => Ok(EngineStep::Yield),
            Step::Ready(_) => Err(Error::Invalid("first sample requires source end")),
        }
    }

    fn poll_packet(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        let (at_track, reader) = self
            .packet
            .as_mut()
            .ok_or(Error::Invalid("pending sample missing"))?;
        let at_track = *at_track;
        let step = if self.plan.demux_recovery && self.plan.demux_row(self.row).is_some() {
            reader.poll_recovery(&mut self.input, w, c, RunRecoveryProfile)?
        } else {
            match reader.poll(&mut self.input, w, c)? {
                Step::Ready(packet) => Step::Ready(RecoverySampleRead::Complete(packet)),
                Step::NeedInput(r) => Step::NeedInput(r),
                Step::Yield => Step::Yield,
                Step::End(at) => Step::End(at),
            }
        };
        match step {
            Step::Ready(RecoverySampleRead::Complete(source)) => {
                let event = self.emit_packet(at_track, source, cursor)?;
                self.packet = None;
                Ok(event)
            }
            Step::Ready(RecoverySampleRead::Short(prefix)) => {
                let configuration =
                    self.packet_config(at_track, prefix.declared.description_index)?;
                let mut occurrence = self
                    .plan
                    .demux_row(self.row)
                    .ok_or(Error::Invalid("short packet needs native occurrence"))?
                    .clone();
                occurrence.demux.flags |= 2;
                let encoded_skip_samples = self.skip_samples(at_track, &occurrence)?;
                self.packet = None;
                self.ended_short = true;
                Ok(EngineStep::Event(Event::Mp4(Mp4Event::ShortPacket(
                    Mp4ShortPacket {
                        prefix,
                        configuration,
                        occurrence,
                        encoded_skip_samples,
                    },
                ))))
            }
            Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
            Step::Yield => Ok(EngineStep::Yield),
            Step::End(_) => Err(Error::Truncated),
        }
    }

    /// One packet of a run of empty demux packets at the front read boundary.
    #[cold]
    fn emit_empty_run(
        &mut self,
        mut packet: Mp4EmptyPacket,
        remaining: u32,
        w: &mut WorkBudget,
    ) -> Result<EngineStep> {
        if !w.try_charge(1) {
            return Ok(EngineStep::Yield);
        }
        if remaining > 1 {
            self.empty_run = Some((packet, remaining - 1));
            packet.duration = 0;
        } else {
            self.empty_run = None;
        }
        Ok(EngineStep::Event(Event::Mp4(Mp4Event::EmptyPacket(packet))))
    }

    /// Reads the media a demux-order replay serves from memory, once per engine.
    #[cold]
    fn fill_media_cache(&mut self, w: &mut WorkBudget, c: &AtomicBool) -> Result<EngineStep> {
        let Some(replay) = self.media_cache_fill.as_mut() else {
            return Ok(EngineStep::Yield);
        };
        match replay.poll(&mut self.input, w, c)? {
            Step::Ready(cached) => {
                self.plan
                    .media_cache
                    .try_push(cached)
                    .map_err(|failure| failure.error)?;
                self.media_cache_fill = None;
                Ok(EngineStep::Yield)
            }
            Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
            Step::Yield => Ok(EngineStep::Yield),
            Step::End(_) => Err(Error::Truncated),
        }
    }

    /// Publishes the configuration of track `at` for `row`'s sample description
    /// under the next revision.
    #[cold]
    fn publish_configuration(
        &mut self,
        at: usize,
        row: &SourceRow,
        w: &mut WorkBudget,
    ) -> Result<EngineStep> {
        w.take(1);
        let revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        let cfg = self.plan.tracks.as_slice()[at]
            .configuration(row.sample.description_index, revision)?;
        self.configs[at] = Some(CachedDescription {
            track: row.track_id,
            description: row.sample.description_index,
            revision,
        });
        self.revision = revision;
        Ok(EngineStep::Event(Event::Mp4(Mp4Event::Configuration(cfg))))
    }

    // The measured per-packet spine: row, configuration check, gap, buffered
    // sample, `emit_packet`. Its rare arms are cold methods called in tail position.
    #[inline(never)]
    pub(crate) fn poll(
        &mut self,
        w: &mut WorkBudget,
        c: &AtomicBool,
        mut cursor: Option<&mut PacketCursor<'_>>,
    ) -> Result<EngineStep> {
        if self.ended_short {
            self.input.read_into(&mut [], w, c)?;
            return Ok(EngineStep::End);
        }
        if self.plan.unavailable_first.is_some() {
            return self.poll_unavailable_first(w, c);
        }
        if let Some((packet, remaining)) = self.empty_run
            && self.front_boundary == Some(packet.boundary)
        {
            return self.emit_empty_run(packet, remaining, w);
        }
        if self.media_cache_fill.is_some() {
            return self.fill_media_cache(w, c);
        }
        if self.packet.is_some() {
            return self.poll_packet(w, c, cursor);
        }
        if let SeekReplay::DemuxOrder(replay) = &mut self.seek {
            match replay.poll(self.plan.demux_rows(), w, c)? {
                DemuxReplayStep::Ready(row) => self.row = row,
                DemuxReplayStep::Yield => return Ok(EngineStep::Yield),
                DemuxReplayStep::End => return Ok(EngineStep::End),
            }
        }
        if let SeekReplay::SkipVideoUntil { video, sync } = self.seek {
            while self.row < sync && self.plan.source_rows()[self.row].track_id == video {
                if c.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                self.row += 1;
            }
            if self.row >= sync {
                self.seek = SeekReplay::Forward;
            }
        }
        let row = if let Some(cursor) = cursor.as_deref_mut() {
            match cursor.poll(w, c)? {
                CursorStatus::Ready => {
                    let occurrence = cursor
                        .occurrence()
                        .ok_or(Error::Invalid("missing native range occurrence"))?;
                    Some(SourceRow {
                        track_id: occurrence.track_id,
                        sample: occurrence.source.clone(),
                    })
                }
                CursorStatus::Yield => return Ok(EngineStep::Yield),
                CursorStatus::End => return Ok(EngineStep::End),
            }
        } else {
            self.plan.row(self.row)
        };
        let Some(row) = row else {
            return self.poll_progressive(w, c);
        };
        if self.plan.forward_regular_extents.is_some() {
            let [extent] = self.plan.media.as_slice() else {
                return Err(Error::Invalid("forward regular live media extent"));
            };
            if row.sample.require_within(extent.clone()).is_err() {
                if row.sample.range.start >= extent.end {
                    return self.poll_progressive(w, c);
                }
                if !w.try_charge(1) {
                    return Ok(EngineStep::Yield);
                }
                return Err(Error::Invalid("sample outside media extent"));
            }
        }
        if w.remaining() == 0 {
            return Ok(EngineStep::Yield);
        }
        let at = self
            .plan
            .tracks
            .as_slice()
            .iter()
            .position(|t| t.snapshot.track_id == row.track_id)
            .ok_or(Error::Invalid("sample track missing"))?;
        let changed = self.configs[at].is_none_or(|d| {
            d.track != row.track_id || d.description != row.sample.description_index
        });
        if changed {
            return self.publish_configuration(at, &row, w);
        }
        let sequential = !self.input.capabilities().random_access;
        let position = self.input.position();
        if sequential && position < row.sample.range.start {
            let count = usize::try_from((row.sample.range.start - position).min(usize::MAX as u64))
                .map_err(|_| Error::Overflow)?;
            // Discard charges the byte work itself. With a one-unit quantum
            // that unit must reach the input cursor for the gap to advance.
            return match self.input.read_discard(count, w, c)? {
                Step::Ready(_) | Step::Yield => Ok(EngineStep::Yield),
                Step::NeedInput(r) => Ok(EngineStep::NeedInput(r)),
                Step::End(_) => Err(Error::Truncated),
            };
        }
        w.take(1);
        if sequential && position > row.sample.range.start {
            if let Some(payload) = self.plan.cached_payload(&row.sample.range)? {
                self.input.read_into(&mut [], w, c)?;
                if payload.len() > self.maximum_packet {
                    return Err(Error::Limit {
                        requested: payload.len(),
                        maximum: self.maximum_packet,
                    });
                }
                let source = SourcePacket {
                    sample: row.sample.clone(),
                    payload,
                    source: self.input.source(),
                    token: self.input.token(),
                };
                return self.emit_packet(at, source, cursor);
            }
            return Err(Error::Unsupported(
                "sequential source sample bytes already passed",
            ));
        }
        let extent = self
            .plan
            .media
            .as_slice()
            .iter()
            .find(|r| row.sample.require_within((*r).clone()).is_ok())
            .cloned();
        let media = if let Some(extent) = extent {
            RecoverySampleExtent::Finite(extent)
        } else if let Some(terminal) = self.plan.terminal_mdat {
            RecoverySampleExtent::Terminal(terminal)
        } else {
            return Err(Error::Invalid("sample has no source mdat"));
        };
        let packet = if self.plan.demux_recovery && self.plan.demux_row(self.row).is_some() {
            SampleReader::new_recovery(
                &mut self.input,
                row.sample,
                media,
                self.maximum_packet,
                RunRecoveryProfile,
            )?
        } else {
            match media {
                RecoverySampleExtent::Finite(extent) => {
                    // The extent lookup checked this range. A whole sample
                    // at the cursor can be emitted without staging a read.
                    let bytes = row.sample.range.end - row.sample.range.start;
                    if position == row.sample.range.start
                        && (1..=self.maximum_packet as u64).contains(&bytes)
                        && w.remaining() > bytes
                        && self.input.buffered_owner().is_some()
                        && bytes <= self.input.buffered().len() as u64
                    {
                        if !sequential {
                            self.input.request_range_reusing_window(position)?;
                        }
                        let Step::Ready(payload) = self.input.read_span(bytes as usize, w, c)?
                        else {
                            return Err(Error::Invalid("buffered sample unavailable"));
                        };
                        w.take(1);
                        let source = SourcePacket {
                            sample: row.sample,
                            payload,
                            source: self.input.source(),
                            token: self.input.token(),
                        };
                        return self.emit_packet(at, source, cursor);
                    }
                    SampleReader::new_with_shared_window(
                        &mut self.input,
                        row.sample,
                        extent,
                        self.maximum_packet,
                    )?
                }
                RecoverySampleExtent::Terminal(terminal) => SampleReader::new_terminal_mdat(
                    &mut self.input,
                    row.sample,
                    terminal,
                    self.maximum_packet,
                )?,
            }
        };
        self.packet = Some((at, packet));
        if w.remaining() == 0 {
            return Ok(EngineStep::Yield);
        }
        self.poll_packet(w, c, cursor)
    }
}
