use std::sync::atomic::{AtomicBool, Ordering};
use triergon_core::{
    Budget, Bytes, Error, InputCursor, InputWindow, SourceCapabilities, SourceId, SourceLocation,
    Step, WorkBudget,
};
use triergon_mpegts::compat::opaque_video::{OpaqueRecovery, RecoveredOpaque, RecoveryTrigger};
use triergon_mpegts::compat::pes::TransportObservation;
use triergon_mpegts::psi::DescriptorNamespace;
use triergon_mpegts::{FaultKind, ParseLimits, TraceFault};

const SOURCE: SourceId = SourceId(1);
const SYNC_TAIL: &[u8] = include_bytes!("data/transport-sync-loss.ts");
const CONTINUATION: &[u8] =
    include_bytes!("../../../harness/fixtures/mpegts/finite-continuation-body-401.ts");

fn cursor(pool: &Budget, bytes: usize) -> InputCursor {
    InputCursor::new(
        pool,
        SOURCE,
        SourceCapabilities {
            length: Some(bytes as u64),
            random_access: false,
        },
        64,
    )
    .unwrap()
}

fn trigger(kind: FaultKind, error: Error, offset: u64, pid: Option<u16>) -> RecoveryTrigger {
    RecoveryTrigger::from_fault(&TraceFault {
        kind,
        pid,
        location: SourceLocation {
            source: SOURCE,
            offset,
        },
        error,
        rejected: None,
    })
    .unwrap()
}

fn recovery(input: &InputCursor, trigger: RecoveryTrigger) -> OpaqueRecovery {
    OpaqueRecovery::new(
        input,
        ParseLimits::default(),
        1,
        DescriptorNamespace::Unspecified,
        trigger,
    )
    .unwrap()
}

fn run(
    proof: &mut OpaqueRecovery,
    input: &mut InputCursor,
    raw: &Bytes,
    cancel_once: bool,
) -> Result<RecoveredOpaque, Error> {
    let cancel = AtomicBool::new(false);
    let mut interrupted = false;
    for _ in 0..100_000 {
        match proof.poll(input, &mut WorkBudget::new(1), &cancel)? {
            Step::Ready(output) => {
                assert!(!cancel_once || interrupted);
                return Ok(output);
            }
            Step::NeedInput(next) => {
                let offset = usize::try_from(next.offset).unwrap();
                if offset == raw.len() {
                    input.finish(next.source, next.token)?;
                } else {
                    input.provide_owned(InputWindow::new(
                        next.source,
                        next.token,
                        next.offset,
                        raw.slice(offset..offset + 1)?,
                    )?)?;
                }
            }
            Step::Yield => {
                if cancel_once && !interrupted && input.position() >= 376 {
                    cancel.store(true, Ordering::Relaxed);
                    assert!(matches!(
                        proof.poll(input, &mut WorkBudget::new(1), &cancel),
                        Err(Error::Cancelled)
                    ));
                    cancel.store(false, Ordering::Relaxed);
                    interrupted = true;
                }
            }
            Step::End(_) => panic!("source ended before its opaque observation"),
        }
    }
    panic!("single-unit grants stalled at offset {}", input.position());
}

#[test]
fn one_byte_windows_keep_the_bound_fault_and_retained_payload() {
    let pool = Budget::new(1 << 20).unwrap();
    let baseline = pool.used();
    let raw = Bytes::copy_from_slice(&pool, SYNC_TAIL).unwrap();
    let mut input = cursor(&pool, raw.len());
    let mut other = cursor(&pool, raw.len());
    let mut proof = recovery(
        &input,
        trigger(
            FaultKind::Transport,
            Error::Invalid("TS sync byte"),
            564,
            None,
        ),
    );
    assert!(matches!(
        proof.poll(&mut other, &mut WorkBudget::new(1), &AtomicBool::new(false)),
        Err(Error::StaleGeneration { .. })
    ));
    drop(other);
    let output = run(&mut proof, &mut input, &raw, true).unwrap();
    let end = output.physical_end().location();
    assert_eq!(end.offset, raw.len() as u64);
    assert!(matches!(
        proof.poll(&mut input, &mut WorkBudget::new(0), &AtomicBool::new(false)),
        Ok(Step::End(actual)) if actual == end
    ));
    assert!(
        output.source_finished().source_event_ordinal
            > output.diagnostic_context().source_event_ordinal
    );
    drop(proof);
    drop(input);
    drop(raw);
    assert_eq!(output.source_fault().location.offset, 564);
    assert_eq!(
        output
            .source_fault()
            .rejected
            .as_ref()
            .unwrap()
            .bytes()
            .as_ref(),
        &SYNC_TAIL[564..]
    );
    assert!(matches!(
        output.observations(),
        [TransportObservation::Accepted(_)]
    ));
    assert_eq!(output.payload_len(), 27);
    assert_eq!(
        output.payload().flatten().copied().collect::<Vec<_>>(),
        b"lane-ts-original-payload\0\x47\xff"
    );
    assert!(pool.used() > baseline);
    drop(output);
    assert_eq!(pool.used(), baseline);
}

#[test]
fn a_different_source_diagnostic_stops_the_bound_proof() {
    let pool = Budget::new(1 << 20).unwrap();
    let raw = Bytes::copy_from_slice(&pool, SYNC_TAIL).unwrap();
    let mut input = cursor(&pool, raw.len());
    let mut proof = recovery(
        &input,
        trigger(FaultKind::Transport, Error::Truncated, 564, None),
    );
    let error = run(&mut proof, &mut input, &raw, false).unwrap_err();
    assert_eq!(
        error,
        Error::Unsupported("opaque recovery diagnostic mismatch")
    );
    assert!(matches!(
        proof.poll(&mut input, &mut WorkBudget::new(4096), &AtomicBool::new(false)),
        Err(actual) if actual == error
    ));
}

#[test]
fn proof_requires_the_bound_cursor_to_start_at_the_first_byte() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut input = cursor(&pool, SYNC_TAIL.len());
    let trigger = trigger(
        FaultKind::Transport,
        Error::Invalid("TS sync byte"),
        564,
        None,
    );
    let mut proof = recovery(&input, trigger);
    let cancel = AtomicBool::new(false);
    let token = input.token();
    input
        .provide_inline(SOURCE, token, 0, &SYNC_TAIL[..1])
        .unwrap();
    assert!(matches!(
        input.read_discard(1, &mut WorkBudget::new(1), &cancel),
        Ok(Step::Ready(1))
    ));
    let mut work = WorkBudget::new(1);
    assert_eq!(
        proof.poll(&mut input, &mut work, &cancel).unwrap_err(),
        Error::Invalid("opaque recovery cursor advanced outside poll")
    );
    assert_eq!(work.remaining(), 1);
    assert!(
        OpaqueRecovery::new(
            &input,
            ParseLimits::default(),
            1,
            DescriptorNamespace::Unspecified,
            trigger,
        )
        .is_err()
    );
}

#[test]
fn retained_observations_grow_under_one_unit_grants() {
    let pool = Budget::new(1 << 20).unwrap();
    let mut bytes = SYNC_TAIL[..376].to_vec();
    for counter in 0..5 {
        let mut carrier: [u8; 188] = SYNC_TAIL[376..564].try_into().unwrap();
        carrier[3] = carrier[3] & 0xf0 | counter;
        bytes.extend(carrier);
    }
    let offset = bytes.len() as u64;
    bytes.extend(&SYNC_TAIL[564..]);
    let raw = Bytes::try_from_vec(&pool, bytes).unwrap();
    let mut input = cursor(&pool, raw.len());
    let mut proof = recovery(
        &input,
        trigger(
            FaultKind::Transport,
            Error::Invalid("TS sync byte"),
            offset,
            None,
        ),
    );
    let output = run(&mut proof, &mut input, &raw, false).unwrap();
    assert_eq!(output.observations().len(), 5);
    assert_eq!(output.payload_len(), 5 * 27);
    assert_eq!(output.source_fault().location.offset, offset);
}

#[test]
fn an_annex_b_prefix_across_the_continuation_stops_recovery() {
    let pool = Budget::new(1 << 20).unwrap();
    let trigger = trigger(
        FaultKind::Pes,
        Error::Invalid("bytes beyond finite PES"),
        752,
        Some(256),
    );
    let raw = Bytes::copy_from_slice(&pool, CONTINUATION).unwrap();
    let mut input = cursor(&pool, raw.len());
    let mut proof = recovery(&input, trigger);
    let output = run(&mut proof, &mut input, &raw, false).unwrap();
    assert_eq!(output.observations().len(), 2);
    assert_eq!(output.payload_len(), 401);
    drop(output);
    drop(proof);
    drop(input);

    let mut bytes = CONTINUATION.to_vec();
    let next_carrier = 4 * 188;
    let continuation = next_carrier + 5 + usize::from(bytes[next_carrier + 4]);
    bytes[next_carrier - 2..next_carrier].fill(0);
    bytes[continuation..continuation + 2].copy_from_slice(&[1, 0x65]);
    let raw = Bytes::try_from_vec(&pool, bytes).unwrap();
    let mut input = cursor(&pool, raw.len());
    let mut proof = recovery(&input, trigger);
    assert_eq!(
        run(&mut proof, &mut input, &raw, false).unwrap_err(),
        Error::Unsupported("recovery contains Annex-B video")
    );
}
