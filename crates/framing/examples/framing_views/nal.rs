use std::{ffi::OsString, io::Write, ops::Range, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::{
    BuildStep,
    hevc::{
        HvcAnnexBWriter, HvcConfigLimits, HvcConfigParser, HvcConfigSource, HvcConfiguration,
        HvcDecoderConfigOutput, HvcDecoderConfigWriter,
    },
    nal::{
        AnnexBConfigParser, AnnexBWriter, Codec, ConfigEncoding, ConfigLimits, Configuration,
        DeclaredConfigArray, DeclaredConfigLimits, DeclaredConfigNal, DeclaredConfigParser,
        Framing, LengthSize, NalHeader, NalReader, ReaderLimits,
    },
};

use super::{
    envelope::{self, Event, Script},
    protocol::{Operation, Work},
};

const SOURCE: SourceId = SourceId(1);
const POOL_BYTES: usize = 32 << 20;
const NAL_BYTES: usize = 1 << 20;
const CONFIG_BYTES: usize = 1 << 20;
const START_CODE: &[u8] = &[0, 0, 0, 1];
static NO: AtomicBool = AtomicBool::new(false);
const HVC_LIMITS: HvcConfigLimits = HvcConfigLimits {
    maximum_record_bytes: 128 * 1024,
    maximum_arrays: 64,
    maximum_nals: 256,
    maximum_nal_bytes: 65_535,
};
const CONFIG_LIMITS: ConfigLimits = ConfigLimits {
    maximum_source_bytes: CONFIG_BYTES,
    maximum_parameter_sets: 512,
    reader: reader_limits(4096),
};

const fn reader_limits(window: usize) -> ReaderLimits {
    ReaderLimits {
        maximum_nal_bytes: NAL_BYTES,
        maximum_padding_bytes: 4096,
        maximum_nal_units: 4096,
        maximum_input_window: window,
    }
}

struct Output<'a, 't> {
    work: &'a mut Work,
    trace: Option<&'t mut dyn Write>,
}

impl Output<'_, '_> {
    fn record(&mut self, kind: u64, words: &[u64], payload: &[u8]) -> Result<(), String> {
        self.work.record(kind, words, payload);
        if let Some(trace) = self.trace.as_mut() {
            write!(trace, "{kind}").map_err(message)?;
            for word in words {
                write!(trace, " {word}").map_err(message)?;
            }
            if !payload.is_empty() {
                write!(trace, " {}", triergon_harness_protocol::hex(payload)).map_err(message)?;
            }
            writeln!(trace).map_err(message)?;
        }
        Ok(())
    }

    fn detail(&mut self, detail: impl std::fmt::Debug) -> Result<(), String> {
        if let Some(trace) = self.trace.as_mut() {
            writeln!(trace, "# {detail:?}").map_err(message)?;
        }
        Ok(())
    }

    fn refusal(&mut self, words: &[u64], error: Error) -> Result<(), String> {
        match error {
            Error::Invalid("completed before output") => Err(message(error)),
            Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated | Error::Limit { .. } => {
                self.detail(error)?;
                self.record(8, words, &[])
            }
            _ => Err(message(error)),
        }
    }

    fn config_unit(&mut self, nal_type: u8, payload: &[u8]) -> Result<(), String> {
        self.record(1, &[u64::from(nal_type), payload.len() as u64], payload)
    }
}

fn message(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn ready<T>(
    mut step: impl FnMut(&mut WorkBudget, &AtomicBool) -> triergon_core::Result<BuildStep<T>>,
) -> triergon_core::Result<T> {
    loop {
        match step(&mut WorkBudget::new(65_536), &NO)? {
            BuildStep::Ready(value) => return Ok(value),
            BuildStep::Yield => {}
            BuildStep::Complete => return Err(Error::Invalid("completed before output")),
        }
    }
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().ok_or("non-UTF-8 NAL argument"))
        .collect::<Result<Vec<_>, _>>()?;
    match (prefix, args.as_slice()) {
        ("nal-units-", [codec, window]) => {
            let codec = parse_codec(codec)?;
            let window = window
                .parse::<usize>()
                .ok()
                .filter(|n| matches!(n, 1 | 7 | 4096))
                .ok_or("NAL window must be 1, 7 or 4096")?;
            let script = Script::parse(input).map_err(message)?;
            let (script_codec, width_at) = match script.codec {
                envelope::Codec::Avc => (Codec::Avc, 4),
                envelope::Codec::Hevc => (Codec::Hevc, 21),
            };
            if codec != script_codec {
                return Err("NAL script codec differs from the argument".into());
            }
            let width = script
                .configuration
                .get(width_at)
                .ok_or("NAL script lacks a sample width")?;
            let width = LengthSize::new((width & 3) + 1).map_err(message)?;
            Ok(Operation {
                name: "nal-units",
                run: Box::new(move |work, trace| {
                    let mut output = Output { work, trace };
                    let pool = Budget::new(POOL_BYTES).map_err(message)?;
                    for (sample, data) in script
                        .events
                        .iter()
                        .filter_map(|event| match event {
                            Event::Packet { data, .. } => Some(*data),
                            Event::Flush | Event::Drain => None,
                        })
                        .enumerate()
                    {
                        let mut count = 0;
                        match read_sample(
                            &pool,
                            codec,
                            Framing::LengthPrefixed(width),
                            data,
                            window,
                            |unit| {
                                let facts = unit.facts();
                                let (first, second) = match facts.header {
                                    NalHeader::Avc { nal_ref_idc, .. } => (nal_ref_idc, 0),
                                    NalHeader::Hevc {
                                        layer_id,
                                        temporal_id_plus_one,
                                        ..
                                    } => (layer_id, temporal_id_plus_one),
                                };
                                output.record(
                                    1,
                                    &[
                                        sample as u64,
                                        count,
                                        facts.payload_offset,
                                        u64::from(facts.header.nal_unit_type()),
                                        u64::from(first),
                                        u64::from(second),
                                        facts.payload_length as u64,
                                    ],
                                    unit.bytes(),
                                )?;
                                count += 1;
                                Ok(())
                            },
                        )? {
                            Ok(()) => output.record(7, &[sample as u64, count], &[])?,
                            Err(error) => output.refusal(&[sample as u64], error)?,
                        }
                    }
                    Ok(())
                }),
            })
        }
        ("nal-config-", ["annexb", codec]) => {
            let codec = parse_codec(codec)?;
            Ok(Operation {
                name: "nal-config",
                run: Box::new(move |work, trace| {
                    annexb_config(input, codec, &mut Output { work, trace })
                }),
            })
        }
        ("nal-config-", ["declared", codec, width, declarations]) => {
            let codec = parse_codec(codec)?;
            let width = LengthSize::new(width.parse::<u8>().map_err(message)?).map_err(message)?;
            let layout = parse_layout(declarations)?;
            Ok(Operation {
                name: "nal-config",
                run: Box::new(move |work, trace| {
                    declared_config(input, codec, width, &layout, &mut Output { work, trace })
                }),
            })
        }
        ("hvcc-annexb-", [mode @ ("all" | "decoder")]) => {
            let decoder = *mode == "decoder";
            Ok(Operation {
                name: "hvcc-annexb",
                run: Box::new(move |work, trace| hvcc(input, decoder, &mut Output { work, trace })),
            })
        }
        _ => Err(format!("invalid arguments for {prefix}")),
    }
}

fn parse_codec(value: &str) -> Result<Codec, String> {
    match value {
        "avc" => Ok(Codec::Avc),
        "hevc" => Ok(Codec::Hevc),
        _ => Err("expected avc or hevc".into()),
    }
}

fn read_sample(
    pool: &Budget,
    codec: Codec,
    framing: Framing,
    sample: &[u8],
    window: usize,
    mut unit: impl FnMut(triergon_framing::nal::NalUnit) -> Result<(), String>,
) -> Result<triergon_core::Result<()>, String> {
    let bytes = Bytes::copy_from_slice(pool, sample).map_err(message)?;
    let mut reader = NalReader::new(
        pool,
        codec,
        framing,
        SOURCE,
        SourceCapabilities {
            length: Some(sample.len() as u64),
            random_access: false,
        },
        reader_limits(window),
    )
    .map_err(message)?;
    loop {
        match reader.step(&mut WorkBudget::new(65_536), &NO) {
            Ok(Step::Ready(nal)) => unit(nal)?,
            Ok(Step::Yield) => {}
            Ok(Step::End(end)) if end.offset == sample.len() as u64 => return Ok(Ok(())),
            Ok(Step::End(_)) => return Err("reader ended before sample end".into()),
            Ok(Step::NeedInput(request)) => {
                let start = usize::try_from(request.offset).map_err(message)?;
                if start == sample.len() {
                    reader
                        .finish(request.source, request.token)
                        .map_err(message)?;
                } else {
                    let end = sample.len().min(start + request.maximum.min(window));
                    let slice = bytes.slice(start..end).map_err(message)?;
                    let window =
                        InputWindow::new(request.source, request.token, request.offset, slice)
                            .map_err(message)?;
                    reader.provide_owned(window).map_err(message)?;
                }
            }
            Err(error) => return Ok(Err(error)),
        }
    }
}

fn annexb_config(input: &[u8], codec: Codec, output: &mut Output<'_, '_>) -> Result<(), String> {
    let pool = Budget::new(POOL_BYTES).map_err(message)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(message)?;
    let config =
        AnnexBConfigParser::new(&pool, codec, SOURCE, bytes, Framing::AnnexB, CONFIG_LIMITS)
            .and_then(|mut parser| ready(|work, cancel| parser.step(work, cancel)));
    let config = match config {
        Ok(config) => config,
        Err(error) => return output.refusal(&[], error),
    };
    output.detail((
        config.encoding(),
        config.inspection(),
        config.sample_framing(),
    ))?;
    for parameter in config.parameter_sets() {
        let mut writer = AnnexBWriter::new(&pool, parameter.unit().clone(), CONFIG_BYTES + 4)
            .map_err(message)?;
        let framed = ready(|work, cancel| writer.step(work, cancel)).map_err(message)?;
        let nal = framed
            .bytes()
            .strip_prefix(START_CODE)
            .ok_or("Annex B writer did not emit a four-byte start code")?;
        let nal_type = match parameter.kind() {
            triergon_framing::nal::ParameterSetKind::AvcSps => 7,
            triergon_framing::nal::ParameterSetKind::AvcPps => 8,
            triergon_framing::nal::ParameterSetKind::HevcVps => 32,
            triergon_framing::nal::ParameterSetKind::HevcSps => 33,
            triergon_framing::nal::ParameterSetKind::HevcPps => 34,
        };
        output.config_unit(nal_type, nal)?;
    }
    output.record(7, &[config.parameter_sets().len() as u64], &[])
}

struct DeclaredArray {
    range: Range<usize>,
    nal_type: u8,
    completeness: Option<bool>,
    nals: Vec<DeclaredConfigNal>,
}

// Each array is type, completeness, start, end, then triples of
// length start, payload start and payload end. A dash means no completeness.
fn parse_layout(argument: &str) -> Result<Vec<DeclaredArray>, String> {
    let mut layout = Vec::new();
    for array in argument.split(';').filter(|array| !array.is_empty()) {
        let mut fields = array.split(',');
        let mut next = || fields.next().ok_or("incomplete declaration");
        let nal_type = next()?.parse::<u8>().map_err(message)?;
        let completeness = match next()? {
            "-" => None,
            "0" => Some(false),
            "1" => Some(true),
            _ => return Err("invalid declared completeness".into()),
        };
        let start = next()?.parse::<usize>().map_err(message)?;
        let end = next()?.parse::<usize>().map_err(message)?;
        let positions = fields
            .map(str::parse::<usize>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(message)?;
        let (triples, remainder) = positions.as_chunks::<3>();
        if !remainder.is_empty() {
            return Err("incomplete NAL declaration".into());
        }
        let nals = triples
            .iter()
            .map(|&[start, payload, end]| DeclaredConfigNal {
                length_field: start..payload,
                payload: payload..end,
            })
            .collect();
        layout.push(DeclaredArray {
            range: start..end,
            nal_type,
            completeness,
            nals,
        });
    }
    Ok(layout)
}

fn declared_config(
    input: &[u8],
    codec: Codec,
    width: LengthSize,
    layout: &[DeclaredArray],
    output: &mut Output<'_, '_>,
) -> Result<(), String> {
    let pool = Budget::new(POOL_BYTES).map_err(message)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(message)?;
    let encoding = match codec {
        Codec::Avc => ConfigEncoding::AvcDecoderConfigurationRecord,
        Codec::Hevc => ConfigEncoding::HevcDecoderConfigurationRecord,
    };
    let raw = match Configuration::declared_record(
        &pool,
        codec,
        encoding,
        bytes.clone(),
        Framing::LengthPrefixed(width),
        CONFIG_LIMITS,
    ) {
        Ok(raw) => raw,
        Err(error) => return output.refusal(&[], error),
    };
    output.detail((raw.encoding(), raw.inspection(), raw.sample_framing()))?;
    output.record(2, &[raw.source_bytes().len() as u64], raw.source_bytes())?;
    let arrays: Vec<_> = layout
        .iter()
        .map(|array| DeclaredConfigArray {
            raw_range: array.range.clone(),
            nal_unit_type: array.nal_type,
            completeness: array.completeness,
            nals: &array.nals,
        })
        .collect();
    let config = DeclaredConfigParser::new(
        &pool,
        codec,
        SOURCE,
        bytes,
        width,
        &arrays,
        DeclaredConfigLimits {
            maximum_record_bytes: CONFIG_BYTES,
            maximum_arrays: 256,
            maximum_nals: 4096,
            maximum_nal_bytes: NAL_BYTES,
        },
    )
    .and_then(|mut parser| ready(|work, cancel| parser.step(work, cancel)));
    let config = match config {
        Ok(config) => config,
        Err(error) => return output.refusal(&[], error),
    };
    if config.source_bytes().as_ref() != raw.source_bytes().as_ref() {
        return Err("declared parser changed the source record".into());
    }
    for nal in config.nals() {
        output.config_unit(nal.header().nal_unit_type(), nal.source_bytes())?;
    }
    output.record(7, &[config.nals().len() as u64], &[])
}

fn hvcc(input: &[u8], decoder: bool, output: &mut Output<'_, '_>) -> Result<(), String> {
    let pool = Budget::new(POOL_BYTES).map_err(message)?;
    let bytes = Bytes::copy_from_slice(&pool, input).map_err(message)?;
    let config = HvcConfigParser::new(
        &pool,
        HvcConfigSource {
            source: SOURCE,
            generation: 0,
            offset: 0,
        },
        bytes,
        HVC_LIMITS,
    )
    .and_then(|mut parser| ready(|work, cancel| parser.step(work, cancel)));
    let config = match config {
        Ok(config) => config,
        Err(error) => return output.refusal(&[], error),
    };
    if decoder {
        let mut writer =
            HvcDecoderConfigWriter::new(&pool, config, CONFIG_BYTES).map_err(message)?;
        match ready(|work, cancel| writer.step(work, cancel)) {
            Ok(HvcDecoderConfigOutput::Converted(converted)) => {
                hvcc_units(converted.bytes(), converted.configuration(), output, true)
            }
            Ok(HvcDecoderConfigOutput::Refused {
                configuration,
                reason,
            }) => {
                output.detail(reason)?;
                if configuration.source_bytes().as_ref() != input {
                    return Err("decoder refusal changed the source record".into());
                }
                output.record(8, &[u64::from(reason.declared_nal_type)], &[])
            }
            Err(error) => output.refusal(&[], error),
        }
    } else {
        let mut writer = HvcAnnexBWriter::new(&pool, config, CONFIG_BYTES).map_err(message)?;
        match ready(|work, cancel| writer.step(work, cancel)) {
            Ok(converted) => {
                hvcc_units(converted.bytes(), converted.configuration(), output, false)
            }
            Err(error) => output.refusal(&[], error),
        }
    }
}

fn hvcc_units(
    mut bytes: &[u8],
    source: &HvcConfiguration,
    output: &mut Output<'_, '_>,
    decoder: bool,
) -> Result<(), String> {
    for link in source.nals() {
        let payload = bytes
            .strip_prefix(START_CODE)
            .ok_or("hvcC writer did not emit a four-byte start code")?;
        let (nal, rest) = payload
            .split_at_checked(link.source_bytes().len())
            .ok_or("hvcC writer truncated a unit")?;
        let nal_type = nal.first().map_or(64, |first| (first >> 1) & 63);
        output.config_unit(nal_type, nal)?;
        bytes = rest;
    }
    if !bytes.is_empty() {
        return Err("hvcC writer emitted bytes after the final unit".into());
    }
    let end = [source.nal_count() as u64, 0];
    output.record(7, if decoder { &end } else { &end[..1] }, &[])
}
