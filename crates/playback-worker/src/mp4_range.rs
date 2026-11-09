//! Compare seekable MP4 range playback and dedicated video sample entries.

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
            EncodedEditLimits as EditLimits, EncodedEditPlan,
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
    mp4_open::{Movie, MovieTrack, Mp4Open, OpenLimits, OpenProgress},
    mp4_plan::PlanLimits,
};

pub const PREFIX: &str = "mp4-range-";
const OPERATION: &str = "mp4-range-playback";
const WINDOW: usize = 32 * 1024;
const WORK: u64 = 4096;

#[derive(Default)]
struct Work {
    packets: u64,
    bytes: u64,
    digest: u64,
}

impl Work {
    fn configuration(&mut self, tag: [u8; 4], data: &[u8]) {
        fold(&mut self.digest, u64::from(u32::from_le_bytes(tag)));
        fold(&mut self.digest, data.len() as u64);
        fold_bytes(&mut self.digest, data);
    }

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

pub(crate) fn prepare_edit(
    movie: &Movie,
    track: &MovieTrack,
    input: &mut InputCursor,
    grant: u64,
    cancel: &AtomicBool,
) -> Result<Option<EncodedEditPlan>> {
    let Some(edits) = track.edits() else {
        return Ok(None);
    };
    let table = track.table();
    let [description] = table.descriptions() else {
        return Err("encoded edit requires one sample description".into());
    };
    let header = track.header();
    let mut config = ConfigurationJob::new_range_playback(
        header,
        table,
        input,
        DescriptionSelection {
            major_brand: movie.major_brand(),
            description: description.index,
            limits: ConfigurationLimits::default(),
        },
    )?;
    while config.poll(input, &mut WorkBudget::new(grant), cancel)? != CursorStatus::Ready {}
    let configs = [config.take(input, cancel)?];
    let mut job = EditJob::new_range_playback(
        EditContext {
            table,
            edits,
            stream: &configs[0],
            media_base: header.time_base()?,
            movie_base: movie.header().time_base()?,
        },
        &configs,
        input,
        EditLimits::default(),
    )?;
    while job.poll(input, &mut WorkBudget::new(grant), cancel)? != CursorStatus::Ready {}
    Ok(Some(job.take(input, cancel)?))
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
    let work = || WorkBudget::new(WORK);
    let mut open = Mp4Open::new(selected, OpenLimits::default())?;
    while !matches!(open.advance(&mut work(), &cancel)?, OpenProgress::Ready) {}
    let mut opened = open.finish()?;
    let movie = &opened.movie;
    let mut plans = BudgetVec::new(pool);
    plans.try_reserve_exact(movie.tracks().len())?;
    let mut video = None;
    let mut audio = false;
    let mut result = Work::default();
    for track in movie.tracks() {
        let header = track.header();
        let table = track.table();
        let descriptions = table.descriptions();
        match (header.handler, descriptions) {
            (kind, [entry])
                if kind == *b"vide"
                    && matches!(&entry.format, b"avc1" | b"dvh1" | b"dvhe" | b"dav1")
                    && video.is_none() =>
            {
                video = Some((header.track_id, entry.format));
            }
            (kind, [entry])
                if kind == *b"soun" && matches!(&entry.format, b"mp4a" | b"Opus") && !audio =>
            {
                audio = true;
            }
            _ => return Err("unexpected MP4 range track".into()),
        }
        let codec = match &descriptions[0].format {
            b"dvh1" | b"dvhe" => Some("hevc"),
            b"dav1" => Some("av1"),
            b"Opus" => Some("opus"),
            _ => None,
        };
        let edits = track.edits().filter(|_| track.fragments().is_empty());
        let config = if edits.is_some() || codec.is_some() {
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
            let config = config.take(&mut opened.input, &cancel)?;
            if let Some(codec) = codec {
                let tag = config.source_description().format;
                let stream = config.demux();
                if tag != descriptions[0].format || stream.codec != codec {
                    return Err("incorrect MP4 range configuration".into());
                }
                result.configuration(tag, stream.extradata.as_ref());
            }
            Some(config)
        } else {
            None
        };
        let plan = if let Some(edits) = edits {
            let configs = [config.ok_or("missing MP4 edit configuration")?];
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
    let (video, tag) = video.ok_or("missing MP4 range video track")?;
    if !audio && tag != *b"dav1" {
        return Err("missing MP4 range audio track".into());
    }
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
