use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon::framing::nal::{self, NalFraming, RawAccessUnit, RawAnnexBLimits, RawAnnexBReader};
use triergon::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_codec_h264 as h264;
use triergon_codec_hevc as hevc;
use triergon_harness_protocol::{digest::signed, interleaved::InterleavedDigest, md5};
use triergon_harness_runner::{BenchLine, Boundary, Invocation, Mode, Timer};
use triergon_pixel::letterbox::{Canvas, Scaler};
use triergon_pixel::{Colour, PictureView, Plane};

use triergon_codec_hevc::AccessUnitSplitter;
use triergon_harness_protocol::digest::fold_bytes;

use super::protocol::{Operation, Work};

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    match prefix {
        "hevc-access-units-" => access_units(input, arguments),
        "h264-inspect-" => inspect(input, arguments),
        "picture-props-" | "video-nal-" => decode_operation(prefix, input, arguments),
        _ => Err(format!("usage: {prefix} operation is unavailable")),
    }
}

fn access_units<'a>(input: &'a [u8], arguments: &[OsString]) -> Result<Operation<'a>, String> {
    if !matches!(arguments, [window] if window == "4096" || window == "whole") {
        return Err("usage: hevc-access-units FILE whole|4096".into());
    }
    Ok(Operation {
        name: "hevc-access-units",
        run: Box::new(move |work, mut trace| {
            for unit in AccessUnitSplitter::new(input) {
                work.record(1, &[unit.len() as u64], unit);
                if let Some(trace) = &mut trace {
                    let mut digest = 0;
                    fold_bytes(&mut digest, unit);
                    writeln!(trace, "1 {} {digest}", unit.len())
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        }),
    })
}

pub(super) fn command(command: &str, arguments: &[OsString]) -> Result<(), String> {
    let (operation, mode) = command.rsplit_once('-').ok_or("expected video operation")?;
    let codec = match operation {
        "video-h264" => nal::Codec::Avc,
        "video-hevc" => nal::Codec::Hevc,
        _ => return Err(format!("unknown operation {operation}")),
    };
    let args: Vec<_> = arguments
        .iter()
        .map(|arg| arg.to_str().map(str::to_owned).ok_or("invalid argument"))
        .collect::<Result<_, _>>()?;
    let raw_mode = mode == "raw";
    let invocation = Invocation::parse_mode(
        if raw_mode { "fold" } else { mode },
        &args,
        None,
        || "expected FILE [ITERATIONS|OUTPUT]",
    )
    .map_err(|error| error.to_string())?;
    invocation
        .check_iterations(|| "invalid iteration count")
        .map_err(|error| error.to_string())?;
    let mut raw = match invocation.arguments {
        [output] if raw_mode => Some(std::io::BufWriter::new(
            std::fs::File::create(output).map_err(|error| error.to_string())?,
        )),
        [] if !raw_mode => None,
        _ => return Err("unexpected video workload arguments".into()),
    };
    let input = invocation
        .input(256 << 20)
        .map_err(|error| error.to_string())?;
    let memory = invocation.mode == Mode::Memory;
    let mut stdout = std::io::stdout().lock();
    if memory {
        Boundary::Begin
            .mark(&mut stdout)
            .map_err(|error| error.to_string())?;
    }
    let mut packets = 0;
    let mut bytes = 0;
    let mut digest = InterleavedDigest::default();
    let timer = Timer::start(invocation.mode == Mode::Bench);
    for _ in 0..invocation.iterations {
        let mut scaler = Scaler::default();
        let mut rgba = vec![0; Canvas::default().bytes()];
        decode(&input, codec, Feed::Nal, |picture| {
            scaler.letterbox(&picture.view(), &mut rgba);
            packets += 1;
            bytes += rgba.len() as u64;
            digest.update(&rgba);
            if let Some(raw) = &mut raw {
                raw.write_all(&rgba).map_err(|error| error.to_string())?;
            }
            Ok(())
        })?;
    }
    let elapsed = timer.elapsed().as_nanos();
    let digest = digest.finish();
    if memory {
        Boundary::End
            .mark(&mut stdout)
            .map_err(|error| error.to_string())?;
        writeln!(stdout, "{{\"packets\":{packets},\"bytes\":{bytes}}}")
            .map_err(|error| error.to_string())?;
        eprintln!("{{\"digest\":{digest}}}");
    } else if invocation.mode == Mode::Bench {
        let line = BenchLine::with_schema(invocation.iterations, elapsed, (packets, bytes, digest));
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    } else {
        if let Some(raw) = &mut raw {
            raw.flush().map_err(|error| error.to_string())?;
        }
        writeln!(
            stdout,
            "{{\"packets\":{packets},\"bytes\":{bytes},\"digest\":{digest}}}"
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Feed {
    AccessUnit,
    Nal,
    Split,
}

fn decode_operation<'a>(
    prefix: &str,
    input: &'a [u8],
    args: &[OsString],
) -> Result<Operation<'a>, String> {
    let properties = prefix == "picture-props-";
    let (codec, feed) = match args {
        [codec] if properties => (codec, Feed::AccessUnit),
        [codec, feed] if !properties && feed == "nal" => (codec, Feed::Nal),
        [codec, feed] if !properties && codec == "h264" && feed == "nals-split" => {
            (codec, Feed::Split)
        }
        _ => return Err("expected h264|hevc and, for NAL input, nal|nals-split".into()),
    };
    let codec = match codec.to_str() {
        Some("h264") => nal::Codec::Avc,
        Some("hevc") => nal::Codec::Hevc,
        _ => return Err("expected h264 or hevc".into()),
    };
    Ok(Operation {
        name: if properties {
            "picture-properties"
        } else {
            "video-nal-input"
        },
        run: Box::new(move |work, mut trace| {
            let mut scratch = Vec::new();
            let refused = decode(input, codec, feed, |picture| {
                output_picture(&picture, properties, work, &mut trace, &mut scratch)
            })?;
            if refused && work.packets == 0 {
                work.record(8, &[], &[]);
                if let Some(trace) = trace {
                    writeln!(trace, "8").map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        }),
    })
}

/// The input schedule varies at byte offsets. The reader retains split syntax
/// and submits only complete NAL units to the decoder's Annex B interface.
fn units(
    input: &[u8],
    codec: nal::Codec,
    feed: Feed,
    mut receive: impl FnMut(RawAccessUnit) -> Result<(), String>,
) -> Result<bool, String> {
    let source = SourceId(0);
    let caps = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    };
    let pool = Budget::new(32 << 20).map_err(|error| error.to_string())?;
    let mut reader = RawAnnexBReader::new(&pool, codec, source, caps, RawAnnexBLimits::default())
        .map_err(|error| error.to_string())?;
    let mut work = WorkBudget::new(64 << 10);
    let cancel = AtomicBool::new(false);
    let mut seed = 0x7769_6e64_u32;
    loop {
        match reader.step(&mut work, &cancel) {
            Ok(Step::Ready(unit)) => receive(unit)?,
            Ok(Step::NeedInput(request)) => {
                let at = request.offset as usize;
                if at >= input.len() {
                    reader
                        .finish(request.source, request.token)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let window = if matches!(feed, Feed::Split) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    1 + (seed as usize >> 16) % 4096
                } else {
                    request.maximum
                };
                let end = at + window.min(request.maximum).min(input.len() - at);
                let bytes =
                    Bytes::copy_from_slice(&pool, &input[at..end]).map_err(|e| e.to_string())?;
                let window = InputWindow::new(request.source, request.token, request.offset, bytes)
                    .map_err(|error| error.to_string())?;
                reader
                    .provide_owned(window)
                    .map_err(|error| error.to_string())?;
            }
            Ok(Step::Yield) => work = WorkBudget::new(64 << 10),
            Ok(Step::End(_)) => return Ok(false),
            Ok(Step::Backpressure(never)) => match never {},
            Err(error)
                if matches!(
                    error,
                    triergon::Error::Allocation | triergon::Error::Backpressure(_)
                ) =>
            {
                return Err(error.to_string());
            }
            Err(error) => {
                eprintln!("framing: {error}");
                return Ok(true);
            }
        }
    }
}

enum Picture {
    Avc(h264::Picture),
    Hevc(hevc::Picture),
}

impl Picture {
    fn view(&self) -> PictureView<'_> {
        match self {
            Self::Avc(picture) => PictureView {
                width: picture.width,
                height: picture.height,
                bit_depth: picture.bit_depth as u8,
                planes: std::array::from_fn(|i| {
                    let (data, stride, offset) = picture.planes[i].raw();
                    Plane::U8 {
                        data,
                        stride,
                        offset,
                    }
                }),
                pts: picture.pts,
                sar: picture.sar,
                colour: Colour {
                    matrix: picture.colour.matrix,
                    full_range: picture.colour.full_range,
                },
            },
            Self::Hevc(picture) => PictureView {
                width: picture.width as usize,
                height: picture.height as usize,
                bit_depth: picture.bit_depth as u8,
                planes: std::array::from_fn(|i| {
                    let (row, stride, offset) = picture.planes[i].raw();
                    match row {
                        hevc::Row::U8(data) => Plane::U8 {
                            data,
                            stride,
                            offset,
                        },
                        hevc::Row::U16(data) => Plane::U16 {
                            data,
                            stride,
                            offset,
                        },
                    }
                }),
                pts: picture.pts,
                sar: picture.sar,
                colour: Colour {
                    matrix: picture.colour.matrix,
                    full_range: picture.colour.full_range,
                },
            },
        }
    }
}

trait OutputDecoder {
    fn frame(&mut self) -> Option<Picture>;
}

impl OutputDecoder for h264::Decoder {
    fn frame(&mut self) -> Option<Picture> {
        self.receive().map(Picture::Avc)
    }
}

impl OutputDecoder for hevc::Decoder {
    fn frame(&mut self) -> Option<Picture> {
        self.receive().map(Picture::Hevc)
    }
}

fn drain(
    decoder: &mut impl OutputDecoder,
    output: &mut impl FnMut(Picture) -> Result<(), String>,
) -> Result<(), String> {
    while let Some(picture) = decoder.frame() {
        output(picture)?;
    }
    Ok(())
}

fn decode(
    input: &[u8],
    codec: nal::Codec,
    feed: Feed,
    mut output: impl FnMut(Picture) -> Result<(), String>,
) -> Result<bool, String> {
    let mut refused = false;
    let mut pts = 0_i64;
    if codec == nal::Codec::Avc {
        let mut decoder = h264::Decoder::new();
        let framing = units(input, codec, feed, |unit| {
            let data = &input[unit.range.start as usize..unit.range.end as usize];
            match feed {
                Feed::AccessUnit => refused |= decoder.send(data, Some(pts)).is_err(),
                Feed::Nal => {
                    for (index, nal) in unit.nals().iter().enumerate() {
                        refused |= decoder
                            .send_nal(nal.bytes().as_ref(), (index == 0).then_some(pts))
                            .is_err();
                    }
                }
                Feed::Split => {
                    for (index, nal) in unit.nals().iter().enumerate() {
                        let facts = nal.facts();
                        let NalFraming::AnnexB {
                            start_code_bytes, ..
                        } = facts.framing
                        else {
                            return Err("a raw access unit lacks an Annex B prefix".into());
                        };
                        let start = facts.payload_offset as usize - usize::from(start_code_bytes);
                        let end = facts.payload_offset as usize + facts.payload_length;
                        refused |= decoder
                            .send_nals(&input[start..end], (index == 0).then_some(pts))
                            .is_err();
                    }
                }
            }
            pts += 1;
            drain(&mut decoder, &mut output)
        })?;
        refused |= framing;
        decoder.flush();
        drain(&mut decoder, &mut output)?;
    } else {
        let mut decoder = hevc::Decoder::new();
        if matches!(feed, Feed::AccessUnit) {
            for unit in AccessUnitSplitter::new(input) {
                refused |= decoder.send(unit, Some(pts)).is_err();
                pts += 1;
                drain(&mut decoder, &mut output)?;
            }
        } else {
            let framing = units(input, codec, feed, |unit| {
                for (index, nal) in unit.nals().iter().enumerate() {
                    refused |= decoder
                        .send_nal(nal.bytes().as_ref(), (index == 0).then_some(pts))
                        .is_err();
                }
                pts += 1;
                drain(&mut decoder, &mut output)
            })?;
            refused |= framing;
        }
        decoder.flush();
        drain(&mut decoder, &mut output)?;
    }
    Ok(refused)
}

fn sar((num, den): (u32, u32)) -> (u64, u64) {
    if let Ok(base) = triergon::TimeBase::new(u64::from(num), u64::from(den)) {
        (base.numerator(), base.denominator())
    } else {
        (0, 0)
    }
}

fn output_picture(
    picture: &Picture,
    properties: bool,
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    scratch: &mut Vec<u8>,
) -> Result<(), String> {
    let (geometry, pts, facts, hashes): (_, _, _, [[u8; 16]; 3]) = match picture {
        Picture::Avc(p) => {
            let (num, den) = sar(p.sar);
            let colour = p.colour;
            let hashes = std::array::from_fn(|index| {
                let plane = &p.planes[index];
                scratch.clear();
                for y in 0..plane.height {
                    scratch.extend(plane.row(y));
                }
                md5::digest(scratch)
            });
            (
                [p.width as u64, p.height as u64, u64::from(p.bit_depth), 3],
                p.pts,
                [
                    num,
                    den,
                    u64::from(colour.primaries),
                    u64::from(colour.transfer),
                    u64::from(colour.matrix),
                    u64::from(colour.full_range),
                    u64::from(p.interlaced),
                ],
                hashes,
            )
        }
        Picture::Hevc(p) => {
            let (num, den) = sar(p.sar);
            let colour = p.colour;
            let hashes = std::array::from_fn(|index| {
                scratch.clear();
                p.planes[index].write_raw(scratch);
                md5::digest(scratch)
            });
            (
                [
                    u64::from(p.width),
                    u64::from(p.height),
                    u64::from(p.bit_depth),
                    3,
                ],
                p.pts,
                [
                    num,
                    den,
                    u64::from(colour.primaries),
                    u64::from(colour.transfer),
                    u64::from(colour.matrix),
                    if colour.video_signal_type_present {
                        1 + u64::from(colour.full_range)
                    } else {
                        0
                    },
                    0,
                ],
                hashes,
            )
        }
    };
    let mut fields = [0; 12];
    fields[..4].copy_from_slice(&geometry);
    let (kind, count) = if properties {
        fields[4..11].copy_from_slice(&facts);
        (4, 11)
    } else if let Some(pts) = pts {
        fields[4] = 1;
        fields[5] = signed(pts);
        (1, 6)
    } else {
        (1, 5)
    };
    let payload = hashes.as_flattened();
    fields[count] = payload.len() as u64;
    let fields = &fields[..=count];
    work.record(kind, fields, payload);
    if let Some(trace) = trace {
        write!(trace, "{kind}").map_err(|error| error.to_string())?;
        for value in fields {
            write!(trace, " {value}").map_err(|error| error.to_string())?;
        }
        writeln!(trace, " {}", triergon_harness_protocol::hex(payload))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn inspect<'a>(data: &'a [u8], args: &[OsString]) -> Result<Operation<'a>, String> {
    if !args.is_empty() {
        return Err("H.264 inspection takes no arguments".into());
    }
    Ok(Operation {
        name: "h264-stream-inspection",
        run: Box::new(move |work, mut trace| {
            let features = h264::inspect::inspect(data);
            let mut id = 0;
            let mut emit = |values: Vec<(u64, u64)>| -> Result<(), String> {
                let count = values.len() as u64;
                work.record(3, &[id, count], &[]);
                if let Some(trace) = &mut trace {
                    writeln!(trace, "3 {id} {count}").map_err(|error| error.to_string())?;
                }
                for (a, b) in values {
                    work.record(3, &[id, a, b], &[]);
                    if let Some(trace) = &mut trace {
                        writeln!(trace, "3 {id} {a} {b}").map_err(|error| error.to_string())?;
                    }
                }
                id += 1;
                Ok(())
            };
            fn singles(values: impl IntoIterator<Item = impl Into<u64>>) -> Vec<(u64, u64)> {
                values.into_iter().map(|value| (value.into(), 0)).collect()
            }
            emit(singles(features.profiles))?;
            emit(singles(features.constraint_flags))?;
            emit(singles(features.chroma_format_idc))?;
            emit(singles(features.bit_depth))?;
            emit(
                features
                    .sizes
                    .into_iter()
                    .map(|(a, b)| (a.into(), b.into()))
                    .collect(),
            )?;
            emit(singles(features.cabac))?;
            emit(singles(features.frame_mbs_only))?;
            for flag in [features.mbaff_sps, features.field_pics, features.mbaff_pics] {
                emit(singles([flag]))?;
            }
            emit(singles(features.slice_types.into_iter().map(
                |kind| match kind {
                    "B" => 0_u8,
                    "I" => 1,
                    "P" => 2,
                    "SI" => 3,
                    "SP" => 4,
                    _ => 255,
                },
            )))?;
            for flag in [
                features.fmo,
                features.redundant_pic_cnt_present,
                features.redundant_slices,
                features.weighted_explicit,
                features.weighted_implicit,
                features.transform_8x8,
                features.scaling_matrices,
            ] {
                emit(singles([flag]))?;
            }
            emit(singles(features.poc_types))?;
            for flag in [
                features.constrained_intra_pred,
                features.direct_spatial,
                features.direct_temporal,
                features.ref_list_mods,
            ] {
                emit(singles([flag]))?;
            }
            emit(singles(features.mmco_ops))?;
            emit(singles([features.long_term_idr]))?;
            emit(singles([features.multi_slice]))?;
            emit(singles(features.nal_types))?;
            for flag in [features.gaps_allowed, features.lossless, features.crop] {
                emit(singles([flag]))?;
            }
            emit(singles([features.max_slices_per_pic]))?;
            emit(
                features
                    .bitstream_restriction
                    .into_iter()
                    .map(|(a, b)| (a.into(), b.into()))
                    .collect(),
            )?;
            emit(singles([!features.errors.is_empty()]))?;
            if trace.is_some() {
                for error in &features.errors {
                    eprintln!("{error}");
                }
            }
            Ok(())
        }),
    })
}
