//! Compile-time projection of the same typed demux/state jobs. The existing
//! legacy packet API has no configuration snapshot in its return contract.
use super::*;

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
