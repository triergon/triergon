//! HDMV presentation graphics (PGS) in TS and M2TS.
//!
//! [`PgsTsTrack`] takes the strict PES of one stream of type 0x90 from an
//! active PMT and hands each data field to the
//! [`PgsBridge`](crate::bitmap_bridge::PgsBridge) as a [`BitmapInput`], on
//! the player's 90 kHz timeline.
use crate::Output;
use crate::bitmap_bridge::{BitmapInput, BitmapScope};
use crate::ts_pes::{PesBinding, PesData, TakenPes};
use triergon_core::{Error, Result, TimeBase, WorkBudget};
use triergon_mpegts::clock::SourceTimestamp;
use triergon_subtitles::pgs::PacketClock;

/// The BD-ROM stream type of presentation graphics.
const STREAM_TYPE_PGS: u8 = 0x90;

/// The source facts of a TS bitmap packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsPesFacts {
    pub pid: u16,
    /// Where the PES payload starts in the elementary stream.
    pub elementary_offset: u64,
    pub pts: Option<SourceTimestamp>,
    pub dts: Option<SourceTimestamp>,
}

/// What a poll did.
#[expect(
    clippy::large_enum_variant,
    reason = "the packet moves its charged owners inline, as the bitmap bridge takes it"
)]
pub enum PgsTsStep {
    /// Push the next PES.
    NeedPacket,
    /// The work budget ran out; poll again.
    Yield,
    /// The pushed PES, ready for the PGS bridge.
    Packet(BitmapInput<TsPesFacts>),
}

/// One PGS stream of one active PMT. A new playback generation, PMT epoch or
/// TS clock epoch needs a new track.
pub struct PgsTsTrack {
    binding: PesBinding,
    scope: BitmapScope,
    sequence: u64,
    pending: Option<(PesData, TsPesFacts)>,
}

impl PgsTsTrack {
    /// Bind to `pid` of the active PMT `output`; it must be a PGS stream.
    pub fn new(output: &Output, pid: u16, packet_limit: usize) -> Result<Self> {
        let (binding, stream) = PesBinding::new(output, pid, packet_limit)?;
        if stream.stream_type != STREAM_TYPE_PGS {
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

    /// Move a PES of this stream out of `mailbox`. A busy track returns
    /// `false`; a refused output stays in the mailbox.
    pub fn push(&mut self, mailbox: &mut Option<Output>) -> Result<bool> {
        if self.pending.is_some() {
            return Ok(false);
        }
        let Some(TakenPes { pes, pts, dts }) = self.binding.take(mailbox)? else {
            return Ok(false);
        };
        let facts = TsPesFacts {
            pid: pes.pid,
            elementary_offset: pes.elementary_offset,
            pts,
            dts,
        };
        self.pending = Some((PesData::new(pes, self.binding.pool()), facts));
        Ok(true)
    }

    /// Gather the pushed PES into one packet under `work`.
    pub fn poll(&mut self, work: &mut WorkBudget) -> Result<PgsTsStep> {
        let Some((data, facts)) = &mut self.pending else {
            return Ok(PgsTsStep::NeedPacket);
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
                timebase: TimeBase::new(1, 90_000)?,
            },
            duration: None,
            metadata: *facts,
        };
        self.pending = None;
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        Ok(PgsTsStep::Packet(input))
    }
}
