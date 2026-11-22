use std::{ffi::OsString, sync::atomic::AtomicBool};

use triergon_core::{Budget, Bytes, Error, SourceId, WorkBudget};
use triergon_framing::{
    BuildStep,
    aac::{AacAdtsParser, AacSource, inspect_adts_header},
};

use super::{obu::record, protocol::Operation};

static CANCEL: AtomicBool = AtomicBool::new(false);

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    if !arguments.is_empty() {
        return Err("ADTS operations take no arguments".into());
    }
    let wire = prefix == "adts-wire-";
    Ok(Operation {
        name: if wire {
            "aac-adts-header"
        } else {
            "aac-adts-frames"
        },
        run: Box::new(move |work, mut trace| {
            let pool = Budget::new(input.len().saturating_add(16_384)).map_err(message)?;
            let source = Bytes::copy_from_slice(&pool, input).map_err(message)?;
            if input.is_empty() {
                return match AacAdtsParser::new(&pool, location(0), source, 8191) {
                    Err(Error::Truncated) => record(work, &mut trace, 8, &[0], &[]),
                    other => Err(format!("unexpected empty ADTS buffer outcome: {other:?}")),
                };
            }
            let mut offset = 0;
            let mut count = 0;
            while offset < input.len() {
                let inspected = inspect_adts_header(&input[offset..]).ok();
                if let Some(header) = inspected.filter(|_| wire) {
                    let words = [
                        offset as u64,
                        u64::from(header.syncword()),
                        u64::from(header.mpeg2_id()),
                        u64::from(header.layer()),
                        u64::from(header.protection_absent()),
                        u64::from(header.profile_code()),
                        u64::from(header.frequency_index()),
                        u64::from(header.private_bit()),
                        u64::from(header.channel_configuration()),
                        u64::from(header.original_copy()),
                        u64::from(header.home()),
                        u64::from(header.copyright_id_bit()),
                        u64::from(header.copyright_id_start()),
                        u64::from(header.frame_length()),
                        u64::from(header.buffer_fullness()),
                        u64::from(header.raw_blocks_minus_one()),
                        u64::from(header.selected_wire_issues()),
                        header.single_block_header_bytes().unwrap_or(0) as u64,
                        u64::from(header.coded_frequency_hz().unwrap_or(0)),
                        u64::from(header.coded_channel_count().unwrap_or(0)),
                        7,
                    ];
                    record(work, &mut trace, 3, &words, &header.raw_prefix())?;
                }
                let remaining = input.len() - offset;
                let length = inspected.map_or(remaining, |header| {
                    usize::from(header.frame_length()).clamp(7, remaining)
                });
                let raw = source.slice(offset..offset + length).map_err(message)?;
                let mut parser =
                    AacAdtsParser::new(&pool, location(offset), raw, 8191).map_err(message)?;
                let frame = match parser.step(&mut WorkBudget::new(3), &CANCEL) {
                    Ok(BuildStep::Ready(frame)) => frame,
                    Ok(_) => {
                        return Err(
                            "ADTS buffer parser did not finish with three work units".into()
                        );
                    }
                    Err(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated) => {
                        return record(work, &mut trace, 8, &[offset as u64], &[]);
                    }
                    Err(error) => return Err(message(error)),
                };
                if wire {
                    let crc = frame.crc_word();
                    record(
                        work,
                        &mut trace,
                        1,
                        &[
                            offset as u64,
                            u64::from(crc.is_some()),
                            u64::from(crc.unwrap_or(0)),
                            frame.payload_bytes().len() as u64,
                        ],
                        frame.payload_bytes().as_ref(),
                    )?;
                } else {
                    let header = frame.header();
                    let object_type = header.profile_code() + 1;
                    if count == 0 {
                        let frequency = header.frequency_index();
                        let channel = header.channel_configuration();
                        let asc = [
                            object_type << 3 | frequency >> 1,
                            frequency << 7 | channel << 3,
                        ];
                        record(work, &mut trace, 2, &[2], &asc)?;
                    }
                    record(
                        work,
                        &mut trace,
                        1,
                        &[
                            offset as u64,
                            u64::from(header.frame_length()),
                            u64::from(object_type),
                            u64::from(header.frequency_index()),
                            u64::from(header.channel_configuration()),
                            u64::from(header.protection_absent()),
                            frame.payload_bytes().len() as u64,
                        ],
                        frame.payload_bytes().as_ref(),
                    )?;
                }
                count += 1;
                offset += frame.source_bytes().len();
            }
            record(work, &mut trace, 7, &[count], &[])
        }),
    })
}

fn message(error: Error) -> String {
    format!("{error:?}")
}

fn location(offset: usize) -> AacSource {
    AacSource {
        source: SourceId(1),
        generation: 0,
        offset: offset as u64,
    }
}
