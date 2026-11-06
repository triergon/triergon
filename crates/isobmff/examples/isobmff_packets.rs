//! Print the tracks and then the samples of an MP4 or MOV file.
//!
//! ```text
//! cargo run -p triergon-isobmff --example isobmff_packets -- FILE
//! ```
//!
//! The file is read through a core `InputCursor`: only `moov` is retained, each
//! track's sample table is indexed, and every sample is read from its `mdat` in
//! file order. Timestamps are ticks of the track timescale. Samples in movie
//! fragments (`moof`) need `FragmentBuilder` as well and are not listed here.

use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom},
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, Bytes, InputCursor, InputRequest, InputWindow, SourceCapabilities, SourceId, Step,
    WorkBudget,
};
use triergon_isobmff::{
    BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, InterleaveStatus, Limits,
    PhysicalInterleaveLimits, PhysicalSampleCursor, SampleReader, SampleTableBuilder, SourceTrack,
    TrackHeader,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Work units per poll; every parser yields when they are spent.
const WORK: u64 = 1 << 16;

/// Answers the cursor's byte requests from the file.
struct Source {
    file: File,
    pool: Budget,
    input: InputCursor,
    buffer: Vec<u8>,
}

impl Source {
    fn supply(&mut self, request: InputRequest) -> Result<()> {
        self.buffer.resize(request.maximum, 0);
        self.file.seek(SeekFrom::Start(request.offset))?;
        let read = self.file.read(&mut self.buffer)?;
        if read == 0 {
            self.input.finish(request.source, request.token)?;
        } else {
            let bytes = Bytes::copy_from_slice(&self.pool, &self.buffer[..read])?;
            let window = InputWindow::new(request.source, request.token, request.offset, bytes)?;
            self.input.provide_owned(window)?;
        }
        Ok(())
    }
}

fn children(parent: &BoxView) -> Result<Vec<BoxView>> {
    let mut walk = parent.children(0, Limits::default())?;
    let mut out = Vec::new();
    loop {
        match walk.poll(&mut WorkBudget::new(WORK))? {
            BoxStep::Box(child) => out.push(child),
            BoxStep::Yield => {}
            BoxStep::End => return Ok(out),
        }
    }
}

fn child(parent: &BoxView, kind: &[u8; 4]) -> Result<BoxView> {
    children(parent)?
        .into_iter()
        .find(|b| b.header().kind == *kind)
        .ok_or_else(|| format!("missing {} box", String::from_utf8_lossy(kind)).into())
}

fn optional(value: Option<i128>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

fn main() -> Result<()> {
    let path = env::args().nth(1).ok_or("usage: isobmff_packets FILE")?;
    let file = File::open(path)?;
    let caps = SourceCapabilities {
        length: Some(file.metadata()?.len()),
        random_access: true,
    };
    let pool = Budget::new(256 << 20)?;
    let input = InputCursor::new(&pool, SourceId(1), caps, 64 * 1024)?;
    let mut source = Source {
        file,
        pool: pool.clone(),
        input,
        buffer: Vec::new(),
    };
    let cancel = AtomicBool::new(false);

    // Top-level boxes: keep `moov`, record the payload extent of every `mdat`.
    let mut boxes = BoxReader::new(&source.input, 16 << 20)?;
    let mut movie = None;
    let mut media = Vec::new();
    loop {
        match boxes.poll(&mut source.input, &mut WorkBudget::new(WORK), &cancel)? {
            Step::NeedInput(request) => source.supply(request)?,
            Step::Yield => {}
            Step::End(_) => break,
            Step::Ready(BoxEvent::Header(header)) if header.kind == *b"moov" => {
                boxes.read_metadata(&source.input)?;
            }
            Step::Ready(BoxEvent::Header(_)) => boxes.skip(&mut source.input)?,
            Step::Ready(BoxEvent::Metadata(moov)) => movie = Some(moov),
            Step::Ready(BoxEvent::Skipped(header)) => {
                if header.kind == *b"mdat" {
                    media.push(header.payload_range()?);
                }
            }
        }
    }
    let movie = movie.ok_or("no moov box")?;

    // One sample table per track.
    let mut tracks = Vec::new();
    for trak in children(&movie)?
        .iter()
        .filter(|b| b.header().kind == *b"trak")
    {
        let mdia = child(trak, b"mdia")?;
        let header = TrackHeader::parse(
            &child(trak, b"tkhd")?,
            &child(&mdia, b"mdhd")?,
            &child(&mdia, b"hdlr")?,
        )?;
        let stbl = child(&child(&mdia, b"minf")?, b"stbl")?;
        let mut builder = SampleTableBuilder::new(stbl, pool.clone(), Limits::default())?;
        while builder.poll(&mut WorkBudget::new(WORK))? == BuildStatus::Yield {}
        let table = builder.finish()?;
        let codec = table.descriptions().first().map_or(*b"none", |d| d.format);
        println!(
            "track {}: {} {} timescale {} samples {}",
            header.track_id,
            String::from_utf8_lossy(&header.handler),
            String::from_utf8_lossy(&codec),
            header.timescale,
            table.sample_count()
        );
        tracks.push((header.track_id, table));
    }

    // Every sample of every track, in file order.
    let sources = tracks
        .iter()
        .map(|(id, table)| SourceTrack::new(*id, Some(table), &[]))
        .collect::<triergon_core::Result<Vec<_>>>()?;
    let mut cursor = PhysicalSampleCursor::new_with_media_extents(
        &pool,
        &sources,
        &media,
        PhysicalInterleaveLimits::default(),
    )?;
    loop {
        match cursor.poll(&mut WorkBudget::new(WORK), &cancel)? {
            InterleaveStatus::Yield => {}
            InterleaveStatus::End => break,
            InterleaveStatus::Ready => {
                let next = cursor.take_sample()?;
                let extent = media
                    .iter()
                    .find(|m| next.sample.require_within((*m).clone()).is_ok())
                    .ok_or("sample outside every mdat")?
                    .clone();
                let mut reader =
                    SampleReader::new(&mut source.input, next.sample, extent, 64 << 20)?;
                let packet = loop {
                    match reader.poll(&mut source.input, &mut WorkBudget::new(WORK), &cancel)? {
                        Step::Ready(packet) => break packet,
                        Step::NeedInput(request) => source.supply(request)?,
                        Step::Yield => {}
                        Step::End(_) => return Err("source ended inside a sample".into()),
                    }
                };
                let sample = &packet.sample;
                println!(
                    "track {} dts {} pts {} size {} key {}",
                    next.track_id,
                    optional(sample.dts_ticks),
                    optional(sample.cts_ticks),
                    packet.payload.len(),
                    sample.is_container_sync()
                );
            }
        }
    }
    Ok(())
}
