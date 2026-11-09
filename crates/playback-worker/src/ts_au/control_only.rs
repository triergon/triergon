use super::{FIRST_SOURCE, Request, WINDOW, WORK, ts_input};
use std::sync::atomic::AtomicBool;
use triergon_core::{Budget, Error, InputCursor, InputWindow, Result, Step, WorkBudget};
use triergon_mpegts::compat::pes::SourceContext;
use triergon_mpegts::reference_trace::packet_completion::PacketCompletion;
use triergon_mpegts::{ParseLimits, TraceEvent, TransportStream};

/// Confirm a single control-only entity through physical End and logical EOF.
/// The access-unit reader can stop on a PSI diagnostic before either boundary.
#[cold]
pub(super) fn confirm(pool: &Budget, request: &Request, cancel: &AtomicBool) -> Result<bool> {
    if !request.following.is_empty() {
        return Ok(false);
    }
    let raw = &request.input;
    let limits = ParseLimits::default();
    let mut input = InputCursor::new(pool, FIRST_SOURCE, ts_input::capabilities(raw), WINDOW)?;
    let mut transport = TransportStream::new_with_namespace(
        pool.clone(),
        request.format,
        limits,
        request.program,
        request.namespace,
    )?;
    let mut proof =
        PacketCompletion::new_for_format(pool.clone(), limits, request.program, request.format)?;
    let mut ordinal = 0_u64;
    loop {
        match transport.poll(&mut input, &mut WorkBudget::new(WORK), cancel)? {
            Step::Ready(event) => {
                let context = SourceContext {
                    source_event_ordinal: ordinal,
                    epochs: transport.source_epochs(),
                    byte_run_epoch: None,
                };
                let grant = proof.required_work(&event)?;
                if !proof.observe(
                    &event,
                    input.source(),
                    context,
                    &mut WorkBudget::new(grant),
                    cancel,
                )? {
                    return Err(Error::Invalid("control proof exhausted its work grant"));
                }
                ordinal = ordinal.checked_add(1).ok_or(Error::Overflow)?;
                if matches!(event, TraceEvent::Finished) {
                    let grant = proof.required_end_work()?;
                    return Ok(proof
                        .confirm_no_media(&mut input, &mut WorkBudget::new(grant), cancel)?
                        .is_some());
                }
            }
            Step::Yield => (),
            Step::NeedInput(request) => {
                let offset = usize::try_from(request.offset).map_err(|_| Error::Overflow)?;
                if offset == raw.len() {
                    input.finish(request.source, request.token)?;
                } else {
                    let end = raw.len().min(offset.saturating_add(request.maximum));
                    input.provide_owned(InputWindow::new(
                        request.source,
                        request.token,
                        request.offset,
                        raw.slice(offset..end)?,
                    )?)?;
                }
            }
            Step::End(_) => {
                let end = transport
                    .media_entity_end(&mut input, &mut WorkBudget::new(WORK), cancel)?
                    .ok_or(Error::Invalid("control proof did not reach physical end"))?;
                let grant = proof.required_end_work()?;
                if !proof.seal_physical_end(
                    &end,
                    &mut input,
                    &mut WorkBudget::new(grant),
                    cancel,
                )? {
                    return Err(Error::Invalid("control proof exhausted its end grant"));
                }
                transport.end_of_source()?;
            }
        }
    }
}
