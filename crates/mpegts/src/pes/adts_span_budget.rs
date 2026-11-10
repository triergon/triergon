use super::*;

const PID: u16 = 256;
const ENTITY: SourceId = SourceId(7);
const HEADER: [u8; 9] = [0, 0, 1, 0xc0, 0, 0, 0x80, 0, 0];
static RUNNING: AtomicBool = AtomicBool::new(false);

fn assembler(pool: &Budget) -> PesAssembler {
    let mut pes = PesAssembler::new(
        PID,
        false,
        pool.clone(),
        ParseLimits::default(),
        SourceEpochs::default(),
    )
    .unwrap();
    pes.allow_unbounded_adts();
    pes
}

fn poll(pes: &mut PesAssembler) -> Result<AssemblyStep<Pes>> {
    for _ in 0..64 {
        let mut work = WorkBudget::new(1);
        match pes.poll(&mut work, &RUNNING)? {
            AssemblyStep::Yield => assert_eq!(work.spent(), 1),
            next => return Ok(next),
        }
    }
    panic!("unit grants did not advance PES assembly");
}

fn yield_one(pes: &mut PesAssembler) {
    let mut work = WorkBudget::new(1);
    assert!(matches!(
        pes.poll(&mut work, &RUNNING),
        Ok(AssemblyStep::Yield)
    ));
    assert_eq!(work.spent(), 1);
}

#[test]
fn rejected_unbounded_adts_prepays_span_growth_before_advancing_source() {
    let pool = Budget::new(64 * 1024).unwrap();
    let mut pes = assembler(&pool);
    let start = [HEADER.as_slice(), b"a"].concat();
    pes.push(
        Bytes::copy_from_slice(&pool, &start).unwrap(),
        true,
        ENTITY,
        100,
    )
    .unwrap();
    assert!(matches!(poll(&mut pes), Ok(AssemblyStep::NeedPayload)));
    for (offset, bytes) in [(200, b"b"), (300, b"c")] {
        pes.push(
            Bytes::copy_from_slice(&pool, bytes).unwrap(),
            false,
            ENTITY,
            offset,
        )
        .unwrap();
        assert!(matches!(poll(&mut pes), Ok(AssemblyStep::NeedPayload)));
    }
    pes.push(
        Bytes::copy_from_slice(&pool, b"d").unwrap(),
        false,
        ENTITY,
        400,
    )
    .unwrap();
    assert_eq!(pes.spans.len(), 3);
    assert_eq!(pes.spans.capacity(), 3);
    let used = pool.used();
    for _ in 0..3 {
        yield_one(&mut pes);
        assert_eq!(pes.spans.capacity(), 3);
        assert_eq!(pool.used(), used);
        assert_eq!(pes.payload.as_ref().unwrap().position, 0);
    }

    let mut cancelled = WorkBudget::new(1);
    assert_eq!(
        pes.poll(&mut cancelled, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(cancelled.spent(), 0);
    assert_eq!(pes.spans.capacity(), 3);

    // The final unit pays for the reservation. There is no byte grant yet.
    yield_one(&mut pes);
    assert_eq!(pes.spans.capacity(), 4);
    assert_eq!(pes.spans.len(), 3);
    assert_eq!(pes.payload_len, 3);
    assert_eq!(pes.payload.as_ref().unwrap().position, 0);
    assert!(matches!(
        pes.poll(&mut WorkBudget::new(1), &RUNNING),
        Ok(AssemblyStep::NeedPayload)
    ));

    pes.end_of_source();
    let AssemblyStep::Output(packet) = poll(&mut pes).unwrap() else {
        panic!("unbounded ADTS did not seal at EOF");
    };
    assert_eq!(packet.header.as_bytes(), HEADER);
    assert_eq!(packet.payload_len, 4);
    assert_eq!(packet.spans().len(), 4);
    for (span, (offset, bytes)) in
        packet
            .spans()
            .iter()
            .zip([(109, b"a"), (200, b"b"), (300, b"c"), (400, b"d")])
    {
        assert_eq!(span.as_ref(), bytes);
        assert_eq!(
            span.source_range(),
            SourceRange {
                entity: ENTITY,
                offset,
                bytes: 1
            }
        );
    }
}

#[test]
fn rejected_unbounded_adts_span_growth_reports_permanent_limit() {
    let start = [HEADER.as_slice(), b"a"].concat();
    let tail = b"bc";
    let required = Budget::control_storage_bytes()
        + size_of::<SourceRange>()
        + 2 * Bytes::owner_overhead_bytes()
        + start.len()
        + tail.len()
        + 3 * size_of::<PesSpan>();
    let pool = Budget::new(required - 1).unwrap();
    let mut pes = assembler(&pool);
    pes.push(
        Bytes::copy_from_slice(&pool, &start).unwrap(),
        true,
        ENTITY,
        100,
    )
    .unwrap();
    assert!(matches!(poll(&mut pes), Ok(AssemblyStep::NeedPayload)));
    pes.push(
        Bytes::copy_from_slice(&pool, tail).unwrap(),
        false,
        ENTITY,
        200,
    )
    .unwrap();

    assert_eq!(
        poll(&mut pes).unwrap_err(),
        Error::Limit {
            requested: required,
            maximum: required - 1,
        }
    );
    pes.discard(SourceEpochs::default());
    assert!(matches!(pes.span_growth, SpanGrowth::Credit(0)));
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}

#[test]
fn rejected_unbounded_adts_span_growth_retries_after_caller_pressure() {
    let source = [HEADER.as_slice(), b"ad!"].concat();
    let middle = b"bc";
    let required = Budget::control_storage_bytes()
        + size_of::<SourceRange>()
        + 2 * Bytes::owner_overhead_bytes()
        + source.len()
        + middle.len()
        + 5 * size_of::<PesSpan>();
    let pool = Budget::new(required).unwrap();
    let mut pes = assembler(&pool);
    let bytes = Bytes::copy_from_slice(&pool, &source).unwrap();
    let last = bytes.slice(HEADER.len() + 1..HEADER.len() + 2).unwrap();
    pes.push(
        bytes.into_slice(0..HEADER.len() + 1).unwrap(),
        true,
        ENTITY,
        100,
    )
    .unwrap();
    assert!(matches!(poll(&mut pes), Ok(AssemblyStep::NeedPayload)));
    pes.push(
        Bytes::copy_from_slice(&pool, middle).unwrap(),
        false,
        ENTITY,
        200,
    )
    .unwrap();
    assert!(matches!(poll(&mut pes), Ok(AssemblyStep::NeedPayload)));
    pes.push(last, false, ENTITY, 300).unwrap();

    let caller = pool.reserve(1).unwrap();
    for _ in 0..=pes.spans.len() {
        yield_one(&mut pes);
    }
    assert!(matches!(pes.span_growth, SpanGrowth::Audit(_)));
    let mut cancelled = WorkBudget::new(0);
    assert_eq!(
        pes.poll(&mut cancelled, &AtomicBool::new(true))
            .unwrap_err(),
        Error::Cancelled
    );
    assert_eq!(cancelled.spent(), 0);
    let replacement = 3 * size_of::<PesSpan>();
    let pressure = Error::Backpressure {
        requested: replacement,
        available: replacement - 1,
    };
    assert_eq!(poll(&mut pes).unwrap_err(), pressure);
    let mut retry = WorkBudget::new(1);
    assert_eq!(pes.poll(&mut retry, &RUNNING).unwrap_err(), pressure);
    assert_eq!(retry.spent(), 0);
    drop(caller);
    assert!(matches!(
        pes.poll(&mut retry, &RUNNING),
        Ok(AssemblyStep::NeedPayload)
    ));
    assert_eq!(retry.spent(), 1);
    pes.end_of_source();
    let AssemblyStep::Output(packet) = poll(&mut pes).unwrap() else {
        panic!("unbounded ADTS did not seal at EOF");
    };
    assert_eq!(packet.header.as_bytes(), HEADER);
    assert_eq!(packet.header.declared_packet_length, 0);
    assert_eq!(packet.payload_len, 4);
    assert_eq!(
        packet.payload().collect::<Vec<_>>(),
        [b"a".as_slice(), b"bc", b"d"]
    );
    let ranges = packet.source_ranges().collect::<Vec<_>>();
    assert_eq!(
        ranges,
        [
            SourceRange {
                entity: ENTITY,
                offset: 109,
                bytes: 1
            },
            SourceRange {
                entity: ENTITY,
                offset: 200,
                bytes: 2
            },
            SourceRange {
                entity: ENTITY,
                offset: 300,
                bytes: 1
            },
        ]
    );
    assert!(
        packet.spans()[0]
            .backing_bytes()
            .shares_storage_with(packet.spans()[2].backing_bytes())
    );
    drop(packet);
    assert_eq!(pool.used(), Budget::control_storage_bytes());
}
