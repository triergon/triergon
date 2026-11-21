//! Access units being assembled from their PES pieces and published as packets.
use super::*;

pub(super) enum BuildUnit {
    Packet(AccessUnit),
    Unframed(UnframedKind, Range<u64>),
}

impl BuildUnit {
    fn range_and_anchor(&self) -> (Range<u64>, u64) {
        match self {
            Self::Packet(unit) => (unit.range(), unit.anchor_offset()),
            Self::Unframed(_, range) => (range.clone(), range.start),
        }
    }
}

pub(super) enum BuildOrigins {
    ReserveTable(usize),
    Ready(PayloadSources),
}

pub(super) struct Build {
    pub(super) unit: BuildUnit,
    pub(super) buffer: Option<BudgetVec<u8>>,
    pub(super) bytes: Option<Bytes>,
    pub(super) nal: usize,
    pub(super) part: u8,
    pub(super) copied: usize,
    pub(super) origins: BuildOrigins,
    pub(super) origin: usize,
    pub(super) clock: Option<PacketClock>,
    pub(super) picture_origin: Option<(SourceEpochs, u64, u64)>,
    pub(super) picture_anchor: Option<usize>,
    pub(super) last_anchor: u64,
    pub(super) retire: usize,
    pub(super) recovered_adts_bytes: bool,
}

impl Build {
    pub(super) fn new(unit: AccessUnit, pool: &Budget, last_anchor: u64) -> Self {
        Self::with_unit(BuildUnit::Packet(unit), None, pool, last_anchor)
    }
    pub(super) fn with_unit(
        unit: BuildUnit,
        bytes: Option<Bytes>,
        pool: &Budget,
        last_anchor: u64,
    ) -> Self {
        let origins = match &unit {
            // The DVB storage check already accounts for a separate table.
            BuildUnit::Packet(AccessUnit::DvbSubtitle(_)) => {
                PayloadSources::Table(BudgetVec::new(pool))
            }
            _ => PayloadSources::Inline(None),
        };
        let buffer =
            matches!(unit, BuildUnit::Packet(AccessUnit::Video(_))).then(|| BudgetVec::new(pool));
        Self {
            unit,
            buffer,
            bytes,
            nal: 0,
            part: 0,
            copied: 0,
            origins: BuildOrigins::Ready(origins),
            origin: 0,
            clock: None,
            picture_origin: None,
            picture_anchor: None,
            last_anchor,
            retire: 0,
            recovered_adts_bytes: false,
        }
    }
    #[expect(
        clippy::expect_used,
        reason = "a typed coalescing state measured +0.17 to +0.25% more instructions"
    )]
    pub(super) fn copy(&mut self, work: &mut WorkBudget, cancel: &AtomicBool) -> Result<bool> {
        if self.bytes.is_some() {
            return Ok(true);
        }
        let BuildUnit::Packet(AccessUnit::Video(unit)) = &self.unit else {
            return Ok(true);
        };
        let size =
            usize::try_from(unit.range.end - unit.range.start).map_err(|_| Error::Overflow)?;
        let buffer = self.buffer.as_mut().expect("pending coalescing storage");
        buffer.try_reserve_exact(size.saturating_sub(buffer.len()))?;
        const ZEROS: [u8; 256] = [0; 256];
        while let Some(nal) = unit.nals().get(self.nal) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let facts = nal.facts();
            let triergon_framing::nal::NalFraming::AnnexB {
                leading_zero_bytes,
                start_code_bytes,
                ..
            } = facts.framing
            else {
                unreachable!()
            };
            let (bytes, total, zeros): (&[u8], usize, bool) = match self.part {
                0 => (&ZEROS, leading_zero_bytes, true),
                1 if start_code_bytes == 4 => (&[0, 0, 0, 1], 4, false),
                1 => (&[0, 0, 1], 3, false),
                2 => (nal.bytes().as_ref(), nal.bytes().len(), false),
                3 => (&ZEROS, facts.trailing_zero_bytes, true),
                _ => unreachable!(),
            };
            if self.copied == total {
                if !work.try_charge(1) {
                    return Ok(false);
                }
                self.copied = 0;
                self.part += 1;
                if self.part == 4 {
                    self.part = 0;
                    self.nal += 1;
                }
                continue;
            }
            let count = (total - self.copied).min(if zeros { ZEROS.len() } else { total });
            let count = usize::try_from(work.take(count as u64)).map_err(|_| Error::Overflow)?;
            if count == 0 {
                return Ok(false);
            }
            let start = if zeros { 0 } else { self.copied };
            buffer.try_extend_from_slice(&bytes[start..start + count])?;
            self.copied += count;
        }
        if buffer.len() != size {
            return Err(Error::Invalid("MPEG-TS AU source extent"));
        }
        if !work.try_charge(1) {
            return Ok(false);
        }
        match self.buffer.take().expect("coalescing storage").freeze() {
            Ok(bytes) => self.bytes = Some(bytes),
            Err(error) => {
                self.buffer = Some(error.value);
                return Err(error.error);
            }
        }
        Ok(true)
    }
}

impl Lane {
    pub(super) fn start_build(&mut self, unit: AccessUnit) -> Result<()> {
        self.check_output_limit()?;
        self.build = Some(Build::new(unit, &self.pool, self.last_anchor));
        Ok(())
    }

    #[expect(
        clippy::expect_used,
        reason = "moving the build out of its slot to publish measured +0.35 to +0.72% more instructions"
    )]
    fn finish_build(&mut self, consumed_end: u64) -> Build {
        let build = self.build.take().expect("prepared publication");
        self.retire = build.retire;
        self.last_anchor = build.last_anchor;
        self.completed += 1;
        self.published = consumed_end;
        build
    }

    #[expect(
        clippy::expect_used,
        reason = "moving the build out of its slot to publish measured +0.35 to +0.72% more instructions"
    )]
    fn probe_picture(
        &mut self,
        range: Range<u64>,
        anchor_offset: u64,
        (has_vcl, random_access): (bool, bool),
        scan: bool,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<LaneStep> {
        let build = self.build.as_mut().expect("pending picture");
        let pieces = self.pieces.as_slice();
        // Track the same source extent and anchor as a delivered packet without
        // retaining PES owners or computing decode times for discarded pictures.
        while let Some(piece) = pieces.get(build.origin) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            if piece.range.start < range.end
                && range.start < piece.range.end
                && build.picture_origin.is_none()
            {
                let pes = piece.source.pes();
                build.picture_origin =
                    Some((pes.epochs, pes.byte_run_epoch, pes.elementary_offset));
            }
            if piece.range.start <= anchor_offset && piece.serial > build.last_anchor {
                build.picture_anchor = Some(build.origin);
                build.last_anchor = piece.serial;
            }
            build.origin += 1;
        }
        while let Some(piece) = pieces.get(build.retire) {
            if piece.range.end > range.end || piece.serial > build.last_anchor {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            build.retire += 1;
        }
        if !work.try_charge(1) {
            return Ok(LaneStep::Yield);
        }
        let (epochs, byte_run, pes_offset) = build
            .picture_origin
            .ok_or(Error::Invalid("MPEG-TS AU has no source extent"))?;
        let anchor = build.picture_anchor.map(|index| &pieces[index]);
        let (ticks, time) = if scan
            && has_vcl
            && let Some(anchor) = anchor
            && !anchor.clock.quarantined
        {
            let pts = anchor.source.pes().header.pts_raw33;
            if self.limits.time_policy == SourceTimePolicy::RelativeEpochZero && !random_access {
                // The validated raw PTS is sufficient for detecting a wrap.
                (pts.map(i128::from), None)
            } else if random_access {
                let stamp = source_timestamp(pts, anchor.clock.key, self.limits.time_policy)?;
                (stamp.ticks90, stamp.exact)
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        self.finish_build(range.end);
        Ok(LaneStep::PictureProbe(SeekProbe {
            offset: range.start,
            epochs,
            byte_run,
            pes_offset,
            ticks,
            time,
            random_access,
            has_vcl,
        }))
    }

    #[expect(
        clippy::expect_used,
        reason = "moving the build out of its slot to publish measured +0.35 to +0.72% more instructions"
    )]
    pub(super) fn publish(
        &mut self,
        picture_delivery: PictureDelivery,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<LaneStep> {
        let build = self.build.as_mut().expect("pending publication");
        let (range, anchor_offset) = build.unit.range_and_anchor();
        let probing = match picture_delivery {
            PictureDelivery::Full => false,
            PictureDelivery::Scan | PictureDelivery::Discard => true,
            PictureDelivery::Landing {
                offset,
                byte_run,
                publish_headers,
            } => {
                let header_group = publish_headers
                    && matches!(&build.unit, BuildUnit::Packet(AccessUnit::Video(unit)) if !unit.has_vcl);
                !header_group
                    && (offset != range.start
                        || !matches!(self.run, PesRun::Active(_, run) if run == byte_run))
            }
        };
        if probing && let BuildUnit::Packet(AccessUnit::Video(unit)) = &build.unit {
            let flags = (unit.has_vcl, unit.random_access);
            return self.probe_picture(
                range,
                anchor_offset,
                flags,
                matches!(picture_delivery, PictureDelivery::Scan),
                work,
                cancel,
            );
        }
        if let BuildOrigins::ReserveTable(count) = build.origins {
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            let mut origins = BudgetVec::new(&self.pool);
            origins.try_reserve_exact(count)?;
            build.origins = BuildOrigins::Ready(PayloadSources::Table(origins));
        }
        if !build.copy(work, cancel)? {
            return Ok(LaneStep::Yield);
        }
        let BuildOrigins::Ready(origins) = &mut build.origins else {
            unreachable!("prepared PES sources");
        };
        let consumed_end = match &build.unit {
            BuildUnit::Packet(AccessUnit::DvbSubtitle(unit)) => unit.pes_end,
            _ => range.end,
        };
        while let Some(piece) = self.pieces.as_slice().get(build.origin) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            let start = range.start.max(piece.range.start);
            let end = range.end.min(piece.range.end);
            if start < end {
                if !origins.as_slice().is_empty()
                    && self.track.stream().stream_type == 0x87
                    && matches!(&build.unit, BuildUnit::Packet(AccessUnit::Ac3(_)))
                {
                    return Err(Error::Invalid("ATSC E-AC-3 access unit crosses PES"));
                }
                origins.try_push(
                    &self.pool,
                    PayloadSource {
                        elementary_range: start..end,
                        payload_range: usize::try_from(start - piece.range.start)
                            .map_err(|_| Error::Overflow)?
                            ..usize::try_from(end - piece.range.start)
                                .map_err(|_| Error::Overflow)?,
                        pes: piece.source.clone(),
                    },
                )?;
                build.recovered_adts_bytes |= matches!(
                    piece.source.event(),
                    crate::stream::TraceEvent::UnboundedAdtsPes(_)
                );
            }
            if piece.range.start <= anchor_offset && piece.serial > build.last_anchor {
                let pes = piece.source.pes();
                let policy = if piece.clock.quarantined {
                    SourceTimePolicy::Unanchored
                } else {
                    self.limits.time_policy
                };
                build.clock = Some(PacketClock {
                    key: piece.clock.key,
                    quarantined: piece.clock.quarantined,
                    pts: source_timestamp(pes.header.pts_raw33, piece.clock.key, policy)?,
                    dts: source_timestamp(pes.header.dts_raw33, piece.clock.key, policy)?,
                    pes: piece.source.clone(),
                });
                build.last_anchor = piece.serial;
            }
            build.origin += 1;
        }
        while let Some(piece) = self.pieces.as_slice().get(build.retire) {
            if piece.range.end > consumed_end || piece.serial > build.last_anchor {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if !work.try_charge(1) {
                return Ok(LaneStep::Yield);
            }
            build.retire += 1;
        }
        if !work.try_charge(1) {
            return Ok(LaneStep::Yield);
        }
        let epochs = origins
            .as_slice()
            .first()
            .ok_or(Error::Invalid("MPEG-TS AU has no source extent"))?
            .pes
            .pes()
            .epochs;
        let (charge, duration) = match &build.unit {
            BuildUnit::Packet(unit) => {
                (Shared::<PacketData>::reserve(&self.pool)?, unit.duration()?)
            }
            BuildUnit::Unframed(..) => (Shared::<UnframedData>::reserve(&self.pool)?, None),
        };
        let duration90 = duration
            .map(|time| -> Result<Option<i64>> {
                const TICKS_PER_SECOND: u32 = 90_000;
                // Time is reduced, so the denominator must divide the clock rate.
                let Ok(denominator) = u32::try_from(time.denominator()) else {
                    return Ok(None);
                };
                if !TICKS_PER_SECOND.is_multiple_of(denominator) {
                    return Ok(None);
                }
                let numerator = i64::try_from(time.numerator()).map_err(|_| Error::Overflow)?;
                let scale = i64::from(TICKS_PER_SECOND / denominator);
                Ok(Some(numerator.checked_mul(scale).ok_or(Error::Overflow)?))
            })
            .transpose()?
            .flatten();
        let packet_clock = match &build.unit {
            BuildUnit::Packet(_) => build.clock.as_ref(),
            BuildUnit::Unframed(..) => None,
        };
        let pts = packet_clock
            .and_then(|clock| clock.pts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let dts = packet_clock
            .and_then(|clock| clock.dts.ticks90)
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::Overflow)?;
        let build = self.finish_build(consumed_end);
        let recovered_adts = build.recovered_adts_bytes
            || build.clock.as_ref().is_some_and(|clock| {
                matches!(
                    clock.pes.event(),
                    crate::stream::TraceEvent::UnboundedAdtsPes(_)
                )
            });
        let track = self.track.clone();
        let BuildOrigins::Ready(origins) = build.origins else {
            unreachable!("prepared PES sources");
        };
        match build.unit {
            BuildUnit::Packet(unit) => Ok(LaneStep::Packet(Packet(Shared::new(
                PacketData {
                    track,
                    unit,
                    video_bytes: build.bytes,
                    sources: origins,
                    clock: build.clock,
                    epochs,
                    duration,
                    pts,
                    dts,
                    duration90,
                    recovered_adts,
                },
                charge,
            )))),
            BuildUnit::Unframed(kind, range) => {
                let PayloadSources::Table(sources) = origins else {
                    unreachable!("unframed PES sources use a table");
                };
                let data = Shared::new(
                    UnframedData {
                        track,
                        range,
                        bytes: build.bytes.expect("prepared unframed bytes"),
                        sources,
                        clock: build.clock,
                        epochs,
                    },
                    charge,
                );
                Ok(match kind {
                    UnframedKind::Video => LaneStep::IncompleteVideo(IncompleteVideo(data)),
                    UnframedKind::Audio(carriage) => {
                        LaneStep::UnframedAudio(UnframedAudio { data, carriage })
                    }
                })
            }
        }
    }
}
