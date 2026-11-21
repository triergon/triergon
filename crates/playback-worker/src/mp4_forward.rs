//! Open and index an MP4 or MOV input, then fold every track's native packets
//! through the end of the source.
//!
//! A `regular` input is seekable and plays through the checked range index. A `progressive`
//! input is a forward-only front-`moov` file: its native front plan
//! (`Mp4IndexJob::new_native_terminal_v1`) plays through
//! `Engine::mp4_native_terminal_v1`, the progressive engine with native MOV
//! attachment, which continues through later `moof`/`mdat` pairs and ends only at
//! the source's actual end. Times are in the track's media time base; the flags are
//! the native packet flags.
//! The progressive-edited operation also folds source offsets, time bases and sample trim.

use crate::packet_fold::{Packet, Work, drive};
use crate::runner::Result;
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_isobmff::compat::mov::edits::EncodedEditPlan;
use triergon_playback::{
    ByteSource, Engine, Event, Format, Mp4Event, Mp4RangeOpen, Output, Player, PlayerPolicy,
    Progress, Selection,
    compat::mov,
    io::{SelectedInput, SourceOwner},
    mp4_open::{Movie, Mp4Open, OpenLimits, OpenProgress, OpenedMp4},
    mp4_plan::{Mp4IndexJob, PlanLimits},
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
    ProgressiveEdited,
    ProgressiveEditedRecovery,
}

#[inline(never)]
fn open(
    pool: &Budget,
    raw: &Bytes,
    layout: Layout,
    cancel: &AtomicBool,
) -> Result<OpenedMp4<ByteSource>> {
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
    let limits = OpenLimits::default();
    let mut open = if layout == Layout::ProgressiveEditedRecovery {
        Mp4Open::new_recovery(selected, limits)?
    } else {
        Mp4Open::new(selected, limits)?
    };
    while !matches!(
        open.advance(&mut WorkBudget::new(WORK), cancel)?,
        OpenProgress::Ready
    ) {}
    Ok(open.finish()?)
}

fn fold<const INCLUDE_TRIM: bool>(work: &mut Work, event: &Mp4Event) -> Result<()> {
    let (native, payload, skip) = match event {
        Mp4Event::Packet(packet) => {
            let native = packet
                .occurrence
                .as_ref()
                .ok_or("source packet without a native occurrence")?;
            (
                native,
                packet.source.payload.as_ref(),
                packet.encoded_skip_samples,
            )
        }
        Mp4Event::ShortPacket(short) => (
            &short.occurrence,
            short.prefix.payload.as_ref(),
            short.encoded_skip_samples,
        ),
        Mp4Event::EmptyPacket(_) | Mp4Event::MissingSample(_) => {
            return Err("native packet without a track".into());
        }
        Mp4Event::Configuration(_) => return Ok(()),
    };
    let demux = &native.demux;
    work.packet(Packet {
        track: u64::from(native.track_id),
        pts: demux.pts,
        dts: demux.dts,
        duration: u64::try_from(demux.duration).unwrap_or(0),
        flags: u64::from(demux.flags),
        size: payload.len(),
        payload: [payload],
    })?;
    if INCLUDE_TRIM {
        work.location(native.source.range.start, native.media_clock.demux_base);
        work.skip_samples(skip);
    }
    Ok(())
}

fn collect<const INCLUDE_TRIM: bool>(
    mut work: Work,
    mut read: impl FnMut(&mut Option<Output>) -> triergon_core::Result<Progress>,
) -> Result<Work> {
    let mut pending = None;
    loop {
        let progress = read(&mut pending)?;
        if let Some(output) = pending.take()
            && let Event::Mp4(event) = output.event()
        {
            fold::<INCLUDE_TRIM>(&mut work, event)?;
        }
        match progress {
            Progress::Ended => return Ok(work),
            Progress::NeedInput(_) | Progress::Yield | Progress::Queued => {}
            other => return Err(format!("unexpected player progress: {other:?}").into()),
        }
    }
}

/// Each track's edit plan over the open input.
#[inline(never)]
fn edit_plans(
    pool: &Budget,
    movie: &Movie,
    input: &mut InputCursor,
    cancel: &AtomicBool,
) -> Result<BudgetVec<Option<EncodedEditPlan>>> {
    let mut plans = BudgetVec::new(pool);
    plans.try_reserve_exact(movie.tracks().len())?;
    for track in movie.tracks() {
        let plan = crate::mp4_range::prepare_edit(movie, track, input, WORK, cancel)?;
        plans.try_push(plan).map_err(|error| error.error)?;
    }
    Ok(plans)
}

/// Plays a seekable input through the checked range index.
#[inline(never)]
fn regular(
    pool: &Budget,
    opened: OpenedMp4<ByteSource>,
    work: Work,
    cancel: &AtomicBool,
) -> Result<Work> {
    let OpenedMp4 {
        movie,
        mut input,
        source,
    } = opened;
    let edits = if movie.tracks().iter().any(|track| {
        track.edits().is_some()
            && track.table().has_composition_offsets()
            && track.fragments().is_empty()
    }) {
        Some(edit_plans(pool, &movie, &mut input, cancel)?)
    } else {
        None
    };
    let mut index = Mp4RangeOpen::new(
        &movie,
        edits.as_ref().map(BudgetVec::as_slice),
        PlanLimits::default(),
    )?;
    while !index.poll(&mut WorkBudget::new(WORK), cancel)? {}
    let mut player = index.finish(
        input,
        source,
        MAX_PACKET,
        Selection::All,
        PlayerPolicy::default(),
    )?;
    collect::<false>(work, |pending| {
        player.read_output(pending, &mut WorkBudget::new(WORK), cancel)
    })
}

/// Plays a forward-only input through the progressive engine.
#[inline(never)]
fn progressive(
    pool: &Budget,
    opened: OpenedMp4<ByteSource>,
    include_trim: bool,
    work: Work,
    cancel: &AtomicBool,
) -> Result<Work> {
    let OpenedMp4 {
        movie,
        mut input,
        source,
    } = opened;
    let limits = PlanLimits::default();
    let plans = if include_trim && movie.trex().is_empty() {
        Some(edit_plans(pool, &movie, &mut input, cancel)?)
    } else {
        None
    };
    let mut index = match &plans {
        Some(plans) => mov::forward_encoded_edits(&movie, plans.as_slice(), limits)?,
        None => Mp4IndexJob::new_native_terminal_v1(&movie, limits)?,
    };
    while !index.poll(&mut WorkBudget::new(WORK), cancel)? {}
    let engine = Engine::mp4_native_terminal_v1(
        input,
        index.finish()?,
        MAX_PACKET,
        ProgressiveLimits::default(),
    )?;
    let mut player = Player::new_direct(engine, source, Selection::All, PlayerPolicy::default())?;
    let read = |pending: &mut Option<Output>| {
        player.read_output(pending, &mut WorkBudget::new(WORK), cancel)
    };
    if include_trim {
        collect::<true>(work, read)
    } else {
        collect::<false>(work, read)
    }
}

fn operation(pool: &Budget, raw: &Bytes, layout: Layout, work: Work) -> Result<Work> {
    let cancel = AtomicBool::new(false);
    let opened = open(pool, raw, layout, &cancel)?;
    match layout {
        Layout::Regular => regular(pool, opened, work, &cancel),
        Layout::Progressive | Layout::ProgressiveEdited | Layout::ProgressiveEditedRecovery => {
            progressive(pool, opened, layout != Layout::Progressive, work, &cancel)
        }
    }
}

/// `mp4-forward-fold FILE LAYOUT`, `mp4-forward-bench FILE ITERATIONS LAYOUT` or
/// `mp4-forward-memory FILE LAYOUT`; LAYOUT is `regular`, `progressive`,
/// `progressive-edited` or `progressive-edited-recovery`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, work: Work) -> Result<Work> {
    let layout = match argument {
        "regular" => Layout::Regular,
        "progressive" => Layout::Progressive,
        "progressive-edited" => Layout::ProgressiveEdited,
        "progressive-edited-recovery" => Layout::ProgressiveEditedRecovery,
        _ => return Err("expected regular, progressive or progressive-edited".into()),
    };
    operation(pool, raw, layout, work)
}
