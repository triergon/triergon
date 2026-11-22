use std::{ffi::OsString, io::Write, sync::atomic::AtomicBool};

use triergon_core::{
    Budget, Bytes, Error, InputWindow, SourceCapabilities, SourceId, Step, WorkBudget,
};
use triergon_framing::av1::{Av1Obu, Av1ObuFraming, Av1ObuLimits, Av1ObuReader};

use super::{
    envelope,
    protocol::{Operation, Work},
};

const SOURCE: SourceId = SourceId(1);
const OBU_BYTES: usize = 16 << 20;
const OBU_COUNT: usize = 64;
static CANCEL: AtomicBool = AtomicBool::new(false);

pub(super) fn operation<'a>(
    prefix: &str,
    input: &'a [u8],
    arguments: &[OsString],
) -> Result<Operation<'a>, String> {
    let wire = prefix == "obu-wire-";
    let [framing, window, excluded] = arguments else {
        return Err("OBU needs framing, window, and excluded filter type".into());
    };
    let framing = match framing.to_str() {
        Some("low-overhead") => Av1ObuFraming::LowOverhead,
        Some("single") => Av1ObuFraming::OneObu,
        _ => return Err("unknown OBU framing".into()),
    };
    let window = window
        .to_str()
        .and_then(|value| value.parse().ok())
        .filter(|value| matches!(value, 1 | 7 | 4096))
        .ok_or("invalid OBU window")?;
    if !matches!(excluded.to_str(), Some("0" | "14")) || input.get(8..12) != Some(b"AV01") {
        return Err("OBU needs AV1 IVF and an excluded type of 0 or 14".into());
    }
    let units = envelope::ivf(input)
        .and_then(Iterator::collect::<triergon_core::Result<Vec<_>>>)
        .map_err(message)?;
    Ok(Operation {
        name: if wire {
            "av1-obu-wire"
        } else {
            "av1-obu-reading"
        },
        run: Box::new(move |work, mut trace| {
            let pool = Budget::new(OBU_BYTES * 3).map_err(message)?;
            for (index, frame) in units.iter().enumerate() {
                let unit = Unit {
                    framing,
                    window,
                    index: index as u64,
                    wire,
                };
                unit.run(&pool, frame.data, work, &mut trace)?;
            }
            Ok(())
        }),
    })
}

fn message(error: Error) -> String {
    format!("{error:?}")
}

struct Unit {
    framing: Av1ObuFraming,
    window: usize,
    index: u64,
    wire: bool,
}

enum ReadError {
    Input(Error),
    Worker(String),
}

impl From<Error> for ReadError {
    fn from(error: Error) -> Self {
        Self::Input(error)
    }
}

impl Unit {
    fn read(
        &self,
        pool: &Budget,
        input: &[u8],
        mut emit: impl FnMut(Av1Obu) -> Result<(), String>,
    ) -> Result<u64, ReadError> {
        let mut reader = Av1ObuReader::new(
            pool,
            SOURCE,
            SourceCapabilities {
                length: Some(input.len() as u64),
                random_access: false,
            },
            self.framing,
            Av1ObuLimits {
                maximum_obu_bytes: OBU_BYTES,
                maximum_obus: OBU_COUNT,
                maximum_input_window: self.window,
            },
        )?;
        let mut count = 0;
        loop {
            match reader.step(&mut WorkBudget::new(4096), &CANCEL)? {
                Step::Ready(obu) => {
                    emit(obu).map_err(ReadError::Worker)?;
                    count += 1;
                }
                Step::NeedInput(request) => {
                    let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
                    if offset == input.len() {
                        reader.finish(request.source, request.token)?;
                    } else {
                        let data = input.get(offset..).ok_or(Error::Overflow)?;
                        let end = data.len().min(self.window).min(request.maximum);
                        let bytes = Bytes::copy_from_slice(pool, &data[..end])?;
                        reader.provide(&InputWindow::new(
                            request.source,
                            request.token,
                            request.offset,
                            bytes,
                        )?)?;
                    }
                }
                Step::Yield => {}
                Step::End(_) => return Ok(count),
                Step::Backpressure(_) => {
                    return Err(ReadError::Worker(
                        "OBU reader exhausted its worker pool".into(),
                    ));
                }
            }
        }
    }

    fn run(
        &self,
        pool: &Budget,
        input: &[u8],
        work: &mut Work,
        trace: &mut Option<&mut dyn Write>,
    ) -> Result<(), String> {
        let index = self.index;
        let result = self.read(pool, input, |obu| {
            let facts = obu.facts();
            let header = facts.header;
            let payload = obu.payload_bytes().map_err(message)?;
            if self.wire {
                let leb = facts.size_field;
                let words = [
                    index,
                    facts.offset,
                    facts.payload_offset,
                    facts.wire_length as u64,
                    facts.payload_length as u64,
                    u64::from(header.raw_byte),
                    header.extension.map_or(256, |ext| {
                        u64::from(
                            ext.temporal_id() << 5 | ext.spatial_id() << 3 | ext.reserved_bits(),
                        )
                    }),
                    leb.map_or(0, |size| u64::from(size.width())),
                    leb.map_or(0, |size| u64::from(size.value())),
                    u64::from(header.syntax_issues().bits()),
                    facts.external_length.unwrap_or(0),
                    obu.wire_bytes().len() as u64,
                ];
                record(work, trace, 3, &words, obu.wire_bytes().as_ref())?;
                record(
                    work,
                    trace,
                    1,
                    &[index, facts.offset, payload.len() as u64],
                    payload.as_ref(),
                )
            } else {
                let extension = header.extension;
                let words = [
                    index,
                    u64::from(header.obu_type()),
                    u64::from(header.extension_present()),
                    extension.map_or(0, |ext| u64::from(ext.temporal_id())),
                    extension.map_or(0, |ext| u64::from(ext.spatial_id())),
                    u64::from(header.has_size_field()),
                    payload.len() as u64,
                ];
                record(work, trace, 1, &words, payload.as_ref())
            }
        });
        match result {
            Ok(count) => record(work, trace, 7, &[index, count], &[]),
            Err(ReadError::Input(
                Error::Invalid(_)
                | Error::Unsupported(_)
                | Error::Truncated
                | Error::Limit { .. }
                | Error::Policy(_),
            )) => record(work, trace, 8, &[index], &[]),
            Err(ReadError::Input(error)) => Err(message(error)),
            Err(ReadError::Worker(error)) => Err(error),
        }
    }
}

pub(super) fn record(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    words: &[u64],
    bytes: &[u8],
) -> Result<(), String> {
    work.record(kind, words, bytes);
    if let Some(trace) = trace.as_mut() {
        write!(trace, "{kind}").map_err(|error| error.to_string())?;
        for word in words {
            write!(trace, " {word}").map_err(|error| error.to_string())?;
        }
        if !bytes.is_empty() {
            write!(trace, " {}", triergon_harness_protocol::hex(bytes))
                .map_err(|error| error.to_string())?;
        }
        writeln!(trace).map_err(|error| error.to_string())?;
    }
    Ok(())
}
