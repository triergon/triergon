use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_matroska::compat::block_additions::{self, BlockAdditionContainer};
use triergon_matroska::{Error, Event, Hdr10Plus, Hdr10PlusPeakGrid, Limits, ParseStep, Reader};

use super::extensions::track_extensions;
use super::protocol::{Operation, Work};

pub(super) const SOURCE: SourceId = SourceId(1);
const WINDOW: usize = 32 * 1024;

pub(super) struct Input<'a> {
    bytes: &'a [u8],
    pub reader: Reader,
    pub cancel: AtomicBool,
    pending: Option<Event>,
}

impl<'a> Input<'a> {
    pub fn new(bytes: &'a [u8], compatible: bool) -> triergon_matroska::Result<Self> {
        let limits = Limits::default();
        let pool = Budget::new(limits.live_storage_bytes)?;
        let caps = SourceCapabilities {
            length: Some(bytes.len() as u64),
            random_access: true,
        };
        let reader = if compatible {
            block_additions::new_reader(&pool, SOURCE, caps, limits, WINDOW)?
        } else {
            Reader::new(&pool, SOURCE, caps, limits, WINDOW)?
        };
        Ok(Self {
            bytes,
            reader,
            cancel: AtomicBool::new(false),
            pending: None,
        })
    }

    fn advance(&mut self) -> triergon_matroska::Result<Step<Event, triergon_core::Backpressure>> {
        let step = self
            .reader
            .step(&mut WorkBudget::new(WINDOW as u64), &self.cancel)?;
        match step {
            Step::Backpressure(pressure) => Err(Error::Backpressure(pressure)),
            Step::NeedInput(request) => {
                let start = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
                let rest = self.bytes.get(start..).ok_or(Error::Truncated)?;
                if rest.is_empty() {
                    self.reader.finish(request.source, request.token)?;
                } else {
                    let bytes = Bytes::copy_from_slice(
                        self.reader.budget(),
                        &rest[..rest.len().min(request.maximum)],
                    )?;
                    self.reader.provide(&InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        bytes,
                    )?)?;
                }
                Ok(Step::Yield)
            }
            other => Ok(other),
        }
    }

    pub fn next(&mut self) -> triergon_matroska::Result<Option<Event>> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
        loop {
            match self.advance()? {
                Step::Ready(event) => return Ok(Some(event)),
                Step::End(_) => return Ok(None),
                _ => (),
            }
        }
    }

    pub fn open(&mut self) -> triergon_matroska::Result<()> {
        while !self.reader.is_ready() {
            match self.advance()? {
                Step::Ready(event @ Event::Packet(_)) => {
                    self.pending = Some(event);
                    break;
                }
                Step::End(_) => return Err(Error::Truncated),
                _ => (),
            }
        }
        Ok(())
    }
}

pub(super) struct Rows<'a, 'b> {
    pub work: &'a mut Work,
    pub trace: Option<&'b mut dyn Write>,
}

impl Rows<'_, '_> {
    pub fn record(
        &mut self,
        kind: u64,
        words: &[u64],
        payload: &[u8],
    ) -> triergon_matroska::Result<()> {
        self.work.record(kind, words, payload);
        if let Some(trace) = &mut self.trace {
            write!(trace, "{kind}")?;
            for word in words {
                write!(trace, " {word}")?;
            }
            if !payload.is_empty() {
                write!(trace, " ")?;
                for byte in payload {
                    write!(trace, "{byte:02x}")?;
                }
            }
            writeln!(trace)?;
        }
        Ok(())
    }

    pub fn optional(
        &mut self,
        kind: u64,
        field: u64,
        value: Option<&[u8]>,
    ) -> triergon_matroska::Result<()> {
        let bytes = value.unwrap_or_default();
        self.record(
            kind,
            &[field, u64::from(value.is_some()), bytes.len() as u64],
            bytes,
        )
    }

    pub fn finish(&mut self, result: triergon_matroska::Result<()>) -> Result<(), String> {
        // I/O errors and exhausted worker storage are harness failures.
        if matches!(
            result,
            Err(Error::Io(_) | Error::Allocation | Error::Backpressure(_))
        ) {
            return result.map_err(|error| error.to_string());
        }
        if self.trace.is_some()
            && let Err(error) = result
        {
            eprintln!("{error}");
        }
        self.record(if result.is_ok() { 7 } else { 8 }, &[], &[])
            .map_err(|error| error.to_string())
    }
}

fn ratio(words: &mut Vec<u64>, value: impl Into<u64>, denominator: u64) {
    words.extend([value.into(), denominator]);
}

fn grid(words: &mut Vec<u64>, grid: Option<Hdr10PlusPeakGrid<'_>>) {
    words.push(u64::from(grid.is_some()));
    if let Some(grid) = grid {
        words.extend([u64::from(grid.rows()), u64::from(grid.columns())]);
        for value in grid.values() {
            ratio(words, value, 15);
        }
    }
}

fn hdr_words(hdr: &Hdr10Plus<'_>) -> Vec<u64> {
    let mut words = vec![
        3,
        u64::from(hdr.application_version),
        hdr.windows().len() as u64,
    ];
    ratio(&mut words, hdr.target_luminance, 1);
    grid(&mut words, hdr.target_grid);
    grid(&mut words, hdr.mastering_grid);
    for window in hdr.windows() {
        words.push(u64::from(window.geometry.is_some()));
        if let Some(geometry) = window.geometry {
            for coord in geometry.rectangle {
                ratio(&mut words, coord, 1);
            }
            words.extend(geometry.ellipse_center.map(u64::from));
            words.extend([
                u64::from(geometry.rotation),
                u64::from(geometry.internal_semimajor_axis),
                u64::from(geometry.external_semimajor_axis),
                u64::from(geometry.external_semiminor_axis),
                u64::from(geometry.overlap_process_option),
            ]);
        }
        for value in window.max_scl.into_iter().chain([window.average_max_rgb]) {
            ratio(&mut words, value, 100_000);
        }
        words.push(window.percentiles().len() as u64);
        for item in window.percentiles() {
            words.push(u64::from(item.percentage));
            ratio(&mut words, item.max_rgb, 100_000);
        }
        ratio(&mut words, window.fraction_bright_pixels, 1000);
        words.push(u64::from(window.tone_curve.is_some()));
        if let Some(tone) = window.tone_curve {
            for value in tone.knee {
                ratio(&mut words, value, 4095);
            }
            words.push(tone.anchors().len() as u64);
            for &value in tone.anchors() {
                ratio(&mut words, value, 1023);
            }
        }
        words.push(u64::from(window.saturation_weight.is_some()));
        if let Some(weight) = window.saturation_weight {
            ratio(&mut words, weight, 8);
        }
    }
    words
}

fn side_data(
    bytes: &[u8],
    container: BlockAdditionContainer,
    rows: &mut Rows<'_, '_>,
) -> triergon_matroska::Result<()> {
    let mut input = Input::new(bytes, true)?;
    input.open()?;
    let configuration = input
        .reader
        .configuration()
        .ok_or(Error::Invalid("missing configuration"))?;
    let extensions = (0..configuration.tracks().len())
        .map(|index| track_extensions(configuration, index, &input.cancel))
        .collect::<triergon_matroska::Result<Vec<_>>>()?;
    while let Some(event) = input.next()? {
        let Event::Packet(packet) = event else {
            continue;
        };
        let track = packet.packet.track;
        rows.record(1, &[track as u64], &[])?;
        let extensions = &extensions[track];
        let mut work = WorkBudget::new(u64::MAX);
        let ParseStep::Event(hdr) =
            block_additions::hdr10_plus(&packet, extensions, container, &mut work, &input.cancel)?
        else {
            return Err(Error::Invalid("HDR10+ projection did not finish"));
        };
        match block_additions::packet_side_data(
            &packet,
            extensions,
            container,
            &mut work,
            &input.cancel,
        ) {
            Ok(ParseStep::Event(entries)) => {
                let mut entries = entries.into_iter().flatten().collect::<Vec<_>>();
                entries.sort_by_key(|entry| entry.wire_parts().0.is_none());
                for entry in entries {
                    let (prefix, data) = entry.wire_parts();
                    if let Some(prefix) = prefix {
                        rows.record(
                            4,
                            &[1, data.len() as u64 + 8],
                            &[&prefix[..], data].concat(),
                        )?;
                    } else {
                        rows.record(4, &[2, data.len() as u64], data)?;
                    }
                }
            }
            Err(Error::Unsupported(_)) if hdr.is_some() => (),
            Err(error) => return Err(error),
            Ok(_) => return Err(Error::Invalid("side data projection did not finish")),
        }
        if let Some(hdr) = hdr {
            let ParseStep::Event(direct) =
                Hdr10Plus::parse_t35(hdr.raw(), &mut work, &input.cancel)?
            else {
                return Err(Error::Invalid("direct HDR10+ parse did not finish"));
            };
            let words = hdr_words(&direct);
            if words != hdr_words(&hdr) {
                return Err(Error::Invalid("HDR10+ entry points disagree"));
            }
            rows.record(4, &words, &[])?;
        }
    }
    Ok(())
}

fn state_words(state: &triergon_matroska::CodecState) -> Vec<u64> {
    let mut words = vec![
        state.data().len() as u64,
        u64::from(state.position().is_some()),
    ];
    if let Some(position) = state.position() {
        words.extend([
            position.source.0,
            position.header,
            position.data,
            position.length,
            position.segment_position,
        ]);
    }
    words
}

fn codec_state(bytes: &[u8], rows: &mut Rows<'_, '_>) -> triergon_matroska::Result<()> {
    let mut input = Input::new(bytes, false)?;
    while let Some(event) = input.next()? {
        match event {
            Event::Configuration(configuration) => {
                for track in 0..configuration.tracks().len() {
                    let mut words = vec![
                        track as u64,
                        u64::from(configuration.codec_state(track).is_some()),
                    ];
                    if let Some(state) = configuration.codec_state(track) {
                        words.extend(state_words(state));
                        rows.record(2, &words, state.data())?;
                    } else {
                        rows.record(2, &words, &[])?;
                    }
                }
            }
            Event::Packet(packet) => {
                let track = packet.packet.track;
                let state = packet.configuration.codec_state(track);
                let mut words = vec![track as u64, u64::from(state.is_some())];
                if let Some(state) = state {
                    words.extend(state_words(state));
                }
                rows.record(
                    1,
                    &words,
                    state.map_or(&[], triergon_matroska::CodecState::data),
                )?;
            }
            _ => (),
        }
    }
    Ok(())
}

pub(super) fn operation<'a>(
    prefix: &str,
    bytes: &'a [u8],
    args: &[OsString],
) -> Result<Operation<'a>, String> {
    match (prefix, args) {
        ("mkv-codec-state-", []) => Ok(Operation {
            name: "mkv-codec-state",
            run: Box::new(move |work, trace| {
                let mut rows = Rows { work, trace };
                let result = codec_state(bytes, &mut rows);
                rows.finish(result)
            }),
        }),
        ("mkv-side-data-", [container]) => {
            let container = match container.to_str() {
                Some("matroska") => BlockAdditionContainer::Matroska,
                Some("webm") => BlockAdditionContainer::WebM,
                _ => return Err("expected matroska or webm".into()),
            };
            Ok(Operation {
                name: "mkv-side-data",
                run: Box::new(move |work, trace| {
                    let mut rows = Rows { work, trace };
                    let result = side_data(bytes, container, &mut rows);
                    rows.finish(result)
                }),
            })
        }
        _ => Err(format!("invalid {prefix} arguments")),
    }
}
