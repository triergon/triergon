use std::ffi::OsString;
use std::fmt::{Debug, Write as _};
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon_core::{
    Budget, Bytes, Error, InputRequest, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::dts::{
    self, Dts14BitUnpacker, Dts14CarrierLimits, Dts14CarrierReader, Dts14CarrierSyntax,
    Dts14SyncLimits, Dts14SyncReader, DtsCoreField, DtsCoreFrameLimits, DtsCoreFrameReader,
    DtsFrameExtentLimits, DtsHeaderInspector, DtsInspection, DtsNormalLimits,
    DtsNormalSourceReader, DtsPackedBitLocation, DtsPackedLimits, DtsPackedPayloadJob, DtsPacking,
    DtsPackingLimits, DtsSourceIdentity, DtsWordOrder, FieldObservation,
};
use triergon_framing::reference_trace::dts::{
    ParserHints, ParserOperation, ParserOperationEvent, ParserOperationLimits, ParserOperationStep,
    ParserOperationTerminal,
};
use triergon_harness_protocol::digest::signed;

use super::protocol::{Operation, Work};

const SOURCE: SourceId = SourceId(1);
static CANCEL: AtomicBool = AtomicBool::new(false);
const CAPABILITIES: SourceCapabilities = SourceCapabilities {
    length: None,
    random_access: false,
};
const FRAME_BYTES: usize = 16384;
const PACKET_BYTES: usize = 256 * 1024 * 1024;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy)]
enum Reader {
    Sync14(DtsWordOrder),
    Carrier14(DtsWordOrder),
    Normal(DtsPacking),
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("invalid DTS argument"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let size = |value: &str| {
        value
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0 && n <= PACKET_BYTES && value.bytes().all(|b| b.is_ascii_digit()))
            .ok_or("invalid DTS byte count")
    };
    let window = |value| size(value).map(|size| size.min(input.len().max(1)));
    match (prefix, args.as_slice()) {
        ("dts-core-", [chunk, limit, mode @ ("frames" | "decoded" | "core")]) => {
            let (window, maximum, decoded) = (window(chunk)?, size(limit)?, *mode == "decoded");
            Ok(Operation {
                name: "dts-core-frames",
                run: Box::new(move |work, trace| {
                    core(input, window, maximum, decoded, work, trace)
                }),
            })
        }
        ("dts-header-", [mode @ ("prefix" | "stream")]) => {
            let stream = *mode == "stream";
            Ok(Operation {
                name: "dts-core-header",
                run: Box::new(move |work, trace| headers(input, stream, work, trace)),
            })
        }
        ("dts-packed-", [reader, format, chunk]) => {
            let (window, format) = (window(chunk)?, packing(format)?);
            let reader = match *reader {
                "sync14" => Reader::Sync14(order(format)?),
                "carrier14" => Reader::Carrier14(order(format)?),
                "normal" => Reader::Normal(format),
                _ => return Err("invalid DTS packed reader".into()),
            };
            Ok(Operation {
                name: "dts-packed-frames",
                run: Box::new(move |work, trace| packed(input, window, reader, work, trace)),
            })
        }
        ("dts-unpack-", [format]) => {
            let format = packing(format)?;
            Ok(Operation {
                name: "dts-unpacking",
                run: Box::new(move |work, trace| unpack(input, format, work, trace)),
            })
        }
        ("dts-unpack-", [format, "frames"]) => {
            let order = order(packing(format)?)?;
            Ok(Operation {
                name: "dts-unpacking",
                run: Box::new(move |work, trace| sync_canonical(input, order, work, trace)),
            })
        }
        ("dts-parser-", [format, chunk]) => {
            let (order, window) = (order(packing(format)?)?, window(chunk)?);
            Ok(Operation {
                name: "dts-parser-trace",
                run: Box::new(move |work, trace| parser(input, window, order, work, trace)),
            })
        }
        _ => Err(format!("invalid {prefix} arguments")),
    }
}

fn packing(format: &str) -> Result<DtsPacking> {
    match format {
        "be16" => Ok(DtsPacking::CanonicalBe),
        "le16" => Ok(DtsPacking::Words16Le),
        "be14" => Ok(DtsPacking::Words14Be),
        "le14" => Ok(DtsPacking::Words14Le),
        _ => Err("invalid DTS packing".into()),
    }
}

fn order(format: DtsPacking) -> Result<DtsWordOrder> {
    match format {
        DtsPacking::Words14Be => Ok(DtsWordOrder::BigEndian),
        DtsPacking::Words14Le => Ok(DtsWordOrder::LittleEndian),
        _ => Err("DTS reader requires 14-bit packing".into()),
    }
}

fn error(error: impl Debug) -> String {
    format!("{error:?}")
}

fn pool(input: usize, window: usize) -> Result<Budget> {
    Budget::new(input.saturating_mul(5).saturating_add(window).max(1 << 20)).map_err(error)
}

fn sync_limits(window: usize) -> Dts14SyncLimits {
    Dts14SyncLimits {
        maximum_physical_frame_bytes: 2 * FRAME_BYTES,
        maximum_canonical_frame_bytes: FRAME_BYTES,
        maximum_frames: PACKET_BYTES,
        maximum_input_window: window,
    }
}

fn carrier_limits(window: usize) -> Dts14CarrierLimits {
    Dts14CarrierLimits {
        maximum_packet_bytes: PACKET_BYTES,
        maximum_packets: PACKET_BYTES,
        maximum_input_window: window,
    }
}

fn input_window(
    pool: &Budget,
    input: &[u8],
    request: &InputRequest,
    window: usize,
) -> Result<Option<InputWindow>> {
    let start = usize::try_from(request.offset).map_err(error)?;
    let available = input.get(start..).ok_or("DTS request past the input")?;
    if available.is_empty() {
        return Ok(None);
    }
    let length = available.len().min(window).min(request.maximum);
    let bytes = Bytes::copy_from_slice(pool, &available[..length]).map_err(error)?;
    InputWindow::new(request.source, request.token.clone(), request.offset, bytes)
        .map(Some)
        .map_err(error)
}

struct Output<'a, 'b> {
    work: &'a mut Work,
    trace: Option<&'b mut dyn Write>,
}

impl Output<'_, '_> {
    fn packet(&mut self, words: &[u64], bytes: &[u8]) -> Result<()> {
        self.work.record(1, words, bytes);
        if let Some(trace) = self.trace.as_mut() {
            write!(trace, "1").map_err(error)?;
            for word in words {
                write!(trace, " {word}").map_err(error)?;
            }
            writeln!(trace).map_err(error)?;
        }
        Ok(())
    }

    fn end(&mut self, result: triergon_core::Result<()>) -> Result<()> {
        match result {
            Ok(()) => self.row("7 end", 7, &[], &[]),
            Err(
                reason @ (Error::Invalid(_)
                | Error::Truncated
                | Error::Unsupported(_)
                | Error::Limit { .. }),
            ) => self.row(&format!("8 {reason:?}"), 8, &[], &[]),
            Err(reason) => Err(error(reason)),
        }
    }

    fn row(&mut self, row: &str, kind: u64, words: &[u64], bytes: &[u8]) -> Result<()> {
        self.work.record(kind, words, bytes);
        if let Some(trace) = self.trace.as_mut() {
            writeln!(trace, "{row}").map_err(error)?;
        }
        Ok(())
    }
}

// All four readers use the same bounded window protocol. The publication arm
// stays at the call site so it can use the frame's own type.
macro_rules! drive {
    ($reader:ident, $pool:ident, $input:ident, $window:ident, $output:ident, $frame:ident => $emit:block) => {
        loop {
            match $reader.step(&mut WorkBudget::new(65536), &CANCEL) {
                Ok(Step::Ready($frame)) => $emit,
                Ok(Step::Yield) => {}
                Ok(Step::NeedInput(request)) => {
                    let response = match input_window(&$pool, $input, &request, $window)? {
                        Some(bytes) => $reader.provide(&bytes),
                        None => $reader.finish(request.source, request.token),
                    };
                    response.map_err(error)?;
                }
                Ok(Step::End(_)) => break $output.end(Ok(())),
                Err(reason) => break $output.end(Err(reason)),
            }
        }
    };
}

fn core(
    input: &[u8],
    window: usize,
    maximum: usize,
    mut decoded: bool,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let pool = pool(4 * FRAME_BYTES, window)?;
    let mut output = Output { work, trace };
    let mut reader = DtsCoreFrameReader::new(
        &pool,
        SOURCE,
        CAPABILITIES,
        DtsCoreFrameLimits {
            maximum_frame_bytes: maximum,
            maximum_frames: PACKET_BYTES,
            maximum_input_window: window,
        },
    )
    .map_err(error)?;
    drive!(reader, pool, input, window, output, frame => {
        let bytes = frame.bytes();
        let facts = frame.facts();
        let identity = DtsSourceIdentity { source: facts.source, generation: facts.generation, offset: facts.offset };
        let owned = inspect(&pool, identity, bytes.clone())?;
        let packed = dts::inspect_header_bits(bytes.as_ref(), bytes.len() * 8).map_err(error)?;
        verify_inspectors(&owned, &packed)?;
        let declared = owned.views().declared_frame_bytes.ok_or("complete frame has no size")?;
        let extent = dts::probe_frame_extent(bytes.as_ref(), DtsFrameExtentLimits {
            maximum_header_bytes: 15,
            maximum_frame_bytes: maximum,
        }).map_err(error)?.frame_bytes();
        if decoded {
            let header = facts.header;
            // ETSI TS 102 114 audio arrangement and the optional LFE channel.
            let mask = match header.amode {
                0 => 0x4,
                1..=4 => 0x3,
                5 => 0x7,
                6 => 0x103,
                7 => 0x107,
                8 => 0x603,
                9 => 0x607,
                _ => 0,
            } | if matches!(header.lfe, 1 | 2) { 8 } else { 0 };
            let channels = u64::from(header.full_bandwidth_channels.unwrap_or(0)) + u64::from(header.lfe != 0);
            let rate = u64::from(header.nominal_bitrate.unwrap_or(0));
            // A normal core header has a source PCM resolution code; the decoder
            // reports raw depth only when its decoded stream supplies that metadata.
            output.row(&format!("3 {channels} {mask} {rate} 0"), 3, &[channels, mask, rate, 0], &[])?;
            decoded = false;
        }
        let words = [
            facts.offset, bytes.len() as u64, u64::from(declared),
            u64::from(packed.views().declared_frame_bytes.ok_or("packed frame has no size")?),
            extent as u64, u64::from(facts.header.core_rate_hz),
            u64::from(facts.header.coding_window_samples),
        ];
        output.packet(&words, bytes.as_ref())?;
        output.packet(&[extent as u64], &bytes.as_ref()[..extent])?;
    })
}

fn packed(
    input: &[u8],
    window: usize,
    reader: Reader,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let mut output = Output { work, trace };
    let pool = pool(input.len(), window)?;
    match reader {
        Reader::Sync14(order) => {
            let mut reader =
                Dts14SyncReader::per_frame(&pool, SOURCE, CAPABILITIES, order, sync_limits(window))
                    .map_err(error)?;
            drive!(reader, pool, input, window, output, frame => {
                let facts = frame.facts();
                packed_packet(&mut output, facts.identity.offset, frame.physical_bytes().as_ref(), facts.header.core_rate_hz, facts.header.coding_window_samples)?;
                if let Some(trace) = output.trace.as_mut() {
                    writeln!(trace, "canonical {} {}", facts.identity.offset, hex(frame.canonical_bytes().as_ref())).map_err(error)?;
                }
                // Keep the comparison fold confined to public parser output.
            })
        }
        Reader::Carrier14(order) => {
            let mut reader =
                Dts14CarrierReader::new(&pool, SOURCE, CAPABILITIES, order, carrier_limits(window))
                    .map_err(error)?;
            drive!(reader, pool, input, window, output, packet => {
                let header = match packet.syntax {
                    Dts14CarrierSyntax::ExactNormal(header) => Some(header),
                    _ => packet.canonical_bytes().and_then(|bytes| dts::probe_core_header(bytes.as_ref()).ok()),
                };
                let (rate, samples) = header.map_or((0, 0), |h| (h.core_rate_hz, h.coding_window_samples));
                packed_packet(&mut output, packet.identity.offset, packet.source_bytes().as_ref(), rate, samples)?;
            })
        }
        Reader::Normal(packing) => {
            let mut reader = DtsNormalSourceReader::new(
                &pool,
                SOURCE,
                CAPABILITIES,
                packing,
                DtsNormalLimits {
                    maximum_source_frame_bytes: 2 * FRAME_BYTES,
                    maximum_storage_bytes: FRAME_BYTES + 2,
                    maximum_frames: PACKET_BYTES,
                    maximum_input_window: window,
                },
            )
            .map_err(error)?;
            drive!(reader, pool, input, window, output, frame => {
                let facts = frame.facts();
                let views = facts.header.views();
                packed_packet(&mut output, facts.identity.offset, frame.source_bytes().as_ref(), views.core_rate_hz.unwrap_or(0), views.coded_window_samples.unwrap_or(0))?;
            })
        }
    }
}

fn packed_packet(
    output: &mut Output<'_, '_>,
    offset: u64,
    raw: &[u8],
    rate: u32,
    samples: u32,
) -> Result<()> {
    output.packet(
        &[
            offset,
            raw.len() as u64,
            u64::from(rate),
            u64::from(samples),
        ],
        raw,
    )
}

fn parser(
    input: &[u8],
    window: usize,
    order: DtsWordOrder,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let pool = pool(input.len(), window)?;
    let mut output = Output { work, trace };
    let mut operation = ParserOperation::new(
        &pool,
        SOURCE,
        CAPABILITIES,
        order,
        ParserOperationLimits {
            source: sync_limits(window),
            parser: carrier_limits(window),
        },
    )
    .map_err(error)?;
    loop {
        match operation
            .step(&mut WorkBudget::new(65536), &CANCEL)
            .map_err(error)?
        {
            ParserOperationStep::Yield => {}
            ParserOperationStep::NeedInput { lane, request } => {
                match input_window(&pool, input, &request, window)? {
                    Some(bytes) => operation.provide(lane, &bytes),
                    None => operation.finish(lane, request.source, request.token),
                }
                .map_err(error)?;
            }
            ParserOperationStep::Event => match operation.take_event().map_err(error)? {
                ParserOperationEvent::SourceFrame(frame) => {
                    if let Some(trace) = output.trace.as_mut() {
                        writeln!(
                            trace,
                            "source {} {}",
                            frame.facts().identity.offset,
                            frame.physical_bytes().len()
                        )
                        .map_err(error)?;
                    }
                }
                ParserOperationEvent::ViewRefusal {
                    lane,
                    error: reason,
                    ..
                } => {
                    if let Some(trace) = output.trace.as_mut() {
                        writeln!(trace, "refusal {lane:?} {reason:?}").map_err(error)?;
                    }
                }
                ParserOperationEvent::ParserPacket(packet) => {
                    let bytes = packet.source_bytes().as_ref();
                    let hints = ParserHints::observe(packet.packet()).map_err(error)?;
                    if let Some(trace) = output.trace.as_mut() {
                        writeln!(trace, "hints {:?}", hints.origin).map_err(error)?;
                    }
                    match packet.clock() {
                        Ok(clock) => {
                            if let Some(trace) = output.trace.as_mut() {
                                writeln!(trace, "clock {:?}", clock.disposition).map_err(error)?;
                            }
                            output.packet(
                                &[
                                    packet.physical_range().start,
                                    bytes.len() as u64,
                                    signed(clock.duration),
                                    u64::from(clock.codec_sample_rate),
                                    signed(i64::from(clock.profile)),
                                    signed(hints.frame_offset),
                                    signed(hints.consumed_offset),
                                    signed(hints.opaque_next_offset),
                                ],
                                bytes,
                            )?;
                        }
                        Err(reason @ Error::Unsupported(_)) => {
                            output.row(&format!("8 clock {reason:?}"), 8, &[], &[])?;
                            output.packet(
                                &[
                                    packet.physical_range().start,
                                    bytes.len() as u64,
                                    signed(hints.frame_offset),
                                    signed(hints.consumed_offset),
                                    signed(hints.opaque_next_offset),
                                ],
                                bytes,
                            )?;
                        }
                        Err(reason) => return Err(error(reason)),
                    }
                }
            },
            ParserOperationStep::End { parser, .. } => {
                return output.end(match parser {
                    ParserOperationTerminal::End(_) => Ok(()),
                    ParserOperationTerminal::Refused { error, .. } => Err(error),
                });
            }
        }
    }
}

fn inspect(pool: &Budget, identity: DtsSourceIdentity, bytes: Bytes) -> Result<DtsInspection> {
    let mut job = DtsHeaderInspector::new(pool, identity, bytes, PACKET_BYTES).map_err(error)?;
    ready(|| job.step(&mut WorkBudget::new(65536), &CANCEL)).map_err(error)
}

fn ready<T>(mut step: impl FnMut() -> triergon_core::Result<Step<T>>) -> triergon_core::Result<T> {
    loop {
        match step()? {
            Step::Ready(value) => return Ok(value),
            Step::Yield => {}
            _ => return Err(Error::Invalid("DTS job did not publish a result")),
        }
    }
}

fn verify_inspectors(owned: &DtsInspection, packed: &dts::DtsPackedHeader) -> Result<()> {
    if owned.views() != packed.views()
        || owned.issues().bits() != packed.issues()
        || owned.required_header_bits() != packed.required_header_bits()
        || owned.source_status() != packed.source_status()
        || DtsCoreField::ALL
            .into_iter()
            .any(|field| owned.field(field) != packed.field(field))
    {
        return Err(format!(
            "DTS inspectors disagree at byte {}",
            owned.identity().offset
        ));
    }
    Ok(())
}

fn headers(
    input: &[u8],
    stream: bool,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let pool = pool(input.len(), 0)?;
    let mut output = Output { work, trace };
    let mut offset = 0;
    while let Some(rest) = input.get(offset..) {
        if stream && rest.is_empty() {
            break;
        }
        let identity = DtsSourceIdentity {
            source: SOURCE,
            generation: 0,
            offset: offset as u64,
        };
        let count = rest.len().min(15);
        let prefix = &rest[..count];
        let owned = inspect(
            &pool,
            identity,
            Bytes::copy_from_slice(&pool, prefix).map_err(error)?,
        )?;
        let packed = dts::inspect_header_bits(prefix, prefix.len() * 8).map_err(error)?;
        output.row(&format!("3 header {offset}"), 3, &[offset as u64], &[])?;
        header_rows(
            &mut output,
            "3 owned",
            |field| owned.field(field),
            owned.views(),
            owned.issues().bits(),
            owned.required_header_bits(),
            owned.source_status(),
        )?;
        header_rows(
            &mut output,
            "3 packed",
            |field| packed.field(field),
            packed.views(),
            packed.issues(),
            packed.required_header_bits(),
            packed.source_status(),
        )?;
        let views = owned.views();
        let Some(size) = views.declared_frame_bytes else {
            break;
        };
        if !stream || size < 96 {
            break;
        }
        offset += size as usize;
    }
    Ok(())
}

fn header_rows(
    output: &mut Output<'_, '_>,
    inspector: &str,
    observe: impl Fn(DtsCoreField) -> FieldObservation,
    views: dts::DtsInspectionViews,
    issues: u32,
    required: Option<u16>,
    status: dts::DtsInspectionSourceStatus,
) -> Result<()> {
    output.row(
        inspector,
        3,
        &[inspector.len() as u64],
        inspector.as_bytes(),
    )?;
    for field in DtsCoreField::ALL {
        let index = field as u64;
        let (status, start, width, observed, value) = match observe(field) {
            FieldObservation::NotReached => (0, 0, 0, 0, 0),
            FieldObservation::NotInSyntax => (1, 0, 0, 0, 0),
            FieldObservation::Bits {
                offset,
                width,
                observed_bits,
                prefix,
            } => (
                2,
                u64::from(offset),
                u64::from(width),
                u64::from(observed_bits),
                u64::from(prefix),
            ),
        };
        output.row(
            &format!("3 field {index} {status} {start} {width} {observed} {value}"),
            3,
            &[index, status, start, width, observed, value],
            &[],
        )?;
    }
    let optional = |value: Option<i64>| value.map_or_else(|| "-".to_owned(), |n| n.to_string());
    let duration = views.normal_window.map_or_else(
        || "-".to_owned(),
        |t| format!("{}/{}", t.numerator(), t.denominator()),
    );
    let row = format!(
        "3 views {} {} {} {} {} {} {} {} {} {} {:?}",
        issues,
        optional(views.declared_frame_bytes.map(i64::from)),
        optional(views.core_rate_hz.map(i64::from)),
        optional(views.coded_window_samples.map(i64::from)),
        optional(views.full_bandwidth_channels.map(i64::from)),
        optional(views.nominal_bitrate.map(i64::from)),
        optional(views.open_bitrate.map(i64::from)),
        duration,
        optional(views.dialog_gain_db.map(i64::from)),
        optional(required.map(i64::from)),
        status
    );
    output.row(&row, 3, &[row.len() as u64], row.as_bytes())?;
    Ok(())
}

fn hex(data: &[u8]) -> String {
    let mut result = String::with_capacity(data.len() * 2);
    for byte in data {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn location(
    output: &mut Output<'_, '_>,
    index: usize,
    position: DtsPackedBitLocation,
) -> Result<()> {
    let DtsPackedBitLocation {
        source,
        generation,
        offset,
        msb0_bit,
    } = position;
    output.row(
        &format!(
            "3 bit {index} {} {generation} {offset} {msb0_bit}",
            source.0
        ),
        3,
        &[
            index as u64,
            source.0,
            generation,
            offset,
            u64::from(msb0_bit),
        ],
        &[],
    )
}

fn unpack(
    input: &[u8],
    format: DtsPacking,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let mut output = Output { work, trace };
    let pool = pool(input.len(), 0)?;
    let identity = DtsSourceIdentity {
        source: SOURCE,
        generation: 0,
        offset: 0,
    };
    let raw = Bytes::copy_from_slice(&pool, input).map_err(error)?;
    let mut job = match DtsPackedPayloadJob::new(
        &pool,
        identity,
        raw.clone(),
        format,
        DtsPackedLimits {
            maximum_source_bytes: PACKET_BYTES,
            maximum_storage_bytes: PACKET_BYTES,
        },
    ) {
        Ok(job) => job,
        Err(Error::Truncated) => return output.row("8 odd source byte", 8, &[], &[]),
        Err(reason) => return Err(error(reason)),
    };
    let payload = ready(|| job.step(&mut WorkBudget::new(65536), &CANCEL)).map_err(error)?;
    let bytes = payload.storage_bytes().as_ref();
    let bits = payload.valid_bits();
    let head = header_payload_bits(bytes, bits)?;
    output.row(
        &format!("1 {bits} {}", hex(bytes)),
        1,
        &[bits as u64, bytes.len() as u64],
        bytes,
    )?;
    if let Ok(order) = order(format) {
        let mut unpacker = Dts14BitUnpacker::new(
            &pool,
            identity,
            raw,
            order,
            DtsPackingLimits {
                maximum_source_bytes: PACKET_BYTES,
                maximum_output_bytes: PACKET_BYTES,
            },
        )
        .map_err(error)?;
        let unpacked =
            ready(|| unpacker.step(&mut WorkBudget::new(65536), &CANCEL)).map_err(error)?;
        if unpacked.canonical_bytes().as_ref() != &bytes[..bits / 8]
            || unpacked.payload_bits() != bits
        {
            return Err("DTS unpackers disagree on canonical payload".into());
        }
        for index in 0..head {
            let source = payload.source_bit(index).map_err(error)?;
            if source != unpacked.payload_bit_source(index).map_err(error)? {
                return Err("DTS unpackers disagree on source bits".into());
            }
            location(&mut output, index, source)?;
        }
    } else {
        for index in 0..head {
            location(
                &mut output,
                index,
                payload.source_bit(index).map_err(error)?,
            )?;
        }
    }
    Ok(())
}

fn sync_canonical(
    input: &[u8],
    order: DtsWordOrder,
    work: &mut Work,
    trace: Option<&mut dyn Write>,
) -> Result<()> {
    let window = 4096;
    let pool = pool(input.len(), window)?;
    let mut output = Output { work, trace };
    let mut reader =
        Dts14SyncReader::per_frame(&pool, SOURCE, CAPABILITIES, order, sync_limits(window))
            .map_err(error)?;
    drive!(reader, pool, input, window, output, frame => {
        let facts = frame.facts();
        let data = frame.canonical_bytes().as_ref();
        output.row(
            &format!("1 frame {} {} {}", facts.identity.offset, facts.physical_bytes, hex(data)),
            1,
            &[facts.identity.offset, facts.physical_bytes as u64, data.len() as u64],
            data,
        )?;
    })
}

fn header_payload_bits(bytes: &[u8], bits: usize) -> Result<usize> {
    let header = dts::inspect_header_bits(bytes, bits).map_err(error)?;
    Ok(header
        .required_header_bits()
        .map_or(32, usize::from)
        .min(bits))
}
