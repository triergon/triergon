use super::*;
use crate::reader::input_feeder::{FeedStep, InputFeeder};
use std::io::{Read, Seek};
use triergon_core::InputCursor;

type Reposition<R> = fn(&mut R, u64) -> std::io::Result<u64>;
type Position<R> =
    fn(&mut InputFeeder<R>, &mut InputCursor, &InputRequest, &AtomicBool) -> Result<()>;
/// How a seekable source repositions and resynchronizes its cursor.
type Seeking<R> = (Reposition<R>, Position<R>);

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
    for audio in tracks.iter().filter(|track| track.kind == 2) {
        delay = delay.max(RecoveryAudio::BlockStart.delay_ns(audio, scale)?);
    }
    Ok(delay != 0 && time < i128::from(delay))
}

impl<R: Read> BlockingReader<R> {
    /// Opens a sequential source in a new pool of `limits.live_storage_bytes` and
    /// reads until Tracks and the first Cluster are known; the metadata must precede
    /// that Cluster. Fails with [`Error::Limit`] for a pool too small to create,
    /// [`Error::Truncated`] when the source ends first, [`Error::Transport`] for an
    /// I/O error, and with the parse error of malformed or refused metadata.
    pub fn open(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in(source, limits, &pool)
    }
    /// [`open`](Self::open), charging all storage to the caller's `pool`.
    pub fn open_in(source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        let policy = crate::storage::PayloadOwnership::OwnedExact;
        Self::open_in_with_payload_ownership(source, limits, pool, policy)
    }
    /// [`open`](Self::open) with the block-payload policy `policy`.
    pub fn open_with_payload_ownership(
        source: R,
        limits: Limits,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_in_with_payload_ownership(source, limits, &pool, policy)
    }
    /// [`open_in`](Self::open_in) with the block-payload policy `policy`.
    pub fn open_in_with_payload_ownership(
        source: R,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let reader = Reader::new(pool, SOURCE, capabilities(false), limits, WINDOW)?;
        Self::construct(reader, InputFeeder::new(source, WINDOW)?, None, policy)
    }
    /// The block-payload policy this reader was opened with.
    pub fn payload_ownership(&self) -> crate::storage::PayloadOwnership {
        self.reader.payload_ownership()
    }
    fn reposition(&self) -> Option<Reposition<R>> {
        self.seek.map(|(reposition, _)| reposition)
    }
    fn construct(
        mut reader: Reader,
        input: InputFeeder<R>,
        seek: Option<Seeking<R>>,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        reader.envelope.set_payload_ownership(policy);
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
            &mut WorkBudget::new(WORK),
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
                let reposition = self.reposition();
                let result = if let Some(plan) = refill.as_ref().filter(|_| discard) {
                    self.input.feed_typed_discard_refill(
                        self.reader.envelope.range_input(),
                        plan,
                        cancel,
                        reposition,
                    )
                } else if let Some(plan) = refill {
                    self.input.feed_typed_header_refill(
                        self.reader.envelope.range_input(),
                        &plan,
                        cancel,
                        reposition,
                    )
                } else if discard {
                    self.input.feed_typed_discard_request(
                        self.reader.envelope.range_input(),
                        &request,
                        cancel,
                        reposition,
                    )
                } else {
                    self.input.feed_typed_request(
                        self.reader.envelope.range_input(),
                        &request,
                        header,
                        cancel,
                        reposition,
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
            } => Err(Error::Backpressure(Backpressure {
                requested,
                available,
            })),
            other => Ok(other),
        }
    }
    fn synchronize(&mut self, cancel: &AtomicBool) -> Result<()> {
        if let Some((_, position)) = self.seek {
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
        self.reader.catalogue.retained_bytes()
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
                ParseStep::Event(Event::Packet(packet)) => return Ok(Some(packet)),
                ParseStep::Finished => return Ok(None),
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
        if self.reader.failed
            || self.reader.pending_seek() != Some(request)
            || self.reader.recovery_audio != audio
        {
            self.reader.begin_seek(request, audio)?;
        }
        loop {
            // Lent for this poll only; see EpochRestoreJob::try_owned_prefix.
            self.reader.owned_root = self.input.owned_source().cloned();
            let step = self.poll(cancel);
            self.reader.owned_root = None;
            match step? {
                ParseStep::Event(Event::Seek(receipt)) => {
                    self.synchronize(cancel)?;
                    return Ok(receipt);
                }
                ParseStep::Event(Event::Packet(_)) => {
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
    const SEEK: Seeking<R> = (
        |reader, offset| Seek::seek(reader, std::io::SeekFrom::Start(offset)),
        |feed, cursor, request, cancel| feed.synchronize_seekable_position(cursor, request, cancel),
    );
    /// Opens a seekable source in a new pool of `limits.live_storage_bytes`, following
    /// SeekHead chains to load late Tracks, Cues and segment metadata; the reader can
    /// then seek. Fails as [`open`](Self::open) does.
    pub fn open_seekable(source: R, limits: Limits) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_seekable_in(source, limits, &pool)
    }
    /// [`open_seekable`](Self::open_seekable), charging all storage to the caller's `pool`.
    pub fn open_seekable_in(source: R, limits: Limits, pool: &Budget) -> Result<Self> {
        let policy = crate::storage::PayloadOwnership::OwnedExact;
        Self::open_seekable_in_with_payload_ownership(source, limits, pool, policy)
    }
    /// [`open_seekable`](Self::open_seekable) with the block-payload policy `policy`.
    pub fn open_seekable_with_payload_ownership(
        source: R,
        limits: Limits,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let pool = Budget::new(limits.live_storage_bytes)?;
        Self::open_seekable_in_with_payload_ownership(source, limits, &pool, policy)
    }
    /// [`open_seekable_in`](Self::open_seekable_in) with the block-payload policy `policy`.
    pub fn open_seekable_in_with_payload_ownership(
        source: R,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
    ) -> Result<Self> {
        let reader = Reader::new(pool, SOURCE, capabilities(true), limits, WINDOW)?;
        Self::construct(
            reader,
            InputFeeder::new(source, WINDOW)?,
            Some(Self::SEEK),
            policy,
        )
    }
}

fn capabilities(random_access: bool) -> SourceCapabilities {
    SourceCapabilities {
        length: None,
        random_access,
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

    fn open_owned_with_profile(
        source: triergon_core::OwnedInputBytes,
        limits: Limits,
        pool: &Budget,
        policy: crate::storage::PayloadOwnership,
        webm_vp9_t35: bool,
    ) -> Result<Self> {
        if !source.as_bytes().belongs_to(pool) {
            return Err(Error::ForeignPool);
        }
        let reader = if webm_vp9_t35 {
            new_reader(pool, SOURCE, capabilities(true), limits, WINDOW)?
        } else {
            Reader::new(pool, SOURCE, capabilities(true), limits, WINDOW)?
        };
        let input = InputFeeder::from_owned(std::io::Cursor::new(source), WINDOW)?;
        Self::construct(reader, input, Some(Self::SEEK), policy)
    }
}

/// [`BlockingReader::open_owned_seekable_in`] with WebM VP9's implicit ITU-T T.35
/// addition admitted, as [`new_reader`](super::new_reader) does. Documented on
/// [`compat::block_additions`](crate::compat::block_additions).
pub fn open_owned_seekable_in(
    source: triergon_core::OwnedInputBytes,
    limits: Limits,
    pool: &Budget,
    policy: crate::storage::PayloadOwnership,
) -> Result<BlockingReader<std::io::Cursor<triergon_core::OwnedInputBytes>>> {
    BlockingReader::open_owned_with_profile(source, limits, pool, policy, true)
}

#[cfg(test)]
mod tests;
