//! Helpers shared by the playback integration tests.
// Each test binary uses part of this file.
#![allow(dead_code)]

use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, Result, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenProgress},
    *,
};

/// Steps a fixture is allowed before a test calls it stalled.
const STEPS: usize = 2_000_000;

/// A cancellation flag that was never raised.
pub fn off() -> AtomicBool {
    AtomicBool::new(false)
}

/// Advance an opener until it is ready to finish.
pub fn run_open<S: Source>(open: &mut Mp4Open<S>, units: u64) -> Result<()> {
    for _ in 0..STEPS {
        if matches!(
            open.advance(&mut WorkBudget::new(units), &off())?,
            OpenProgress::Ready
        ) {
            return Ok(());
        }
    }
    panic!("MP4 open did not make bounded progress");
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

fn read_units<S: Source>(player: &mut Player<S>, units: u64, out: &mut Vec<Output>) -> Result<()> {
    for _ in 0..STEPS {
        let progress = player.advance(&mut WorkBudget::new(units), &off())?;
        while let Some(output) = player.pop() {
            out.push(output);
        }
        if matches!(progress, Progress::Ended) {
            return Ok(());
        }
    }
    panic!("bounded fixture stalled");
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
            RawEvent::Mp4(Mp4Event::Packet(packet)) => Some(packet),
            _ => None,
        })
        .collect()
}
