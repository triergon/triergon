use std::ffi::OsString;

use triergon::{Rounding, TimeBase};
use triergon_bitstream::crc::{Crc32Ieee, Crc32Mpeg, crc8, crc16_a52};
use triergon_harness_protocol::digest::signed;
use triergon_harness_runner::TickScale;

use super::protocol::Operation;

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    match prefix {
        "crc-" => crc_operation(input, arguments),
        "rescale-" => rescale_operation(input, arguments),
        _ => Err(format!("unknown operation {prefix}")),
    }
}

#[derive(Clone, Copy)]
enum Crc {
    Mpeg,
    Ogg,
    Ieee,
    Header,
    Audio,
    MpegAudio,
}

fn crc_operation<'a>(data: &'a [u8], args: &[OsString]) -> Result<Operation<'a>, String> {
    let [function, schedule] = args else {
        return Err("usage: crc FILE FUNCTION whole|SPLIT".into());
    };
    let function = match function.to_str() {
        Some("mpeg") => Crc::Mpeg,
        Some("ogg") => Crc::Ogg,
        Some("ieee") => Crc::Ieee,
        Some("flac-header") => Crc::Header,
        Some("flac-ac3") => Crc::Audio,
        Some("mpeg-audio") => Crc::MpegAudio,
        _ => return Err("unknown CRC function".into()),
    };
    let split = if schedule == "whole" {
        None
    } else {
        Some(
            schedule
                .to_str()
                .and_then(|offset| offset.parse::<usize>().ok())
                .filter(|&offset| offset <= data.len())
                .ok_or("invalid CRC split")?,
        )
    };
    Ok(Operation {
        name: "crc-values",
        run: Box::new(move |work, trace| {
            let (head, tail) = data.split_at(split.unwrap_or(data.len()));
            let result = match function {
                Crc::Mpeg if split.is_none() => u64::from(Crc32Mpeg::of(data)),
                Crc::Mpeg | Crc::Ogg => {
                    let mut crc = if matches!(function, Crc::Mpeg) {
                        Crc32Mpeg::default()
                    } else {
                        Crc32Mpeg::from_register(0)
                    };
                    crc.update(head);
                    if split.is_some()
                        && let Some((&first, rest)) = tail.split_first()
                    {
                        crc.update_byte(first);
                        crc.update(rest);
                    }
                    u64::from(crc.remainder())
                }
                Crc::Ieee if split.is_none() => u64::from(Crc32Ieee::of(data)),
                Crc::Ieee => {
                    let mut crc = Crc32Ieee::default();
                    crc.update(head);
                    crc.update(tail);
                    u64::from(crc.finish())
                }
                Crc::Header => {
                    let crc = crc8(0, head);
                    u64::from(if split.is_some() {
                        crc8(crc, tail)
                    } else {
                        crc
                    })
                }
                Crc::Audio | Crc::MpegAudio => {
                    let initial = if matches!(function, Crc::MpegAudio) {
                        u16::MAX
                    } else {
                        0
                    };
                    let crc = crc16_a52(initial, head);
                    u64::from(if split.is_some() {
                        crc16_a52(crc, tail)
                    } else {
                        crc
                    })
                }
            };
            work.record(3, &[data.len() as u64, result], &[]);
            if let Some(trace) = trace {
                writeln!(trace, "3 {} {result}", data.len()).map_err(|error| error.to_string())?;
            }
            Ok(())
        }),
    })
}

const MODES: [Rounding; 5] = [
    Rounding::TowardZero,
    Rounding::AwayFromZero,
    Rounding::Floor,
    Rounding::Ceil,
    Rounding::NearestAway,
];

fn base(arg: &OsString) -> Result<TimeBase, String> {
    let (num, den) = arg
        .to_str()
        .and_then(|text| text.split_once('/'))
        .ok_or("expected numerator/denominator")?;
    let component = |term: &str| {
        term.parse::<u64>()
            .ok()
            .filter(|&value| (1..=i32::MAX as u64).contains(&value))
            .ok_or("time base component outside the reference range")
    };
    TimeBase::new(component(num)?, component(den)?).map_err(|error| error.to_string())
}

fn rescale_operation<'a>(data: &'a [u8], args: &[OsString]) -> Result<Operation<'a>, String> {
    let [from, to, path] = args else {
        return Err("usage: rescale FILE FROM TO direct|prepared".into());
    };
    let (from, to) = (base(from)?, base(to)?);
    let prepared = match path.to_str() {
        Some("direct") => false,
        Some("prepared") => true,
        _ => return Err("invalid time rescale path".into()),
    };
    let (encoded, trailing) = data.as_chunks::<8>();
    if !trailing.is_empty() {
        return Err("partial time value".into());
    }
    let ticks: Vec<_> = encoded.iter().copied().map(i64::from_le_bytes).collect();
    Ok(Operation {
        name: "time-rescale",
        run: Box::new(move |work, mut trace| {
            let scale = prepared.then(|| TickScale::new(from, to));
            for &tick in &ticks {
                for (mode, rounding) in MODES.into_iter().enumerate() {
                    let result = if let Some(scale) = &scale {
                        scale.rescale(tick, rounding)
                    } else {
                        from.rescale(tick, to, rounding)
                    };
                    let fields = [signed(tick), mode as u64];
                    match result {
                        Ok(value) => {
                            work.record(3, &[fields[0], fields[1], signed(value)], &[]);
                            if let Some(trace) = &mut trace {
                                writeln!(trace, "3 {tick} {mode} {value}")
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                        Err(triergon::Error::Overflow) => {
                            work.record(8, &fields, &[]);
                            if let Some(trace) = &mut trace {
                                writeln!(trace, "8 {tick} {mode}")
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
            Ok(())
        }),
    })
}
