//! The storage audit: every owner of reader storage, walked one edge per work unit.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageCursor {
    Transport(TransportStorageCursor),
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
    // One structural edge per call, including empty tables. Checking earlier
    // entries for shared backing also consumes cooperative work.
    #[expect(
        clippy::expect_used,
        reason = "the framer cursors are entered only for a lane with a framer"
    )]
    fn step<'a>(&mut self, reader: &'a crate::reader::Reader) -> Result<Option<StorageItem<'a>>> {
        let item = match *self {
            Self::Transport(cursor) => {
                let mut next = cursor;
                let Some(stream) = reader.stream() else {
                    return Err(Error::Invalid("MPEG-TS storage audit changed during scan"));
                };
                let item = next.step(stream)?;
                *self = if next == TransportStorageCursor::End {
                    Self::Map
                } else {
                    Self::Transport(next)
                };
                item
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
                    .expect("live framer");
                Some(StorageItem::Fixed(framer.input_wait_storage()?))
            }
            Self::FramerBacking(lane, index) => {
                let framer = reader.lanes.as_slice()[lane]
                    .framer
                    .as_ref()
                    .expect("live framer");
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
    pub(in crate::reader) fn step(
        &mut self,
        reader: &crate::reader::Reader,
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
                let Some(StorageItem::Bytes(bytes)) = candidate.step(reader)? else {
                    return Err(Error::Invalid("MPEG-TS storage audit changed during scan"));
                };
                if self.prior == at {
                    self.total = self
                        .total
                        .checked_add(bytes.retained_storage_bytes())
                        .ok_or(Error::Overflow)?;
                    self.candidate = None;
                } else if self
                    .prior
                    .step(reader)?
                    .and_then(StorageItem::backing)
                    .is_some_and(|prior| prior.shares_storage_with(bytes))
                {
                    self.candidate = None;
                }
            } else {
                let at = self.cursor;
                match self.cursor.step(reader)? {
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
