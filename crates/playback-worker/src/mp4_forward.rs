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
use triergon_playback::{
    ByteSource, Engine, Format, Mp4Event, Mp4RangeOpen, Output, Player, PlayerPolicy, Progress,
    RawEvent, Selection,
    compat::mov,
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress, OpenedMp4},
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
}

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
    let mut open = Mp4Open::new(selected, OpenLimits::default())?;
    while !matches!(
        open.advance(&mut WorkBudget::new(WORK), cancel)?,
        OpenProgress::Ready
    ) {}
    Ok(open.finish()?)
}

fn fold<const INCLUDE_TRIM: bool>(work: &mut Work, event: &Mp4Event) -> Result<()> {
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
    })?;
    if INCLUDE_TRIM {
        let Mp4Event::Packet(packet) = event else {
            return Err("encoded edit packet is incomplete".into());
        };
        work.location(
            packet.source.sample.range.start,
            packet
                .native
                .as_ref()
                .ok_or("encoded edit packet has no media clock")?
                .media_clock
                .demux_base,
        );
        work.skip_samples(packet.encoded_skip_samples);
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
            && let RawEvent::Mp4(event) = output.event()
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

fn operation(pool: &Budget, raw: &Bytes, layout: Layout, work: Work) -> Result<Work> {
    let cancel = AtomicBool::new(false);
    let mut opened = open(pool, raw, layout, &cancel)?;
    let movie = &opened.movie;
    match layout {
        Layout::Regular => {
            let edits = if movie.tracks().iter().any(|track| {
                track.edits().is_some()
                    && track.table().has_composition_offsets()
                    && track.fragments().is_empty()
            }) {
                let mut plans = BudgetVec::new(pool);
                plans.try_reserve_exact(movie.tracks().len())?;
                for track in movie.tracks() {
                    let plan = crate::mp4_range::prepare_edit(
                        movie,
                        track,
                        &mut opened.input,
                        WORK,
                        &cancel,
                    )?;
                    plans.try_push(plan).map_err(|error| error.error)?;
                }
                Some(plans)
            } else {
                None
            };
            let mut index = Mp4RangeOpen::new(
                movie,
                edits.as_ref().map(BudgetVec::as_slice),
                PlanLimits::default(),
            )?;
            while !index.poll(&mut WorkBudget::new(WORK), &cancel)? {}
            let mut player = index.finish(
                opened.input,
                opened.source,
                MAX_PACKET,
                Selection::All,
                PlayerPolicy::default(),
            )?;
            collect::<false>(work, |pending| {
                player.read_output(pending, &mut WorkBudget::new(WORK), &cancel)
            })
        }
        Layout::Progressive | Layout::ProgressiveEdited => {
            let include_trim = layout == Layout::ProgressiveEdited;
            let mut plans = BudgetVec::new(pool);
            let limits = PlanLimits::default();
            let mut index = if include_trim {
                plans.try_reserve_exact(movie.tracks().len())?;
                for track in movie.tracks() {
                    let plan = crate::mp4_range::prepare_edit(
                        movie,
                        track,
                        &mut opened.input,
                        WORK,
                        &cancel,
                    )?;
                    plans.try_push(plan).map_err(|error| error.error)?;
                }
                mov::forward_encoded_edits(movie, plans.as_slice(), limits)?
            } else {
                Mp4IndexJob::new_native_terminal_v1(movie, limits)?
            };
            while !index.poll(&mut WorkBudget::new(WORK), &cancel)? {}
            let engine = Engine::mp4_native_terminal_v1(
                opened.input,
                index.finish()?,
                MAX_PACKET,
                ProgressiveLimits::default(),
            )?;
            let mut player = Player::new_direct(
                engine,
                opened.source,
                Selection::All,
                PlayerPolicy::default(),
            )?;
            let read = |pending: &mut Option<Output>| {
                player.read_output(pending, &mut WorkBudget::new(WORK), &cancel)
            };
            if include_trim {
                collect::<true>(work, read)
            } else {
                collect::<false>(work, read)
            }
        }
    }
}

/// `mp4-forward-fold FILE LAYOUT`, `mp4-forward-bench FILE ITERATIONS LAYOUT` or
/// `mp4-forward-memory FILE LAYOUT`; LAYOUT is `regular`, `progressive` or
/// `progressive-edited`.
pub fn run(args: &[String]) -> Result<()> {
    drive(args, PREFIX, OPERATION, operate)
}

/// One operation on a preloaded input with the case argument.
pub fn operate(pool: &Budget, raw: &Bytes, argument: &str, work: Work) -> Result<Work> {
    let layout = match argument {
        "regular" => Layout::Regular,
        "progressive" => Layout::Progressive,
        "progressive-edited" => Layout::ProgressiveEdited,
        _ => return Err("expected regular, progressive or progressive-edited".into()),
    };
    operation(pool, raw, layout, work)
}
