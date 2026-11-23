use super::*;
use crate::reader::input_feeder::{FeedStep, InputFeeder};
use crate::storage::PayloadOwnership;
use std::io::{Cursor, Read, Seek};
use triergon_core::{InputCursor, OwnedInputBytes};

type Reposition<R> = fn(&mut R, u64) -> std::io::Result<u64>;
type Position<R> =
    fn(&mut InputFeeder<R>, &mut InputCursor, &InputRequest, &AtomicBool) -> Result<()>;
/// How a seekable source repositions and resynchronizes its cursor.
type Seeking<R> = (Reposition<R>, Position<R>);

type OpenOwned<R> = fn(R, Limits, &Budget, PayloadOwnership, bool) -> Result<BlockingReader<R>>;

/// Source access and payload ownership for a [BlockingReader].
///
/// Seekable sources convert through [From]. Use [Self::stream] for forward-only
/// input and [Self::owned] for immutable input already charged to the pool.
#[derive(Debug)]
pub struct BlockingSource<R: Read> {
    source: R,
    seek: Option<Seeking<R>>,
    owned: Option<OpenOwned<R>>,
    payload: PayloadOwnership,
}

impl<R: Read> BlockingSource<R> {
    /// Read forward from the source's current position.
    pub fn stream(source: R) -> Self {
        Self {
            source,
            seek: None,
            owned: None,
            payload: PayloadOwnership::OwnedExact,
        }
    }

    /// Set how packets retain their payloads. The default is
    /// [PayloadOwnership::OwnedExact].
    #[must_use]
    pub fn with_payload_ownership(mut self, payload: PayloadOwnership) -> Self {
        self.payload = payload;
        self
    }
}

impl<R: Read + Seek> From<R> for BlockingSource<R> {
    fn from(source: R) -> Self {
        Self {
            seek: Some(BlockingReader::<R>::SEEK),
            ..Self::stream(source)
        }
    }
}

impl BlockingSource<Cursor<OwnedInputBytes>> {
    /// Read charged, immutable input without copying its windows. Opening
    /// returns [Error::ForeignPool] if the input uses another pool.
    pub fn owned(source: OwnedInputBytes) -> Self {
        Self {
            owned: Some(BlockingReader::open_owned),
            ..Self::from(Cursor::new(source))
        }
    }
}

/// The one source a blocking reader reads.
const SOURCE: SourceId = SourceId(1);
/// Input window and per-step work of a blocking reader.
const WINDOW: usize = 8192;
const WORK: u64 = 8192;
/// Blocking adapter over the same retained typed jobs. It adds only std I/O
/// supply; it does not own a second input cursor, parser, clock or pool.
#[derive(Debug)]
pub struct BlockingReader<R: Read> {
    reader: Reader,
    input: InputFeeder<R>,
    seek: Option<Seeking<R>>,
}

/// Seek for playback with audio selected by whole block in the prefix
/// recovered for subtitles. Packet timing and the video landing stay in the
/// normal reader's source clock.
pub fn at_block_start<R: Read>(
    reader: &mut BlockingReader<R>,
    track: usize,
    requested_ns: i64,
    cancel: &AtomicBool,
) -> Result<SeekReceipt> {
    reader.seek(
        SeekRequest {
            track,
            requested_ns,
            playback_preroll: true,
        },
        RecoveryAudio::BlockStart,
        cancel,
    )
}

/// Whether the seek trace includes initial padding after reopening at the start.
#[cfg(feature = "reference-trace")]
pub fn seek_initial_trim<R: Read>(
    reader: &BlockingReader<R>,
    track: usize,
    landing: &SeekReceipt,
) -> Result<bool> {
    if !landing.replays_from_start {
        return Ok(false);
    }
    let tracks = reader.tracks();
    let video = tracks
        .get(track)
        .ok_or(Error::Unsupported("seek requires video track"))?;
    let time = i128::from(landing.actual_ns) + i128::from(video.codec_delay_ns);
    let scale = reader.timestamp_scale_ns();
    let mut delay = 0;
    for audio in tracks
        .iter()
        .filter(|track| track.kind == crate::track::Track::AUDIO)
    {
        delay = delay.max(RecoveryAudio::BlockStart.delay_ns(audio, scale)?);
    }
    Ok(delay != 0 && time < i128::from(delay))
}

impl<R: Read> BlockingReader<R> {
    /// Open a seekable source or one described by [BlockingSource] in a new
    /// pool bounded by [Limits::live_storage_bytes]. Fails on I/O errors or
    /// refused metadata.
    pub fn open(source: impl Into<BlockingSource<R>>, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }

    /// Open a source and charge its storage to the caller's pool. A seekable
    /// source can follow SeekHead links to late metadata.
    pub fn open_in(
        source: impl Into<BlockingSource<R>>,
        limits: Limits,
        pool: &Budget,
    ) -> Result<Self> {
        Self::open_source(source.into(), limits, pool, false)
    }

    #[inline(always)]
    fn open_source(
        source: BlockingSource<R>,
        limits: Limits,
        pool: &Budget,
        webm_vp9_t35: bool,
    ) -> Result<Self> {
        if let Some(open) = source.owned {
            return open(source.source, limits, pool, source.payload, webm_vp9_t35);
        }
        let reader = Reader::new(
            pool,
            SOURCE,
            capabilities(source.seek.is_some()),
            limits,
            WINDOW,
        )?;
        let input = InputFeeder::new(source.source, WINDOW)?;
        Self::construct(reader, input, source.seek, source.payload)
    }

    /// The block-payload policy this reader was opened with.
    pub fn payload_ownership(&self) -> PayloadOwnership {
        self.reader.payload_ownership()
    }
    fn reposition(&self) -> Option<Reposition<R>> {
        self.seek.map(|(reposition, _)| reposition)
    }
    fn construct(
        mut reader: Reader,
        input: InputFeeder<R>,
        seek: Option<Seeking<R>>,
        policy: PayloadOwnership,
    ) -> Result<Self> {
        reader.stream.envelope.set_payload_ownership(policy);
        Self::initialize(Self {
            reader,
            input,
            seek,
        })
    }
    fn initialize(mut result: Self) -> Result<Self> {
        let cancel = AtomicBool::new(false);
        while !result.reader.is_ready() {
            match result.poll(&cancel)? {
                Step::Ready(Event::Packet(_)) => {
                    return Err(Error::Invalid("packet before stream initialization"));
                }
                Step::End(_) => return Err(Error::Truncated),
                _ => (),
            }
        }
        result.synchronize(&cancel)?;
        Ok(result)
    }
    fn poll(&mut self, cancel: &AtomicBool) -> ReaderStep<Event> {
        self.poll_projected::<TypedProjection>(cancel, false)
    }
    /// One reader step and, when it asks for input, one read of the source.
    /// `lend_root` lends an immutable source's bytes to the step: a seek restores
    /// state from them (see `EpochRestoreJob::try_owned_prefix`).
    fn poll_projected<D: PacketProjection>(
        &mut self,
        cancel: &AtomicBool,
        lend_root: bool,
    ) -> ReaderStep<D::Event> {
        if self.input.has_pending_read() {
            self.input.discard_stale(&self.reader.request());
        }
        self.reader
            .set_adapter_storage(self.input.retained_buffer_bytes());
        match self.reader.step_projected_with_source::<D>(
            &mut WorkBudget::new(WORK),
            cancel,
            self.input.owned_source_storage(),
            if lend_root {
                self.input.owned_source()
            } else {
                None
            },
        )? {
            Step::NeedInput(request) => {
                let header = self.reader.needs_header_input();
                // Ordinary canonical refill as the reader asks. The seek engine
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
                let reposition = self.reposition();
                let result = if let Some(plan) = refill.as_ref().filter(|_| discard) {
                    self.input.feed_typed_discard_refill(
                        self.reader.stream.envelope.range_input(),
                        plan,
                        cancel,
                        reposition,
                    )
                } else if let Some(plan) = refill {
                    self.input.feed_typed_header_refill(
                        self.reader.stream.envelope.range_input(),
                        &plan,
                        cancel,
                        reposition,
                    )
                } else if discard {
                    self.input.feed_typed_discard_request(
                        self.reader.stream.envelope.range_input(),
                        &request,
                        cancel,
                        reposition,
                    )
                } else {
                    self.input.feed_typed_request(
                        self.reader.stream.envelope.range_input(),
                        &request,
                        header,
                        cancel,
                        reposition,
                    )
                };
                self.reader
                    .set_adapter_storage(self.input.retained_buffer_bytes());
                match result {
                    Ok(FeedStep::Provided | FeedStep::End) => Ok(Step::Yield),
                    Ok(FeedStep::Backpressure { requested, .. }) => {
                        self.reader.allocation_blocked(requested);
                        Ok(Step::Yield)
                    }
                    Err(error) => {
                        if !matches!(error, Error::Backpressure { .. }) {
                            self.reader.latch_failure();
                        }
                        Err(error)
                    }
                }
            }
            Step::Backpressure(pressure) => Err(Error::Backpressure(pressure)),
            other => Ok(other),
        }
    }
    fn synchronize(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some((_, position)) = self.seek {
            let request = self.reader.request();
            if let Err(error) = position(
                &mut self.input,
                self.reader.stream.envelope.range_input(),
                &request,
                cancel,
            ) {
                self.reader.latch_failure();
                return Err(error);
            }
        }
        self.reader
            .set_adapter_storage(self.input.retained_buffer_bytes());
        Ok(())
    }
    /// Closes parser jobs and pending supply before returning the input source.
    /// Caller-retained packet/configuration aliases keep their charge.
    pub fn into_inner(self) -> R {
        let Self { reader, input, .. } = self;
        drop(reader);
        input.into_inner()
    }
    /// The pool all reader storage is charged to.
    pub fn budget(&self) -> &Budget {
        self.reader.budget()
    }
    /// The tracks of the current configuration, in TrackEntry order.
    pub fn tracks(&self) -> &[crate::track::Track] {
        self.reader.tracks()
    }
    /// TimestampScale: nanoseconds per Segment tick.
    pub fn timestamp_scale_ns(&self) -> u64 {
        self.reader.timestamp_scale_ns()
    }
    /// The segment-metadata documents read so far, in reading order.
    pub fn metadata_documents(&self) -> &[SharedDocument] {
        self.reader.catalogue()
    }
    /// Bytes the metadata documents and their list keep charged.
    pub fn metadata_retained_bytes(&self) -> usize {
        self.reader.stream.catalogue.retained_bytes()
    }
    /// Each metadata document as its element ID and payload bytes, in reading order.
    pub fn opaque_metadata(&self) -> impl Iterator<Item = (u64, &[u8])> + '_ {
        self.reader
            .catalogue()
            .iter()
            .map(|document| (document.kind(), document.raw()))
    }
    /// The next packet with its configuration and BlockGroup side data; `None` at the
    /// end of the Segment. Fails with [`Error::Invalid`] while a failed seek is
    /// pending, with retryable [`Error::Backpressure`] when the pool is full of
    /// packets the caller holds, and with the error of malformed or refused input.
    pub fn next_typed_packet(&mut self, cancel: &AtomicBool) -> Result<Option<TypedPacket>> {
        if self.reader.pending_seek().is_some() {
            return Err(Error::Invalid("pending seek must be completed"));
        }
        loop {
            match self.poll(cancel)? {
                Step::Ready(Event::Packet(packet)) => return Ok(Some(packet)),
                Step::End(_) => return Ok(None),
                _ => (),
            }
        }
    }
    /// The next packet's frame alone, as [`next_typed_packet`](Self::next_typed_packet)
    /// reads it. Fails with [`Error::Unsupported`] for a packet whose BlockGroup
    /// carries block additions or CodecState.
    pub fn next_packet(&mut self, cancel: &AtomicBool) -> Result<Option<crate::packet::Packet>> {
        if self.reader.pending_seek().is_some() {
            return Err(Error::Invalid("pending seek must be completed"));
        }
        loop {
            match self.poll_projected::<LegacyProjection>(cancel, false)? {
                Step::Ready(LegacyEvent::Packet(packet)) => return Ok(Some(packet)),
                Step::Ready(LegacyEvent::FullPacket(packet)) => {
                    if packet.group.as_ref().is_some_and(|group| {
                        group.additions().len() != 0 || group.codec_state().is_some()
                    }) {
                        self.reader.latch_failure();
                        return Err(Error::Unsupported(
                            "Block additions/CodecState require next_typed_packet",
                        ));
                    }
                    return Ok(Some(packet.packet));
                }
                Step::Ready(LegacyEvent::Envelope(event)) => drop(event),
                Step::Ready(LegacyEvent::Configuration(configuration)) => drop(configuration),
                Step::Ready(LegacyEvent::Document(document)) => drop(document),
                Step::Ready(LegacyEvent::Seek(receipt)) => {
                    let _ = receipt;
                }
                Step::End(_) => return Ok(None),
                _ => (),
            }
        }
    }
    /// Seeks video track index `track` to its latest keyframe at or before
    /// `requested_ns` nanoseconds; packets continue from the landing. Fails as
    /// [`Reader::seek_previous`] does; a call that failed with
    /// [`Error::Backpressure`] can be repeated, and after any other failure the next
    /// seek starts over.
    pub fn seek_previous(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: false,
            },
            RecoveryAudio::Overlap,
            cancel,
        )
    }
    /// [`seek_previous`](Self::seek_previous) to the target less the largest track
    /// SeekPreRoll, replaying the landing Cluster's interleaved audio so every track
    /// can decode from the target.
    pub fn seek_for_playback(
        &mut self,
        track: usize,
        requested_ns: i64,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        self.seek(
            SeekRequest {
                track,
                requested_ns,
                playback_preroll: true,
            },
            RecoveryAudio::Overlap,
            cancel,
        )
    }
    fn seek(
        &mut self,
        request: SeekRequest,
        audio: RecoveryAudio,
        cancel: &AtomicBool,
    ) -> Result<SeekReceipt> {
        if self.reader.failed()
            || self
                .reader
                .control
                .seek()
                .is_none_or(|seek| seek.request() != request || seek.audio() != audio)
        {
            self.reader.begin_seek(request, audio)?;
        }
        loop {
            match self.poll_projected::<TypedProjection>(cancel, true)? {
                Step::Ready(Event::Seek(receipt)) => {
                    self.synchronize(cancel)?;
                    return Ok(receipt);
                }
                Step::Ready(Event::Packet(_)) => {
                    return Err(Error::Invalid("packet before seek receipt"));
                }
                Step::End(_) => {
                    return Err(Error::Invalid("seek completed without receipt"));
                }
                _ => (),
            }
        }
    }
}
impl<R: Read + Seek> BlockingReader<R> {
    const SEEK: Seeking<R> = (
        |reader, offset| Seek::seek(reader, std::io::SeekFrom::Start(offset)),
        |feed, cursor, request, cancel| feed.synchronize_seekable_position(cursor, request, cancel),
    );
}

fn capabilities(random_access: bool) -> SourceCapabilities {
    SourceCapabilities {
        length: None,
        random_access,
    }
}

impl BlockingReader<Cursor<OwnedInputBytes>> {
    fn open_owned(
        source: Cursor<OwnedInputBytes>,
        limits: Limits,
        pool: &Budget,
        policy: PayloadOwnership,
        webm_vp9_t35: bool,
    ) -> Result<Self> {
        if !source.get_ref().as_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let reader = if webm_vp9_t35 {
            new_reader(pool, SOURCE, capabilities(true), limits, WINDOW)?
        } else {
            Reader::new(pool, SOURCE, capabilities(true), limits, WINDOW)?
        };
        let input = InputFeeder::from_owned(source, WINDOW)?;
        Self::construct(reader, input, Some(Self::SEEK), policy)
    }
}

/// Open immutable input with WebM VP9's implicit ITU-T T.35 addition admitted.
pub fn open_owned_seekable_in(
    source: OwnedInputBytes,
    limits: Limits,
    pool: &Budget,
    policy: PayloadOwnership,
) -> Result<BlockingReader<Cursor<OwnedInputBytes>>> {
    BlockingReader::open_source(
        BlockingSource::owned(source).with_payload_ownership(policy),
        limits,
        pool,
        true,
    )
}

#[cfg(test)]
mod tests;
