//! HDMV presentation graphics (PGS) in TS and M2TS.
//!
//! [`PgsTsTrack`] takes the strict PES of one stream of type 0x90 from an
//! active PMT and hands each data field to the
//! [`PgsBridge`](crate::subtitles::bitmap::PgsBridge) as a [`BitmapInput`], on
//! the player's 90 kHz timeline.
use crate::subtitles::bitmap::{BitmapInput, BitmapScope};
use crate::subtitles::ts::{PesBinding, PesData, Taken, TakenPes};
use triergon_core::{Error, Result, TimeBase, WorkBudget};
use triergon_mpegts::{clock::SourceTimestamp, psi::stream_type};
use triergon_playback::Output;
use triergon_subtitles::pgs::PacketClock;

/// Where a PGS packet came from in the transport stream, kept as its
/// [`BitmapInput`] metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsPesFacts {
    /// The PID of the stream.
    pub pid: u16,
    /// The offset of the PES data field's first byte in the PID's elementary stream.
    pub elementary_offset: u64,
    /// The PES presentation time as coded and on the player's timeline.
    pub pts: Option<SourceTimestamp>,
    /// The PES decode time as coded and on the player's timeline.
    pub dts: Option<SourceTimestamp>,
}

/// What a poll did.
#[expect(
    clippy::large_enum_variant,
    reason = "the packet moves its charged owners inline, as the bitmap bridge takes it"
)]
#[derive(Debug)]
pub enum PgsTsStep {
    /// Push the next PES.
    NeedPacket,
    /// The work budget ran out; poll again.
    Yield,
    /// The pushed PES, ready for the PGS bridge.
    Packet(BitmapInput<TsPesFacts>),
    /// The player discarded a PES of the PID and reported this fault in its
    /// place. Push the next.
    Discarded(Error),
}

/// What a pushed output left for the next poll.
#[expect(
    clippy::large_enum_variant,
    reason = "one pending PES per bridge, stepped in place; a box would be an allocation the pool does not charge"
)]
#[derive(Debug)]
enum Pending {
    /// A PES being gathered, with its source facts.
    Pes(PesData, TsPesFacts),
    /// The fault the player reported for a PES it discarded.
    Discarded(Error),
}

/// One PGS stream of one active PMT. A new playback generation, PMT epoch or
/// TS clock epoch needs a new track.
#[derive(Debug)]
pub struct PgsTsTrack {
    binding: PesBinding,
    scope: BitmapScope,
    sequence: u64,
    pending: Option<Pending>,
}

impl PgsTsTrack {
    /// Bind to `pid` of the active PMT `output`; it must be a PGS stream.
    pub fn new(output: &Output, pid: u16, packet_limit: usize) -> Result<Self> {
        let (binding, stream) = PesBinding::new(output, pid, packet_limit)?;
        if stream.stream_type != stream_type::HDMV_PGS {
            return Err(Error::Unsupported("PGS track requires stream type 0x90"));
        }
        let scope = BitmapScope {
            source: output.source_id(),
            track: u64::from(pid),
            generation: output.generation(),
            configuration: binding.configuration_epoch,
        };
        Ok(Self {
            binding,
            scope,
            sequence: 0,
            pending: None,
        })
    }

    /// The scope of this track's packets, for the PGS bridge.
    pub fn scope(&self) -> BitmapScope {
        self.scope
    }

    /// Moves the next PES of this stream out of `mailbox`, or the fault the
    /// player reported for the PID in place of one, which the next poll returns
    /// as [`PgsTsStep::Discarded`]. A busy track returns `false`; a refused
    /// output stays in the mailbox.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        if self.pending.is_some() {
            return Ok(false);
        }
        self.pending = match self.binding.take(mailbox)? {
            None => return Ok(false),
            Some(Taken::Discarded(error)) => Some(Pending::Discarded(error)),
            Some(Taken::Pes(TakenPes { pes, pts, dts })) => {
                let facts = TsPesFacts {
                    pid: pes.pid,
                    elementary_offset: pes.elementary_offset,
                    pts,
                    dts,
                };
                Some(Pending::Pes(PesData::new(pes, self.binding.pool()), facts))
            }
        };
        Ok(true)
    }

    /// Gather the pushed PES into one packet under `work`.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<PgsTsStep> {
        let (data, facts) = match &mut self.pending {
            None => return Ok(PgsTsStep::NeedPacket),
            Some(Pending::Discarded(error)) => {
                let error = *error;
                self.pending = None;
                return Ok(PgsTsStep::Discarded(error));
            }
            Some(Pending::Pes(data, facts)) => (data, facts),
        };
        let Some(raw) = data.poll(self.binding.pool(), work)? else {
            return Ok(PgsTsStep::Yield);
        };
        // Only times on the player's timeline; raw 33-bit values stay in the facts.
        let ticks = |t: Option<SourceTimestamp>| {
            t.and_then(|t| t.ticks90)
                .map(|ticks| i64::try_from(ticks).map_err(|_| Error::Overflow))
                .transpose()
        };
        let input = BitmapInput {
            scope: self.scope,
            sequence: self.sequence,
            raw: raw.clone(),
            clock: PacketClock {
                pts: ticks(facts.pts)?,
                dts: ticks(facts.dts)?,
                timebase: TimeBase::HZ_90000,
            },
            duration: None,
            metadata: *facts,
        };
        self.pending = None;
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        Ok(PgsTsStep::Packet(input))
    }
}
