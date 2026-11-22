use std::io::Write;

use triergon_core::{Budget, Bytes, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget};
use triergon_framing::{
    BuildStep,
    ac3::{
        Ac3Frame, Ac3FrameDetails, Ac3FrameHeader, Ac3FrameLimits, Ac3FrameReader,
        Ac3StreamSelection, Eac3BitRange, Eac3Bsi, Eac3BsiLimits, Eac3BsiParser,
    },
};

use super::{CANCEL, error, record, refusal};
use crate::protocol::{Action, Work};

const SOURCE: SourceId = SourceId(1);
const FRAME_BYTES: usize = 4096;
const BSI_BITS: usize = FRAME_BYTES * 8;

fn frames(
    input: &[u8],
    pool: &Budget,
    mut observe: impl FnMut(Ac3Frame) -> Result<bool, String>,
) -> Result<Option<u64>, String> {
    let capabilities = SourceCapabilities {
        length: Some(input.len() as u64),
        random_access: false,
    };
    let mut reader = Ac3FrameReader::new(
        pool,
        SOURCE,
        capabilities,
        Ac3StreamSelection::Eac3Only,
        Ac3FrameLimits {
            maximum_frame_bytes: FRAME_BYTES,
            maximum_frames: input.len() / 6 + 1,
            maximum_input_window: input.len().max(1),
        },
    )
    .map_err(error)?;
    let source = Bytes::copy_from_slice(pool, input).map_err(error)?;
    let mut frame_offset = 0;
    loop {
        match reader.step(&mut WorkBudget::new(16384), &CANCEL) {
            Ok(Step::Ready(frame)) => {
                frame_offset = frame.facts().offset + frame.bytes().len() as u64;
                if !observe(frame)? {
                    return Ok(None);
                }
            }
            Ok(Step::NeedInput(request)) if request.offset == input.len() as u64 => {
                reader.finish(SOURCE, request.token).map_err(error)?;
            }
            Ok(Step::NeedInput(request)) => {
                let start = request.offset as usize;
                let end = start.saturating_add(request.maximum).min(input.len());
                let bytes = source.slice(start..end).map_err(error)?;
                let window = InputWindow::new(SOURCE, request.token, request.offset, bytes)
                    .map_err(error)?;
                reader.provide_owned(window).map_err(error)?;
            }
            Ok(Step::End(_)) => return Ok(None),
            Ok(Step::Yield) => {}
            Ok(Step::Backpressure(_)) => return Err("E-AC-3 comparison budget exhausted".into()),
            Err(value) => {
                refusal(value)?;
                return Ok(Some(frame_offset));
            }
        }
    }
}

fn parse(
    pool: &Budget,
    frame: Ac3Frame,
    maximum_bsi_bits: usize,
    maximum_frame_bytes: usize,
) -> Result<Option<Eac3Bsi>, String> {
    let limits = Eac3BsiLimits {
        maximum_frame_bytes,
        maximum_bsi_bits,
    };
    let result = Eac3BsiParser::new(pool, frame, limits)
        .and_then(|mut parser| parser.step(&mut WorkBudget::new(BSI_BITS as u64 + 1), &CANCEL));
    match result {
        Ok(BuildStep::Ready(bsi)) => Ok(Some(bsi)),
        Ok(_) => Err("E-AC-3 BSI parser did not complete".into()),
        Err(value) => {
            refusal(value)?;
            Ok(None)
        }
    }
}

pub(super) fn inspect(input: &[u8], bits: usize, bytes: usize) -> Action<'_> {
    Box::new(move |work, mut trace| {
        let pool = Budget::new(input.len() + 32768).map_err(error)?;
        let mut count = 0;
        let mut refused = false;
        let failure = frames(input, &pool, |frame| {
            let offset = frame.facts().offset;
            let Some(bsi) = parse(&pool, frame, bits, bytes)? else {
                refused = true;
                record(work, &mut trace, 8, &[offset], &[])?;
                return Ok(false);
            };
            wire_rows(&bsi, work, &mut trace)?;
            count += 1;
            Ok(true)
        })?;
        if let Some(offset) = failure {
            record(work, &mut trace, 8, &[offset], &[])?;
        } else if !refused {
            record(work, &mut trace, 7, &[count], &[])?;
        }
        Ok(())
    })
}

fn wire_rows(
    bsi: &Eac3Bsi,
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
) -> Result<(), String> {
    fn scalar(value: Option<impl Into<u64>>) -> [u64; 3] {
        [u64::from(value.is_some()), value.map_or(0, Into::into), 0]
    }
    fn range(value: Option<Eac3BitRange>) -> [u64; 3] {
        value.map_or([0; 3], |r| [1, r.start_bit() as u64, r.bit_count() as u64])
    }
    let frame = bsi.frame().facts();
    let f = bsi.facts();
    record(
        work,
        trace,
        1,
        &[frame.offset, frame.header.frame_bytes as u64],
        &[],
    )?;
    let rows = [
        scalar(Some(f.end_bit as u64)),
        scalar(Some(f.prefix_end_bit as u64)),
        range(f.mixing_metadata),
        scalar(f.mixing_definition),
        range(f.informational_metadata),
        scalar(f.bitstream_mode),
        scalar(f.copyright),
        scalar(f.original),
        scalar(f.surround_mode),
        scalar(f.headphone_mode),
        scalar(f.surround_ex_mode),
        scalar(f.production),
        scalar(f.second_production),
        scalar(f.source_fscod),
        scalar(f.converter_sync),
        scalar(f.block_id),
        scalar(f.effective_block_id),
        scalar(f.converted_frame_size_code),
        range(f.additional_bsi),
        scalar(Some(f.trailer.auxiliary_data_present)),
        scalar(f.trailer.auxiliary_length_code),
        scalar(Some(f.trailer.encoder_info)),
        scalar(Some(f.trailer.crc2_word)),
        scalar(bsi.channel_map_channels()),
        scalar(bsi.channel_map_matches_coded_count()),
    ];
    for (index, [present, first, last]) in rows.into_iter().enumerate() {
        record(work, trace, 3, &[index as u64, present, first, last], &[])?;
    }
    if let Some(range) = f.additional_bsi {
        let span = bsi.bit_span(range).map_err(error)?;
        let bytes = span.covering_bytes().map_err(error)?;
        record(work, trace, 2, &[bytes.len() as u64], bytes.as_ref())?;
    }
    Ok(())
}

pub(super) fn decoded(input: &[u8]) -> Action<'_> {
    Box::new(move |work, mut trace| {
        let pool = Budget::new(input.len() + 32768).map_err(error)?;
        let mut packet: Option<Packet> = None;
        let mut count = 0;
        let mut refused = false;
        let failure = frames(input, &pool, |frame| {
            let offset = frame.facts().offset;
            let header = frame.facts().header;
            let Ac3FrameDetails::Eac3 {
                stream_type,
                channel_map,
                ..
            } = header.details
            else {
                return Err("unexpected non-E-AC-3 frame".into());
            };
            if stream_type != 1
                && let Some(previous) = packet.take()
            {
                previous.emit(work, &mut trace)?;
                count += 1;
            }
            let Some(bsi) = parse(&pool, frame, BSI_BITS, FRAME_BYTES)? else {
                packet = None;
                record(work, &mut trace, 8, &[offset], &[])?;
                refused = true;
                return Ok(false);
            };
            if stream_type == 1 {
                let group = packet
                    .as_mut()
                    .ok_or("dependent E-AC-3 frame without a primary frame")?;
                group.size += header.frame_bytes;
                group.layout |= channel_map.map_or_else(|| layout(header), custom_layout);
            } else {
                packet = Some(Packet::new(&bsi));
            }
            Ok(true)
        })?;
        if let Some(previous) = packet {
            previous.emit(work, &mut trace)?;
            count += 1;
        }
        if let Some(offset) = failure {
            record(work, &mut trace, 8, &[offset], &[])
        } else if refused {
            Ok(())
        } else {
            record(work, &mut trace, 7, &[count], &[])
        }
    })
}

struct Packet {
    offset: u64,
    size: usize,
    rate: u32,
    samples: u16,
    layout: u64,
    service: u8,
    matrix: u8,
    mix: Option<[u64; 6]>,
}

impl Packet {
    fn new(bsi: &Eac3Bsi) -> Self {
        let frame = bsi.frame().facts();
        let header = frame.header;
        let facts = bsi.facts();
        let service = facts.bitstream_mode.unwrap_or(0);
        let matrix = match (
            facts.surround_mode,
            facts.headphone_mode,
            facts.surround_ex_mode,
        ) {
            (_, Some(2), _) => 6,
            (Some(2), _, _) => 1,
            (_, _, Some(2)) => 5,
            (_, _, Some(3)) => 4,
            _ => 0,
        };
        Self {
            offset: frame.offset,
            size: header.frame_bytes,
            rate: header.sample_rate_hz,
            samples: header.coded_samples(),
            layout: layout(header),
            service: if service == 7 && header.acmod > 1 {
                8
            } else {
                service
            },
            matrix,
            mix: facts
                .mixing_metadata
                .filter(|_| header.acmod > 2)
                .map(|range| mix_levels(bsi.frame().bytes(), range, header)),
        }
    }

    fn emit(self, work: &mut Work, trace: &mut Option<&mut dyn Write>) -> Result<(), String> {
        record(
            work,
            trace,
            1,
            &[
                self.offset,
                self.size as u64,
                self.rate.into(),
                self.samples.into(),
                self.layout.count_ones().into(),
                self.layout,
                self.service.into(),
                self.matrix.into(),
            ],
            &[],
        )?;
        if let Some(levels) = self.mix {
            record(work, trace, 3, &levels, &[])?;
        }
        Ok(())
    }
}

// Native AVChannelLayout masks in the published AVChannel order.
fn layout(header: Ac3FrameHeader) -> u64 {
    [3, 4, 3, 7, 259, 263, 1539, 1543][usize::from(header.acmod)]
        | if header.lfe_on { 8 } else { 0 }
}

fn custom_layout(map: u16) -> u64 {
    const LOCATION_MASKS: [u64; 16] = [
        1,
        4,
        2,
        1 << 9,
        1 << 10,
        (1 << 6) | (1 << 7),
        (1 << 4) | (1 << 5),
        1 << 8,
        1 << 11,
        (1 << 33) | (1 << 34),
        (1 << 31) | (1 << 32),
        (1 << 12) | (1 << 14),
        1 << 13,
        (1 << 36) | (1 << 37),
        1 << 35,
        1 << 3,
    ];
    LOCATION_MASKS
        .iter()
        .enumerate()
        .filter(|(bit, _)| map & (0x8000 >> bit) != 0)
        .fold(0, |layout, (_, mask)| layout | mask)
}

fn mix_levels(bytes: &Bytes, range: Eac3BitRange, header: Ac3FrameHeader) -> [u64; 6] {
    let mut bit = range.start_bit() + 1;
    let mut field = |width: usize| {
        let start = bit;
        bit += width;
        (start..bit).fold(0_u8, |value, k| {
            value << 1 | (bytes[k / 8] >> (7 - k % 8)) & 1
        })
    };
    let mode = header.acmod;
    // The wire assigns 1 to Lt/Rt and 2 to Lo/Ro. AVDownmixType reverses these.
    let downmix = [0_u64, 2, 1, 3][usize::from(field(2))];
    let (center_ltrt, center) = if matches!(mode, 3 | 5 | 7) {
        (field(3), field(3))
    } else {
        (0, 4)
    };
    let (surround_ltrt, surround) = if mode >= 4 {
        (field(3), field(3))
    } else {
        (0, 5)
    };
    let lfe = if header.lfe_on && field(1) != 0 {
        let db = 10.0 - f64::from(field(5));
        (10_f64.powf(db / 20.0) * 100_000.0).round() as u64
    } else {
        0
    };
    // The E-AC-3 mix-level table, scaled to five decimal places.
    const LEVEL: [u64; 8] = [141421, 118921, 100000, 84090, 70711, 59460, 50000, 0];
    [
        downmix,
        LEVEL[usize::from(center)],
        LEVEL[usize::from(center_ltrt)],
        LEVEL[usize::from(surround)],
        LEVEL[usize::from(surround_ltrt)],
        lfe,
    ]
}
