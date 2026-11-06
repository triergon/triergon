//! Research source-trace adapter, separate from library parsing and all timing.
//! Reads bounded windows through core InputCursor; never buffers the media file.
//! No native reference, codec decoder or output normalization.
#![forbid(unsafe_code)]

use std::{
    env,
    fs::File,
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
    sync::atomic::AtomicBool,
};
use triergon_core::{
    Budget, BudgetVec, Bytes, Error, InputCursor, InputRequest, InputWindow, SourceCapabilities,
    SourceFailure, SourceId, Step, Time, TimeBase, WorkBudget,
};
use triergon_harness_protocol::write_hex;
use triergon_isobmff::{
    BoxEvent, BoxReader, BoxStep, BoxView, BuildStatus, EditList, FragmentBuilder, FragmentContext,
    FragmentIndex, ImplicitBase, IndexStep, InterleaveStatus, Limits, Mov9ClockOrigin,
    Mov9CursorStatus, Mov9DurationBasis, Mov9PacketCursor, Mov9PacketLimits, Mov9PacketView,
    Mov9Profile, Mov9StreamView, Mov9Track, MovieHeader, PhysicalInterleaveLimits,
    PhysicalSampleCursor, Sample, SampleReader, SampleStep, SampleTable, SampleTableBuilder,
    SegmentIndex, SourceTrack, TrackHeader, TrackSampleCursor, Trex,
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
    major_brand: Option<[u8; 4]>,
}

struct NativeTrack {
    id: u32,
    base: TimeBase,
    table: SampleTable,
    edits: Option<EditList>,
    available: bool,
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
        let descriptions = word(child(&stbl, b"stsd", budget)?.payload(), 4)?;
        if descriptions > Limits::default().max_descriptions {
            return Err(Error::Limit {
                requested: usize::try_from(descriptions).map_err(|_| Error::Overflow)?,
                maximum: usize::try_from(Limits::default().max_descriptions)
                    .map_err(|_| Error::Overflow)?,
            }
            .into());
        }
        // Both selected size layouts carry sample_count at payload offset8.
        // The real SampleTableBuilder below validates their full wire grammar.
        let sizes = match (
            optional(&stbl, b"stsz", budget)?,
            optional(&stbl, b"stz2", budget)?,
        ) {
            (Some(_), Some(_)) => return Err(Error::Invalid("both stsz and stz2").into()),
            (Some(raw), None) | (None, Some(raw)) => raw,
            (None, None) => return Err(Error::Invalid("missing sample size table").into()),
        };
        let regular_count = word(sizes.payload(), 8)?;
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
        let mut previous: Option<triergon_core::Result<u64>> = None;
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
            let implicit_base = if word(tfhd.payload(), 0)? & 0x020001 != 0 {
                // Either explicit base or moof-relative addressing makes the
                // preceding traf's implicit endpoint irrelevant.
                ImplicitBase::FirstTraf
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
    fn native_origin(
        &mut self,
        track_id: u32,
        source: &Sample,
        origin: Mov9ClockOrigin,
    ) -> Result<()> {
        match origin {
            Mov9ClockOrigin::SourceClock => {}
            Mov9ClockOrigin::PredecessorEnd(previous) => {
                writeln!(
                    self.out,
                    "{{\"event\":\"native_origin\",\"kind\":\"predecessor-end\",\"track_id\":{track_id},\"source_sample\":{},\"predecessor_sample\":{},\"predecessor_native_dts_ticks\":{},\"predecessor_duration_ticks\":{},\"predecessor_source_dts_known\":{},\"derived_dts_ticks\":{},\"source_dts_known\":{},\"source_cts_known\":{}}}",
                    source.ordinal,
                    previous.predecessor_sample,
                    previous.predecessor_native_dts_ticks,
                    previous.predecessor_duration_ticks,
                    previous.predecessor_source_dts_known,
                    previous.derived_dts_ticks,
                    source.dts_ticks.is_some(),
                    source.cts_ticks.is_some()
                )?;
            }
            Mov9ClockOrigin::CompleteMediaStart => {
                writeln!(
                    self.out,
                    "{{\"event\":\"native_origin\",\"kind\":\"complete-media-start\",\"track_id\":{track_id},\"source_sample\":{},\"derived_dts_ticks\":0,\"source_dts_known\":{},\"source_cts_known\":{}}}",
                    source.ordinal,
                    source.dts_ticks.is_some(),
                    source.cts_ticks.is_some()
                )?;
            }
            Mov9ClockOrigin::FrontSourceMediaStartV1 => {
                writeln!(
                    self.out,
                    "{{\"event\":\"native_origin\",\"kind\":\"front-source-media-start-v1\",\"track_id\":{track_id},\"source_sample\":{},\"derived_dts_ticks\":0,\"source_dts_known\":{},\"source_cts_known\":{}}}",
                    source.ordinal,
                    source.dts_ticks.is_some(),
                    source.cts_ticks.is_some()
                )?;
            }
        }
        Ok(())
    }
    fn native_packet(
        &mut self,
        track_id: u32,
        packet: &Mov9PacketView,
        payload: &[u8],
        occurrence: Option<(u64, u32, Mov9DurationBasis)>,
    ) -> Result<()> {
        if packet.dts_field.api_ticks().is_none()
            || packet.pts_field.api_ticks().is_none()
            || packet.dts_field.exact_native_ticks != i128::from(packet.dts_field.signed_ticks())
            || packet.pts_field.exact_native_ticks != i128::from(packet.pts_field.signed_ticks())
        {
            writeln!(
                self.out,
                "{{\"event\":\"native_clock_projection\",\"profile\":\"mov9-signed64-null-min\",\"track_id\":{},\"source_sample\":{},\"exact_native_dts_ticks\":{},\"exact_native_pts_ticks\":{},\"native_dts_bits\":{},\"native_pts_bits\":{},\"source_knownness_unchanged\":true}}",
                track_id,
                packet.source_ordinal,
                packet.dts_field.exact_native_ticks,
                packet.pts_field.exact_native_ticks,
                packet.dts_field.native_bits,
                packet.pts_field.native_bits
            )?;
        }
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
        write_hex(&mut self.out, payload)?;
        self.out.write_all(b"\"")?;
        if let Some((sequence, edit, basis)) = occurrence {
            let basis = match basis {
                Mov9DurationBasis::TerminalEmptyEndpointObservation => {
                    "terminal-empty-endpoint-observation"
                }
                Mov9DurationBasis::OriginalDecodeTableOrdinal => "original-decode-table-ordinal",
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
    fn trace_native(
        &mut self,
        records: &[NativeTrack],
        fragments: &[FragmentIndex],
        media: &[Range<u64>],
        movie_base: TimeBase,
    ) -> Result<()> {
        if records.iter().any(|track| !track.available) {
            // Source facts have already been emitted. An incomplete native
            // catalogue cannot establish the selected all-stream operation.
            return Ok(());
        }
        let mut tracks = BudgetVec::new(&self.budget);
        for record in records {
            append(
                &mut tracks,
                Mov9Track {
                    track_id: record.id,
                    table: &record.table,
                    fragments,
                    media_base: record.base,
                    edits: record.edits.as_ref().map(|edits| (edits, movie_base)),
                },
            )?;
        }
        let result: Result<()> = (|| {
            // This adapter has read the complete file's metadata from offset0
            // through confirmed EOF. No isolated segment is passed as complete.
            let mut cursor = Mov9PacketCursor::new_complete_source(
                &self.budget,
                tracks.as_slice(),
                media,
                Mov9PacketLimits::default(),
                Mov9Profile::PinnedNoParseNoFillin,
            )?;
            loop {
                match cursor.poll(&mut WorkBudget::new(self.quantum), &self.cancel)? {
                    Mov9CursorStatus::Yield => {}
                    Mov9CursorStatus::End => return Ok(()),
                    Mov9CursorStatus::Ready => {
                        let occurrence = cursor.take_occurrence()?;
                        let extent = media
                            .iter()
                            .find(|range| {
                                occurrence
                                    .source
                                    .require_within(range.start..range.end)
                                    .is_ok()
                            })
                            .cloned()
                            .ok_or(Error::Invalid("native source sample outside mdat"))?;
                        let payload = self.payload(occurrence.source.clone(), extent)?;
                        self.native_origin(
                            occurrence.track_id,
                            &occurrence.source,
                            occurrence.clock_origin,
                        )?;
                        match occurrence.scheduling_clock {
                            triergon_isobmff::Mov9SchedulingClock::ApiSentinel => {
                                writeln!(
                                    self.out,
                                    "{{\"event\":\"native_scheduling_projection\",\"track_id\":{},\"source_sample\":{},\"cause\":\"api-sentinel\",\"source_knownness_unchanged\":true}}",
                                    occurrence.track_id, occurrence.source.ordinal
                                )?;
                            }
                            triergon_isobmff::Mov9SchedulingClock::RescaleOverflow {
                                exact_microseconds,
                            } => {
                                write!(
                                    self.out,
                                    "{{\"event\":\"native_scheduling_projection\",\"track_id\":{},\"source_sample\":{},\"cause\":\"checked-rescale-overflow\",\"exact_rounded_microseconds\":",
                                    occurrence.track_id, occurrence.source.ordinal
                                )?;
                                integer(&mut self.out, exact_microseconds)?;
                                writeln!(self.out, ",\"source_knownness_unchanged\":true}}")?;
                            }
                            _ => {}
                        }
                        self.native_packet(
                            occurrence.track_id,
                            &occurrence.native,
                            payload.as_ref(),
                            occurrence.edit.map(|edit| {
                                (edit.sequence, edit.edit_ordinal, edit.duration_basis)
                            }),
                        )?;
                    }
                }
            }
        })();
        match result {
            Ok(()) => {
                for record in records {
                    writeln!(
                        self.out,
                        "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"complete\",\"qualification\":false}}",
                        record.id
                    )?;
                }
                writeln!(
                    self.out,
                    "{{\"event\":\"native_end\",\"confirmed_eof\":true,\"programs\":[],\"qualification\":false}}"
                )?;
            }
            Err(Failure::Core(error))
                if matches!(error, Error::Unsupported(_) | Error::Overflow) =>
            {
                for record in records {
                    write!(
                        self.out,
                        "{{\"event\":\"native_status\",\"track_id\":{},\"status\":\"unsupported\",\"reason\":",
                        record.id
                    )?;
                    string(&mut self.out, &error.to_string())?;
                    writeln!(self.out, ",\"qualification\":false}}")?;
                }
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }
    fn scan(&mut self) -> Result<Scanned> {
        let mut reader = BoxReader::new(&self.input, 16 * 1024 * 1024)?;
        let mut movie = None;
        let mut major_brand = None;
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
                    write_hex(&mut self.out, &header.kind)?;
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
                    write_hex(&mut self.out, raw.bytes().as_ref())?;
                    writeln!(self.out, "\"}}")?;
                    if raw.header().kind == *b"ftyp" {
                        let payload = raw.payload();
                        if payload.len() < 8 || payload.len() % 4 != 0 {
                            return Err(Error::Invalid("file type box extent").into());
                        }
                        if major_brand.is_some() {
                            return Err(Error::Invalid("duplicate file type box").into());
                        }
                        major_brand = Some(payload[..4].try_into().map_err(|_| Error::Truncated)?);
                    }
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
            major_brand,
        })
    }
    fn trace_physical(&mut self, scanned: Scanned) -> Result<()> {
        let Scanned {
            movie,
            media,
            fragments,
            ..
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
                    write_hex(&mut self.out, payload.as_ref())?;
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
            major_brand,
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
        let mut native_tracks = BudgetVec::new(&budget);
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
                write_hex(&mut self.out, description.raw().bytes().as_ref())?;
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
                match Mov9StreamView::for_source(&header, &table, has_track_fragments, major_brand)
                {
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
                        write_hex(&mut self.out, stream.extradata.as_ref())?;
                        self.out.write_all(b"\",\"language_hex\":\"")?;
                        write_hex(&mut self.out, &stream.language)?;
                        self.out.write_all(b"\",\"handler_name_hex\":\"")?;
                        write_hex(&mut self.out, stream.handler_name.as_ref())?;
                        writeln!(
                            self.out,
                            "\",\"zero_vendor\":{}}}",
                            stream.vendor_id == Some([0; 4])
                        )?;
                        if stream.clock.cause
                            == triergon_isobmff::Mov9MediaClockCause::ObservedAllOnesScaleWord
                        {
                            writeln!(
                                self.out,
                                "{{\"event\":\"native_timebase_projection\",\"track_id\":{},\"cause\":\"observed-all-ones-scale-word\",\"source_time_base\":[{},{}],\"native_time_base\":[{},{}],\"source_knownness_unchanged\":true}}",
                                stream.track_id,
                                stream.clock.source_base.require()?.numerator(),
                                stream.clock.source_base.require()?.denominator(),
                                stream.clock.native_base.numerator(),
                                stream.clock.native_base.denominator()
                            )?;
                        }
                        Some(())
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
                write_hex(&mut self.out, payload.as_ref())?;
                writeln!(self.out, "\"}}")?;
                sample_total = sample_total.checked_add(1).ok_or(Error::Overflow)?;
            }
            append(
                &mut native_tracks,
                NativeTrack {
                    id: header.track_id,
                    base,
                    table,
                    edits,
                    available: native.is_some(),
                },
            )?;
        }
        self.trace_native(
            native_tracks.as_slice(),
            fragment_indexes.as_slice(),
            media,
            movie_header.time_base()?,
        )?;
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
        "triergon.isobmff.physical-source-trace.v1"
    } else {
        "triergon.isobmff.source-trace.v2"
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
    let Ok((window, quantum)) = parameters else {
        eprintln!("invalid numeric argument");
        std::process::exit(2);
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
    use std::fs;
    use std::path::Path;

    include!("../tests/support/native_clock27.rs");

    #[test]
    fn native_clock29_remaining_global_rescale_and_catalogue_controls() {
        native_behavior_cases("native-clock29", "clock", 9);
    }

    #[test]
    fn native_clock29_terminal_gap_and_unaligned_fractional_controls() {
        native_behavior_cases("native-clock29", "edits", 2);
    }

    #[test]
    fn native_clock29_all_ones_native_base_keeps_unsigned_source_scale() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native-clock29/order27-MIN-u32max-scale-cat12.mp4");
        for (window, quantum) in [(4096, 4096), (1, 3)] {
            let mut output = Vec::new();
            run(&fixture, window, quantum, false, &mut output).unwrap();
            let trace = String::from_utf8(output).unwrap();
            let sources: Vec<_> = trace
                .lines()
                .filter(|line| {
                    line.starts_with("{\"event\":\"sample\",") && field(line, "track_id") == "1"
                })
                .collect();
            assert_eq!(sources.len(), 6);
            assert!(
                sources
                    .iter()
                    .all(|line| field(line, "timescale") == "4294967295")
            );
            assert_eq!(field(sources[0], "dts_ticks"), "9223372036854775808");
            let stream = trace
                .lines()
                .find(|line| {
                    line.starts_with("{\"event\":\"native_stream\",")
                        && field(line, "track_id") == "1"
                })
                .expect("required separately named native stream view");
            assert!(stream.contains("\"time_base\":[1,1]"));
            assert!(trace.contains("\"event\":\"native_end\",\"confirmed_eof\":true"));
        }
    }

    #[test]
    fn native_clock28_observed_signed_null_and_bounded_order_controls() {
        native_behavior_cases("native-clock28", "clock", 11);
    }

    #[test]
    fn native_clock28_observed_fractional_clocks_and_decode_table_ordinals() {
        native_behavior_cases("native-clock28", "fractional", 18);
    }

    // Test-only scalar reader for this example's controlled JSONL writer. This
    // is not an input parser or a production JSON dependency. Expected S rows
    // come from an independent source oracle; N rows retain exact pinned-native
    // observations and global order. Their provenance is beside the fixtures.
    fn field<'a>(line: &'a str, name: &str) -> &'a str {
        line.split_once(&format!("\"{name}\":"))
            .unwrap_or_else(|| panic!("missing {name} in {line}"))
            .1
            .split([',', '}'])
            .next()
            .unwrap()
            .trim_matches('"')
    }

    fn source_row(line: &str) -> Vec<&str> {
        let mut row: Vec<_> = [
            "track_id",
            "sample",
            "description",
            "offset",
            "size",
            "timescale",
            "dts_ticks",
            "cts_ticks",
            "duration_ticks",
        ]
        .into_iter()
        .map(|name| field(line, name))
        .collect();
        if line.contains("\"composition_offset\":null") {
            row.extend(["null"; 3]);
        } else {
            row.extend(["version", "raw_bits", "signed_value"].map(|name| field(line, name)));
        }
        row.extend(["fragment_flags", "sync", "payload_hex"].map(|name| field(line, name)));
        row
    }

    fn native_row(line: &str) -> Vec<&str> {
        [
            "track_id",
            "source_sample",
            "description",
            "position",
            "pts",
            "dts",
            "duration",
            "flags",
            "payload_hex",
        ]
        .into_iter()
        .map(|name| field(line, name))
        .collect()
    }

    fn native_behavior_family(family: &str) {
        let expected_cases = match family {
            "clock" => 16,
            "flags" => 14,
            "order" => 17,
            _ => panic!("unknown authored regression family"),
        };
        native_behavior_cases("native-behavior", family, expected_cases);
    }

    fn native_behavior_cases(folder: &str, family: &str, expected_cases: usize) {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let expected =
            fs::read_to_string(fixtures.join(folder).join(format!("{family}.txt"))).unwrap();
        let mut nonfits = Vec::new();
        let mut cases = 0;
        for block in expected.split("\n\n") {
            let Some(name) = block.lines().find_map(|line| line.strip_prefix("case ")) else {
                continue;
            };
            cases += 1;
            let rows = |prefix: &str| {
                block
                    .lines()
                    .filter_map(|line| line.strip_prefix(prefix))
                    .map(|line| line.split_whitespace().collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            };
            let source_expected = rows("S ");
            let native_expected = rows("N ");
            let tags_expected = rows("T ");
            assert!(!source_expected.is_empty());
            assert!(!native_expected.is_empty());
            for (window, quantum) in [(4096, 4096), (1, 3)] {
                let mut output = Vec::new();
                run(&fixtures.join(name), window, quantum, false, &mut output)
                    .unwrap_or_else(|error| panic!("{name} {window}/{quantum}: {error:?}"));
                let trace = String::from_utf8(output).unwrap();
                assert!(trace.contains("\"event\":\"native_end\",\"confirmed_eof\":true"));
                assert!(trace.contains("\"event\":\"status\",\"status\":\"complete\""));
                let source_actual: Vec<_> = trace
                    .lines()
                    .filter(|line| line.starts_with("{\"event\":\"sample\","))
                    .map(source_row)
                    .collect();
                assert_eq!(source_actual.len(), source_expected.len(), "{name}");
                // Match immutable source identity without imposing a global
                // source traversal order on a future native occurrence cursor.
                for wanted in &source_expected {
                    let matching: Vec<_> = source_actual
                        .iter()
                        .filter(|actual| actual[..2] == wanted[..2])
                        .collect();
                    assert_eq!(matching.len(), 1, "source identity {name}: {wanted:?}");
                    assert_eq!(
                        matching[0], wanted,
                        "source facts {name} {window}/{quantum}"
                    );
                }
                for wanted in &tags_expected {
                    let matching: Vec<_> = trace
                        .lines()
                        .filter(|line| line.starts_with("{\"event\":\"native_stream\","))
                        .filter(|line| field(line, "track_id") == wanted[0])
                        .collect();
                    assert_eq!(matching.len(), 1, "native catalogue {name}");
                    assert_eq!(
                        field(matching[0], "zero_vendor"),
                        wanted[1],
                        "native vendor presence {name} {window}/{quantum}"
                    );
                }
                let native_actual: Vec<_> = trace
                    .lines()
                    .filter(|line| line.starts_with("{\"event\":\"native_packet\","))
                    .map(|line| {
                        assert!(line.contains("\"side_data\":[]"));
                        native_row(line)
                    })
                    .collect();
                // Compare the actual global sequence directly. No sorting,
                // timestamp rescale, flag masking or oracle-fed output.
                if native_actual != native_expected {
                    let first = native_actual
                        .iter()
                        .zip(&native_expected)
                        .position(|(actual, wanted)| actual != wanted);
                    nonfits.push(format!(
                        "{name} {window}/{quantum}: native counts {}/{}, first differing row {first:?}: actual {:?}, expected {:?}",
                        native_actual.len(),
                        native_expected.len(),
                        first.and_then(|i| native_actual.get(i)),
                        first.and_then(|i| native_expected.get(i)),
                    ));
                }
            }
        }
        assert_eq!(cases, expected_cases, "regression coverage");
        assert!(nonfits.is_empty(), "{}", nonfits.join("\n"));
    }

    #[test]
    fn native_behavior_clock_observations_preserve_exact_source_time_and_offset_bits() {
        native_behavior_family("clock");
    }

    #[test]
    fn native_behavior_fragment_flag_observations_preserve_raw_source_flags() {
        native_behavior_family("flags");
    }

    #[test]
    fn native_behavior_global_order_matches_exact_pinned_all_stream_operation() {
        native_behavior_family("order");
    }

    #[test]
    fn native_behavior_followup_clock_runs_and_unsigned_source_bits() {
        native_behavior_cases("native-followup", "clock", 13);
    }

    #[test]
    fn native_behavior_followup_flag_boundaries_and_track_isolation() {
        native_behavior_cases("native-followup", "flags", 8);
    }

    #[test]
    fn native_behavior_followup_global_cycles_and_submicrosecond_boundaries() {
        native_behavior_cases("native-followup", "order", 13);
    }

    #[test]
    fn native_behavior_followup_major_and_compatible_brand_catalogue_controls() {
        native_behavior_cases("native-followup", "brand", 5);
    }

    #[test]
    fn required_absent_tfdt_native_origin_keeps_unknown_source_and_explicit_predecessor() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/f-no-second-tfdt.mp4");
        for (window, quantum) in [(4096, 4096), (1, 3)] {
            let mut output = Vec::new();
            run(&fixture, window, quantum, false, &mut output).unwrap();
            let trace = String::from_utf8(output).unwrap();
            let sources: Vec<_> = trace
                .lines()
                .filter(|line| line.starts_with("{\"event\":\"sample\","))
                .collect();
            assert_eq!(sources.len(), 6);
            for (index, line) in sources.iter().enumerate() {
                assert_eq!(field(line, "sample"), (index + 1).to_string());
                assert_eq!(field(line, "duration_ticks"), "1000");
                if index < 3 {
                    assert_eq!(field(line, "dts_ticks"), (index * 1000).to_string());
                    assert_eq!(field(line, "cts_ticks"), (index * 1000).to_string());
                } else {
                    for name in ["dts_ticks", "cts_ticks", "dts_seconds", "cts_seconds"] {
                        assert_eq!(
                            field(line, name),
                            "null",
                            "absent wire/caller origin must stay unknown"
                        );
                    }
                }
            }
            // Exact original native observations provide the compatibility
            // expectations; no known source clock is fabricated to obtain them.
            assert!(
                trace.contains("\"event\":\"native_end\",\"confirmed_eof\":true"),
                "required predecessor-derived native view missing: {window}/{quantum}"
            );
            let packets: Vec<_> = trace
                .lines()
                .filter(|line| line.starts_with("{\"event\":\"native_packet\","))
                .collect();
            assert_eq!(packets.len(), 6);
            for (index, line) in packets.iter().enumerate() {
                assert_eq!(field(line, "source_sample"), (index + 1).to_string());
                assert_eq!(field(line, "dts"), (index * 1000).to_string());
                assert_eq!(field(line, "pts"), (index * 1000).to_string());
                assert_eq!(
                    field(line, "payload_hex"),
                    field(sources[index], "payload_hex")
                );
            }
            let origins: Vec<_> = trace
                .lines()
                .filter(|line| line.starts_with("{\"event\":\"native_origin\","))
                .collect();
            assert_eq!(origins.len(), 3);
            for (index, line) in origins.iter().enumerate() {
                assert_eq!(field(line, "kind"), "predecessor-end");
                assert_eq!(field(line, "track_id"), "1");
                assert_eq!(field(line, "source_sample"), (index + 4).to_string());
                assert_eq!(field(line, "predecessor_sample"), (index + 3).to_string());
                assert_eq!(
                    field(line, "predecessor_native_dts_ticks"),
                    ((index + 2) * 1000).to_string()
                );
                assert_eq!(field(line, "predecessor_duration_ticks"), "1000");
                assert_eq!(
                    field(line, "derived_dts_ticks"),
                    ((index + 3) * 1000).to_string()
                );
                assert_eq!(field(line, "source_dts_known"), "false");
                assert_eq!(field(line, "source_cts_known"), "false");
                assert_eq!(
                    field(line, "predecessor_source_dts_known"),
                    if index == 0 { "true" } else { "false" }
                );
            }
        }
    }

    // Partition controls preserve source facts and challenge the pinned
    // previous-run append boundary at both work schedules.
    #[test]
    fn absent_tfdt_after_backward_anchor_keeps_source_unknown_and_native_discard_exact() {
        struct Case {
            fixture: &'static str,
            source: &'static [Option<i128>],
            native: &'static [i128],
            duration: &'static [u32],
            flags: &'static [u32],
        }
        // Exact source oracle and actual pinned-native values, without retiming or
        // flag normalization. Explicit-equivalent and split-run cases differ only
        // in their wire run partition.
        let cases = [
            Case {
                fixture: "selected-native18/origin18-three-backward-then-absent.mp4",
                source: &[Some(7000), Some(8000), Some(1000), Some(2000), None, None],
                native: &[7000, 8000, 1000, 2000, 3000, 4000],
                duration: &[1000; 6],
                flags: &[1, 0, 4, 5, 0, 0],
            },
            Case {
                fixture: "append-boundary/discard22-explicit-equivalent.mp4",
                source: &[
                    Some(7000),
                    Some(8000),
                    Some(1000),
                    Some(2000),
                    Some(3000),
                    Some(4000),
                ],
                native: &[7000, 8000, 1000, 2000, 3000, 4000],
                duration: &[1000; 6],
                flags: &[1, 0, 4, 5, 0, 0],
            },
            Case {
                fixture: "append-boundary/discard22-later-anchor-5000.mp4",
                source: &[
                    Some(7000),
                    Some(8000),
                    Some(1000),
                    Some(2000),
                    None,
                    None,
                    Some(5000),
                    Some(6000),
                ],
                native: &[7000, 8000, 1000, 2000, 3000, 4000, 5000, 6000],
                duration: &[1000; 8],
                flags: &[1, 0, 4, 5, 0, 0, 1, 0],
            },
            Case {
                fixture: "append-boundary/discard22-long-derived-then-intermediate-anchor.mp4",
                source: &[
                    Some(7000),
                    Some(8000),
                    Some(1000),
                    Some(2000),
                    None,
                    None,
                    Some(10000),
                    Some(11000),
                ],
                native: &[7000, 8000, 1000, 2000, 25000, 26000, 10000, 11000],
                duration: &[1000, 1000, 1000, 23000, 1000, 1000, 1000, 1000],
                flags: &[1, 0, 4, 5, 0, 0, 5, 4],
            },
            Case {
                fixture: "append-boundary/discard22-zero-delta-derived-equality.mp4",
                source: &[Some(7000), Some(8000), Some(1000), Some(2000), None, None],
                native: &[7000, 8000, 1000, 2000, 3000, 3000],
                duration: &[1000, 1000, 1000, 1000, 0, 1000],
                flags: &[1, 0, 4, 5, 0, 0],
            },
            Case {
                fixture: "append-boundary/discard24-split-backward-run.mp4",
                source: &[
                    Some(7000),
                    Some(8000),
                    Some(1000),
                    Some(2000),
                    Some(3000),
                    Some(4000),
                ],
                native: &[7000, 8000, 1000, 2000, 3000, 4000],
                duration: &[1000; 6],
                flags: &[1, 0, 4, 1, 0, 0],
            },
        ];
        for case in cases {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(case.fixture);
            for (window, quantum) in [(4096, 4096), (1, 3)] {
                let mut output = Vec::new();
                run(&fixture, window, quantum, false, &mut output).unwrap();
                let trace = String::from_utf8(output).unwrap();
                let sources: Vec<_> = trace
                    .lines()
                    .filter(|line| line.starts_with("{\"event\":\"sample\","))
                    .collect();
                assert_eq!(sources.len(), case.source.len(), "{}", case.fixture);
                for (index, line) in sources.iter().enumerate() {
                    assert_eq!(field(line, "sample"), (index + 1).to_string());
                    assert_eq!(
                        field(line, "duration_ticks"),
                        case.duration[index].to_string()
                    );
                    assert_eq!(field(line, "composition_offset"), "null");
                    let raw_flags = if matches!(index, 0 | 3 | 6) {
                        0x02000000u32
                    } else {
                        0x01010000
                    };
                    assert_eq!(field(line, "fragment_flags"), raw_flags.to_string());
                    if let Some(ticks) = case.source[index] {
                        assert_eq!(field(line, "dts_ticks"), ticks.to_string());
                        assert_eq!(field(line, "cts_ticks"), ticks.to_string());
                    } else {
                        for key in ["dts_ticks", "cts_ticks", "dts_seconds", "cts_seconds"] {
                            assert_eq!(field(line, key), "null");
                        }
                    }
                }
                assert!(trace.contains("\"event\":\"native_end\",\"confirmed_eof\":true"));
                let packets: Vec<_> = trace
                    .lines()
                    .filter(|line| line.starts_with("{\"event\":\"native_packet\","))
                    .collect();
                assert_eq!(packets.len(), case.native.len());
                for (index, line) in packets.iter().enumerate() {
                    assert_eq!(field(line, "source_sample"), (index + 1).to_string());
                    assert_eq!(field(line, "dts"), case.native[index].to_string());
                    assert_eq!(field(line, "pts"), case.native[index].to_string());
                    assert_eq!(field(line, "duration"), case.duration[index].to_string());
                    assert_eq!(
                        field(line, "flags"),
                        case.flags[index].to_string(),
                        "{} {window}/{quantum} sample{}",
                        case.fixture,
                        index + 1
                    );
                    assert_eq!(field(line, "position"), field(sources[index], "offset"));
                    assert_eq!(
                        field(line, "payload_hex"),
                        field(sources[index], "payload_hex")
                    );
                }
                let origins: Vec<_> = trace
                    .lines()
                    .filter(|line| line.starts_with("{\"event\":\"native_origin\","))
                    .collect();
                let unknown: Vec<_> = case
                    .source
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| value.is_none())
                    .map(|(index, _)| index)
                    .collect();
                assert_eq!(origins.len(), unknown.len());
                for (line, index) in origins.iter().zip(unknown) {
                    let previous = index
                        .checked_sub(1)
                        .expect("these probes have an established first source clock");
                    assert_eq!(field(line, "kind"), "predecessor-end");
                    assert_eq!(field(line, "track_id"), "1");
                    assert_eq!(field(line, "source_sample"), (index + 1).to_string());
                    assert_eq!(field(line, "predecessor_sample"), index.to_string());
                    assert_eq!(
                        field(line, "predecessor_native_dts_ticks"),
                        case.native[previous].to_string()
                    );
                    assert_eq!(
                        field(line, "predecessor_duration_ticks"),
                        case.duration[previous].to_string()
                    );
                    assert_eq!(
                        field(line, "derived_dts_ticks"),
                        case.native[index].to_string()
                    );
                    assert_eq!(field(line, "source_dts_known"), "false");
                    assert_eq!(field(line, "source_cts_known"), "false");
                    assert_eq!(
                        field(line, "predecessor_source_dts_known"),
                        case.source[previous].is_some().to_string()
                    );
                }
            }
        }
    }

    #[test]
    fn physical_outside_mdat_is_invalid_before_offset_order_profile_rejection() {
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
