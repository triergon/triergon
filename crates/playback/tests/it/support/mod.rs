//! Helpers shared by the playback integration tests: bounded stepping, MP4 opening and
//! indexing, reading a player to its end, and the fixture players and golds.
pub(crate) mod golds;
pub(crate) mod players;
pub(crate) mod sequential;
pub(crate) mod terminal;

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, Result, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{
    mp4::{Movie, Mp4Event, Mp4IndexJob, Mp4Open, Mp4Packet, OpenProgress, PlanLimits},
    probe::SelectedInput,
    *,
};

/// Steps a fixture is allowed before a test calls it stalled.
const STEPS: usize = 2_000_000;

/// A cancellation flag that was never raised.
pub fn off() -> AtomicBool {
    AtomicBool::new(false)
}

/// Repeat `step` until it reports completion, panicking if it does not
/// finish within the step allowance.
pub fn bounded(what: &str, mut step: impl FnMut() -> Result<bool>) -> Result<()> {
    for _ in 0..STEPS {
        if step()? {
            return Ok(());
        }
    }
    panic!("{what} did not make bounded progress");
}

/// Advance an opener until it is ready to finish.
pub fn run_open<S: Source>(open: &mut Mp4Open<S>, units: u64) -> Result<()> {
    bounded("MP4 open", || {
        Ok(matches!(
            open.advance(&mut WorkBudget::new(units), &off())?,
            OpenProgress::Ready
        ))
    })
}

/// Which index a test plays: source samples or the MOV demux view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Source,
    Demux,
}

/// The index job of `movie` in `view`.
pub fn index_job(movie: &Movie, view: View, limits: PlanLimits) -> Result<Mp4IndexJob<'_>> {
    match view {
        View::Source => Mp4IndexJob::new(movie, limits),
        View::Demux => compat::mov::index(movie, limits),
    }
}

/// Advance an index job until its plan is ready.
pub fn run_index(job: &mut Mp4IndexJob<'_>, units: u64) -> Result<()> {
    bounded("MP4 index", || {
        job.poll(&mut WorkBudget::new(units), &off())
    })
}

/// An MP4 input served from memory, `quantum` bytes at a time.
pub fn byte_input(
    pool: &Budget,
    id: SourceId,
    raw: &[u8],
    quantum: usize,
    capabilities: SourceCapabilities,
    window: usize,
) -> Result<SelectedInput<ByteSource>> {
    let bytes = Bytes::copy_from_slice(pool, raw)?;
    Ok(SelectedInput {
        input: InputCursor::new(pool, id, capabilities, window)?,
        source: SourceOwner::new(ByteSource::new(id, bytes, quantum)?),
        format: Format::Mp4,
    })
}

/// Advance a player to its end one work unit at a time, appending what it delivers.
pub fn read<S: Source>(player: &mut Player<S>, out: &mut Vec<Output>) -> Result<()> {
    read_units(player, 1, out)
}

/// Advance a player to its end `units` at a time, appending what it delivers.
pub fn read_units<S: Source>(
    player: &mut Player<S>,
    units: u64,
    out: &mut Vec<Output>,
) -> Result<()> {
    bounded("player", || {
        let progress = player.advance(&mut WorkBudget::new(units), &off())?;
        while let Some(output) = player.pop() {
            out.push(output);
        }
        Ok(matches!(progress, Progress::Ended))
    })
}

/// Advance a player to its end and return what it delivered.
pub fn collect<S: Source>(player: &mut Player<S>, units: u64) -> Result<Vec<Output>> {
    let mut out = Vec::new();
    read_units(player, units, &mut out)?;
    Ok(out)
}

/// The MP4 packets among delivered outputs.
pub fn packets(out: &[Output]) -> Vec<&Mp4Packet> {
    out.iter()
        .filter_map(|output| match output.event() {
            Event::Mp4(Mp4Event::Packet(packet)) => Some(packet),
            _ => None,
        })
        .collect()
}
