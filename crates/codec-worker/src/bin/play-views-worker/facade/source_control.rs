//! Exercise the facade's source-side contract independently of native seeks.

use std::io::Write;
use std::sync::atomic::AtomicBool;

use triergon::{
    AnyReader, Budget, ContainerReader, Error, InputRequest, Probe, SourceCapabilities,
    SourceFailure, SourceId, Step, WorkBudget,
};

use super::{Operation, Work};

const SOURCE: SourceId = SourceId(1);

fn expect_request(reader: &mut AnyReader, expected: &InputRequest) -> Result<(), String> {
    let step = reader
        .step(&mut WorkBudget::new(1 << 16), &AtomicBool::new(false))
        .map_err(|error| error.to_string())?;
    match step {
        Step::NeedInput(request)
            if request.source == SOURCE
                && request.source == expected.source
                && request.token == expected.token
                && request.offset == 0
                && request.offset == expected.offset
                && request.maximum != 0
                && request.maximum <= expected.maximum =>
        {
            Ok(())
        }
        other => Err(format!(
            "restart request {expected:?} disagrees with first step: {other:?}"
        )),
    }
}

fn respond_with_failure(
    reader: &mut AnyReader,
    request: &InputRequest,
    failure: SourceFailure,
    error: Error,
) -> Result<(), String> {
    if reader.fail(SourceId(2), request.token.clone(), failure) != Err(Error::SourceMismatch) {
        return Err("a failure from a different source was not refused".into());
    }
    reader
        .fail(SOURCE, request.token.clone(), failure)
        .map_err(|error| error.to_string())?;
    for _ in 0..2 {
        let step = reader.step(&mut WorkBudget::new(1 << 16), &AtomicBool::new(false));
        if !matches!(step, Err(actual) if actual == error) {
            return Err(format!("source failure {failure:?} returned {step:?}"));
        }
    }
    Ok(())
}

fn record(
    work: &mut Work,
    trace: &mut Option<&mut dyn Write>,
    kind: u64,
    caps: SourceCapabilities,
) -> Result<(), String> {
    let length = caps.length.unwrap_or(0);
    let random_access = u64::from(caps.random_access);
    work.record(kind, &[length, random_access], &[]);
    if let Some(trace) = trace {
        writeln!(trace, "{kind} {length} {random_access}").map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(super) fn prepare<'a>(input: &'a [u8], args: &[&str]) -> Result<Operation<'a>, String> {
    let format = match args {
        ["mkv"] => Probe::Matroska,
        ["mp4"] => Probe::Isobmff,
        ["188"] => Probe::Ts,
        ["192"] => Probe::M2ts,
        _ => return Err("expected mkv, mp4, 188 or 192".into()),
    };
    if triergon::probe(input) != format {
        return Err("unexpected source-control fixture format".into());
    }
    let pool = Budget::new(16 << 20).map_err(|error| error.to_string())?;
    Ok(Operation {
        name: "facade-source-control",
        run: Box::new(move |work, mut trace: Option<&mut dyn Write>| {
            for length in [None, Some(input.len() as u64)] {
                for random_access in [false, true] {
                    let caps = SourceCapabilities {
                        length,
                        random_access,
                    };
                    let opened = AnyReader::open(input, &pool, SOURCE, caps);
                    if format == Probe::Isobmff && !random_access {
                        if !matches!(
                            opened,
                            Err(Error::Unsupported("ISO BMFF reader needs random access"))
                        ) {
                            return Err("ISO BMFF did not refuse a sequential source".into());
                        }
                        record(work, &mut trace, 3, caps)?;
                        continue;
                    }
                    let mut reader = opened.map_err(|error| error.to_string())?;
                    let mut request = reader.reset().map_err(|error| error.to_string())?;
                    for (kind, failure, error) in [
                        (1, SourceFailure::Transport, Error::Transport),
                        (2, SourceFailure::Cancelled, Error::Cancelled),
                    ] {
                        if reader.capabilities() != caps {
                            return Err("facade changed its source capabilities".into());
                        }
                        expect_request(&mut reader, &request)?;
                        respond_with_failure(&mut reader, &request, failure, error)?;
                        let old = request;
                        request = reader.reset().map_err(|error| error.to_string())?;
                        let stale = Error::StaleGeneration {
                            expected: request.token.generation(),
                            received: old.token.generation(),
                        };
                        if request.token.generation() == old.token.generation()
                            || reader.fail(SOURCE, old.token, failure) != Err(stale)
                        {
                            return Err(
                                "a restarted reader accepted the previous generation".into()
                            );
                        }
                        record(work, &mut trace, kind, reader.capabilities())?;
                    }
                    expect_request(&mut reader, &request)?;
                }
            }
            Ok(())
        }),
    })
}
