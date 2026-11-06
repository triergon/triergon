//! Compile-time projection of the same typed demux/state jobs. The existing
//! legacy packet API has no configuration snapshot in its return contract.
use super::*;
use crate::matroska::typed::packet::ScanPacketFields;

#[derive(Debug)]
pub(super) enum LegacyEvent {
    Envelope(ElementEvent),
    Configuration(Configuration),
    Document(SharedDocument),
    Packet(crate::matroska::Packet),
    FullPacket(crate::typed::Packet),
    Seek(crate::matroska::SeekReceipt),
}
pub(super) trait PacketProjection {
    type Event;
    fn event(event: Event) -> Result<Self::Event>;
    fn envelope(event: ElementEvent) -> Result<Self::Event> {
        Self::event(Event::Envelope(event))
    }
    fn configuration(value: Configuration) -> Result<Self::Event> {
        Self::event(Event::Configuration(value))
    }
    fn document(value: SharedDocument) -> Result<Self::Event> {
        Self::event(Event::Document(value))
    }
    fn packet(value: crate::typed::Packet) -> Result<Self::Event> {
        Self::event(Event::Packet(value))
    }
    fn seek(value: crate::matroska::SeekReceipt) -> Result<Self::Event> {
        Self::event(Event::Seek(value))
    }
    fn seeking(
        reader: &mut Reader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Self::Event>> {
        Self::progress(reader.step_seeking(work, cancel))
    }
    // The generic mapping remains fallible and keeps its original owner/error
    // order. Typed projection can return the identical progress value directly.
    fn progress(result: Result<ParseStep<Event>>) -> Result<ParseStep<Self::Event>> {
        match result? {
            ParseStep::Event(event) => Ok(ParseStep::Event(Self::event(event)?)),
            other => crate::matroska::typed::pending(other),
        }
    }
    fn simple(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: &Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<SimplePacketJob>;
    fn take_simple(
        job: &mut SimplePacketJob,
        configuration: Option<&Configuration>,
    ) -> Result<Self::Event>;
}
pub(super) struct TypedProjection;
impl PacketProjection for TypedProjection {
    type Event = Event;
    fn progress(result: Result<ParseStep<Event>>) -> Result<ParseStep<Event>> {
        result
    }
    fn event(event: Event) -> Result<Self::Event> {
        Ok(event)
    }
    fn simple(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: &Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<SimplePacketJob> {
        SimplePacketJob::new(
            source,
            cluster_ticks,
            scale,
            configuration.clone(),
            limits,
            pool,
        )
    }
    fn take_simple(
        job: &mut SimplePacketJob,
        configuration: Option<&Configuration>,
    ) -> Result<Self::Event> {
        Ok(Event::Packet(job.take_ready_borrowing(configuration)?))
    }
}
pub(super) struct LegacyProjection;
impl PacketProjection for LegacyProjection {
    type Event = LegacyEvent;
    fn event(event: Event) -> Result<Self::Event> {
        Ok(match event {
            Event::Envelope(event) => LegacyEvent::Envelope(event),
            Event::Configuration(configuration) => LegacyEvent::Configuration(configuration),
            Event::Document(document) => LegacyEvent::Document(document),
            Event::Seek(receipt) => LegacyEvent::Seek(receipt),
            // Keep the complete fallback publication alive through Reader's
            // successful coverage commit. The legacy refusal stays at the
            // blocking boundary, after that commit, with the same owners.
            Event::Packet(packet) => LegacyEvent::FullPacket(packet),
        })
    }
    fn simple(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: &Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<SimplePacketJob> {
        SimplePacketJob::new_borrowed(source, cluster_ticks, scale, configuration, limits, pool)
    }
    fn take_simple(
        job: &mut SimplePacketJob,
        _configuration: Option<&Configuration>,
    ) -> Result<Self::Event> {
        Ok(LegacyEvent::Packet(job.take_legacy_ready()?))
    }
}

/// Private scan/receipt facts. Retain every original owning field until the
/// same consumer finishes; only unused Copy scalar fields leave the return ABI.
#[derive(Debug)]
pub(super) struct ScanPacket {
    pub fields: ScanPacketFields,
    // TypedPacket drops packet.data, configuration, group in this order.
    _data: crate::matroska::PacketData,
    _configuration: Configuration,
    _group: Option<crate::typed::BlockGroupSource>,
}
impl ScanPacket {
    fn from_typed(value: crate::typed::Packet) -> Self {
        let crate::typed::Packet {
            packet,
            configuration,
            group,
        } = value;
        Self {
            fields: ScanPacketFields {
                track: packet.track,
                pts_ns: packet.pts_ns,
                keyframe: packet.keyframe,
            },
            _data: packet.data,
            _configuration: configuration,
            _group: group,
        }
    }
}
#[derive(Debug)]
pub(super) enum ScanEvent {
    Envelope(ElementEvent),
    Configuration(Configuration),
    Document(SharedDocument),
    Packet(ScanPacket),
    Seek(crate::matroska::SeekReceipt),
}
pub(super) struct ScanProjection;
impl PacketProjection for ScanProjection {
    type Event = ScanEvent;
    fn event(event: Event) -> Result<Self::Event> {
        match event {
            Event::Envelope(value) => Self::envelope(value),
            Event::Configuration(value) => Self::configuration(value),
            Event::Document(value) => Self::document(value),
            Event::Packet(value) => Self::packet(value),
            Event::Seek(value) => Self::seek(value),
        }
    }
    fn envelope(value: ElementEvent) -> Result<Self::Event> {
        Ok(ScanEvent::Envelope(value))
    }
    fn configuration(value: Configuration) -> Result<Self::Event> {
        Ok(ScanEvent::Configuration(value))
    }
    fn document(value: SharedDocument) -> Result<Self::Event> {
        Ok(ScanEvent::Document(value))
    }
    fn packet(value: crate::typed::Packet) -> Result<Self::Event> {
        Ok(ScanEvent::Packet(ScanPacket::from_typed(value)))
    }
    fn seek(value: crate::matroska::SeekReceipt) -> Result<Self::Event> {
        Ok(ScanEvent::Seek(value))
    }
    fn seeking(
        reader: &mut Reader,
        work: &mut WorkBudget,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<Self::Event>> {
        reader.step_seeking_projected::<Self>(work, cancel)
    }
    fn simple(
        source: SharedBytes,
        cluster_ticks: u64,
        scale: u64,
        configuration: &Configuration,
        limits: Limits,
        pool: &Budget,
    ) -> Result<SimplePacketJob> {
        // This is the typed validation/snapshot path, including rare
        // encoded/group promotion. Do not substitute legacy refusal semantics.
        TypedProjection::simple(source, cluster_ticks, scale, configuration, limits, pool)
    }
    fn take_simple(
        job: &mut SimplePacketJob,
        configuration: Option<&Configuration>,
    ) -> Result<Self::Event> {
        Self::packet(job.take_ready_borrowing(configuration)?)
    }
}
