use super::{EOF, MAX_PACKET, Output, Source, extent};
use std::{ops::Range, sync::atomic::AtomicBool};
use triergon_core::{Error, InputCursor, Result, Step, WorkBudget};
use triergon_isobmff::{
    BuildStatus, CursorStatus, Limits, Next, Sample,
    boxes::{BoxEvent, BoxView},
    compat::mov::{
        packets::{PacketLimits, PacketView},
        recovery::{
            DescriptionAssociation, DescriptionContext, DescriptionEmission, DescriptionEvent,
            DescriptionRead, DescriptionRecoveryBuilder, DescriptionRecoveryProfile, DirectContext,
            DirectRead, DirectRecoveryProfile, PhysicalRead, PhysicalReadProfile, PhysicalReader,
            RecoveryTrackHeader, RunRecoveryBuilder, RunRecoveryProfile,
        },
    },
    open,
};

fn child(parent: &BoxView, kind: &[u8; 4], quantum: u64) -> Result<BoxView> {
    let mut children = parent.children(0, Limits::default())?;
    loop {
        match children.poll(&mut WorkBudget::new(quantum))? {
            Next::Item(child) if &child.header().kind == kind => return Ok(child),
            Next::Item(_) | Next::Yield => (),
            Next::End => return Err(Error::Invalid("missing recovery box")),
        }
    }
}

fn roots(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
) -> Result<(BoxView, Vec<Range<u64>>)> {
    let mut reader = open::box_reader(input, MAX_PACKET, true)?;
    let mut movie = None;
    let mut media = Vec::new();
    loop {
        match reader.poll(input, &mut WorkBudget::new(source.quantum), cancel)? {
            Step::Ready(BoxEvent::Header(header)) if header.kind == *b"moov" => {
                reader.read_metadata(input)?;
            }
            Step::Ready(BoxEvent::Header(header))
                if header.kind == *b"mdat"
                    && header
                        .end()?
                        .is_some_and(|end| end > source.bytes.len() as u64) =>
            {
                media.push(header.payload_range()?);
                return Ok((movie.ok_or(Error::Invalid("missing movie box"))?, media));
            }
            Step::Ready(BoxEvent::Header(_)) => reader.skip(input)?,
            Step::Ready(BoxEvent::Metadata(raw)) => movie = Some(raw),
            Step::Ready(BoxEvent::Skipped(header)) if header.kind == *b"mdat" => {
                media.push(header.payload_range()?);
            }
            Step::Ready(BoxEvent::Skipped(_)) | Step::Yield => (),
            Step::NeedInput(request) => source.supply(input, request)?,
            Step::End(_) => return Ok((movie.ok_or(Error::Invalid("missing movie box"))?, media)),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Profile {
    Run,
    Direct,
    Physical,
}

pub(super) fn run(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
    profile: Profile,
    out: &mut Output<'_, '_>,
) -> Result<()> {
    let (movie, media) = roots(source, input, cancel)?;
    let q = source.quantum;
    let track = child(&movie, b"trak", q)?;
    let mdia = child(&track, b"mdia", q)?;
    let header = RecoveryTrackHeader::parse(
        &child(&track, b"tkhd", q)?,
        &child(&mdia, b"mdhd", q)?,
        &child(&mdia, b"hdlr", q)?,
        RunRecoveryProfile,
    )?;
    let raw = child(&child(&mdia, b"minf", q)?, b"stbl", q)?;
    let pool = input.budget().clone();
    if matches!(profile, Profile::Physical) {
        return physical(source, input, cancel, &header, raw, &media, out);
    }
    let direct = matches!(profile, Profile::Direct);
    let mut builder = if direct {
        RunRecoveryBuilder::new_direct(raw, pool.clone(), Limits::default(), DirectRecoveryProfile)?
    } else {
        RunRecoveryBuilder::new(raw, pool.clone(), Limits::default(), RunRecoveryProfile)?
    };
    while builder.poll(&mut WorkBudget::new(q), cancel)? == BuildStatus::Yield {}
    let table = builder.finish()?;
    let length = source.bytes.len() as u64;
    let mut reader = RecoveryPayload {
        source,
        input,
        cancel,
        media: &media,
        previous: None,
    };
    if direct {
        let mut cursor = table.direct_packet_cursor(
            &pool,
            &header,
            &media,
            PacketLimits::default(),
            DirectContext {
                physical_length: length,
            },
            DirectRecoveryProfile,
        )?;
        loop {
            match cursor.poll(&mut WorkBudget::new(q), cancel)? {
                CursorStatus::Yield => (),
                CursorStatus::End => break,
                CursorStatus::Ready => {
                    let item = cursor.take_occurrence()?;
                    match item.read {
                        DirectRead::Available => {
                            if !reader.packet(item.declaration, item.demux, out)? {
                                break;
                            }
                        }
                        DirectRead::ObservedForwardEof => break,
                        DirectRead::UnreviewedSeekRefusal => {
                            if reader.follows_short(&item.demux.source_range) {
                                break;
                            }
                            return Err(Error::Unsupported("unreviewed past-end seek"));
                        }
                    }
                }
            }
        }
    } else {
        let mut cursor = table.packet_cursor(&pool, &header, &media, PacketLimits::default())?;
        loop {
            match cursor.poll(&mut WorkBudget::new(q), cancel)? {
                CursorStatus::Yield => (),
                CursorStatus::End => break,
                CursorStatus::Ready => {
                    let item = cursor.take_occurrence()?;
                    if !reader.packet(item.source, item.demux, out)? {
                        break;
                    }
                }
            }
        }
    }
    out.terminal(Some(EOF))
}

fn physical(
    source: Source<'_>,
    input: &mut InputCursor,
    cancel: &AtomicBool,
    header: &RecoveryTrackHeader,
    raw: BoxView,
    media: &[Range<u64>],
    out: &mut Output<'_, '_>,
) -> Result<()> {
    let q = source.quantum;
    let pool = input.budget().clone();
    let mut builder = DescriptionRecoveryBuilder::new(
        raw,
        pool.clone(),
        Limits::default(),
        DescriptionRecoveryProfile,
    )?;
    while builder.poll(&mut WorkBudget::new(q), cancel)? == BuildStatus::Yield {}
    let table = builder.finish()?;
    let length = source.bytes.len() as u64;
    let [declared] = media else {
        return Err(Error::Unsupported(
            "physical read requires one media extent",
        ));
    };
    let physical_extent = declared.start..declared.end.min(length);
    let media = std::slice::from_ref(&physical_extent);
    let mut cursor = table.physical_packet_cursor(
        &pool,
        header,
        media,
        PacketLimits::default(),
        DescriptionContext {
            physical_length: length,
            custom_read_maximum: source.window,
        },
        PhysicalReadProfile,
    )?;
    let mut reader = RecoveryPayload {
        source,
        input,
        cancel,
        media,
        previous: None,
    };
    loop {
        match cursor.poll(&mut WorkBudget::new(q), cancel)? {
            CursorStatus::Yield => (),
            CursorStatus::End => break,
            CursorStatus::Ready => {
                let event = cursor.take_event()?;
                if event.packet.is_some() && !reader.event(event, out)? {
                    break;
                }
            }
        }
    }
    out.terminal(Some(EOF))
}

struct RecoveryPayload<'a, 'b> {
    source: Source<'a>,
    input: &'b mut InputCursor,
    cancel: &'b AtomicBool,
    media: &'b [Range<u64>],
    previous: Option<PhysicalRead>,
}

impl RecoveryPayload<'_, '_> {
    fn follows_short(&self, range: &Range<u64>) -> bool {
        self.previous.as_ref().is_some_and(|plan| {
            plan.is_short_at_physical_end()
                && plan.requested().end == range.start
                && (1..=MAX_PACKET as u64).contains(&(range.end - range.start))
        })
    }

    fn packet(
        &mut self,
        sample: Sample,
        packet: PacketView,
        out: &mut Output<'_, '_>,
    ) -> Result<bool> {
        let range = packet.source_range.clone();
        let length = self.source.bytes.len() as u64;
        if range.is_empty() {
            self.previous = None;
            out.view_packet(0, &packet, &[])?;
        } else if let Ok(media) = extent(self.media, &range)
            && range.end <= length
        {
            let payload =
                self.source
                    .sample(self.input, Sample { range, ..sample }, media, self.cancel)?;
            self.previous = None;
            out.view_packet(0, &packet, payload.as_ref())?;
        } else {
            let event = DescriptionEvent {
                declaration: sample,
                packet: Some(packet.clone()),
                emission: DescriptionEmission::Packet,
                association: DescriptionAssociation::DeclarationCoordinate,
                raw_chunk_description: packet.source_description,
                physical_chunk: None,
                observed_count_row: None,
                source_timescale: 0,
                source_error: None,
                range_error: None,
                read: if range.start < length {
                    DescriptionRead::Available
                } else {
                    DescriptionRead::UnavailablePhysicalStart
                },
                demux_index_distance: 0,
                modeled_transport_read: None,
            };
            return self.event(event, out);
        }
        Ok(true)
    }
    fn event(&mut self, event: DescriptionEvent, out: &mut Output<'_, '_>) -> Result<bool> {
        if event.read != DescriptionRead::Available {
            if self.previous.as_ref().is_some_and(|previous| {
                previous.modeled_following_eof(&event, PhysicalReadProfile)
                    || (event.read == DescriptionRead::UnavailablePhysicalStart
                        && previous.requested().end == previous.physical_length()
                        && event.packet.as_ref().is_some_and(|packet| {
                            packet.source_range.start == previous.physical_length()
                        }))
            }) {
                return Ok(false);
            }
            return Err(Error::Unsupported("no model for the next physical read"));
        }
        let extent = self
            .media
            .first()
            .ok_or(Error::Invalid("missing media extent"))?;
        let length = self.source.bytes.len() as u64;
        let plan = PhysicalRead::from_event(
            &event,
            self.input,
            extent.start..extent.end.min(length),
            MAX_PACKET,
            PhysicalReadProfile,
        )?;
        let mut reader =
            PhysicalReader::new(self.input, event, plan, MAX_PACKET, PhysicalReadProfile)?;
        let result = self.source.read(self.input, |input, work| {
            reader.poll(input, work, self.cancel)
        })?;
        let mut packet = result
            .event
            .packet
            .ok_or(Error::Invalid("missing physical packet"))?;
        if result.plan.is_short_at_physical_end() {
            packet.flags |= 2;
        }
        out.view_packet(0, &packet, result.payload.as_ref())?;
        self.previous = Some(result.plan);
        Ok(true)
    }
}
