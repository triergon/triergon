//! The storage audit: every owner of reader storage, walked one edge per work unit.
use super::*;

/// The reader's owners are not the ones the suspended audit was walking.
const CHANGED: Error = Error::Invalid("MPEG-TS storage audit changed during scan");

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageCursor {
    /// The stream around the PES route whose growth was refused.
    Transport(TransportStorageCursor),
    /// The whole open stream, then the event the reader is admitting.
    OpenTransport(TransportStorageCursor),
    Cause(usize),
    Map,
    MapSource,
    MapProgram,
    MapStream(usize),
    Boundary,
    BoundaryBacking,
    Input,
    Lane(usize),
    Framer(usize),
    FramerBacking(usize, usize),
    Piece {
        lane: usize,
        piece: usize,
    },
    Span {
        lane: usize,
        piece: usize,
        span: usize,
    },
    End,
}

impl StorageCursor {
    /// The edges only an audit inside the open stream walks: the stream, then `cause`.
    // Out of line, so the walk of the PES growth audit, which some workloads run per
    // carrier, keeps its code.
    #[cold]
    #[inline(never)]
    fn step_in_stream<'a>(
        &mut self,
        reader: &'a crate::reader::Reader,
        cause: Option<&'a SourceEvent>,
    ) -> Result<Option<StorageItem<'a>>> {
        Ok(match *self {
            Self::OpenTransport(cursor) => {
                let mut next = cursor;
                let Some(stream) = reader.stream() else {
                    return Err(CHANGED);
                };
                let item = next.step(stream, None)?;
                *self = if next == TransportStorageCursor::End {
                    Self::Cause(0)
                } else {
                    Self::OpenTransport(next)
                };
                item
            }
            Self::Cause(index) => {
                let backing = cause
                    .ok_or(CHANGED)
                    .map(|cause| event_backing(cause.event(), index))?;
                *self = if backing.is_some() {
                    Self::Cause(index + 1)
                } else {
                    Self::Map
                };
                backing.map(StorageItem::Bytes)
            }
            _ => return Err(CHANGED),
        })
    }

    // One structural edge per call, including empty tables. Checking earlier
    // entries for shared backing also consumes cooperative work.
    fn step<'a, C: Cause<'a>>(
        &mut self,
        reader: &'a crate::reader::Reader,
        cause: C,
    ) -> Result<Option<StorageItem<'a>>> {
        let item = match *self {
            Self::Transport(cursor) => {
                let mut next = cursor;
                let Some(stream) = reader.stream() else {
                    return Err(CHANGED);
                };
                let blocked = stream
                    .assembling_route()
                    .ok_or(Error::Invalid("PES storage audit outside drain"))?;
                let item = next.step(stream, Some(blocked))?;
                *self = if next == TransportStorageCursor::End {
                    Self::Map
                } else {
                    Self::Transport(next)
                };
                item
            }
            Self::OpenTransport(_) | Self::Cause(_) => {
                return self.step_in_stream(reader, cause.event());
            }
            Self::Map => {
                *self = Self::MapSource;
                reader
                    .map
                    .as_ref()
                    .map(|source| {
                        let map = source.map();
                        Shared::<crate::stream::TraceEvent>::allocation_bytes()?
                            .checked_add(table_storage(&map.streams)?)
                            .and_then(|n| n.checked_add(table_storage(&map.source_ranges).ok()?))
                            .map(StorageItem::Fixed)
                            .ok_or(Error::Overflow)
                    })
                    .transpose()?
            }
            Self::MapSource => {
                *self = Self::MapProgram;
                reader
                    .map
                    .as_ref()
                    .map(|source| StorageItem::Bytes(&source.map().source))
            }
            Self::MapProgram => {
                *self = Self::MapStream(0);
                reader
                    .map
                    .as_ref()
                    .map(|source| StorageItem::Bytes(&source.map().program_descriptors))
            }
            Self::MapStream(index) => {
                let stream = reader
                    .map
                    .as_ref()
                    .and_then(|source| source.map().streams.as_slice().get(index));
                if let Some(stream) = stream {
                    *self = Self::MapStream(index + 1);
                    Some(StorageItem::Bytes(&stream.descriptor_bytes))
                } else {
                    *self = if reader.stream().is_some() {
                        Self::Boundary
                    } else {
                        Self::Input
                    };
                    None
                }
            }
            Self::Boundary => {
                *self = Self::BoundaryBacking;
                if let Some(boundary) = &reader.boundary {
                    if let crate::stream::TraceEvent::ProgramMap { .. } = boundary.event() {
                        // Publishing the PMT commits the same event as the map.
                        debug_assert!(
                            reader
                                .map
                                .as_ref()
                                .is_some_and(|map| { map.0.shares_storage_with(&boundary.0) })
                        );
                        None
                    } else {
                        Some(StorageItem::Fixed(
                            Shared::<crate::stream::TraceEvent>::allocation_bytes()?,
                        ))
                    }
                } else {
                    None
                }
            }
            Self::BoundaryBacking => {
                *self = Self::Input;
                reader
                    .boundary
                    .as_ref()
                    .and_then(|boundary| match boundary.event() {
                        crate::stream::TraceEvent::Diagnostic(fault) => fault
                            .rejected
                            .as_ref()
                            .map(|rejected| StorageItem::Bytes(rejected.backing_bytes())),
                        _ => None,
                    })
            }
            Self::Input => {
                *self = Self::Lane(0);
                reader
                    .input()
                    .ok()
                    .and_then(triergon_core::InputCursor::buffered_owner)
                    .map(StorageItem::Bytes)
            }
            Self::Lane(index) => {
                if let Some(lane) = reader.lanes.as_slice().get(index) {
                    let live = reader.stream().is_some();
                    if lane.input.is_some()
                        || lane.build.is_some()
                        || lane
                            .incomplete
                            .as_ref()
                            .is_some_and(|copy| copy.bytes.is_some())
                        || (!live && (lane.framer.is_some() || lane.residue.is_some()))
                        || reader
                            .map
                            .as_ref()
                            .is_none_or(|map| !map.0.shares_storage_with(&lane.track.source.0))
                    {
                        return Err(Error::Invalid("MPEG-TS storage audit before lane drain"));
                    }
                    *self = if lane.framer.is_some() {
                        Self::Framer(index)
                    } else {
                        Self::Piece {
                            lane: index,
                            piece: 0,
                        }
                    };
                    Some(StorageItem::Fixed(table_storage(&lane.pieces)?))
                } else {
                    *self = Self::End;
                    None
                }
            }
            Self::Framer(lane) => {
                *self = Self::FramerBacking(lane, 0);
                let framer = reader.lanes.as_slice()[lane]
                    .framer
                    .as_ref()
                    .ok_or(CHANGED)?;
                Some(StorageItem::Fixed(framer.retained_storage()?))
            }
            Self::FramerBacking(lane, index) => {
                let framer = reader.lanes.as_slice()[lane]
                    .framer
                    .as_ref()
                    .ok_or(CHANGED)?;
                if let Some(bytes) = framer.input_wait_backing(index)? {
                    *self = Self::FramerBacking(lane, index + 1);
                    Some(StorageItem::Bytes(bytes))
                } else {
                    *self = Self::Piece { lane, piece: 0 };
                    None
                }
            }
            Self::Piece { lane, piece } => {
                if let Some(source) = reader.lanes.as_slice()[lane].pieces.as_slice().get(piece) {
                    // One completed PES event is admitted once to its PID's lane.
                    let bytes = source
                        .source
                        .pes()
                        .table_storage()?
                        .checked_add(Shared::<crate::stream::TraceEvent>::allocation_bytes()?)
                        .ok_or(Error::Overflow)?;
                    *self = Self::Span {
                        lane,
                        piece,
                        span: 0,
                    };
                    Some(StorageItem::Fixed(bytes))
                } else {
                    *self = Self::Lane(lane + 1);
                    None
                }
            }
            Self::Span { lane, piece, span } => {
                let pes = reader.lanes.as_slice()[lane].pieces.as_slice()[piece]
                    .source
                    .pes();
                if let Some(source) = pes.spans().get(span) {
                    *self = Self::Span {
                        lane,
                        piece,
                        span: span + 1,
                    };
                    Some(StorageItem::Bytes(source.backing_bytes()))
                } else {
                    *self = Self::Piece {
                        lane,
                        piece: piece + 1,
                    };
                    None
                }
            }
            Self::End => None,
        };
        Ok(item)
    }
}

/// The `index`th shared backing an event that ends a byte run owns: a rejected carrier, the
/// spans of a PES or the section bytes of a table.
fn event_backing(event: &crate::stream::TraceEvent, index: usize) -> Option<&Bytes> {
    use crate::stream::TraceEvent;
    match event {
        TraceEvent::Diagnostic(fault) => fault
            .rejected
            .as_ref()
            .filter(|_| index == 0)
            .map(crate::transport::RejectedTransport::backing_bytes),
        TraceEvent::Pes(pes)
        | TraceEvent::UnboundedAdtsPes(crate::compat::adts::UnboundedPes(pes)) => pes
            .spans()
            .get(index)
            .map(crate::pes::PesSpan::backing_bytes),
        TraceEvent::ProgramMap { map, .. } => match index {
            0 => Some(&map.source),
            1 => Some(&map.program_descriptors),
            stream => map
                .streams
                .as_slice()
                .get(stream - 2)
                .map(|stream| &stream.descriptor_bytes),
        },
        TraceEvent::Pat(table) => table.source_sections.as_slice().get(index),
        _ => None,
    }
}

/// The tables an event owns besides its shared backings.
fn event_tables(event: &crate::stream::TraceEvent) -> Result<usize> {
    use crate::stream::TraceEvent;
    match event {
        TraceEvent::Pes(pes)
        | TraceEvent::UnboundedAdtsPes(crate::compat::adts::UnboundedPes(pes)) => {
            pes.table_storage()
        }
        TraceEvent::ProgramMap { map, .. } => table_storage(&map.streams)?
            .checked_add(table_storage(&map.source_ranges)?)
            .ok_or(Error::Overflow),
        TraceEvent::Pat(table) => table_storage(&table.programs)?
            .checked_add(table_storage(&table.source_sections)?)
            .and_then(|n| n.checked_add(table_storage(&table.source_ranges).ok()?))
            .ok_or(Error::Overflow),
        _ => Ok(0),
    }
}

/// The source event an audit inside the stream counts beside the reader's state.
// A type rather than an `Option`, so the other audits' walk, which the PES growth audit
// runs per refused carrier, takes no extra argument.
pub(in crate::reader) trait Cause<'a>: Copy {
    fn event(self) -> Option<&'a SourceEvent>;
}

/// The cause of an audit that counts no event in flight.
#[derive(Clone, Copy)]
pub(in crate::reader) struct NoCause;

impl<'a> Cause<'a> for NoCause {
    fn event(self) -> Option<&'a SourceEvent> {
        None
    }
}

impl<'a> Cause<'a> for &'a SourceEvent {
    fn event(self) -> Option<&'a SourceEvent> {
        Some(self)
    }
}

// No allocation is needed to decide whether allocation is possible. Comparing
// earlier owners may take several steps; cursors survive yields and cancellation.
// Caller-only reservations and output owners are never part of this graph.
pub(in crate::reader) struct StorageScan {
    start: StorageCursor,
    cursor: StorageCursor,
    prior: StorageCursor,
    candidate: Option<StorageCursor>,
    total: usize,
}

impl StorageScan {
    /// The audit after `finished`, the source event being admitted, which it counts.
    pub(in crate::reader) fn new(
        reader: &crate::reader::Reader,
        finished: &SourceEvent,
    ) -> Result<Self> {
        if reader.stream().is_some()
            || reader.boundary.is_some()
            || !matches!(finished.event(), crate::stream::TraceEvent::Finished)
        {
            return Err(Error::Invalid("MPEG-TS fragment audit before source EOF"));
        }
        let total = Budget::control_storage_bytes()
            .checked_add(Shared::<crate::stream::TraceEvent>::allocation_bytes()?)
            .ok_or(Error::Overflow)?;
        Self::start(reader, StorageCursor::Map, total)
    }

    pub(in crate::reader) fn pes_growth(
        reader: &crate::reader::Reader,
        floor: usize,
    ) -> Result<Self> {
        Self::start(
            reader,
            StorageCursor::Transport(TransportStorageCursor::BlockedSpan(0)),
            floor,
        )
    }

    /// The audit of everything the reader owns while it admits `cause` inside the open
    /// stream: the stream itself, `cause`, and the state the end-of-source audit counts.
    /// Step it with the same `cause`. `None` while the stream holds table work in
    /// progress, which it does not walk.
    pub(in crate::reader) fn in_stream(
        reader: &crate::reader::Reader,
        cause: &SourceEvent,
    ) -> Result<Option<Self>> {
        let Some(stream) = reader.stream() else {
            return Err(Error::Invalid(
                "MPEG-TS stream audit after the stream ended",
            ));
        };
        let Some(transport) = stream.open_storage()? else {
            return Ok(None);
        };
        let total = Budget::control_storage_bytes()
            .checked_add(transport)
            .and_then(|n| {
                n.checked_add(Shared::<crate::stream::TraceEvent>::allocation_bytes().ok()?)
            })
            .and_then(|n| n.checked_add(event_tables(cause.event()).ok()?))
            .ok_or(Error::Overflow)?;
        Self::start(
            reader,
            StorageCursor::OpenTransport(TransportStorageCursor::OPEN),
            total,
        )
        .map(Some)
    }

    /// Whether this is an audit inside the open stream.
    pub(in crate::reader) fn is_in_stream(&self) -> bool {
        matches!(self.start, StorageCursor::OpenTransport(_))
    }

    fn start(reader: &crate::reader::Reader, start: StorageCursor, total: usize) -> Result<Self> {
        let selection = match &reader.packet_tracks {
            crate::reader::PacketTracks::Selected(tracks) => table_storage(tracks)?,
            _ => 0,
        };
        let entities = if let Some(plan) = &reader.seek_entities {
            plan.charged_bytes()
                .checked_add(plan.entries_bytes()?)
                .ok_or(Error::Overflow)?
        } else {
            0
        };
        let total = total
            .checked_add(table_storage(&reader.seek)?)
            .and_then(|n| n.checked_add(entities))
            .and_then(|n| n.checked_add(table_storage(&reader.lanes).ok()?))
            .and_then(|n| n.checked_add(table_storage(&reader.tracks).ok()?))
            .and_then(|n| n.checked_add(table_storage(&reader.staged_tracks).ok()?))
            .and_then(|n| n.checked_add(selection))
            .ok_or(Error::Overflow)?;
        Ok(Self {
            start,
            cursor: start,
            prior: start,
            candidate: None,
            total,
        })
    }
    pub(in crate::reader) fn bytes(&self) -> usize {
        self.total
    }
    /// Advances the audit within `work`; `true` once every owner is counted. `cause` is the
    /// event an audit [`in_stream`](Self::in_stream) was started for, [`NoCause`] otherwise.
    pub(in crate::reader) fn step<'a, C: Cause<'a>>(
        &mut self,
        reader: &'a crate::reader::Reader,
        cause: C,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if self.cursor == StorageCursor::End && self.candidate.is_none() {
                return Ok(true);
            }
            if !work.try_charge(1) {
                return Ok(false);
            }
            if let Some(at) = self.candidate {
                // Reborrow the backing so a suspended scan cannot keep input charged.
                let mut candidate = at;
                let Some(StorageItem::Bytes(bytes)) = candidate.step(reader, cause)? else {
                    return Err(CHANGED);
                };
                if self.prior == at {
                    self.total = self
                        .total
                        .checked_add(bytes.retained_storage_bytes())
                        .ok_or(Error::Overflow)?;
                    self.candidate = None;
                } else if self
                    .prior
                    .step(reader, cause)?
                    .and_then(StorageItem::backing)
                    .is_some_and(|prior| prior.shares_storage_with(bytes))
                {
                    self.candidate = None;
                }
            } else {
                let at = self.cursor;
                match self.cursor.step(reader, cause)? {
                    Some(StorageItem::Fixed(bytes)) => {
                        self.total = self.total.checked_add(bytes).ok_or(Error::Overflow)?;
                    }
                    Some(StorageItem::Bytes(_)) => {
                        self.candidate = Some(at);
                        self.prior = self.start;
                    }
                    Some(StorageItem::CountedBytes(_)) | None => (),
                }
            }
        }
    }
}
