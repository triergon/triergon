//! The transport owners pinned while a PES span replacement is refused.

use super::{Route, TransportStream};
use crate::{PesAssembler, table_storage};
use triergon_core::{Bytes, Error, Result};

pub(crate) enum StorageItem<'a> {
    Fixed(usize),
    Bytes(&'a Bytes),
    // These owners are already included in the blocked assembler's floor.
    CountedBytes(&'a Bytes),
}

impl<'a> StorageItem<'a> {
    pub(crate) fn backing(self) -> Option<&'a Bytes> {
        match self {
            Self::Bytes(bytes) | Self::CountedBytes(bytes) => Some(bytes),
            Self::Fixed(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportStorageCursor {
    BlockedSpan(usize),
    BlockedPayload,
    Carrier,
    PmtSource,
    PatVersion(usize),
    PatSection(usize, usize),
    PatBacking(usize, usize),
    Route(usize),
    Assembler(usize),
    Span(usize, usize),
    Payload(usize),
    End,
}

impl TransportStorageCursor {
    // Each advance visits at most one section, route, or backing. At this
    // transport phase PSI jobs have drained and the framer owns only its carry.
    pub(crate) fn step<'a>(
        &mut self,
        stream: &'a TransportStream,
    ) -> Result<Option<StorageItem<'a>>> {
        debug_assert_eq!(stream.phase, 6);
        let Route::Pes(blocked) = stream.route else {
            return Err(Error::Invalid("ADTS storage audit outside PES drain"));
        };
        let routes = stream.routes.as_slice();
        let item = match *self {
            Self::BlockedSpan(index) => {
                if let Some(bytes) = routes[blocked]
                    .assembler
                    .as_ref()
                    .and_then(|assembler| assembler.span_backing(index))
                {
                    *self = Self::BlockedSpan(index + 1);
                    Some(StorageItem::CountedBytes(bytes))
                } else {
                    *self = Self::BlockedPayload;
                    None
                }
            }
            Self::BlockedPayload => {
                *self = Self::Carrier;
                routes[blocked]
                    .assembler
                    .as_ref()
                    .and_then(PesAssembler::pending_backing)
                    .map(StorageItem::CountedBytes)
            }
            Self::Carrier => {
                *self = Self::PmtSource;
                stream
                    .packet
                    .as_ref()
                    .map(|packet| StorageItem::Bytes(&packet.source))
            }
            Self::PmtSource => {
                *self = Self::PatVersion(0);
                stream.pmt_source.as_ref().map(StorageItem::Bytes)
            }
            Self::PatVersion(version) => {
                let sections = stream.collector.retained_sections();
                match sections.get(version) {
                    Some(Some(table)) => {
                        *self = Self::PatSection(version, 0);
                        Some(StorageItem::Fixed(table_storage(table)?))
                    }
                    Some(None) => {
                        *self = Self::PatVersion(version + 1);
                        None
                    }
                    None => {
                        *self = Self::Route(0);
                        None
                    }
                }
            }
            Self::PatSection(version, index) => {
                let table =
                    stream.collector.retained_sections()[version].expect("retained PAT version");
                if let Some(section) = table.as_slice().get(index) {
                    *self = Self::PatBacking(version, index);
                    let bytes = table_storage(&section.programs)?
                        .checked_add(table_storage(&section.source_ranges)?)
                        .ok_or(Error::Overflow)?;
                    Some(StorageItem::Fixed(bytes))
                } else {
                    *self = Self::PatVersion(version + 1);
                    None
                }
            }
            Self::PatBacking(version, index) => {
                *self = Self::PatSection(version, index + 1);
                let table =
                    stream.collector.retained_sections()[version].expect("retained PAT version");
                Some(StorageItem::Bytes(&table.as_slice()[index].source))
            }
            Self::Route(index) => {
                if let Some(route) = routes.get(index) {
                    *self = Self::Assembler(index);
                    Some(StorageItem::Bytes(&route.descriptors))
                } else {
                    *self = Self::End;
                    None
                }
            }
            Self::Assembler(index) => {
                if index != blocked
                    && let Some(assembler) = &routes[index].assembler
                {
                    *self = Self::Span(index, 0);
                    Some(StorageItem::Fixed(assembler.table_storage()?))
                } else {
                    *self = Self::Route(index + 1);
                    None
                }
            }
            Self::Span(route, index) => {
                let assembler = routes[route]
                    .assembler
                    .as_ref()
                    .expect("retained PES route");
                if let Some(bytes) = assembler.span_backing(index) {
                    *self = Self::Span(route, index + 1);
                    Some(StorageItem::Bytes(bytes))
                } else {
                    *self = Self::Payload(route);
                    None
                }
            }
            Self::Payload(route) => {
                *self = Self::Route(route + 1);
                routes[route]
                    .assembler
                    .as_ref()
                    .and_then(PesAssembler::pending_backing)
                    .map(StorageItem::Bytes)
            }
            Self::End => None,
        };
        Ok(item)
    }
}
