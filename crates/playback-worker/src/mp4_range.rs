//! Compare every packet of a regular AVC/AAC file through range playback.

use crate::runner::{Invocation, MAX_INPUT, Result, Totals, measure};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, BudgetVec, Bytes, InputCursor, SourceCapabilities, SourceId, WorkBudget,
};
use triergon_harness_protocol::digest::{fold, fold_bytes, signed};
use triergon_isobmff::{
    ConfigurationLimits,
    compat::mov::{
        edits::{
            EncodedEditContext as EditContext, EncodedEditJob as EditJob,
            EncodedEditLimits as EditLimits,
        },
        encoded::{EncodedSelection as DescriptionSelection, EncodedStreamJob as ConfigurationJob},
        packets::CursorStatus,
        seek::SeekLimits,
    },
};
use triergon_playback::{
    ByteSource, Format, Mp4Event, Mp4NativeSeekRequest, Mp4Packet, Mp4RangeOpen, PlayerPolicy,
    Progress, RawEvent, Selection,
    io::{SelectedInput, SourceOwner},
    mp4_open::{Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::PlanLimits,
};

pub const PREFIX: &str = "mp4-range-";
const OPERATION: &str = "mp4-range-playback";
const WINDOW: usize = 32 * 1024;

#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    fn packet(&mut self, packet: &Mp4Packet) -> Result<()> {
        let row = packet.native.as_ref().ok_or("missing projected packet")?;
        let clock = &row.demux;
        let bytes = packet.source.payload.as_ref();
        let skip = packet.encoded_skip_samples.unwrap_or_default();
        let trim_start = u32::from_le_bytes(skip[..4].try_into()?);
        let trim_end = u32::from_le_bytes(skip[4..8].try_into()?);
        for field in [
            u64::from(row.track_id),
            signed(clock.pts.unwrap_or(i64::MIN)),
            signed(clock.dts.unwrap_or(i64::MIN)),
            signed(clock.duration),
            packet.source.sample.range.start,
            bytes.len() as u64,
            u64::from(clock.flags),
            u64::from(trim_start),
            u64::from(trim_end),
            u64::from(skip[8]),
            u64::from(skip[9]),
        ] {
            fold(&mut self.digest, field);
        }
        fold_bytes(&mut self.digest, bytes);
        self.packets += 1;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    fn totals(self) -> Totals {
        Totals {
            packets: self.packets,
            bytes: self.bytes,
            digest: self.digest,
            refusal: None,
        }
    }
}

fn session(pool: &Budget, data: &Bytes, seek: Option<i64>) -> Result<Totals> {
    let id = SourceId(1);
    let input = InputCursor::new(
        pool,
        id,
        SourceCapabilities {
            length: Some(data.len() as u64),
            random_access: true,
        },
        WINDOW,
    )?;
    let selected = SelectedInput {
        input,
        source: SourceOwner::new(ByteSource::new(id, data.clone(), WINDOW)?),
        format: Format::Mp4,
    };
    let cancel = AtomicBool::new(false);
    let work = || WorkBudget::new(4096);
    let mut open = Mp4Open::new(selected, OpenLimits::default())?;
    while !matches!(open.advance(&mut work(), &cancel)?, OpenProgress::Ready) {}
    let mut opened = open.finish()?;
    let movie = &opened.movie;
    let movie_base = movie.header().time_base()?;
    let mut plans = BudgetVec::new(pool);
    plans.try_reserve_exact(movie.tracks().len())?;
    let mut video = None;
    let mut audio = false;
    for track in movie.tracks() {
        let header = track.header();
        let table = track.table();
        let descriptions = table.descriptions();
        match (header.handler, descriptions) {
            (kind, [entry]) if kind == *b"vide" && entry.format == *b"avc1" && video.is_none() => {
                video = Some(header.track_id);
            }
            (kind, [entry]) if kind == *b"soun" && entry.format == *b"mp4a" && !audio => {
                audio = true;
            }
            _ => return Err("expected one AVC and one AAC track".into()),
        }
        if !track.fragments().is_empty() {
            return Err("expected regular sample tables".into());
        }
        let plan = if let Some(edits) = track.edits() {
            let mut config = ConfigurationJob::new_range_playback(
                header,
                table,
                &opened.input,
                DescriptionSelection {
                    major_brand: movie.major_brand(),
                    description: descriptions[0].index,
                    limits: ConfigurationLimits::default(),
                },
            )?;
            while config.poll(&mut opened.input, &mut work(), &cancel)? != CursorStatus::Ready {}
            let configs = [config.take(&mut opened.input, &cancel)?];
            let mut edit = EditJob::new_range_playback(
                EditContext {
                    table,
                    edits,
                    stream: &configs[0],
                    media_base: header.time_base()?,
                    movie_base,
                },
                &configs,
                &opened.input,
                EditLimits::default(),
            )?;
            while edit.poll(&mut opened.input, &mut work(), &cancel)? != CursorStatus::Ready {}
            Some(edit.take(&mut opened.input, &cancel)?)
        } else {
            None
        };
        plans.try_push(plan).map_err(|error| error.error)?;
    }
    let video = video.filter(|_| audio).ok_or("missing AVC/AAC pair")?;
    let mut range = Mp4RangeOpen::new(movie, Some(plans.as_slice()), PlanLimits::default())?;
    while !range.poll(&mut work(), &cancel)? {}
    let mut player = range.finish(
        opened.input,
        opened.source,
        1 << 20,
        Selection::All,
        PlayerPolicy::default(),
    )?;
    let mut output = None;
    if let Some(timestamp) = seek {
        loop {
            let progress = player.read_output(&mut output, &mut work(), &cancel)?;
            if output
                .take()
                .is_some_and(|item| matches!(item.event(), RawEvent::Mp4(Mp4Event::Packet(_))))
            {
                break;
            }
            match progress {
                Progress::Ended => return Err("empty stream before seek".into()),
                Progress::Backpressure { .. } => {
                    return Err("range playback ran out of space".into());
                }
                _ => {}
            }
        }
        let mut job = player.prepare_seek(Mp4NativeSeekRequest {
            video,
            timestamp,
            limits: SeekLimits::default(),
        })?;
        while !job.poll(&mut work(), &cancel)? {}
        let mut command = job.finish()?;
        while !player.apply_seek(&mut command, &mut work(), &cancel)? {}
    }
    let mut result = Work::default();
    loop {
        let progress = player.read_output(&mut output, &mut work(), &cancel)?;
        if let Some(item) = output.take() {
            match item.event() {
                RawEvent::Mp4(Mp4Event::Packet(packet)) => result.packet(packet)?,
                RawEvent::Mp4(Mp4Event::Configuration(_)) | RawEvent::Seek(_) => {}
                _ => return Err("unexpected range playback output".into()),
            }
        }
        match progress {
            Progress::Ended => return Ok(result.totals()),
            Progress::Backpressure { .. } => return Err("range playback ran out of space".into()),
            _ => {}
        }
    }
}

/// `mp4-range-fold FILE [SEEK]`, `mp4-range-bench FILE ITERATIONS [SEEK]` or
/// `mp4-range-memory FILE [SEEK]`; SEEK is a backward seek target in video ticks.
pub fn run(args: &[String]) -> Result<()> {
    let invocation = Invocation::parse(args, PREFIX, " [SEEK]", 0..=1, false)?;
    let seek = invocation
        .arguments
        .first()
        .map(|timestamp| timestamp.parse())
        .transpose()?;
    let pool = Budget::new(2 * usize::try_from(MAX_INPUT)?)?;
    let raw = Bytes::copy_from_slice(&pool, &invocation.input(MAX_INPUT)?)?;
    measure(&invocation, OPERATION, Some(&pool), || {
        session(&pool, &raw, seek)
    })
}
