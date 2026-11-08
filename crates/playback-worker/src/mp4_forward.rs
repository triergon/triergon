//! MP4 or MOV forward playback through the playback engine: open a preloaded
//! input with `Mp4Open`, index it with `Mp4IndexJob` in the native MOV view
//! (`Mp4View::PinnedMov9NoParseNoFillin`), play every track forward through the
//! end of the source and fold every native packet.
//!
//! A `regular` input is seekable and plays through `Engine::mp4`. A `progressive`
//! input is a forward-only front-`moov` file: its native front plan
//! (`Mp4IndexJob::new_native_terminal_v1`) plays through
//! `Engine::mp4_native_terminal_v1`, the progressive engine with native MOV
//! attachment, which continues through later `moof`/`mdat` pairs and ends only at
//! the source's actual end. Times are in the track's media time base; the flags are
//! the native packet flags.

use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget};
use triergon_playback::{
    ByteSource, Engine, Format, Mp4Event, Player, PlayerPolicy, Progress, RawEvent, Selection,
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::{Mp4IndexJob, Mp4View, PlanLimits},
    progressive::ProgressiveLimits,
};

pub const PREFIX: &str = "mp4-forward-";
const OPERATION: &str = "mp4-forward-playback";
const WINDOW: usize = 32 * 1024;
const WORK: u64 = 1 << 16;
const MAX_PACKET: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Regular,
    Progressive,
}

fn player(pool: &Budget, raw: &Bytes, layout: Layout) -> Result<Player<ByteSource>> {
    let cancel = AtomicBool::new(false);
    let id = SourceId(44);
    let selected = SelectedInput {
        input: InputCursor::new(
            pool,
            id,
            SourceCapabilities {
                length: Some(raw.len() as u64),
                random_access: layout == Layout::Regular,
            },
            WINDOW,
        )?,
        source: SourceOwner::new(ByteSource::new(id, raw.clone(), WINDOW)?),
        format: Format::Mp4,
    };
    let mut open = Mp4Open::new(selected, OpenLimits::default())?;
    while !matches!(
        open.advance(&mut WorkBudget::new(WORK), &cancel)?,
        OpenProgress::Ready
    ) {}
    let opened = open.finish()?;
    let mut index = match layout {
        Layout::Regular => Mp4IndexJob::new(
            &opened.movie,
            Mp4View::PinnedMov9NoParseNoFillin,
            PlanLimits::default(),
        )?,
        Layout::Progressive => {
            Mp4IndexJob::new_native_terminal_v1(&opened.movie, PlanLimits::default())?
        }
    };
    while !index.poll(&mut WorkBudget::new(WORK), &cancel)? {}
    let plan = index.finish()?;
    let engine = match layout {
        Layout::Regular => Engine::mp4(opened.input, plan, MAX_PACKET)?,
        Layout::Progressive => Engine::mp4_native_terminal_v1(
            opened.input,
            plan,
            MAX_PACKET,
            ProgressiveLimits::default(),
        )?,
    };
    Ok(Player::new(
        engine,
        opened.source,
        Selection::All,
        PlayerPolicy::default(),
    )?)
}

fn fold(work: &mut Work, event: &Mp4Event) -> Result<()> {
    let (track, demux, payload) = match event {
        Mp4Event::Packet(packet) => {
            let native = packet
                .native
                .as_ref()
                .ok_or("source packet without a native occurrence")?;
            (
                native.track_id,
                &native.demux,
                packet.source.payload.as_ref(),
            )
        }
        Mp4Event::NativeShortPacket(short) => (
            short.native.track_id,
            &short.native.demux,
            short.prefix.payload.as_ref(),
        ),
        Mp4Event::NativeEmptyPacket(_) | Mp4Event::NativeMissingSample(_) => {
            return Err("native packet without a track".into());
        }
        Mp4Event::Configuration(_) => return Ok(()),
    };
    work.packet(Packet {
        track: u64::from(track),
        pts: demux.pts,
        dts: demux.dts,
        duration: u64::try_from(demux.duration).unwrap_or(0),
        flags: u64::from(demux.flags),
        size: payload.len(),
        payload: [payload],
    })
}

fn operation(pool: &Budget, raw: &Bytes, layout: Layout, mut work: Work) -> Result<Work> {
    let cancel = AtomicBool::new(false);
    let mut player = player(pool, raw, layout)?;
    let mut pending = None;
    loop {
        let progress = player.read_output(&mut pending, &mut WorkBudget::new(WORK), &cancel)?;
        if let Some(output) = pending.take()
            && let RawEvent::Mp4(event) = output.event()
        {
            fold(&mut work, event)?;
        }
        match progress {
            Progress::Ended => break,
            Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
            other => return Err(format!("unexpected player progress: {other:?}").into()),
        }
    }
    Ok(work)
}

/// `mp4-forward-fold FILE LAYOUT`, `mp4-forward-bench FILE ITERATIONS LAYOUT` or
/// `mp4-forward-memory FILE LAYOUT`; LAYOUT is `regular` or `progressive`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, work: Work) -> Result<Work> {
    let layout = match argument {
        "regular" => Layout::Regular,
        "progressive" => Layout::Progressive,
        _ => return Err("expected regular or progressive".into()),
    };
    operation(pool, raw, layout, work)
}
