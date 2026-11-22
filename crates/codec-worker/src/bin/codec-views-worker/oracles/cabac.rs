use std::ffi::OsString;
use std::io::Write;

use triergon_bitstream::cabac::{Engine, init_state};

use super::{Operation, Work, number, row, text};

#[derive(Clone, Copy)]
enum Call {
    New(usize),
    Restart(usize),
    Init(usize, i8, i8, i32),
    State(usize, u8),
    Decision(usize, bool),
    Refill,
    Bypass,
    Bits(u32),
    Peek(u32),
    Consume(u32),
    Unary(u32),
    Terminate,
    Data,
}

fn call(line: &str, length: usize) -> Result<Call, String> {
    let mut fields = line.split_ascii_whitespace();
    let operation = fields.next().ok_or("missing CABAC operation")?;
    let call = match operation {
        "new" => Call::New(number(fields.next())?),
        "restart" => Call::Restart(number(fields.next())?),
        "init" => Call::Init(
            number(fields.next())?,
            number(fields.next())?,
            number(fields.next())?,
            number(fields.next())?,
        ),
        "state" => Call::State(number(fields.next())?, number(fields.next())?),
        "decision" | "unfilled" => Call::Decision(number(fields.next())?, operation == "decision"),
        "refill" => Call::Refill,
        "bypass" => Call::Bypass,
        "bits" => Call::Bits(number(fields.next())?),
        "peek" => Call::Peek(number(fields.next())?),
        "consume" => Call::Consume(number(fields.next())?),
        "unary" => Call::Unary(number(fields.next())?),
        "terminate" => Call::Terminate,
        "data" => Call::Data,
        _ => return Err(format!("unknown CABAC operation {operation}")),
    };
    if fields.next().is_some() {
        return Err(format!("too many operands for {operation}"));
    }
    match call {
        Call::New(start) | Call::Restart(start) if start > length + 8 => {
            Err("CABAC start exceeds the scripted padding limit".into())
        }
        Call::Bits(33..) | Call::Peek(17..) | Call::Consume(17..) => {
            Err("CABAC bypass count out of range".into())
        }
        Call::Init(128.., ..) | Call::State(128.., _) | Call::Decision(128.., _) => {
            Err("CABAC context index out of range".into())
        }
        _ => Ok(call),
    }
}

pub(super) fn prepare<'a>(input: &[u8], arguments: &[OsString]) -> Result<Operation<'a>, String> {
    let mut lines = text(input)?.lines();
    let hex = lines.next().ok_or("missing CABAC bytes")?;
    if !hex.is_ascii() || hex.len() % 2 != 0 {
        return Err("expected a complete hexadecimal byte string".into());
    }
    let data = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let calls = lines
        .map(|line| call(line, data.len()))
        .collect::<Result<Vec<_>, _>>()?;
    let [layout] = arguments else {
        return Err("expected CABAC layout plain or headroom".into());
    };
    let execute = match layout.to_str() {
        Some("plain") => run::<false>,
        Some("headroom") => run::<true>,
        _ => return Err("unknown CABAC layout".into()),
    };
    Ok(Operation {
        name: "cabac-engine",
        run: Box::new(move |work, trace| execute(&data, &calls, work, trace)),
    })
}

fn run<const HEADROOM: bool>(
    data: &[u8],
    calls: &[Call],
    work: &mut Work,
    mut trace: Option<&mut dyn Write>,
) -> Result<(), String> {
    let mut engine = None;
    let mut contexts = [0; 128];
    let mut last_peek = None;
    for (index, &call) in calls.iter().enumerate() {
        if !matches!(call, Call::Peek(_) | Call::Consume(_)) {
            last_peek = None;
        }
        let mut words = vec![index as u64];
        let mut bytes = &[][..];
        let mut kind = 3;
        match call {
            Call::New(start) => {
                engine = Engine::<HEADROOM>::new(data, start).ok();
                if engine.is_none() {
                    kind = 8;
                }
            }
            Call::Init(slot, m, n, qp) => {
                let state = init_state(i32::from(m), i32::from(n), qp);
                contexts[slot] = state;
                words.push(u64::from(state));
            }
            Call::State(slot, state) => contexts[slot] = state,
            call => {
                let current = engine
                    .as_mut()
                    .ok_or("CABAC operation before initialization")?;
                match call {
                    Call::Restart(start) => {
                        if current.restart(start).is_err() {
                            kind = 8;
                        }
                    }
                    Call::Refill => current.refill(),
                    Call::Decision(slot, refill) => {
                        let state = &mut contexts[slot];
                        let bin = if refill {
                            current.decision(state)
                        } else {
                            current.decision_unfilled(state)
                        };
                        words.extend([u64::from(bin), u64::from(*state)]);
                    }
                    Call::Bypass => words.push(u64::from(current.bypass())),
                    Call::Bits(count) => words.push(u64::from(current.bypass_bits(count))),
                    Call::Peek(count) => {
                        let bins = current.peek_bypass(count);
                        last_peek = Some((count, bins));
                        words.push(u64::from(bins));
                    }
                    Call::Consume(count) => {
                        let (peeked, bins) = last_peek.take().ok_or("consume without peek")?;
                        if count > peeked {
                            return Err("consume exceeds peek".into());
                        }
                        current.consume_bypass(count, bins >> (peeked - count));
                    }
                    Call::Unary(max) => words.push(u64::from(current.bypass_unary(max))),
                    Call::Terminate => words.push(u64::from(current.terminate())),
                    Call::Data => {
                        bytes = current.data();
                        words.push(bytes.len() as u64);
                    }
                    Call::New(_) | Call::Init(..) | Call::State(..) => unreachable!(),
                }
            }
        }
        if let Some(current) = &engine {
            words.extend([
                current.bits_read() as u64,
                current.aligned_byte_position() as u64,
                current.next_byte() as u64,
            ]);
        }
        row(&mut trace, work, kind, &words, bytes)?;
    }
    Ok(())
}
