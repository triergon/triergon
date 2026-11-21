use std::ffi::OsString;
use std::io::{BufWriter, Write};
use std::{fs, str};

use triergon::{Budget, Bytes, Error, WorkBudget};
use triergon_bitstream::{
    BitCursor, BitError, BitReader, BitSource, CachedBitReader, LsbReader, Rbsp, Reader,
    RiceReader, Window, runs,
};
use triergon_codec::mdct::Imdct;
use triergon_codec::{
    AudioDecoder, AudioFrame, BudgetVec, Divisor, Reservation, divide, divide_u32,
};

use super::protocol::{Operation, Work};

mod storage;

fn text(input: &[u8]) -> Result<&str, String> {
    str::from_utf8(input).map_err(|error| error.to_string())
}

fn number<T: str::FromStr>(field: Option<&str>) -> Result<T, String> {
    field
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| "missing or invalid numeric operand".into())
}

fn row(
    trace: &mut Option<&mut dyn Write>,
    work: &mut Work,
    kind: u64,
    words: &[u64],
    data: &[u8],
) -> Result<(), String> {
    work.record(kind, words, data);
    if let Some(trace) = trace.as_deref_mut() {
        (|| {
            write!(trace, "{kind}")?;
            for word in words {
                write!(trace, " {word}")?;
            }
            if !data.is_empty() {
                write!(trace, " ")?;
                for byte in data {
                    write!(trace, "{byte:02x}")?;
                }
            }
            writeln!(trace)
        })()
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn no_arguments(arguments: &[OsString]) -> Result<(), String> {
    if arguments.is_empty() {
        Ok(())
    } else {
        Err("unexpected operation arguments".into())
    }
}

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    match prefix {
        "bits-" => prepare_bits(input, arguments),
        "opus-range-" => opus(input, arguments),
        _ => {
            no_arguments(arguments)?;
            match prefix {
                "rbsp-" => prepare_rbsp(input),
                "budget-" => storage::prepare(input),
                "divide-" => division(input),
                "windows-" => windows(input),
                _ => Err(format!("unknown operation: {prefix}")),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum BitStep {
    Read(u32),
    Peek(u32),
    Skip(u32),
    Rice(u32),
    Align,
    Position,
}

fn bit_step(line: &str) -> Result<BitStep, String> {
    let fields: Vec<_> = line.split_ascii_whitespace().collect();
    match fields.as_slice() {
        ["align"] => Ok(BitStep::Align),
        ["position"] => Ok(BitStep::Position),
        [op, n] => {
            let n = number(Some(n))?;
            match *op {
                "read" => Ok(BitStep::Read(n)),
                "peek" => Ok(BitStep::Peek(n)),
                "skip" => Ok(BitStep::Skip(n)),
                "rice" => Ok(BitStep::Rice(n)),
                _ => Err(format!("unknown bit step: {line}")),
            }
        }
        _ => Err(format!("invalid bit step: {line}")),
    }
}

enum ReaderKind {
    Direct,
    Cached,
    Checked,
    Lsb,
    Rice,
}

enum Bits<'a> {
    Direct(BitReader<'a>),
    Cached(CachedBitReader<'a>),
    Checked(BitCursor<'a>),
    Lsb(LsbReader<'a>),
    Rice(RiceReader<'a>, usize, &'a [u8]),
}

impl Bits<'_> {
    fn status(&self) -> (usize, bool) {
        match self {
            Self::Direct(reader) => (reader.position(), reader.overrun()),
            Self::Cached(reader) => (reader.position(), reader.overrun()),
            Self::Checked(reader) => (reader.position(), false),
            Self::Lsb(reader) => (reader.position(), reader.overrun()),
            Self::Rice(reader, end, _) => (reader.position(), reader.past_end(*end)),
        }
    }

    fn step(&mut self, step: BitStep) -> Option<u64> {
        if matches!(step, BitStep::Position) {
            return Some(self.status().0 as u64);
        }
        match self {
            Self::Direct(reader) => msb_step(reader, step),
            Self::Cached(reader) => msb_step(reader, step),
            Self::Checked(reader) => match step {
                BitStep::Read(n) if n <= 32 => reader.read(n).ok().map(u64::from),
                BitStep::Peek(n) if n <= 32 => {
                    let mut peek = *reader;
                    peek.read(n).ok().map(u64::from)
                }
                BitStep::Skip(n) => reader.skip(n as usize).ok().map(|()| 0),
                BitStep::Align => {
                    reader.align();
                    Some(0)
                }
                _ => None,
            },
            Self::Lsb(reader) => match step {
                BitStep::Read(n) if n <= 64 => Some(reader.read_u64(n)),
                BitStep::Peek(n) if n <= 32 => Some(u64::from(reader.peek(n))),
                BitStep::Skip(n) if n <= 32 => {
                    reader.skip(n);
                    Some(0)
                }
                BitStep::Align => {
                    reader.skip((reader.position().next_multiple_of(8) - reader.position()) as u32);
                    Some(0)
                }
                _ => None,
            },
            Self::Rice(reader, end, data) => match step {
                BitStep::Read(n) if n <= 32 => {
                    reader.refill();
                    Some(u64::from(reader.take(n)))
                }
                BitStep::Peek(n) if n <= 32 => {
                    let mut peek = *reader;
                    peek.refill();
                    Some(u64::from(peek.take(n)))
                }
                BitStep::Skip(n) if n <= 32769 => {
                    *reader = RiceReader::at(data, reader.position() + n as usize);
                    Some(0)
                }
                BitStep::Align => {
                    *reader = RiceReader::at(data, reader.position().next_multiple_of(8));
                    Some(0)
                }
                BitStep::Rice(k) => reader.rice(k, *end).map(u64::from),
                _ => None,
            },
        }
    }
}

fn msb_step<W: Window>(reader: &mut Reader<'_, W>, step: BitStep) -> Option<u64> {
    match step {
        BitStep::Read(n) if n <= 32 => Some(u64::from(reader.read(n))),
        BitStep::Peek(n) if (1..=32).contains(&n) => Some(u64::from(reader.peek(n))),
        BitStep::Skip(n) => {
            reader.skip(n as usize);
            Some(0)
        }
        BitStep::Align => {
            reader.align(8);
            Some(0)
        }
        _ => None,
    }
}

fn prepare_bits<'a>(input: &'a [u8], arguments: &[OsString]) -> Result<Operation<'a>, String> {
    let [kind] = arguments else {
        return Err("bits takes the reader name".into());
    };
    let kind = match kind.to_str() {
        Some("direct") => ReaderKind::Direct,
        Some("cached") => ReaderKind::Cached,
        Some("checked") => ReaderKind::Checked,
        Some("lsb") => ReaderKind::Lsb,
        Some("rice") => ReaderKind::Rice,
        _ => return Err("unknown bit reader".into()),
    };
    let newline = input
        .iter()
        .position(|&byte| byte == b'\n')
        .ok_or("missing bit script header")?;
    let len: usize = number(Some(text(&input[..newline])?))?;
    let (data, script) = input[newline + 1..]
        .split_at_checked(len)
        .ok_or("short bit buffer")?;
    let steps = text(script)?
        .lines()
        .map(bit_step)
        .collect::<Result<Vec<_>, _>>()?;
    let mut padded = data.to_vec();
    padded.extend([0; 8]);
    Ok(Operation {
        name: "bits",
        run: Box::new(move |work, mut trace| {
            let mut reader = match kind {
                ReaderKind::Direct => Bits::Direct(BitReader::with_end(&padded, data.len() * 8)),
                ReaderKind::Cached => Bits::Cached(CachedBitReader::new(data)),
                ReaderKind::Checked => Bits::Checked(BitCursor::new(data)),
                ReaderKind::Lsb => Bits::Lsb(LsbReader::new(data)),
                ReaderKind::Rice => Bits::Rice(RiceReader::at(data, 0), data.len() * 8, data),
            };
            for &step in &steps {
                let value = reader.step(step);
                let (position, overrun) = reader.status();
                row(
                    &mut trace,
                    work,
                    if value.is_some() { 3 } else { 8 },
                    &[value.unwrap_or(0), position as u64, u64::from(overrun)],
                    &[],
                )?;
            }
            Ok(())
        }),
    })
}

fn sized_packets(mut data: &[u8], header_bytes: usize) -> Result<Vec<&[u8]>, String> {
    let mut packets = Vec::new();
    while !data.is_empty() {
        let (header, rest) = data
            .split_at_checked(header_bytes)
            .ok_or("short packet header")?;
        let length = u32::from_be_bytes(header[..4].try_into().map_err(|_| "short packet length")?);
        let (packet, suffix) = rest
            .split_at_checked(length as usize)
            .ok_or("short packet")?;
        packets.push(packet);
        data = suffix;
    }
    Ok(packets)
}

fn read_rbsp(mut reader: Rbsp<'_>) -> (Vec<u8>, u64) {
    let mut bytes = Vec::new();
    let terminal = loop {
        match reader.read(8) {
            Ok(value) => bytes.push(value as u8),
            Err(BitError::End) => break 0,
            Err(BitError::MissingPrevention) => break 1,
            Err(BitError::InvalidPrevention) => break 2,
            Err(BitError::LongPrefix) => break 3,
        }
    };
    (bytes, terminal)
}

fn prepare_rbsp(input: &[u8]) -> Result<Operation<'_>, String> {
    let units = sized_packets(input, 4)?;
    Ok(Operation {
        name: "rbsp",
        run: Box::new(move |work, mut trace| {
            for (i, payload) in units.iter().enumerate() {
                for (mode, reader) in [Rbsp::new(payload), Rbsp::lenient(payload)]
                    .into_iter()
                    .enumerate()
                {
                    let (bytes, terminal) = read_rbsp(reader);
                    row(
                        &mut trace,
                        work,
                        if terminal == 0 { 3 } else { 8 },
                        &[i as u64, mode as u64, terminal, bytes.len() as u64],
                        &bytes,
                    )?;
                }
                for (part, bytes) in runs(payload).enumerate() {
                    row(
                        &mut trace,
                        work,
                        3,
                        &[i as u64, 2, part as u64, bytes.len() as u64],
                        bytes,
                    )?;
                }
            }
            Ok(())
        }),
    })
}

fn opus<'a>(input: &'a [u8], arguments: &[OsString]) -> Result<Operation<'a>, String> {
    no_arguments(arguments)?;
    let packets = sized_packets(input, 8)?;
    Ok(Operation {
        name: "opus-range",
        run: Box::new(move |work, mut trace| {
            let mut decoder =
                triergon_codec_opus::Decoder::new(b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0")
                    .map_err(|error| error.to_string())?;
            let mut frame = AudioFrame::default();
            for (i, packet) in packets.iter().enumerate() {
                match decoder.decode(packet, &mut frame) {
                    Ok(_) => row(
                        &mut trace,
                        work,
                        3,
                        &[i as u64, u64::from(decoder.final_range())],
                        &[],
                    )?,
                    Err(_) => row(&mut trace, work, 8, &[i as u64], &[])?,
                }
            }
            Ok(())
        }),
    })
}

fn division(input: &[u8]) -> Result<Operation<'_>, String> {
    let mut pairs = Vec::<(i32, u32, u32)>::new();
    for line in text(input)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        let first = fields.next();
        if first == Some("range") {
            let start: u32 = number(fields.next())?;
            let end: u32 = number(fields.next())?;
            if start == 0 || start > end || end > 65535 || fields.next().is_some() {
                return Err(format!("invalid divisor range: {line}"));
            }
            for d in start..=end {
                let max = i32::MAX as u32 / d;
                pairs.extend([
                    (i32::MIN, 0, d),
                    (1 - d as i32, d - 1, d),
                    (-(d as i32), d, d),
                    (d as i32, d + 1, d),
                    (i32::MAX, max - 1, d),
                    (i32::MIN + 1, max, d),
                    (i32::MAX, u32::MAX, d),
                ]);
            }
            continue;
        }
        let a = number(first)?;
        let n = number(fields.next())?;
        let d = number(fields.next())?;
        if d == 0 || d > i32::MAX as u32 || fields.next().is_some() {
            return Err(format!("invalid divisor: {line}"));
        }
        pairs.push((a, n, d));
    }
    Ok(Operation {
        name: "divide",
        run: Box::new(move |work, mut trace| {
            for &(a, n, d) in &pairs {
                let divisor = Divisor::new(d);
                let q = divide(a, d as i32);
                let u = divide_u32(n, d);
                let result = [q as u32 as u64, u64::from(u)];
                if u64::from(n) * u64::from(d) < 1 << 31 {
                    let (q, r) = divisor.div_rem(n);
                    row(
                        &mut trace,
                        work,
                        3,
                        &[
                            result[0],
                            result[1],
                            u64::from(divisor.get()),
                            u64::from(q),
                            u64::from(r),
                        ],
                        &[],
                    )?;
                } else {
                    row(&mut trace, work, 3, &result, &[])?;
                }
            }
            Ok(())
        }),
    })
}

fn windows(input: &[u8]) -> Result<Operation<'_>, String> {
    let mut specs = Vec::new();
    for line in text(input)?.lines() {
        let mut fields = line.split_ascii_whitespace();
        let n: usize = number(fields.next())?;
        let alpha: u8 = number(fields.next())?;
        if !(1..=4096).contains(&n) || alpha > 20 || fields.next().is_some() {
            return Err(format!("invalid window: {line}"));
        }
        specs.push((n, alpha));
    }
    Ok(Operation {
        name: "windows",
        run: Box::new(move |work, mut trace| {
            for &(n, alpha) in &specs {
                let mut window = vec![0.0; n];
                if alpha == 0 {
                    triergon_codec::vorbis_window(&mut window);
                } else {
                    triergon_codec::kbd_window(f64::from(alpha) * 0.5, &mut window);
                }
                let bits: Vec<_> = window
                    .into_iter()
                    .flat_map(|value| value.to_bits().to_be_bytes())
                    .collect();
                row(&mut trace, work, 3, &[n as u64, u64::from(alpha)], &bits)?;
            }
            Ok(())
        }),
    })
}

pub(super) fn command(command: &str, arguments: &[OsString]) -> Result<(), String> {
    let [spec, out] = arguments else {
        return Err(format!("usage: {command} SPEC OUT"));
    };
    let spec = fs::read(spec).map_err(|error| error.to_string())?;
    let (header, data) = spec.split_at_checked(12).ok_or("short DSP case header")?;
    let n = u32::from_le_bytes(header[..4].try_into().map_err(|_| "invalid length")?) as usize;
    let scale = f64::from_le_bytes(header[4..].try_into().map_err(|_| "invalid scale")?);
    if !(4..=8192).contains(&n) || !Imdct::supports(n) || data.len() != n * 2 || !scale.is_finite()
    {
        return Err("invalid DSP case".into());
    }
    let x: Vec<_> = data
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(f32::from_le_bytes)
        .collect();
    let budget = Budget::new(1 << 24).map_err(|error| error.to_string())?;
    let mut transform = Imdct::new(n, scale, &budget).map_err(|error| error.to_string())?;
    let mut y = vec![0.0; n];
    let mut w = vec![0.0; n / 2];
    transform.imdct(&x, &mut y);
    transform.dct4(&x, &mut w);
    let mut file = BufWriter::new(fs::File::create(out).map_err(|error| error.to_string())?);
    for value in y.into_iter().chain(w) {
        file.write_all(&value.to_le_bytes())
            .map_err(|error| error.to_string())?;
    }
    file.flush().map_err(|error| error.to_string())
}
