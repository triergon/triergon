//! Research source-trace adapter, separate from library parsing and all timing.
//! Reads bounded windows through core InputCursor; never buffers the media file.
//! No native reference, codec decoder, output normalization or qualification claim.
#![forbid(unsafe_code)]

use safe_media_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, Time, TimeBase, WorkBudget,
};
use safe_media_isobmff::{
    BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, EditList, FragmentBuilder, FragmentContext,
    FragmentIndex, ImplicitBase, IndexStep, InterleaveStatus, Limits, Mov9CursorStatus,
    Mov9DurationBasis, Mov9EditCursor, Mov9EditLimits, Mov9PacketView, Mov9StreamView, Mov9View,
    MovieHeader, PhysicalInterleaveLimits, PhysicalSampleCursor, Sample, SampleReader, SampleStep,
    SampleTable, SampleTableBuilder, SegmentIndex, SourceTrack, TrackHeader, TrackSampleCursor,
    Trex,
};
use std::{
    env,
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
    sync::atomic::AtomicBool,
};

#[derive(Debug)]
enum Failure {
    Core(Error),
    Io(io::Error),
    SourceIo(io::Error),
}
impl From<Error> for Failure {
    fn from(value: Error) -> Self {
        Self::Core(value)
    }
}
impl From<io::Error> for Failure {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
type Result<T> = std::result::Result<T, Failure>;

fn append<T>(target: &mut BudgetVec<T>, value: T) -> Result<()> {
    target.try_push(value).map_err(|e| Failure::Core(e.error))
}
fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).ok_or(Error::Overflow)?;
    let b = bytes.get(offset..end).ok_or(Error::Truncated)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
fn children(raw: &BoxView, skip: usize, budget: &Budget) -> Result<BudgetVec<BoxView>> {
    let mut cursor = raw.children(skip, Limits::default())?;
    let mut out = BudgetVec::new(budget);
    loop {
        match cursor.poll(&mut WorkBudget::new(4096))? {
            BoxStep::Box(raw) => append(&mut out, raw)?,
            BoxStep::Yield => {}
            BoxStep::End => return Ok(out),
        }
    }
}
fn optional(raw: &BoxView, kind: &[u8; 4], budget: &Budget) -> Result<Option<BoxView>> {
    let mut found = None;
    for child in children(raw, 0, budget)?.as_slice() {
        if child.header().kind == *kind {
            if found.is_some() {
                return Err(Error::Invalid("duplicate required child").into());
            }
            found = Some(child.clone());
        }
    }
    Ok(found)
}
fn child(raw: &BoxView, kind: &[u8; 4], budget: &Budget) -> Result<BoxView> {
    optional(raw, kind, budget)?.ok_or_else(|| Error::Invalid("missing required child").into())
}
fn track_header(track: &BoxView, budget: &Budget) -> Result<(TrackHeader, BoxView)> {
    let mdia = child(track, b"mdia", budget)?;
    let header = TrackHeader::parse(
        &child(track, b"tkhd", budget)?,
        &child(&mdia, b"mdhd", budget)?,
        &child(&mdia, b"hdlr", budget)?,
    )?;
    Ok((header, mdia))
}
fn self_contained_table(mdia: &BoxView, budget: &Budget, quantum: u64) -> Result<SampleTable> {
    let minf = child(mdia, b"minf", budget)?;
    let dref = child(&child(&minf, b"dinf", budget)?, b"dref", budget)?;
    if word(dref.payload(), 0)? != 0 {
        return Err(Error::Unsupported("data reference version/flags").into());
    }
    let refs = children(&dref, 8, budget)?;
    if refs.len() != usize::try_from(word(dref.payload(), 4)?).map_err(|_| Error::Overflow)? {
        return Err(Error::Invalid("data reference count").into());
    }
    let mut builder = SampleTableBuilder::new(
        child(&minf, b"stbl", budget)?,
        budget.clone(),
        Limits::default(),
    )?;
    while builder.poll(&mut WorkBudget::new(quantum))? != BuildStatus::Ready {}
    let table = builder.finish()?;
    for description in table.descriptions() {
        let reference = usize::from(description.data_reference_index)
            .checked_sub(1)
            .ok_or(Error::Invalid("zero data ref"))?;
        let target = refs
            .as_slice()
            .get(reference)
            .ok_or(Error::Invalid("dangling data reference"))?;
        if target.header().kind != *b"url " || target.payload() != [0, 0, 0, 1] {
            return Err(Error::Unsupported(
                "external/unknown data reference; no file or URL opened",
            )
            .into());
        }
    }
    Ok(table)
}
fn hex<W: Write>(out: &mut W, bytes: &[u8]) -> io::Result<()> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for &byte in bytes {
        out.write_all(&[DIGITS[(byte >> 4) as usize], DIGITS[(byte & 15) as usize]])?;
    }
    Ok(())
}
fn time<W: Write>(out: &mut W, value: Option<Time>) -> io::Result<()> {
    match value {
        Some(t) => write!(out, "[{},{}]", t.numerator(), t.denominator()),
        None => out.write_all(b"null"),
    }
}
fn integer<W: Write>(out: &mut W, value: Option<i128>) -> io::Result<()> {
    match value {
        Some(v) => write!(out, "{v}"),
        None => out.write_all(b"null"),
    }
}
fn string<W: Write>(out: &mut W, value: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for c in value.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            c if c <= '\u{1f}' => write!(out, "\\u{:04x}", u32::from(c))?,
            c => write!(out, "{c}")?,
        }
    }
    out.write_all(b"\"")
}

struct Driver<W: Write> {
    file: File,
    input: InputCursor,
    budget: Budget,
    out: W,
    window: usize,
    quantum: u64,
    physical_order: bool,
    cancel: AtomicBool,
}

struct Scanned {
    movie: BoxView,
    media: BudgetVec<Range<u64>>,
    fragments: BudgetVec<BoxView>,
}

struct FragmentTrack {
    id: u32,
    descriptions: u32,
    next_ordinal: u64,
    defaults: Trex,
}

struct PhysicalTrack {
    header: TrackHeader,
    table: SampleTable,
}

fn index_fragments(
    movie: &BoxView,
    moofs: &[BoxView],
    budget: &Budget,
    quantum: u64,
) -> Result<BudgetVec<FragmentIndex>> {
    let mut indexes = BudgetVec::new(budget);
    if moofs.is_empty() {
        return Ok(indexes);
    }
    let extends = child(movie, b"mvex", budget)?;
    let mut defaults = BudgetVec::new(budget);
    for raw in children(&extends, 0, budget)?.as_slice() {
        if raw.header().kind != *b"trex" {
            continue;
        }
        let trex = Trex::parse(raw)?;
        if defaults
            .as_slice()
            .iter()
            .any(|previous: &Trex| previous.track_id == trex.track_id)
        {
            return Err(Error::Invalid("duplicate trex for track").into());
        }
        append(&mut defaults, trex)?;
    }
    let mut tracks = BudgetVec::new(budget);
    for track in children(movie, 0, budget)?
        .as_slice()
        .iter()
        .filter(|b| b.header().kind == *b"trak")
    {
        let media = child(track, b"mdia", budget)?;
        let header = TrackHeader::parse(
            &child(track, b"tkhd", budget)?,
            &child(&media, b"mdhd", budget)?,
            &child(&media, b"hdlr", budget)?,
        )?;
        if tracks.len() >= 64 {
            return Err(Error::Limit {
                requested: tracks.len() + 1,
                maximum: 64,
            }
            .into());
        }
        if tracks
            .as_slice()
            .iter()
            .any(|previous: &FragmentTrack| previous.id == header.track_id)
        {
            return Err(Error::Invalid("duplicate fragment track ID").into());
        }
        let stbl = child(&child(&media, b"minf", budget)?, b"stbl", budget)?;
        if optional(&stbl, b"stz2", budget)?.is_some() {
            return Err(
                Error::Unsupported("fragment compact regular size table remains G01").into(),
            );
        }
        let descriptions = word(child(&stbl, b"stsd", budget)?.payload(), 4)?;
        if descriptions > Limits::default().max_descriptions {
            return Err(Error::Limit {
                requested: usize::try_from(descriptions).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(Limits::default().max_descriptions)
                    .map_err(|_| Error::Overflow)?,
            }
            .into());
        }
        let regular_count = word(child(&stbl, b"stsz", budget)?.payload(), 8)?;
        let defaults = defaults
            .as_slice()
            .iter()
            .find(|d| d.track_id == header.track_id)
            .copied()
            .ok_or(Error::Invalid("missing trex for track"))?;
        append(
            &mut tracks,
            FragmentTrack {
                id: header.track_id,
                descriptions,
                next_ordinal: u64::from(regular_count) + 1,
                defaults,
            },
        )?;
    }
    for moof in moofs {
        let mfhd = child(moof, b"mfhd", budget)?;
        if mfhd.payload().len() != 8 || word(mfhd.payload(), 0)? != 0 {
            return Err(Error::Unsupported("mfhd version/flags/layout").into());
        }
        let mut previous: Option<safe_media_core::Result<u64>> = None;
        for traf in children(moof, 0, budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"traf")
        {
            let tfhd = child(traf, b"tfhd", budget)?;
            let id = word(tfhd.payload(), 4)?;
            let state = tracks
                .as_mut_slice()
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or(Error::Invalid("unknown fragment track"))?;
            let implicit_base = if word(tfhd.payload(), 0)? & 1 != 0 {
                ImplicitBase::FirstTraf // Unused by an explicit absolute base.
            } else {
                match previous {
                    None => ImplicitBase::FirstTraf,
                    Some(Ok(end)) => ImplicitBase::PreviousTraf(end),
                    Some(Err(error)) => return Err(error.into()),
                }
            };
            let context = FragmentContext {
                moof_start: moof.header().start,
                implicit_base,
                decode_origin: None,
                first_ordinal: state.next_ordinal,
                description_count: state.descriptions,
            };
            let mut builder = FragmentBuilder::new(
                traf.clone(),
                state.defaults,
                context,
                budget.clone(),
                Limits::default(),
            )?;
            while builder.poll(&mut WorkBudget::new(quantum))? != BuildStatus::Ready {}
            let index = builder.finish()?;
            state.next_ordinal = state
                .next_ordinal
                .checked_add(index.sample_count())
                .ok_or(Error::Overflow)?;
            previous = Some(index.implicit_data_end());
            append(&mut indexes, index)?;
        }
    }
    Ok(indexes)
}
impl<W: Write> Driver<W> {
    fn supply(&mut self, request: InputRequest) -> Result<()> {
        let mut scratch = [0u8; 4096];
        let limit = request.maximum.min(self.window).min(scratch.len());
        let observed = self
            .file
            .seek(SeekFrom::Start(request.offset))
            .and_then(|_| self.file.read(&mut scratch[..limit]));
        let count = match observed {
            Ok(count) => count,
            Err(error) => {
                self.input
                    .fail(request.source, request.token, SourceFailure::Transport)?;
                return Err(Failure::SourceIo(error));
            }
        };
        if count == 0 {
            self.input.finish(request.source, request.token)?;
        } else {
            let bytes = Bytes::copy_from_slice(&self.budget, &scratch[..count])?;
            self.input.provide(&InputWindow::new(
                request.source,
                request.token,
                request.offset,
                bytes,
            )?)?;
        }
        Ok(())
    }
    fn payload(&mut self, sample: Sample, extent: Range<u64>) -> Result<Bytes> {
        let mut reader = SampleReader::new(&mut self.input, sample, extent, 16 * 1024 * 1024)?;
        loop {
            match reader.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            )? {
                Step::Ready(packet) => return Ok(packet.payload),
                Step::NeedInput(request) => self.supply(request)?,
                Step::Yield => {}
                Step::End(_) => {
                    return Err(Error::Invalid("sample reader returned source EOF").into());
                }
            }
        }
    }
    fn native_packet(
        &mut self,
        track_id: u32,
        packet: &Mov9PacketView,
        payload: &[u8],
        occurrence: Option<(u64, u32, Mov9DurationBasis)>,
    ) -> Result<()> {
        write!(
            self.out,
            "{{\"event\":\"native_packet\",\"track_id\":{},\"source_sample\":{},\"description\":{},\"position\":{},\"pts\":",
            track_id, packet.source_ordinal, packet.source_description, packet.source_range.start
        )?;
        integer(&mut self.out, packet.pts.map(i128::from))?;
        self.out.write_all(b",\"dts\":")?;
        integer(&mut self.out, packet.dts.map(i128::from))?;
        write!(
            self.out,
            ",\"duration\":{},\"flags\":{},\"side_data\":[],\"payload_hex\":\"",
            packet.duration, packet.flags
        )?;
        hex(&mut self.out, payload)?;
        self.out.write_all(b"\"")?;
        if let Some((sequence, edit, basis)) = occurrence {
            let basis = match basis {
                Mov9DurationBasis::FollowingOccurrenceDts => "following-occurrence-dts",
                Mov9DurationBasis::SourceDeltaAtBackwardJump => "source-delta-at-backward-jump",
                Mov9DurationBasis::SourceDeltaAtTerminal => "source-delta-at-terminal",
                Mov9DurationBasis::ExpandedTerminalEndObservation => {
                    "expanded-terminal-end-observation"
                }
            };
            write!(
                self.out,
                ",\"occurrence\":{sequence},\"edit\":{edit},\"duration_basis\":\"{basis}\""
            )?;
        }
        writeln!(self.out, "}}")?;
        Ok(())
    }
    fn edited_native(
        &mut self,
        header: &TrackHeader,
        table: &SampleTable,
        edits: &EditList,
        media: &[Range<u64>],
        movie_base: TimeBase,
    ) -> Result<()> {
        let mut cursor = Mov9EditCursor::new(
            table,
            edits,
            header.time_base()?,
            movie_base,
            Mov9EditLimits::default(),
        )?;
        loop {
            self.input
                .read_into(&mut [], &mut WorkBudget::new(0), &self.cancel)?;
            match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                Mov9CursorStatus::Yield => {}
                Mov9CursorStatus::End => return Ok(()),
                Mov9CursorStatus::Ready => {
                    let occurrence = cursor.take_occurrence()?;
                    let extent = media
                        .iter()
                        .find(|r| occurrence.source.require_within(r.start..r.end).is_ok())
                        .cloned()
                        .ok_or(Error::Invalid("edited source sample outside mdat"))?;
                    let payload = self.payload(occurrence.source, extent)?;
                    self.native_packet(
                        header.track_id,
                        &occurrence.native,
                        payload.as_ref(),
                        Some((
                            occurrence.sequence,
                            occurrence.edit_ordinal,
                            occurrence.duration_basis,
                        )),
                    )?;
                }
            }
        }
    }
    fn scan(&mut self) -> Result<Scanned> {
        let mut reader = BoxReader::new(&self.input, 16 * 1024 * 1024)?;
        let mut movie = None;
        let mut media = BudgetVec::new(&self.budget);
        let mut fragments = BudgetVec::new(&self.budget);
        loop {
            match reader.poll(
                &mut self.input,
                &mut WorkBudget::new(self.quantum),
                &self.cancel,
            )? {
                Step::NeedInput(request) => self.supply(request)?,
                Step::Yield => {}
                Step::End(_) => break,
                Step::Ready(BoxEvent::Header(header)) => {
                    write!(
                        self.out,
                        "{{\"event\":\"header\",\"offset\":{},\"fourcc_hex\":\"",
                        header.start
                    )?;
                    hex(&mut self.out, &header.kind)?;
                    match header.size {
                        Some(size) => writeln!(self.out, "\",\"size\":{size}}}")?,
                        None => writeln!(self.out, "\",\"size\":null}}")?,
                    }
                    if header.kind == *b"mdat" {
                        reader.skip(&mut self.input)?;
                    } else {
                        reader.read_metadata(&self.input)?;
                    }
                }
                Step::Ready(BoxEvent::Skipped(header)) => {
                    append(&mut media, header.payload_range()?)?;
                }
                Step::Ready(BoxEvent::Metadata(raw)) => {
                    write!(
                        self.out,
                        "{{\"event\":\"raw_metadata\",\"offset\":{},\"raw_hex\":\"",
                        raw.header().start
                    )?;
                    hex(&mut self.out, raw.bytes().as_ref())?;
                    writeln!(self.out, "\"}}")?;
                    if raw.header().kind == *b"sidx" {
                        let index = SegmentIndex::parse(raw.clone(), Limits::default())?;
                        let mut cursor = index.cursor();
                        loop {
                            let reference = match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                                IndexStep::Reference(reference) => reference,
                                IndexStep::Yield => continue,
                                IndexStep::End => break,
                            };
                            write!(
                                self.out,
                                "{{\"event\":\"sidx_reference\",\"index_offset\":{},\"reference_id\":{},\"reference\":{},\"references_index\":{},\"offset\":{},\"size\":{},\"earliest_presentation\":",
                                raw.header().start,
                                index.reference_id(),
                                reference.ordinal,
                                reference.references_index,
                                reference.range.start,
                                reference.range.end - reference.range.start
                            )?;
                            time(&mut self.out, Some(reference.earliest_presentation))?;
                            self.out.write_all(b",\"presentation_duration\":")?;
                            time(&mut self.out, Some(reference.presentation_duration))?;
                            writeln!(
                                self.out,
                                ",\"starts_with_sap\":{},\"sap_type\":{},\"sap_delta_ticks\":{},\"raw_size_word\":{},\"raw_sap_word\":{}}}",
                                reference.starts_with_sap,
                                reference.sap_type,
                                reference.sap_delta_ticks,
                                reference.raw_size_word,
                                reference.raw_sap_word
                            )?;
                        }
                    }
                    if raw.header().kind == *b"moov" {
                        if movie.is_some() {
                            return Err(Error::Invalid("duplicate movie").into());
                        }
                        movie = Some(raw);
                    } else if raw.header().kind == *b"moof" {
                        if fragments.len() >= 65536 {
                            return Err(Error::Limit {
                                requested: fragments.len() + 1,
                                maximum: 65536,
                            }
                            .into());
                        }
                        append(&mut fragments, raw)?;
                    }
                }
            }
        }
        Ok(Scanned {
            movie: movie.ok_or(Error::Invalid("missing movie"))?,
            media,
            fragments,
        })
    }
    fn trace_physical(&mut self, scanned: Scanned) -> Result<()> {
        let Scanned {
            movie,
            media,
            fragments,
        } = scanned;
        let budget = self.budget.clone();
        MovieHeader::parse(&child(&movie, b"mvhd", &budget)?)?;
        if optional(&movie, b"mvex", &budget)?.is_none() && !fragments.is_empty() {
            return Err(Error::Invalid("fragment without movie extends").into());
        }
        let indexes = index_fragments(&movie, fragments.as_slice(), &budget, self.quantum)?;
        let mut tracks = BudgetVec::new(&budget);
        for track in children(&movie, 0, &budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"trak")
        {
            if tracks.len() >= 64 {
                return Err(Error::Limit {
                    requested: tracks.len() + 1,
                    maximum: 64,
                }
                .into());
            }
            let (header, mdia) = track_header(track, &budget)?;
            let table = self_contained_table(&mdia, &budget, self.quantum)?;
            append(&mut tracks, PhysicalTrack { header, table })?;
        }
        let mut sources = BudgetVec::new(&budget);
        for track in tracks.as_slice() {
            append(
                &mut sources,
                SourceTrack::new(
                    track.header.track_id,
                    Some(&track.table),
                    indexes.as_slice(),
                )?,
            )?;
        }
        let mut cursor = PhysicalSampleCursor::new_with_media_extents(
            &budget,
            sources.as_slice(),
            media.as_slice(),
            PhysicalInterleaveLimits::default(),
        )?;
        let mut samples = 0u64;
        loop {
            match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                InterleaveStatus::Yield => {}
                InterleaveStatus::End => break,
                InterleaveStatus::Ready => {
                    let next = cursor.take_sample()?;
                    let extent = media
                        .as_slice()
                        .iter()
                        .find(|r| next.sample.require_within(r.start..r.end).is_ok())
                        .cloned()
                        .ok_or(Error::Invalid("physical source sample outside mdat"))?;
                    let track = tracks
                        .as_slice()
                        .iter()
                        .find(|t| t.header.track_id == next.track_id)
                        .ok_or(Error::Invalid("missing physical source track"))?;
                    let times = next.sample.source_times(track.header.time_base()?)?;
                    let sample = &next.sample;
                    let payload = self.payload(sample.clone(), extent)?;
                    write!(
                        self.out,
                        "{{\"event\":\"physical_sample\",\"track_id\":{},\"sample\":{},\"description\":{},\"offset\":{},\"size\":{},\"timescale\":{},\"duration_ticks\":{},\"source\":{},\"generation\":{},\"request\":{},\"dts_ticks\":",
                        next.track_id,
                        sample.ordinal,
                        sample.description_index,
                        sample.range.start,
                        sample.range.end - sample.range.start,
                        track.header.timescale,
                        sample.duration_ticks,
                        self.input.source().0,
                        self.input.generation(),
                        self.input.token().request(),
                    )?;
                    integer(&mut self.out, sample.dts_ticks)?;
                    self.out.write_all(b",\"cts_ticks\":")?;
                    integer(&mut self.out, sample.cts_ticks)?;
                    self.out.write_all(b",\"dts_seconds\":")?;
                    time(&mut self.out, times.dts)?;
                    self.out.write_all(b",\"cts_seconds\":")?;
                    time(&mut self.out, times.cts)?;
                    self.out.write_all(b",\"decode_delta_seconds\":")?;
                    time(&mut self.out, Some(times.decode_delta))?;
                    write!(
                        self.out,
                        ",\"sync\":{},\"partial_sync\":{},\"payload_hex\":\"",
                        sample.is_container_sync(),
                        sample.partial_sync
                    )?;
                    hex(&mut self.out, payload.as_ref())?;
                    writeln!(self.out, "\"}}")?;
                    samples = samples.checked_add(1).ok_or(Error::Overflow)?;
                }
            }
        }
        writeln!(
            self.out,
            "{{\"event\":\"physical_end\",\"samples\":{samples},\"validated_samples\":{},\"native_operation\":\"not-attempted\",\"qualification\":false}}",
            cursor.validated_samples()
        )?;
        Ok(())
    }
    fn trace(&mut self, scanned: Scanned) -> Result<()> {
        if self.physical_order {
            return self.trace_physical(scanned);
        }
        let Scanned {
            movie,
            media,
            fragments,
        } = scanned;
        let media = media.as_slice();
        let budget = self.budget.clone();
        let movie_header = MovieHeader::parse(&child(&movie, b"mvhd", &budget)?)?;
        let extends = optional(&movie, b"mvex", &budget)?;
        if extends.is_none() && !fragments.is_empty() {
            return Err(Error::Invalid("fragment without movie extends").into());
        }
        let fragment_indexes =
            index_fragments(&movie, fragments.as_slice(), &budget, self.quantum)?;
        let mut track_ids = BudgetVec::new(&budget);
        let mut sample_total = 0u64;
        let mut native_tracks_complete = 0usize;
        for track in children(&movie, 0, &budget)?
            .as_slice()
            .iter()
            .filter(|b| b.header().kind == *b"trak")
        {
            let (header, mdia) = track_header(track, &budget)?;
            if track_ids.as_slice().contains(&header.track_id) {
                return Err(Error::Invalid("duplicate track ID").into());
            }
            if track_ids.len() >= 64 {
                return Err(Error::Limit {
                    requested: track_ids.len() + 1,
                    maximum: 64,
                }
                .into());
            }
            append(&mut track_ids, header.track_id)?;
            let table = self_contained_table(&mdia, &budget, self.quantum)?;
            let mut track_samples = table.sample_count();
            let mut has_track_fragments = false;
            for fragment in fragment_indexes
                .as_slice()
                .iter()
                .filter(|f| f.track_id() == header.track_id)
            {
                has_track_fragments = true;
                track_samples = track_samples
                    .checked_add(fragment.sample_count())
                    .ok_or(Error::Overflow)?;
            }
            writeln!(
                self.out,
                "{{\"event\":\"track\",\"track_id\":{},\"timescale\":{},\"movie_timescale\":{},\"samples\":{}}}",
                header.track_id, header.timescale, movie_header.timescale, track_samples
            )?;
            for description in table.descriptions() {
                write!(
                    self.out,
                    "{{\"event\":\"description\",\"track_id\":{},\"description\":{},\"data_reference\":{},\"raw_hex\":\"",
                    header.track_id, description.index, description.data_reference_index
                )?;
                hex(&mut self.out, description.raw().bytes().as_ref())?;
                writeln!(self.out, "\"}}")?;
            }
            let has_edits = optional(track, b"edts", &budget)?;
            let edits = has_edits
                .as_ref()
                .map(|edts| {
                    child(edts, b"elst", &budget).and_then(|raw| {
                        EditList::parse(raw, Limits::default()).map_err(Failure::Core)
                    })
                })
                .transpose()?;
            if let Some(edits) = &edits {
                for index in 0..edits.len() {
                    let edit = edits.entry(index)?;
                    let rate_bits =
                        (u32::from(u16::from_be_bytes(edit.rate_integer.to_be_bytes())) << 16)
                            | u32::from(u16::from_be_bytes(edit.rate_fraction.to_be_bytes()));
                    writeln!(
                        self.out,
                        "{{\"event\":\"edit\",\"track_id\":{},\"edit\":{},\"segment_duration\":{},\"media_time\":{},\"rate_bits\":{}}}",
                        header.track_id,
                        index + 1,
                        edit.segment_duration,
                        edit.media_time,
                        rate_bits
                    )?;
                }
            }
            let base = header.time_base()?;
            let mut selected = None;
            let mut epoch = 0u64;
            let native = {
                match Mov9StreamView::for_sample_table(&header, &table, has_track_fragments) {
                    Ok(stream) => {
                        write!(
                            self.out,
                            "{{\"event\":\"native_stream\",\"track_id\":{},\"codec\":\"{}\",\"type\":{},\"time_base\":[{},{}],\"extradata_hex\":\"",
                            stream.track_id,
                            stream.codec,
                            stream.media_type,
                            stream.time_base.numerator(),
                            stream.time_base.denominator()
                        )?;
                        hex(&mut self.out, stream.extradata.as_ref())?;
                        self.out.write_all(b"\",\"language_hex\":\"")?;
                        hex(&mut self.out, &stream.language)?;
                        self.out.write_all(b"\",\"handler_name_hex\":\"")?;
                        hex(&mut self.out, stream.handler_name.as_ref())?;
                        writeln!(
                            self.out,
                            "\",\"zero_vendor\":{}}}",
                            stream.vendor_id == Some([0; 4])
                        )?;
                        Some(Mov9View::new(
                            base,
                            if !has_track_fragments {
                                table.sync_entry_count()
                            } else {
                                None
                            },
                        ))
                    }
                    _ => None,
                }
            };
            if native.is_none() {
                writeln!(
                    self.out,
                    "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"unsupported\",\"reason\":\"native catalogue profile unresolved\",\"qualification\":false}}",
                    header.track_id
                )?;
            }
            let mut native_valid = native.is_some();
            let mut cursor = TrackSampleCursor::new(SourceTrack::new(
                header.track_id,
                Some(&table),
                fragment_indexes.as_slice(),
            )?);
            loop {
                let sample = match cursor.poll(&mut WorkBudget::new(self.quantum))? {
                    SampleStep::Sample(s) => s,
                    SampleStep::Yield => continue,
                    SampleStep::End => break,
                };
                let extent = media
                    .iter()
                    .find(|r| sample.require_within(r.start..r.end).is_ok())
                    .cloned()
                    .ok_or(Error::Invalid("sample outside self-contained mdat"))?;
                if selected != Some(sample.description_index) {
                    epoch = epoch.checked_add(1).ok_or(Error::Overflow)?;
                    selected = Some(sample.description_index);
                    writeln!(
                        self.out,
                        "{{\"event\":\"configuration\",\"track_id\":{},\"before_sample\":{},\"description\":{},\"epoch\":{}}}",
                        header.track_id, sample.ordinal, sample.description_index, epoch
                    )?;
                }
                let payload = self.payload(sample.clone(), extent)?;
                let times = sample.source_times(base)?;
                write!(
                    self.out,
                    "{{\"event\":\"sample\",\"track_id\":{},\"sample\":{},\"description\":{},\"configuration_epoch\":{},\"offset\":{},\"size\":{},\"timescale\":{},\"duration_ticks\":{},\"source\":{},\"generation\":{},\"request\":{},\"dts_ticks\":",
                    header.track_id,
                    sample.ordinal,
                    sample.description_index,
                    epoch,
                    sample.range.start,
                    sample.range.end - sample.range.start,
                    header.timescale,
                    sample.duration_ticks,
                    self.input.source().0,
                    self.input.generation(),
                    self.input.token().request()
                )?;
                integer(&mut self.out, sample.dts_ticks)?;
                self.out.write_all(b",\"cts_ticks\":")?;
                integer(&mut self.out, sample.cts_ticks)?;
                self.out.write_all(b",\"dts_seconds\":")?;
                time(&mut self.out, times.dts)?;
                self.out.write_all(b",\"cts_seconds\":")?;
                time(&mut self.out, times.cts)?;
                self.out.write_all(b",\"decode_delta_seconds\":")?;
                time(&mut self.out, Some(times.decode_delta))?;
                match sample.composition_offset {
                    Some(offset) => {
                        write!(
                            self.out,
                            ",\"composition_offset\":{{\"version\":{},\"raw_bits\":{},\"ticks\":{},\"signed_value\":",
                            offset.version(),
                            offset.raw_bits(),
                            offset.ticks()
                        )?;
                        integer(&mut self.out, offset.signed_value().map(i128::from))?;
                        self.out.write_all(b"}")?;
                    }
                    None => self.out.write_all(b",\"composition_offset\":null")?,
                }
                self.out.write_all(b",\"dependency\":")?;
                integer(&mut self.out, sample.dependency.map(i128::from))?;
                self.out.write_all(b",\"fragment_flags\":")?;
                integer(&mut self.out, sample.fragment_flags.map(i128::from))?;
                write!(
                    self.out,
                    ",\"sync\":{},\"partial_sync\":{},\"payload_hex\":\"",
                    sample.is_container_sync(),
                    sample.partial_sync
                )?;
                hex(&mut self.out, payload.as_ref())?;
                writeln!(self.out, "\"}}")?;
                if let Some(native) = native.as_ref().filter(|_| edits.is_none()) {
                    match native.project(&sample, None) {
                        Ok(packet) => {
                            self.native_packet(header.track_id, &packet, payload.as_ref(), None)?;
                        }
                        Err(error) => {
                            native_valid = false;
                            write!(
                                self.out,
                                "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"unsupported\",\"reason\":",
                                header.track_id
                            )?;
                            string(&mut self.out, &error.to_string())?;
                            writeln!(self.out, ",\"qualification\":false}}")?;
                        }
                    }
                }
                sample_total = sample_total.checked_add(1).ok_or(Error::Overflow)?;
            }
            if let Some(edits) = edits.as_ref().filter(|_| native_valid) {
                let result = if has_track_fragments {
                    Err(Failure::Core(Error::Unsupported(
                        "native edited fragment occurrence profile",
                    )))
                } else {
                    self.edited_native(&header, &table, edits, media, movie_header.time_base()?)
                };
                match result {
                    Ok(()) => {}
                    Err(Failure::Core(Error::Unsupported(reason))) => {
                        native_valid = false;
                        write!(
                            self.out,
                            "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"unsupported\",\"reason\":",
                            header.track_id
                        )?;
                        string(&mut self.out, reason)?;
                        writeln!(self.out, ",\"qualification\":false}}")?;
                    }
                    Err(error) => return Err(error),
                }
            }
            if native_valid {
                native_tracks_complete += 1;
                writeln!(
                    self.out,
                    "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"complete\",\"qualification\":false}}",
                    header.track_id
                )?;
            }
        }
        if native_tracks_complete == track_ids.len() {
            writeln!(
                self.out,
                "{{\"event\":\"native_end\",\"confirmed_eof\":true,\"programs\":[],\"qualification\":false}}"
            )?;
        }
        writeln!(
            self.out,
            "{{\"event\":\"status\",\"status\":\"complete\",\"scope\":\"regular-and-legacy-fragment-source-subset\",\"qualification\":false,\"samples\":{sample_total}}}"
        )?;
        Ok(())
    }
}

fn run<W: Write>(
    path: &Path,
    window: usize,
    quantum: u64,
    physical_order: bool,
    out: &mut W,
) -> Result<()> {
    if window == 0 || window > 4096 || quantum == 0 {
        return Err(Error::Invalid("window/quantum policy").into());
    }
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let budget = Budget::new(64 * 1024 * 1024)?;
    let input = InputCursor::new(
        &budget,
        SourceId(1),
        SourceCapabilities {
            length: Some(length),
            random_access: true,
        },
        window,
    )?;
    let schema = if physical_order {
        "safe-media.m03.physical-source-trace.v1"
    } else {
        "safe-media.m03.source-trace.v2"
    };
    writeln!(
        out,
        "{{\"event\":\"begin\",\"schema\":\"{schema}\",\"qualification\":false,\"source_length\":{length},\"window\":{window},\"quantum\":{quantum}}}"
    )?;
    let mut driver = Driver {
        file,
        input,
        budget,
        out,
        window,
        quantum,
        physical_order,
        cancel: AtomicBool::new(false),
    };
    let scanned = driver.scan()?;
    driver.trace(scanned)
}

fn main() {
    let args: Vec<_> = env::args_os().collect();
    if args.len() < 2 || args.len() > 5 {
        eprintln!("usage: isobmff_source_trace INPUT [WINDOW<=4096] [WORK_QUANTUM] [physical]");
        std::process::exit(2);
    }
    let physical_order = match args.get(4) {
        None => false,
        Some(mode) if mode == "physical" => true,
        _ => {
            eprintln!("unknown operation; the optional fourth argument is physical");
            std::process::exit(2);
        }
    };
    let parse = |i: usize, default: u64| {
        args.get(i)
            .map(|v| v.to_string_lossy().parse::<u64>())
            .transpose()
            .map(|v| v.unwrap_or(default))
    };
    let parameters = parse(2, 4096).and_then(|w| parse(3, 4096).map(|q| (w, q)));
    let (window, quantum) = match parameters {
        Ok(v) => v,
        Err(_) => {
            eprintln!("invalid numeric argument");
            std::process::exit(2);
        }
    };
    let mut out = BufWriter::new(io::stdout().lock());
    let result = usize::try_from(window)
        .map_err(|_| Failure::Core(Error::Overflow))
        .and_then(|w| run(Path::new(&args[1]), w, quantum, physical_order, &mut out));
    if let Err(error) = result {
        let (status, message) = match error {
            Failure::Core(e) => (
                match e {
                    Error::Unsupported(_) => "unsupported",
                    Error::Truncated => "truncated",
                    Error::Overflow => "overflow",
                    Error::Backpressure { .. } => "backpressure",
                    Error::Limit { .. } => "limit",
                    Error::Cancelled => "cancelled",
                    Error::Transport => "transport",
                    Error::Allocation => "allocation",
                    Error::ForeignPool
                    | Error::SourceMismatch
                    | Error::StaleGeneration { .. }
                    | Error::StaleRequest { .. } => "state-error",
                    _ => "invalid",
                },
                e.to_string(),
            ),
            Failure::Io(e) => ("io-error", e.to_string()),
            Failure::SourceIo(e) => ("transport", e.to_string()),
        };
        let _ = write!(
            out,
            "{{\"event\":\"status\",\"status\":\"{status}\",\"qualification\":false,\"message\":"
        );
        let _ = string(&mut out, &message);
        let _ = writeln!(out, "}}");
        let _ = out.flush();
        std::process::exit(2);
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::{Error, Failure, run};
    use std::path::Path;

    #[test]
    fn physical_outside_mdat_is_invalid_before_offset_order_profile_rejection() {
        // Original fixture and immutable authored bytes are retained. Parent v4
        // observed Unsupported before any packet, while the default operation
        // returned Invalid for the same outside-MDAT address. This regression
        // must fail on that source before a production behavior change.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bad-chunk-offset-outside-source.mov");
        for (window, quantum) in [(4096, 4096), (1, 3)] {
            let mut output = Vec::new();
            let result = run(&fixture, window, quantum, true, &mut output);
            assert!(
                matches!(&result, Err(Failure::Core(Error::Invalid(_)))),
                "outside-MDAT sample must be invalid before physical layout policy; window={window}, quantum={quantum}, got {result:?}"
            );
            let trace = String::from_utf8(output).unwrap();
            assert!(!trace.contains("\"event\":\"physical_sample\""));
            assert!(!trace.contains("\"event\":\"physical_end\""));
        }
    }
}
