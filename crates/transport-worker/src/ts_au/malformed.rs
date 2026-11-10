//! Confirm a reported malformed source reached EOF without packet work.

use super::{WINDOW, WORK};
use crate::ts_input::{FIRST_SOURCE, Request, capabilities};
use std::sync::atomic::AtomicBool;
use triergon_core::{
    Budget, Error, InputCursor, InputWindow, Result, SourceLocation, Step, WorkBudget,
};
use triergon_mpegts::compat::pes::{
    DemuxObserver, DemuxProfile, DemuxScope, MalformedInputCompletion, MalformedSourceDisposition,
    NoPacketsConfirmation, RegisteredCarriage, SourceContext, TransportFaultRecovery,
};
use triergon_mpegts::psi::ProgramMap;
use triergon_mpegts::{FaultKind, PacketFormat, ParseLimits, TraceEvent, TraceFault};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Diagnostic {
    kind: FaultKind,
    location: SourceLocation,
    pid: Option<u16>,
    error: Error,
}

impl From<&TraceFault> for Diagnostic {
    fn from(fault: &TraceFault) -> Self {
        Self {
            kind: fault.kind,
            location: fault.location,
            pid: fault.pid,
            error: fault.error,
        }
    }
}

pub(super) fn confirm(
    pool: &Budget,
    request: &Request,
    diagnostic: Diagnostic,
    cancel: &AtomicBool,
) -> Result<Option<NoPacketsConfirmation>> {
    if request.format != PacketFormat::Ts188 || !request.following.is_empty() {
        return Ok(None);
    }
    match prove(pool, request, diagnostic, cancel) {
        Err(Error::Invalid(_) | Error::Unsupported(_) | Error::Truncated) => Ok(None),
        result => result,
    }
}

fn prove(
    pool: &Budget,
    request: &Request,
    diagnostic: Diagnostic,
    cancel: &AtomicBool,
) -> Result<Option<NoPacketsConfirmation>> {
    let raw = &request.input;
    let mut input = InputCursor::new(pool, FIRST_SOURCE, capabilities(raw), WINDOW)?;
    let mut source = MalformedInputCompletion::new_with_namespace(
        &input,
        request.format,
        ParseLimits::default(),
        request.program,
        request.namespace,
    )?;
    let mut observer = None;
    let mut matched = false;
    loop {
        match source.poll_source(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(mut issued) => {
                let grant = issued.required_work()?;
                let disposition = issued
                    .observe(observer.as_mut(), &mut WorkBudget::new(grant), cancel)?
                    .ok_or(Error::Invalid("malformed observation exhausted its grant"))?;
                match disposition {
                    MalformedSourceDisposition::AwaitDiagnostic => {}
                    MalformedSourceDisposition::Reported(fact) => {
                        // The first reported discard must reproduce the reader's
                        // diagnostic. The replay supplies its own carrier proof.
                        if !matched && Diagnostic::from(fact.source_fault()) != diagnostic {
                            return Ok(None);
                        }
                        matched = true;
                    }
                    MalformedSourceDisposition::Forward => match issued.event() {
                        TraceEvent::ProgramMap {
                            map, active: true, ..
                        } => {
                            if observer.is_some() {
                                return Ok(None);
                            }
                            observer = Some(bind_observer(map, issued.input(), issued.context())?);
                        }
                        TraceEvent::Diagnostic(_) | TraceEvent::Pes(_) => return Ok(None),
                        TraceEvent::Finished => {
                            drop(issued);
                            if !matched {
                                return Ok(None);
                            }
                            return source.confirm_no_packets(
                                observer.as_mut(),
                                &mut input,
                                &mut WorkBudget::new(WORK),
                                cancel,
                            );
                        }
                        _ => {}
                    },
                }
            }
            Step::NeedInput(next) => {
                let start = usize::try_from(next.offset).map_err(|_| Error::Overflow)?;
                let end = raw.len().min(start.saturating_add(next.maximum));
                if start < end {
                    input.provide_owned(InputWindow::new(
                        next.source,
                        next.token,
                        next.offset,
                        raw.slice(start..end)?,
                    )?)?;
                } else if start == raw.len() {
                    input.finish(next.source, next.token)?;
                } else {
                    return Err(Error::Invalid("malformed proof requested beyond input"));
                }
            }
            Step::End(_) => {
                source
                    .seal_physical_end(&mut input, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or(Error::Invalid("malformed End exhausted its grant"))?;
                source.end_of_source()?;
            }
            Step::Yield => {}
        }
    }
}

fn bind_observer(
    map: &ProgramMap,
    input: &InputCursor,
    context: SourceContext,
) -> Result<TransportFaultRecovery> {
    let [stream] = map.streams.as_slice() else {
        return Err(Error::Unsupported(
            "malformed proof requires one AVC stream",
        ));
    };
    let scope = DemuxScope {
        program: map.header.extension,
        pid: stream.pid,
        stream_type: stream.stream_type,
        stream_id: 0xe0,
        entity: input.source(),
        epochs: context.epochs,
    };
    let observer = DemuxObserver::new_registered(
        DemuxProfile::PesPackets,
        scope,
        RegisteredCarriage::Avc,
        input.budget().clone(),
        ParseLimits::default(),
    )?;
    let mut recovery = TransportFaultRecovery::new(observer).map_err(|rejected| rejected.error)?;
    recovery.bind_source_input(input)?;
    Ok(recovery)
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, confirm};
    use crate::ts_input::{FIRST_SOURCE, Request};
    use std::sync::atomic::AtomicBool;
    use triergon_core::{Budget, BudgetVec, Bytes, Error, SourceLocation};
    use triergon_mpegts::psi::DescriptorNamespace;
    use triergon_mpegts::{FaultKind, PacketFormat};

    #[test]
    fn a_different_diagnostic_cannot_prove_empty_completion() {
        let pool = Budget::new(1 << 20).unwrap();
        let request = Request {
            input: Bytes::copy_from_slice(
                &pool,
                include_bytes!("../../../mpegts/tests/data/pes-start-prefix-invalid.ts"),
            )
            .unwrap(),
            format: PacketFormat::Ts188,
            program: 1,
            namespace: DescriptorNamespace::Unspecified,
            following: BudgetVec::new(&pool),
        };
        let mut diagnostic = Diagnostic {
            kind: FaultKind::Pes,
            location: SourceLocation {
                source: FIRST_SOURCE,
                offset: 376,
            },
            pid: Some(256),
            error: Error::Invalid("PES start code"),
        };
        let cancel = AtomicBool::new(false);
        let proof = confirm(&pool, &request, diagnostic, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(proof.physical_end().offset, 564);
        assert_eq!(proof.discarded_carriers(), 1);
        diagnostic.location.offset += 1;
        assert!(
            confirm(&pool, &request, diagnostic, &cancel)
                .unwrap()
                .is_none()
        );
    }
}
