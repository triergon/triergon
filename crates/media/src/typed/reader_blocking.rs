use super::*;
use crate::matroska::typed::blocking::{FeedStep, InputFeeder};
use std::io::{Read, Seek};
use triergon_core::InputCursor;

type Reposition<R> = fn(&mut R, u64) -> std::io::Result<u64>;
type Position<R> =
    fn(&mut InputFeeder<R>, &mut InputCursor, &InputRequest, &AtomicBool) -> Result<()>;
/// Blocking adapter over the same retained typed jobs. It adds only std I/O
/// supply; it does not own a second input cursor, parser, clock or pool.
#[derive(Debug)]
pub struct BlockingReader<R: Read> {
    reader: Reader,
    input: InputFeeder<R>,
    reposition: Option<Reposition<R>>,
    position: Option<Position<R>>,
}
impl<R: Read> BlockingReader<R> {
    pub fn open(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }
    pub fn open_in(source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        Self::construct(
            source,
            limits,
            pool,
            false,
            None,
            None,
            crate::storage::PayloadOwnership::OwnedExact,
        )
    }
    pub fn open_with_payload_ownership(
        source: R,
        limits: Limits,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in_with_payload_ownership(source, limits, &pool, policy)
    }
    pub fn open_in_with_payload_ownership(
        source: R,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        Self::construct(source, limits, pool, false, None, None, policy)
    }
    pub fn payload_ownership(&self) -> crate::storage::PayloadOwnership {
        self.reader.payload_ownership()
    }
    fn construct(
        source: R,
        limits: Limits,
        pool: &Budget,
        random_access: bool,
        reposition: Option<Reposition<R>>,
        position: Option<Position<R>>,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let mut reader = Reader::new(
            pool,
            SourceId(1),
            SourceCapabilities {
                length: None,
                random_access,
            },
            limits,
            8192,
        )?;
        reader.envelope.set_payload_ownership(policy);
        Self::initialize(Self {
            reader,
            input: InputFeeder::new(source, 8192)?,
            reposition,
            position,
        })
    }
    fn initialize(mut result: Self) -> Result<Self> {
        let cancel = AtomicBool::new(false);
        while !result.reader.is_ready() {
            match result.poll(&cancel)? {
                ParseStep::Event(Event::Packet(_)) => {
                    return Err(Error::Invalid("packet before stream initialization"));
                }
                ParseStep::Finished => return Err(Error::Truncated),
                _ => (),
            }
        }
        result.synchronize(&cancel)?;
        Ok(result)
    }
    fn poll(&mut self, cancel: &AtomicBool) -> Result<ParseStep<Event>> {
        self.poll_projected::<TypedProjection>(cancel)
    }
    fn poll_projected<D: PacketProjection>(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<ParseStep<D::Event>> {
        if self.input.has_pending_read() {
            self.input.discard_stale(&self.reader.request());
        }
        self.reader
            .set_adapter_storage(self.input.retained_buffer_bytes());
        match self.reader.step_projected_with_source::<D>(
            &mut WorkBudget::new(8192),
            cancel,
            self.input.owned_source_storage(),
        )? {
            ParseStep::NeedInput(request) => {
                let header = self.reader.needs_header_input();
                // Ordinary canonical refill stays unchanged. The seek engine
                // can separately certify a whole core-owned input response,
                // bounded by its active stop, parent/source and scan allowance.
                // Existing pre-read exact-prefix pressure fallback is retained.
                let discard = !header && self.reader.needs_discard_input();
                let refill = match self.reader.header_refill_for_request(&request)? {
                    Some(plan) => Some(plan),
                    None if discard && self.input.owned_source().is_some() => {
                        self.reader.blocking_seek_discard_refill(&request)?
                    }
                    None => self.reader.blocking_seek_header_refill(&request)?,
                };
                let result = if let Some(plan) = refill.as_ref().filter(|_| discard) {
                    self.input.feed_typed_discard_refill(
                        self.reader.envelope.range_input(),
                        plan,
                        cancel,
                        self.reposition,
                    )
                } else if let Some(plan) = refill {
                    self.input.feed_typed_header_refill(
                        self.reader.envelope.range_input(),
                        &plan,
                        cancel,
                        self.reposition,
                    )
                } else if discard {
                    self.input.feed_typed_discard_request(
                        self.reader.envelope.range_input(),
                        &request,
                        cancel,
                        self.reposition,
                    )
                } else {
                    self.input.feed_typed_request(
                        self.reader.envelope.range_input(),
                        &request,
                        header,
                        cancel,
                        self.reposition,
                    )
                };
                self.reader
                    .set_adapter_storage(self.input.retained_buffer_bytes());
                match result {
                    Ok(FeedStep::Provided | FeedStep::End) => Ok(ParseStep::Yield),
                    Ok(FeedStep::Backpressure { requested, .. }) => {
                        self.reader.allocation_blocked(requested);
                        Ok(ParseStep::Yield)
                    }
                    Err(error) => {
                        if !matches!(error, Error::Backpressure { .. }) {
                            self.reader.failed = true;
                        }
                        Err(error)
                    }
                }
            }
            ParseStep::Backpressure {
                requested,
                available,
            } => Err(Error::Backpressure {
                requested,
                available,
            }),
            other => Ok(other),
        }
    }
    fn synchronize(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some(position) = self.position {
            let request = self.reader.request();
            if let Err(error) = position(
                &mut self.input,
                self.reader.envelope.range_input(),
                &request,
                cancel,
            ) {
                self.reader.failed = true;
                return Err(error);
            }
        }
        self.reader
            .set_adapter_storage(self.input.retained_buffer_bytes());
        Ok(())
    }
    /// Closes parser jobs and pending supply before returning the input source.
    /// Caller-retained packet/configuration aliases retain their original charge.
    pub fn into_inner(self) -> R {
        let Self { reader, input, .. } = self;
        drop(reader);
        input.into_inner()
    }
    pub fn budget(&self) -> &Budget {
        self.reader.budget()
    }
    pub fn tracks(&self) -> &[crate::matroska::Track] {
        self.reader.tracks()
    }
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.reader.timestamp_scale_ns()
    }
    pub fn metadata_documents(&self) -> &[SharedDocument] {
        self.reader.catalogue()
    }
    pub fn metadata_retained_bytes(&self) -> usize {
        self.reader.catalogue.retained_bytes()
    }
    pub fn opaque_metadata(&self) -> impl Iterator<Item = (u64, &[u8])> + '_ {
        self.reader
            .catalogue()
            .iter()
            .map(|document| (document.kind(), document.raw()))
    }
    pub fn next_typed_packet(
        &mut self,
        cancel: &AtomicBool,
    ) -> Result<Option<crate::typed::Packet>> {
        if self.reader.pending_seek().is_some() {
            return Err(Error::Invalid("pending seek must be completed"));
        }
        loop {
            match self.poll(cancel)? {
                ParseStep::Event(Event::Packet(packet)) => return Ok(Some(packet)),
                ParseStep::Finished => return Ok(None),
                _ => (),
            }
        }
    }
    pub fn next_packet(&mut self, cancel: &AtomicBool) -> Result<Option<crate::matroska::Packet>> {
        if self.reader.pending_seek().is_some() {
            return Err(Error::Invalid("pending seek must be completed"));
        }
        loop {
            match self.poll_projected::<LegacyProjection>(cancel)? {
                ParseStep::Event(LegacyEvent::Packet(packet)) => return Ok(Some(packet)),
                ParseStep::Event(LegacyEvent::FullPacket(packet)) => {
                    if packet.group.as_ref().is_some_and(|group| {
                        group.additions().len() != 0 || group.codec_state().is_some()
                    }) {
                        self.reader.failed = true;
                        return Err(Error::Unsupported(
                            "Block additions/CodecState require next_typed_packet",
                        ));
                    }
                    return Ok(Some(packet.packet));
                }
                ParseStep::Event(LegacyEvent::Envelope(event)) => drop(event),
                ParseStep::Event(LegacyEvent::Configuration(configuration)) => drop(configuration),
                ParseStep::Event(LegacyEvent::Document(document)) => drop(document),
                ParseStep::Event(LegacyEvent::Seek(receipt)) => {
                    let _ = receipt;
                }
                ParseStep::Finished => return Ok(None),
                _ => (),
            }
        }
    }
    pub fn seek_previous(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<crate::matroska::SeekReceipt> {
        self.seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: false,
            },
            cancel,
        )
    }
    pub fn seek_for_playback(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<crate::matroska::SeekReceipt> {
        self.seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: true,
            },
            cancel,
        )
    }
    fn seek(
        &mut self,
        request: SeekRequest,
        cancel: &AtomicBool,
    ) -> Result<crate::matroska::SeekReceipt> {
        if self.reader.failed || self.reader.pending_seek() != Some(request) {
            if request.playback_preroll {
                self.reader
                    .seek_for_playback(request.track, request.requested_ns)?;
            } else {
                self.reader
                    .seek_previous(request.track, request.requested_ns)?;
            }
        }
        loop {
            // Lent for this poll only; see EpochRestoreJob::try_owned_prefix.
            self.reader.owned_root = self.input.owned_source().cloned();
            let step = self.poll_projected::<ScanProjection>(cancel);
            self.reader.owned_root = None;
            match step? {
                ParseStep::Event(ScanEvent::Seek(receipt)) => {
                    self.synchronize(cancel)?;
                    return Ok(receipt);
                }
                ParseStep::Event(ScanEvent::Packet(_)) => {
                    return Err(Error::Invalid("packet before seek receipt"));
                }
                ParseStep::Finished => {
                    return Err(Error::Invalid("seek completed without receipt"));
                }
                _ => (),
            }
        }
    }
}
impl<R: Read + Seek> BlockingReader<R> {
    pub fn open_seekable(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_seekable_in(source, limits, &pool)
    }
    pub fn open_seekable_with_payload_ownership(
        source: R,
        limits: Limits,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_seekable_in_with_payload_ownership(source, limits, &pool, policy)
    }
    pub fn open_seekable_in_with_payload_ownership(
        source: R,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        Self::construct(
            source,
            limits,
            pool,
            true,
            Some(|reader, offset| std::io::Seek::seek(reader, std::io::SeekFrom::Start(offset))),
            Some(|feed, cursor, request, cancel| {
                feed.synchronize_seekable_position(cursor, request, cancel)
            }),
            policy,
        )
    }
    pub fn open_seekable_in(source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        Self::construct(
            source,
            limits,
            pool,
            true,
            Some(|reader, offset| std::io::Seek::seek(reader, std::io::SeekFrom::Start(offset))),
            Some(|feed, cursor, request, cancel| {
                feed.synchronize_seekable_position(cursor, request, cancel)
            }),
            crate::storage::PayloadOwnership::OwnedExact,
        )
    }
}

impl BlockingReader<std::io::Cursor<triergon_core::OwnedInputBytes>> {
    /// Explicit immutable-input operation. The complete caller-supplied input capability
    /// owner must already belong to this pool; windows and packet views retain
    /// that full charge. No source is buffered here. Generic Read/Seek and its
    /// default OwnedExact payload policy remain unchanged.
    pub fn open_owned_seekable_in(
        source: triergon_core::OwnedInputBytes,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        Self::open_owned_with_profile(source, limits, pool, policy, false)
    }

    /// Opt into the implicit WebM VP9 T.35 admission on an owned input.
    pub fn open_owned_seekable_webm_vp9_t35_v1(
        source: triergon_core::OwnedInputBytes,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        Self::open_owned_with_profile(source, limits, pool, policy, true)
    }

    fn open_owned_with_profile(
        source: triergon_core::OwnedInputBytes,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
        webm_vp9_t35: bool,
    ) -> Result<Self> {
        if !source.as_bytes().belongs_to(pool) {
            return Err(triergon_core::Error::ForeignPool.into());
        }
        let capabilities = SourceCapabilities {
            length: None,
            random_access: true,
        };
        let mut reader = if webm_vp9_t35 {
            Reader::new_webm_vp9_t35_v1(pool, SourceId(1), capabilities, limits, 8192)?
        } else {
            Reader::new(pool, SourceId(1), capabilities, limits, 8192)?
        };
        reader.envelope.set_payload_ownership(policy);
        Self::initialize(Self {
            reader,
            input: InputFeeder::from_owned(std::io::Cursor::new(source), 8192)?,
            reposition: Some(|reader, offset| reader.seek(std::io::SeekFrom::Start(offset))),
            position: Some(|feed, cursor, request, cancel| {
                feed.synchronize_seekable_position(cursor, request, cancel)
            }),
        })
    }
}

#[cfg(test)]
mod header_refill_blocking_tests {
    use super::*;
    use std::{
        cell::RefCell,
        io::{self, Cursor},
        rc::Rc,
    };
    use triergon_core::Bytes;
    fn element(id: u64, data: &[u8]) -> Vec<u8> {
        let mut bytes: Vec<_> = id
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        assert!(data.len() < 127);
        bytes.push(0x80 | data.len() as u8);
        bytes.extend_from_slice(data);
        bytes
    }
    fn source() -> Vec<u8> {
        let tracks = element(
            0x1654ae6b,
            &element(
                0xae,
                &[
                    element(0xd7, &[1]),
                    element(0x73c5, &[1]),
                    element(0x83, &[1]),
                    element(0x86, b"V_AUTHORED/OPAQUE"),
                ]
                .concat(),
            ),
        );
        let mut body = element(0xe7, &[0]);
        for tick in 0..3 {
            body.extend(element(0xa3, &[0x81, 0, tick, 0x80, b'x', b'y']));
        }
        [
            element(0x1a45dfa3, &element(0x4282, b"matroska")),
            element(0x18538067, &[tracks, element(0x1f43b675, &body)].concat()),
        ]
        .concat()
    }
    type Calls = Rc<RefCell<Vec<(u64, usize)>>>;
    struct Recorded {
        cursor: Cursor<Vec<u8>>,
        calls: Calls,
    }
    impl Read for Recorded {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.calls
                .borrow_mut()
                .push((self.cursor.position(), output.len()));
            self.cursor.read(output)
        }
    }
    #[test]
    fn header_refill_default_matches_canonical_io_packets_and_output_capacity() {
        let raw = source();
        let canonical_pool = Budget::new(4 * 1024 * 1024).unwrap();
        let mut canonical = Reader::new(
            &canonical_pool,
            SourceId(2106),
            SourceCapabilities {
                length: None,
                random_access: false,
            },
            Limits::default(),
            8192,
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let (mut expected_calls, mut canonical_packets) = (Vec::new(), Vec::new());
        let mut finished = false;
        for _ in 0..100_000 {
            match canonical.step(&mut WorkBudget::new(8192), &cancel).unwrap() {
                ParseStep::NeedInput(request) => {
                    let maximum = request.maximum.min(8192);
                    expected_calls.push((request.offset, maximum.max(1)));
                    let at = request.offset as usize;
                    if at == raw.len() {
                        canonical.finish(request.source, request.token).unwrap();
                    } else {
                        let end = (at + maximum).min(raw.len());
                        assert!(end > at);
                        let bytes = Bytes::copy_from_slice(&canonical_pool, &raw[at..end]).unwrap();
                        canonical
                            .provide(
                                &InputWindow::new(
                                    request.source,
                                    request.token,
                                    request.offset,
                                    bytes,
                                )
                                .unwrap(),
                            )
                            .unwrap();
                    }
                }
                ParseStep::Event(Event::Packet(packet)) => canonical_packets.push(packet),
                ParseStep::Event(_) | ParseStep::Yield => (),
                ParseStep::Finished => {
                    finished = true;
                    break;
                }
                other => panic!("canonical complete source: {other:?}"),
            }
        }
        assert!(finished);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut blocking = BlockingReader::open(
            Recorded {
                cursor: Cursor::new(raw),
                calls: calls.clone(),
            },
            Limits::default(),
        )
        .unwrap();
        let mut packets = Vec::new();
        while let Some(packet) = blocking.next_typed_packet(&cancel).unwrap() {
            packets.push(packet);
        }
        assert_eq!(packets.len(), 3);
        assert_eq!(canonical_packets.len(), 3);
        for (at, (a, b)) in packets.iter().zip(&canonical_packets).enumerate() {
            assert_eq!(a.packet.data.as_ref(), b.packet.data.as_ref());
            assert_eq!(a.packet.data.as_ref(), b"xy");
            assert_eq!(a.packet.pts_ns, Some(at as i64 * 1_000_000));
            assert_eq!(
                (
                    a.packet.pts_ns,
                    a.packet.dts_ns,
                    a.packet.duration_ns,
                    a.packet.keyframe
                ),
                (
                    b.packet.pts_ns,
                    b.packet.dts_ns,
                    b.packet.duration_ns,
                    b.packet.keyframe
                )
            );
            assert_eq!(
                a.packet.data.retained_capacity(),
                b.packet.data.retained_capacity()
            );
            assert!(
                a.packet.data.retained_capacity() <= 6,
                "tiny output must not retain hidden 1KiB input capacity"
            );
            assert_eq!(a.configuration.revision(), b.configuration.revision());
        }
        assert_eq!(
            *calls.borrow(),
            expected_calls,
            "blocking Read never widens a canonical engine hint"
        );
        assert_eq!(calls.borrow()[0].1, 4096);
        assert!(calls.borrow().iter().all(|(_, size)| *size <= 4096));
        let pool = blocking.budget().clone();
        let live = pool.used();
        drop(blocking);
        assert!(pool.used() <= live);
        assert_eq!(packets[0].packet.data.as_ref(), b"xy");
        drop(packets);
        assert_eq!(pool.used(), Budget::control_storage_bytes());
    }
}

// This guard uses only pre-existing APIs. Parent can append it to the baseline
// reader_blocking.rs and observe the default header read still requesting1 byte.
#[cfg(test)]
mod header_refill_runtime_red {
    use super::*;
    use std::{
        cell::RefCell,
        io::{self, Cursor},
        rc::Rc,
    };
    #[test]
    fn ordinary_default_header_refill_is_real_io_and_complete_output() {
        fn e(id: u64, data: &[u8]) -> Vec<u8> {
            let mut out: Vec<_> = id
                .to_be_bytes()
                .into_iter()
                .skip_while(|b| *b == 0)
                .collect();
            assert!(data.len() < 127);
            out.push(0x80 | data.len() as u8);
            out.extend_from_slice(data);
            out
        }
        let tracks = e(
            0x1654ae6b,
            &e(
                0xae,
                &[
                    e(0xd7, &[1]),
                    e(0x73c5, &[1]),
                    e(0x83, &[1]),
                    e(0x86, b"V_AUTHORED/OPAQUE"),
                ]
                .concat(),
            ),
        );
        let raw = [
            e(0x1a45dfa3, &e(0x4282, b"matroska")),
            e(
                0x18538067,
                &[
                    tracks,
                    e(
                        0x1f43b675,
                        &[e(0xe7, &[0]), e(0xa3, &[0x81, 0, 0, 0x80, b'x', b'y'])].concat(),
                    ),
                ]
                .concat(),
            ),
        ]
        .concat();
        struct Recorded {
            inner: Cursor<Vec<u8>>,
            calls: Rc<RefCell<Vec<usize>>>,
        }
        impl Read for Recorded {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.calls.borrow_mut().push(output.len());
                self.inner.read(output)
            }
        }
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut reader = BlockingReader::open(
            Recorded {
                inner: Cursor::new(raw),
                calls: calls.clone(),
            },
            Limits::default(),
        )
        .unwrap();
        let packet = reader
            .next_packet(&AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(packet.data.as_ref(), b"xy");
        assert!(packet.data.retained_capacity() <= 6);
        assert!(
            reader
                .next_packet(&AtomicBool::new(false))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            calls.borrow()[0],
            4096,
            "actual default supply must use the explicit bounded header policy"
        );
    }
}

#[cfg(test)]
#[path = "legacy_projection_regressions.rs"]
mod legacy_projection_regressions;

#[cfg(test)]
#[path = "seek_window_regressions.rs"]
mod seek_window_regressions;

#[cfg(test)]
#[path = "seek_window_independent_helpers.rs"]
mod seek_window_independent_helpers;

#[cfg(test)]
#[path = "scan_projection_regressions.rs"]
mod scan_projection_regressions;

#[cfg(test)]
#[path = "owned_blocking_source.rs"]
mod owned_blocking_source;

#[cfg(test)]
#[path = "owned_epoch_prefix_tests.rs"]
mod owned_epoch_prefix_tests;
